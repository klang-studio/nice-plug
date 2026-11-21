use std::sync::Mutex;

use crate::prelude::{
    AudioIOLayout, AuxiliaryBuffers, BoolParam, Buffer, Enum, EnumParam, FloatParam, FloatRange,
    IntParam, ProcessContext, ProcessStatus,
};

use super::*;

#[derive(Clone, Copy, Debug, Enum, PartialEq)]
enum Mode {
    #[id = "off"]
    Off,
    #[id = "on"]
    On,
}

struct StateParams {
    gain: FloatParam,
    enabled: BoolParam,
    steps: IntParam,
    mode: EnumParam<Mode>,
    secondary: EnumParam<Mode>,
    tertiary: EnumParam<Mode>,
    enum_changes: Arc<Mutex<Vec<(&'static str, Mode)>>>,
    remembered: Mutex<Option<String>>,
}

impl Default for StateParams {
    fn default() -> Self {
        let enum_changes = Arc::new(Mutex::new(Vec::new()));
        let record_change = |id| {
            let changes = Arc::clone(&enum_changes);
            Arc::new(move |mode| changes.lock().unwrap().push((id, mode)))
        };
        Self {
            gain: FloatParam::new(
                "Gain",
                0.5,
                FloatRange::Reversed(&FloatRange::Skewed {
                    min: 0.0,
                    max: 1.0,
                    factor: 2.0,
                }),
            ),
            enabled: BoolParam::new("Enabled", true),
            steps: IntParam::new(
                "Steps",
                1,
                IntRange::Reversed(&IntRange::Linear { min: -2, max: 4 }),
            ),
            mode: EnumParam::new("Mode", Mode::Off).with_callback(record_change("mode")),
            secondary: EnumParam::new("Secondary mode", Mode::On)
                .with_callback(record_change("secondary")),
            tertiary: EnumParam::new("Tertiary mode", Mode::Off),
            enum_changes,
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
    fn lookup(&self) -> [(&str, ParamPtr); 6] {
        [
            ("gain", self.gain.as_ptr()),
            ("enabled", self.enabled.as_ptr()),
            ("steps", self.steps.as_ptr()),
            ("mode", self.mode.as_ptr()),
            ("secondary", self.secondary.as_ptr()),
            ("tertiary", self.tertiary.as_ptr()),
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

    fn validate_state(state: &PluginState) -> bool {
        state
            .fields
            .get("remembered")
            .is_none_or(|value| !value.is_empty())
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
fn unknown_stable_enum_id_never_invokes_value_callbacks() {
    let plugin = StatePlugin::default();
    *plugin.0.remembered.lock().unwrap() = Some("existing".into());
    let json = br#"{"version":"1.0.0","params":{"gain":{"f32":0.25},"mode":{"string":"on"},"secondary":{"string":"off"},"tertiary":{"string":"unknown"}},"fields":{"remembered":"new"}}"#;
    let mut state = deserialize_json_bounded(json, json.len()).unwrap();
    assert!(!apply(&mut state, &plugin));
    assert_eq!(plugin.0.gain.value(), 0.5);
    assert_eq!(plugin.0.mode.value(), Mode::Off);
    assert_eq!(plugin.0.secondary.value(), Mode::On);
    assert_eq!(plugin.0.tertiary.value(), Mode::Off);
    assert_eq!(
        *plugin.0.remembered.lock().unwrap(),
        Some("existing".into())
    );
    assert!(plugin.0.enum_changes.lock().unwrap().is_empty());

    state
        .params
        .insert("tertiary".into(), ParamValue::String("on".into()));
    assert!(apply(&mut state, &plugin));
    assert_eq!(plugin.0.gain.value(), 0.25);
    assert_eq!(plugin.0.mode.value(), Mode::On);
    assert_eq!(plugin.0.secondary.value(), Mode::Off);
    assert_eq!(plugin.0.tertiary.value(), Mode::On);
    assert_eq!(*plugin.0.remembered.lock().unwrap(), Some("new".into()));
    assert_eq!(
        *plugin.0.enum_changes.lock().unwrap(),
        [("mode", Mode::On), ("secondary", Mode::Off)]
    );
}

#[test]
fn enum_restore_updates_the_base_under_modulation_without_repeating_callbacks() {
    let plugin = StatePlugin::default();
    let mode = &plugin.0.mode;
    // SAFETY: This test owns the parameter and no processing is active.
    assert!(unsafe { mode._internal_modulate_value(1.0) });
    assert_eq!(mode.value(), Mode::On);
    assert_eq!(mode.unmodulated_plain_value(), Mode::Off);
    plugin.0.enum_changes.lock().unwrap().clear();

    let json = br#"{"version":"1.0.0","params":{"mode":{"string":"on"}},"fields":{}}"#;
    let mut state = deserialize_json_bounded(json, json.len()).unwrap();
    assert!(apply(&mut state, &plugin));
    assert_eq!(mode.value(), Mode::On);
    assert_eq!(mode.unmodulated_plain_value(), Mode::On);

    // SAFETY: This test still owns the parameter and no processing is active.
    assert!(!unsafe { mode._internal_modulate_value(0.0) });
    assert_eq!(mode.value(), Mode::On);
    assert!(plugin.0.enum_changes.lock().unwrap().is_empty());
}

#[test]
fn restore_commits_float_and_bool_bases_masked_by_modulation() {
    let plugin = StatePlugin::default();
    let gain = &plugin.0.gain;
    let enabled = &plugin.0.enabled;
    // SAFETY: The test owns these parameters and no processing is active.
    unsafe {
        gain._internal_modulate_value(-1.0);
        enabled._internal_modulate_value(1.0);
    }
    assert_eq!(gain.value(), 1.0);
    assert!(enabled.value());

    let json = br#"{"version":"1.0.0","params":{"gain":{"f32":0.75},"enabled":{"bool":false}},"fields":{}}"#;
    let mut state = deserialize_json_bounded(json, json.len()).unwrap();
    assert!(apply(&mut state, &plugin));
    assert_eq!(gain.value(), 1.0);
    assert!(enabled.value());
    assert_eq!(gain.unmodulated_plain_value(), 0.75);
    assert!(!enabled.unmodulated_plain_value());

    // SAFETY: No processing can observe these parameters concurrently.
    unsafe {
        gain._internal_modulate_value(0.0);
        enabled._internal_modulate_value(0.0);
    }
    assert_eq!(gain.value(), 0.75);
    assert!(!enabled.value());
}

#[test]
fn numeric_restore_rejects_invalid_values_and_accepts_both_endpoints() {
    let plugin = StatePlugin::default();
    let json = br#"{"version":"1.0.0","params":{"gain":{"f32":0.25},"mode":{"string":"on"},"steps":{"i32":3}},"fields":{"remembered":"new"}}"#;
    let state = deserialize_json_bounded(json, json.len()).unwrap();

    for (id, value) in [
        ("gain", ParamValue::F32(-0.25)),
        ("gain", ParamValue::F32(1.25)),
        ("gain", ParamValue::F32(f32::INFINITY)),
        ("gain", ParamValue::F32(f32::NAN)),
        ("steps", ParamValue::I32(-3)),
        ("steps", ParamValue::I32(5)),
    ] {
        let mut invalid = state.clone();
        invalid.params.insert(id.into(), value);
        assert!(!apply(&mut invalid, &plugin));
        assert_eq!(plugin.0.gain.value(), 0.5);
        assert_eq!(plugin.0.steps.value(), 1);
        assert!(plugin.0.enum_changes.lock().unwrap().is_empty());
        assert!(plugin.0.remembered.lock().unwrap().is_none());
    }

    for (gain, steps) in [(0.0, -2), (1.0, 4)] {
        let mut valid = state.clone();
        valid.params.insert("gain".into(), ParamValue::F32(gain));
        valid.params.insert("steps".into(), ParamValue::I32(steps));
        assert!(apply(&mut valid, &plugin));
        assert_eq!(plugin.0.gain.value(), gain);
        assert_eq!(plugin.0.steps.value(), steps);
    }
}

#[test]
fn state_encoding_and_decoding_share_the_stored_payload_limit() {
    let state = PluginState {
        version: StatePlugin::VERSION.into(),
        params: BTreeMap::new(),
        fields: [("remembered".into(), "a".repeat(1024))].into(),
    };
    let json = serde_json::to_vec(&state).unwrap();
    assert!(deserialize_json_bounded(&json, json.len() - 1).is_none());

    let encoded = encode_json(&state, json.len()).unwrap();
    let decoded = deserialize_json_bounded(&encoded, encoded.len()).unwrap();
    assert_eq!(decoded.fields, state.fields);
    assert!(deserialize_json_bounded(&encoded, encoded.len() - 1).is_none());
    assert!(encode_json(&state, encoded.len() - 1).is_err());

    #[cfg(feature = "zstd")]
    {
        // Reproduce the legacy compressor and exercise the same expansion as a large CLAP
        // state record with a smaller cap.
        let legacy = zstd::encode_all(json.as_slice(), zstd::DEFAULT_COMPRESSION_LEVEL).unwrap();
        assert!(legacy.len() < json.len());
        let decoded = deserialize_json_bounded(&legacy, legacy.len()).unwrap();
        assert_eq!(decoded.fields, state.fields);

        let truncated = &legacy[..legacy.len() - 1];
        assert!(deserialize_json_bounded(truncated, legacy.len()).is_none());
        let mut corrupted = legacy;
        corrupted.extend_from_slice(b"trailing garbage");
        assert!(deserialize_json_bounded(&corrupted, corrupted.len()).is_none());
    }
}

#[test]
fn rejected_persistent_field_leaves_parameters_and_fields_unchanged() {
    let plugin = StatePlugin::default();
    *plugin.0.remembered.lock().unwrap() = Some("existing".into());
    let json = br#"{"params":{"gain":{"f32":0.25}},"fields":{"remembered":""}}"#;
    let mut state = deserialize_json_bounded(json, json.len()).unwrap();
    assert!(!apply(&mut state, &plugin));
    assert_eq!(plugin.0.gain.value(), 0.5);
    assert_eq!(
        *plugin.0.remembered.lock().unwrap(),
        Some("existing".into())
    );
}
