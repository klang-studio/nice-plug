use std::sync::Mutex;

use crate::prelude::{
    AudioIOLayout, AuxiliaryBuffers, Buffer, Enum, EnumParam, FloatParam, FloatRange,
    ProcessContext, ProcessStatus,
};

use super::*;

#[derive(Clone, Copy, Enum, PartialEq)]
enum Mode {
    #[id = "off"]
    Off,
    #[id = "on"]
    On,
}

struct StateParams {
    gain: FloatParam,
    mode: EnumParam<Mode>,
    secondary: EnumParam<Mode>,
    remembered: Mutex<Option<String>>,
}

impl Default for StateParams {
    fn default() -> Self {
        Self {
            gain: FloatParam::new("Gain", 0.5, FloatRange::Linear { min: 0.0, max: 1.0 }),
            mode: EnumParam::new("Mode", Mode::Off),
            secondary: EnumParam::new("Secondary mode", Mode::Off),
            remembered: Mutex::new(None),
        }
    }
}

// SAFETY: The parameter pointers remain valid as long as the test retains the StateParams Arc.
unsafe impl Params for StateParams {
    fn param_map(&self) -> Vec<(String, ParamPtr, String)> {
        self.lookup()
            .into_iter()
            .map(|(id, pointer)| (id.into(), pointer, String::new()))
            .collect()
    }

    fn deserialize_fields(&self, fields: &BTreeMap<String, String>) {
        *self.remembered.lock().unwrap() = fields.get("remembered").cloned();
    }
}

impl StateParams {
    fn lookup(&self) -> [(&str, ParamPtr); 3] {
        [
            ("gain", self.gain.as_ptr()),
            ("mode", self.mode.as_ptr()),
            ("secondary", self.secondary.as_ptr()),
        ]
    }
}

#[derive(Default)]
struct StatePlugin(Arc<StateParams>);

impl Plugin for StatePlugin {
    const NAME: &'static str = "State test";
    const VENDOR: &'static str = "nice-plug";
    const URL: &'static str = "";
    const EMAIL: &'static str = "";
    const VERSION: &'static str = "1.0.0";
    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[];
    type Editor = ();
    type SysExMessage = ();
    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.0.clone()
    }

    fn process(
        &mut self,
        _: &mut Buffer,
        _: &mut AuxiliaryBuffers,
        _: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        ProcessStatus::Normal
    }

    fn filter_state(state: &mut PluginState) {
        if state.version.is_empty()
            && let Some(ParamValue::Bool(enabled)) = state.params.get("gain")
        {
            state.params.insert(
                "gain".into(),
                ParamValue::F32(if *enabled { 1.0 } else { 0.0 }),
            );
        }
    }
}

fn apply(state: &mut PluginState, plugin: &StatePlugin) -> bool {
    // SAFETY: The lookup and Arc both refer to the same live instance's parameters.
    unsafe {
        deserialize_object::<StatePlugin>(
            state,
            plugin.params(),
            |id| {
                plugin
                    .0
                    .lookup()
                    .into_iter()
                    .find(|(key, _)| *key == id)
                    .map(|(_, pointer)| pointer)
            },
            None,
        )
    }
}

#[test]
fn legacy_json_is_migrated_and_removed_parameters_are_ignored() {
    let json = br#"{"params":{"gain":{"bool":true},"mode":{"i32":1},"removed":{"f32":0.3}},"fields":{"remembered":"old"}}"#;
    let mut state = deserialize_json_bounded(json, json.len()).unwrap();
    let plugin = StatePlugin::default();
    assert!(apply(&mut state, &plugin));
    assert_eq!(plugin.0.gain.value(), 1.0);
    assert!(plugin.0.mode.value() == Mode::On);
    assert_eq!(*plugin.0.remembered.lock().unwrap(), Some("old".into()));
}

#[test]
fn invalid_typed_value_leaves_the_existing_parameters_and_fields_unchanged() {
    let plugin = StatePlugin::default();
    *plugin.0.remembered.lock().unwrap() = Some("existing".into());
    let json = br#"{"version":"1.0.0","params":{"gain":{"f32":0.25},"mode":{"bool":true}},"fields":{"remembered":"new"}}"#;
    let mut state = deserialize_json_bounded(json, json.len()).unwrap();
    assert!(!apply(&mut state, &plugin));
    assert_eq!(plugin.0.gain.value(), 0.5);
    assert!(plugin.0.mode.value() == Mode::Off);
    assert_eq!(
        *plugin.0.remembered.lock().unwrap(),
        Some("existing".into())
    );

    for index in [-1, 2] {
        state.params.insert("mode".into(), ParamValue::I32(index));
        assert!(!apply(&mut state, &plugin));
        assert_eq!(plugin.0.gain.value(), 0.5);
        assert!(plugin.0.mode.value() == Mode::Off);
    }
}

#[test]
fn unknown_stable_enum_ids_fail_without_applying_other_values_or_fields() {
    let plugin = StatePlugin::default();
    let json = br#"{"version":"1.0.0","params":{"gain":{"f32":0.25},"mode":{"string":"on"},"secondary":{"string":"unknown"}},"fields":{"remembered":"new"}}"#;
    let mut state = deserialize_json_bounded(json, json.len()).unwrap();
    assert!(!apply(&mut state, &plugin));
    assert_eq!(plugin.0.gain.value(), 0.5);
    assert!(plugin.0.secondary.value() == Mode::Off);
    assert!(plugin.0.remembered.lock().unwrap().is_none());

    // The pinned setter resolves symbolic IDs by applying them. A prior successful setter can
    // already have changed one enum when another symbolic ID proves invalid.
    assert!(plugin.0.mode.value() == Mode::On);
}

#[test]
fn state_encoding_and_decoding_share_the_payload_limit() {
    let state = PluginState {
        version: StatePlugin::VERSION.into(),
        params: BTreeMap::new(),
        fields: [("remembered".into(), "a".repeat(1024))].into(),
    };
    let json = serde_json::to_vec(&state).unwrap();
    assert!(encode_json(&state, json.len() - 1).is_err());
    assert!(deserialize_json_bounded(&json, json.len() - 1).is_none());

    let encoded = encode_json(&state, json.len()).unwrap();
    let decoded = deserialize_json_bounded(&encoded, json.len()).unwrap();
    assert_eq!(decoded.fields, state.fields);

    #[cfg(feature = "zstd")]
    {
        let limit = json.len() - 1;
        assert!(encoded.len() < limit);
        let output = decode_zstd(&encoded, limit - 64).unwrap();
        assert_eq!(output.len(), limit - 63);
        assert!(deserialize_json_bounded(&encoded, limit).is_none());
    }
}
