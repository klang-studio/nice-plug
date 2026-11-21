use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

use anyhow::{Context, Result, ensure};
use nice_plug_core::{
    audio_setup::BufferConfig,
    params::{InternalParamMut, Param, Params, internals::ParamPtr},
    plugin::{ParamValue, Plugin, PluginState},
};

/// Create a parameters iterator from the hashtables stored in the plugin wrappers. This avoids
/// having to call `.param_map()` again, which may include expensive user written code.
pub(crate) fn make_params_iter<'a>(
    param_by_hash: &'a HashMap<u32, ParamPtr>,
    param_id_to_hash: &'a HashMap<String, u32>,
) -> impl IntoIterator<Item = (&'a String, ParamPtr)> {
    param_id_to_hash.iter().filter_map(|(param_id_str, hash)| {
        let param_ptr = param_by_hash.get(hash)?;
        Some((param_id_str, *param_ptr))
    })
}

/// Create a getter function that gets a parameter from the hashtables stored in the plugin by
/// string ID.
pub(crate) fn make_params_getter<'a>(
    param_by_hash: &'a HashMap<u32, ParamPtr>,
    param_id_to_hash: &'a HashMap<String, u32>,
) -> impl Fn(&str) -> Option<ParamPtr> + 'a {
    |param_id_str| {
        param_id_to_hash
            .get(param_id_str)
            .and_then(|hash| param_by_hash.get(hash))
            .copied()
    }
}

/// Serialize a plugin's state to a state object. This is separate from [`serialize_json()`] to
/// allow passing the raw object directly to the plugin. The parameters are not pulled directly from
/// `plugin_params` by default to avoid unnecessary allocations in the `.param_map()` method, as the
/// plugin wrappers will already have a list of parameters handy. See [`make_params_iter()`].
pub(crate) unsafe fn serialize_object<'a, P: Plugin>(
    plugin_params: Arc<dyn Params>,
    params_iter: impl IntoIterator<Item = (&'a String, ParamPtr)>,
) -> PluginState {
    // We'll serialize parameter values as a simple `string_param_id: display_value` map.
    // NOTE: If the plugin is being modulated (and the plugin is a CLAP plugin in Bitwig Studio),
    //       then this should save the values without any modulation applied to it
    let params: BTreeMap<_, _> = unsafe {
        params_iter
            .into_iter()
            .map(|(param_id_str, param_ptr)| match param_ptr {
                ParamPtr::FloatParam(p) => (
                    param_id_str.clone(),
                    ParamValue::F32((*p).unmodulated_plain_value()),
                ),
                ParamPtr::IntParam(p) => (
                    param_id_str.clone(),
                    ParamValue::I32((*p).unmodulated_plain_value()),
                ),
                ParamPtr::BoolParam(p) => (
                    param_id_str.clone(),
                    ParamValue::Bool((*p).unmodulated_plain_value()),
                ),
                ParamPtr::EnumParam(p) => (
                    // Enums are either serialized based on the active variant's index (which may not be
                    // the same as the discriminator), or a custom set stable string ID. The latter
                    // allows the variants to be reordered.
                    param_id_str.clone(),
                    match (*p).unmodulated_plain_id() {
                        Some(id) => ParamValue::String(id.to_owned()),
                        None => ParamValue::I32((*p).unmodulated_plain_value()),
                    },
                ),
            })
            .collect()
    };

    // The plugin can also persist arbitrary fields alongside its parameters. This is useful for
    // storing things like sample data.
    let fields = plugin_params.serialize_fields();

    PluginState {
        version: String::from(P::VERSION),
        params,
        fields,
    }
}

/// Serialize a plugin's state as JSON for the VST3 state stream.
#[cfg(feature = "vst3")]
pub(crate) unsafe fn serialize_json<'a, P: Plugin>(
    plugin_params: Arc<dyn Params>,
    params_iter: impl IntoIterator<Item = (&'a String, ParamPtr)>,
) -> Result<Vec<u8>> {
    unsafe { serialize_json_bounded::<P>(plugin_params, params_iter, usize::MAX) }
}

/// Serialize state with the same decoded and encoded size limit used on load.
pub(crate) unsafe fn serialize_json_bounded<'a, P: Plugin>(
    plugin_params: Arc<dyn Params>,
    params_iter: impl IntoIterator<Item = (&'a String, ParamPtr)>,
    limit: usize,
) -> Result<Vec<u8>> {
    let state = unsafe { serialize_object::<P>(plugin_params, params_iter) };
    encode_json(&state, limit)
}

fn encode_json(state: &PluginState, limit: usize) -> Result<Vec<u8>> {
    let json = serde_json::to_vec(state).context("Could not format as JSON")?;
    ensure!(json.len() <= limit, "Decoded state exceeds the size limit");

    #[cfg(feature = "zstd")]
    {
        let compressed = zstd::encode_all(json.as_slice(), zstd::DEFAULT_COMPRESSION_LEVEL)
            .context("Could not compress state")?;
        ensure!(
            compressed.len() <= limit,
            "Stored state exceeds the size limit"
        );
        Ok(compressed)
    }
    #[cfg(not(feature = "zstd"))]
    Ok(json)
}

/// Deserialize a plugin's state from a [`PluginState`] object. This is used to allow the plugin to
/// do its own internal preset management. Returns `false` and logs an error if the state could not
/// be deserialized.
///
/// This uses a parameter getter function to avoid having to rebuild the parameter map, which may
/// include expensive user written code. See [`make_params_getter()`].
///
/// Make sure to reactivate plugin after deserializing the state so it can react to the new
/// parameter values. The smoothers have already been reset by this function.
///
/// The [`Plugin`] argument is used to call [`Plugin::filter_state()`] just before loading the
/// state.
pub(crate) unsafe fn deserialize_object<P: Plugin>(
    state: &mut PluginState,
    plugin_params: Arc<dyn Params>,
    params_getter: impl Fn(&str) -> Option<ParamPtr>,
    current_buffer_config: Option<&BufferConfig>,
) -> bool {
    // This lets the plugin perform migrations on old state if needed
    P::filter_state(state);

    // Validate after the plugin's migration, before changing any persisted value or field.
    // IDs removed in a newer version remain compatible.
    for (param_id, value) in &state.params {
        let Some(param) = params_getter(param_id) else {
            continue;
        };
        let valid = match (param, value) {
            (ParamPtr::FloatParam(_), ParamValue::F32(value)) => value.is_finite(),
            (ParamPtr::IntParam(_), ParamValue::I32(_))
            | (ParamPtr::BoolParam(_), ParamValue::Bool(_))
            | (ParamPtr::EnumParam(_), ParamValue::String(_)) => true,
            (ParamPtr::EnumParam(param), ParamValue::I32(index)) => {
                // SAFETY: This pointer comes from the live parameter map.
                usize::try_from(*index).is_ok_and(|index| index < unsafe { (*param).len() })
            }
            _ => false,
        };
        if !valid {
            crate::nice_warn!("Invalid serialized value for parameter {param_id:?}");
            return false;
        }
    }

    let sample_rate = current_buffer_config.map(|c| c.sample_rate);
    // The pinned enum API only exposes a mutating stable-ID lookup. Resolve these first:
    // a rejected ID leaves other parameters and fields unchanged, though earlier enum IDs
    // in this same pass may already have been applied.
    for (param_id, value) in &state.params {
        if let (ParamValue::String(id), Some(ParamPtr::EnumParam(param))) =
            (value, params_getter(param_id))
        {
            // SAFETY: This pointer comes from the live parameter map.
            if !unsafe { (*param).set_from_id(id) } {
                crate::nice_warn!("Unknown ID {id:?} for enum parameter {param_id:?}");
                return false;
            }
            if let Some(sample_rate) = sample_rate {
                unsafe { (*param)._internal_update_smoother(sample_rate, true) };
            }
        }
    }

    for (param_id_str, param_value) in &state.params {
        let param_ptr = match params_getter(param_id_str.as_str()) {
            Some(ptr) => ptr,
            None => {
                #[cfg(debug_assertions)]
                crate::nice_warn!("Unknown parameter: {}", param_id_str);
                continue;
            }
        };

        unsafe {
            match (param_ptr, param_value) {
                (ParamPtr::FloatParam(p), ParamValue::F32(v)) => {
                    (*p)._internal_set_plain_value(*v);
                }
                (ParamPtr::IntParam(p), ParamValue::I32(v)) => {
                    (*p)._internal_set_plain_value(*v);
                }
                (ParamPtr::BoolParam(p), ParamValue::Bool(v)) => {
                    (*p)._internal_set_plain_value(*v);
                }
                // Enums are either serialized based on the active variant's index (which may not be the
                // same as the discriminator), or a custom set stable string ID. The latter allows the
                // variants to be reordered.
                (ParamPtr::EnumParam(p), ParamValue::I32(variant_idx)) => {
                    (*p)._internal_set_plain_value(*variant_idx);
                }
                // Stable enum IDs were applied above.
                (ParamPtr::EnumParam(_), ParamValue::String(_)) => continue,
                _ => continue, // Already checked before applying any values.
            }
        }

        // Make sure everything starts out in sync
        if let Some(sample_rate) = sample_rate {
            unsafe { param_ptr._internal_update_smoother(sample_rate, true) };
        }
    }

    // The plugin can also persist arbitrary fields alongside its parameters. This is useful for
    // storing things like sample data.
    plugin_params.deserialize_fields(&state.fields);

    true
}

/// Decode VST3 state without applying it.
#[cfg(feature = "vst3")]
pub(crate) unsafe fn deserialize_json(state: &[u8]) -> Option<PluginState> {
    deserialize_json_bounded(state, usize::MAX)
}

/// Decode bounded CLAP state without modifying any plugin parameters.
pub(crate) fn deserialize_json_bounded(state: &[u8], limit: usize) -> Option<PluginState> {
    match decode_json(state, limit) {
        Ok(state) => Some(state),
        Err(error) => {
            crate::nice_warn!("Error while deserializing plugin state: {error}");
            None
        }
    }
}

fn decode_json(state: &[u8], limit: usize) -> Result<PluginState> {
    ensure!(state.len() <= limit, "Stored state exceeds the size limit");

    #[cfg(feature = "zstd")]
    {
        // Old plugins could save plain JSON before compression was enabled. Try that format
        // only if the compressed decoder fails, never after a successful but oversized decode.
        if let Ok(decompressed) = decode_zstd(state, limit) {
            ensure!(
                decompressed.len() <= limit,
                "Decoded state exceeds the size limit"
            );
            return serde_json::from_slice(&decompressed).context("Invalid plugin state JSON");
        }
    }
    serde_json::from_slice(state).context("Invalid plugin state JSON")
}

#[cfg(feature = "zstd")]
fn decode_zstd(state: &[u8], limit: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read;

    let mut decoder = zstd::stream::read::Decoder::with_buffer(state)?;
    if limit != usize::MAX {
        // Preserve the default compressor's working window even for short frames.
        // Larger windows still cannot exceed the rounded-up state cap.
        decoder.window_log_max((limit.max(1 << 21) - 1).ilog2() + 1)?;
    }
    let mut output = Vec::new();
    // Read one extra byte to distinguish a document exactly at the cap from a longer one.
    decoder
        .take((limit as u64).saturating_add(1))
        .read_to_end(&mut output)?;
    Ok(output)
}

#[cfg(test)]
mod tests;
