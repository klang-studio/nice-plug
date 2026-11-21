use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

use anyhow::{Context, Result, ensure};
use nice_plug_core::{
    audio_setup::BufferConfig,
    params::{
        InternalParamMut, Param, Params,
        internals::ParamPtr,
        range::{FloatRange, IntRange},
    },
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

/// Serialize state with the same stored payload limit used on load.
pub(crate) unsafe fn serialize_json_bounded<'a, P: Plugin>(
    plugin_params: Arc<dyn Params>,
    params_iter: impl IntoIterator<Item = (&'a String, ParamPtr)>,
    limit: usize,
) -> Result<Vec<u8>> {
    let state = unsafe { serialize_object::<P>(plugin_params, params_iter) };
    encode_json(&state, limit)
}

fn encode_json(state: &PluginState, limit: usize) -> Result<Vec<u8>> {
    #[cfg(feature = "zstd")]
    let payload = {
        // Write JSON straight to the compressor so large compressible states do not need a
        // separate uncompressed buffer.
        let encoder =
            zstd::stream::write::Encoder::new(Vec::new(), zstd::DEFAULT_COMPRESSION_LEVEL)
                .context("Could not create state compressor")?;
        let mut writer = std::io::BufWriter::new(encoder);
        serde_json::to_writer(&mut writer, state).context("Could not write compressed JSON")?;
        writer
            .into_inner()
            .map_err(|error| error.into_error())
            .context("Could not flush compressed JSON")?
            .finish()
            .context("Could not finish compressing state")?
    };
    #[cfg(not(feature = "zstd"))]
    let payload = serde_json::to_vec(state).context("Could not format as JSON")?;

    ensure!(
        payload.len() <= limit,
        "Stored state exceeds the size limit"
    );
    Ok(payload)
}

fn float_range_contains(range: FloatRange, value: f32) -> bool {
    match range {
        FloatRange::Linear { min, max }
        | FloatRange::Skewed { min, max, .. }
        | FloatRange::SymmetricalSkewed { min, max, .. } => {
            value.is_finite() && (min..=max).contains(&value)
        }
        FloatRange::Reversed(range) => float_range_contains(*range, value),
    }
}

fn int_range_contains(range: IntRange, value: i32) -> bool {
    match range {
        IntRange::Linear { min, max } => (min..=max).contains(&value),
        IntRange::Reversed(range) => int_range_contains(*range, value),
    }
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
/// The [`Plugin`] argument supplies the migration and preflight validation hooks.
pub(crate) unsafe fn deserialize_object<P: Plugin>(
    state: &mut PluginState,
    plugin_params: Arc<dyn Params>,
    params_getter: impl Fn(&str) -> Option<ParamPtr>,
    current_buffer_config: Option<&BufferConfig>,
) -> bool {
    // This lets the plugin perform migrations on old state if needed
    P::filter_state(state);
    if !P::validate_state(state) {
        crate::nice_warn!("Plugin rejected the serialized state");
        return false;
    }

    // Validate after migration and resolve stable enum IDs before calling any setters.
    // IDs removed in a newer version remain compatible.
    let mut values = Vec::new();
    for (param_id, value) in &state.params {
        let Some(param) = params_getter(param_id) else {
            #[cfg(debug_assertions)]
            crate::nice_warn!("Unknown parameter: {}", param_id);
            continue;
        };
        // SAFETY: The pointers come from the live parameter map supplied by the caller.
        let value = unsafe {
            match (param, value) {
                (ParamPtr::FloatParam(p), ParamValue::F32(value))
                    if float_range_contains((*p).range(), *value) =>
                {
                    Some(ParamValue::F32(*value))
                }
                (ParamPtr::IntParam(p), ParamValue::I32(value))
                    if int_range_contains((*p).range(), *value) =>
                {
                    Some(ParamValue::I32(*value))
                }
                (ParamPtr::BoolParam(_), ParamValue::Bool(value)) => Some(ParamValue::Bool(*value)),
                (ParamPtr::EnumParam(p), ParamValue::I32(index))
                    if usize::try_from(*index).is_ok_and(|index| index < (*p).len()) =>
                {
                    Some(ParamValue::I32(*index))
                }
                (ParamPtr::EnumParam(p), ParamValue::String(id)) => {
                    (*p).index_from_id(id).map(ParamValue::I32)
                }
                _ => None,
            }
        };
        let Some(value) = value else {
            crate::nice_warn!("Invalid serialized value for parameter {param_id:?}");
            return false;
        };
        values.push((param, value));
    }

    let sample_rate = current_buffer_config.map(|c| c.sample_rate);
    for (param, value) in values {
        // SAFETY: These are the same live parameters and validated plain values from above.
        unsafe {
            match (param, value) {
                (ParamPtr::FloatParam(p), ParamValue::F32(v)) => {
                    (*p)._internal_set_plain_value(v);
                }
                (ParamPtr::IntParam(p), ParamValue::I32(v)) => {
                    (*p)._internal_set_plain_value(v);
                }
                (ParamPtr::BoolParam(p), ParamValue::Bool(v)) => {
                    (*p)._internal_set_plain_value(v);
                }
                (ParamPtr::EnumParam(p), ParamValue::I32(v)) => {
                    (*p)._internal_set_plain_value(v);
                }
                _ => unreachable!("Only validated typed values are applied"),
            }

            // Make sure everything starts out in sync.
            if let Some(sample_rate) = sample_rate {
                param._internal_update_smoother(sample_rate, true);
            }
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
    if let Ok(decoder) = zstd::stream::read::Decoder::with_buffer(state)
        && let Ok(decoded) = serde_json::from_reader(std::io::BufReader::new(decoder))
    {
        // The CLAP limit is on stored bytes. Stream the decoded JSON into the state object,
        // since legacy compressed records can expand past that limit.
        return Ok(decoded);
    }

    // Plain JSON remains readable after compression is enabled.
    serde_json::from_slice(state).context("Invalid plugin state JSON")
}

#[cfg(test)]
mod tests;
