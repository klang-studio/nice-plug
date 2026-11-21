use std::cell::Cell;
use std::ptr;

use nice_plug_core::context::process::SendEventError;

use crate::prelude::*;

use super::*;

struct ProbeParams {
    gain: FloatParam,
    mix: FloatParam,
}

impl Default for ProbeParams {
    fn default() -> Self {
        Self {
            gain: FloatParam::new("Gain", 0.375, FloatRange::Linear { min: 0.0, max: 1.0 }),
            mix: FloatParam::new("Mix", 0.5, FloatRange::Linear { min: 0.0, max: 1.0 }),
        }
    }
}

// SAFETY: The pointer refers to a parameter owned by the ProbeParams Arc.
unsafe impl Params for ProbeParams {
    fn param_map(&self) -> Vec<(String, ParamPtr, String)> {
        vec![
            ("gain".into(), self.gain.as_ptr(), String::new()),
            ("mix".into(), self.mix.as_ptr(), String::new()),
        ]
    }
}

thread_local! {
    static OBSERVED_GAINS: Cell<(Option<f32>, Option<f32>)> = const { Cell::new((None, None)) };
}

#[derive(Default)]
struct QueueProbe {
    params: Arc<ProbeParams>,
}

impl Plugin for QueueProbe {
    const NAME: &'static str = "Parameter delivery test";
    const VENDOR: &'static str = "klang-studio";
    const URL: &'static str = "";
    const EMAIL: &'static str = "";
    const VERSION: &'static str = "0";
    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[];
    const MIDI_OUTPUT: MidiConfig = MidiConfig::Basic;
    type Editor = ();
    type SysExMessage = ();
    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn process(
        &mut self,
        _: &mut Buffer,
        _: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        let gain = Some(self.params.gain.value());
        let (first, _) = OBSERVED_GAINS.get();
        OBSERVED_GAINS.set(first.map_or((gain, None), |first| (Some(first), gain)));
        let note = |timing| NoteEvent::NoteOn {
            timing,
            voice_id: VoiceID::Wildcard,
            channel: Channel::Number(0),
            key: Key::Number(60),
            velocity: 1.0,
        };
        assert!(context.try_send_event(note(5)).is_ok());
        assert!(matches!(
            context.try_send_event(note(2)),
            Err((
                NoteEvent::NoteOn { timing: 2, .. },
                SendEventError::OutOfOrder
            ))
        ));
        ProcessStatus::Normal
    }
}

impl ClapPlugin for QueueProbe {
    const CLAP_ID: &'static str = "org.klang-studio.komponist.delivery-test";
    const CLAP_DESCRIPTION: Option<&'static str> = None;
    const CLAP_MANUAL_URL: Option<&'static str> = None;
    const CLAP_SUPPORT_URL: Option<&'static str> = None;
    const CLAP_FEATURES: &'static [ClapFeature] = &[];
}

unsafe extern "C" fn request_callback(host: *const clap_host) {
    let counter = unsafe { &*(*host).host_data.cast::<Cell<u32>>() };
    counter.set(counter.get() + 1);
}

struct Output {
    limit: usize,
    events: Vec<(u32, u16)>,
}

unsafe extern "C" fn try_push(
    events: *const clap_output_events,
    event: *const clap_event_header,
) -> bool {
    let output = unsafe { &mut *(*events).ctx.cast::<Output>() };
    if output.events.len() == output.limit {
        return false;
    }
    let event = unsafe { &*event };
    output.events.push((event.time, event.type_));
    true
}

#[test]
fn rejected_parameter_event_keeps_its_fifo_position_until_accepted() {
    let retries = Cell::new(0);
    let host = clap_host {
        clap_version: clap_sys::version::CLAP_VERSION,
        host_data: ptr::from_ref(&retries).cast_mut().cast(),
        name: c"Test".as_ptr(),
        vendor: c"klang-studio".as_ptr(),
        url: c"".as_ptr(),
        version: c"1".as_ptr(),
        get_extension: None,
        request_restart: None,
        request_process: None,
        request_callback: Some(request_callback),
    };
    let wrapper = unsafe { Wrapper::<QueueProbe>::new(&host).unwrap() };
    let id = 1;
    for event in [
        OutputParamEvent::BeginGesture { param_hash: id },
        OutputParamEvent::SetValue {
            param_hash: id,
            clap_plain_value: 0.75,
        },
        OutputParamEvent::EndGesture { param_hash: id },
    ] {
        assert!(wrapper.push_output_event(event));
    }

    let mut output = Output {
        limit: 1,
        events: Vec::with_capacity(4),
    };
    let callbacks = clap_output_events {
        ctx: ptr::from_mut(&mut output).cast(),
        try_push: Some(try_push),
    };
    let mut accepted = Vec::with_capacity(1);
    for _ in 0..2 {
        unsafe { wrapper.handle_out_events(&callbacks, 0, |id, value| accepted.push((id, value))) };
    }
    assert_eq!(output.events, [(0, CLAP_EVENT_PARAM_GESTURE_BEGIN)]);
    assert_eq!(retries.get(), 1);
    assert!(accepted.is_empty());

    output.limit = 4;
    unsafe { wrapper.handle_out_events(&callbacks, 8, |id, value| accepted.push((id, value))) };
    assert_eq!(
        output.events,
        [
            (0, CLAP_EVENT_PARAM_GESTURE_BEGIN),
            (8, CLAP_EVENT_PARAM_VALUE),
            (8, CLAP_EVENT_PARAM_GESTURE_END),
        ],
    );
    assert_eq!(accepted, [(id, 0.75)]);
    assert!(wrapper.rejected_parameter_event.borrow().is_none());
    assert!(wrapper.output_parameter_events.is_empty());
}

#[test]
fn parameter_output_precedes_notes_without_applying_the_dsp_value_early() {
    unsafe extern "C" fn input_size(_: *const clap_input_events) -> u32 {
        1
    }

    unsafe extern "C" fn input_get(
        input: *const clap_input_events,
        index: u32,
    ) -> *const clap_event_header {
        if index == 0 {
            unsafe { (*input).ctx.cast() }
        } else {
            ptr::null()
        }
    }

    let host = clap_host {
        clap_version: clap_sys::version::CLAP_VERSION,
        host_data: ptr::null_mut(),
        name: c"Test".as_ptr(),
        vendor: c"klang-studio".as_ptr(),
        url: c"".as_ptr(),
        version: c"1".as_ptr(),
        get_extension: Some(no_host_extension),
        request_restart: None,
        request_process: None,
        request_callback: None,
    };
    let wrapper = unsafe { Wrapper::<QueueProbe>::new(&host).unwrap() };
    let plugin = wrapper.clap_plugin.as_ptr();
    let id = unsafe { Wrapper::<QueueProbe>::custom_extension_param_id(plugin, "gain") }.unwrap();
    assert!(unsafe { Wrapper::<QueueProbe>::init(plugin) });
    assert!(unsafe { Wrapper::<QueueProbe>::activate(plugin, 48_000.0, 1, 32) });
    assert!(unsafe { Wrapper::<QueueProbe>::start_processing(plugin) });
    assert!(wrapper.push_output_event(OutputParamEvent::SetValue {
        param_hash: id,
        clap_plain_value: 0.75,
    }));
    let mut output = Output {
        limit: 4,
        events: Vec::with_capacity(4),
    };
    let callbacks = clap_output_events {
        ctx: ptr::from_mut(&mut output).cast(),
        try_push: Some(try_push),
    };
    let mut transport = clap_event_transport {
        header: clap_event_header {
            size: mem::size_of::<clap_event_transport>() as u32,
            time: 8,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_TRANSPORT,
            flags: 0,
        },
        flags: 0,
        song_pos_beats: 0,
        song_pos_seconds: 0,
        tempo: 0.0,
        tempo_inc: 0.0,
        loop_start_beats: 0,
        loop_end_beats: 0,
        loop_start_seconds: 0,
        loop_end_seconds: 0,
        bar_start: 0,
        bar_number: 0,
        tsig_num: 0,
        tsig_denom: 0,
    };
    let inputs = clap_input_events {
        ctx: ptr::from_mut(&mut transport).cast(),
        size: Some(input_size),
        get: Some(input_get),
    };
    let process = clap_process {
        steady_time: 0,
        frames_count: 16,
        transport: ptr::null(),
        audio_inputs: ptr::null(),
        audio_outputs: ptr::null_mut(),
        audio_inputs_count: 0,
        audio_outputs_count: 0,
        in_events: &inputs,
        out_events: &callbacks,
    };
    OBSERVED_GAINS.set((None, None));
    assert_ne!(
        unsafe { Wrapper::<QueueProbe>::process(plugin, &process) },
        CLAP_PROCESS_ERROR
    );
    unsafe {
        Wrapper::<QueueProbe>::stop_processing(plugin);
        Wrapper::<QueueProbe>::deactivate(plugin);
    }
    assert_eq!(
        output.events,
        [
            (0, CLAP_EVENT_PARAM_VALUE),
            (5, CLAP_EVENT_NOTE_ON),
            (13, CLAP_EVENT_NOTE_ON),
        ]
    );
    assert_eq!(OBSERVED_GAINS.get(), (Some(0.375), Some(0.75)));
    let mut value = 0.0;
    assert!(unsafe { Wrapper::<QueueProbe>::ext_params_get_value(plugin, id, &mut value) });
    assert_eq!(value, 0.75);
}

unsafe extern "C" fn no_host_extension(_: *const clap_host, _: *const c_char) -> *const c_void {
    ptr::null()
}

#[test]
fn successful_restore_supersedes_older_matching_values_but_preserves_gestures_and_other_edits() {
    use nice_plug_core::plugin::ParamValue;

    let host = clap_host {
        clap_version: clap_sys::version::CLAP_VERSION,
        host_data: ptr::null_mut(),
        name: c"Test".as_ptr(),
        vendor: c"klang-studio".as_ptr(),
        url: c"".as_ptr(),
        version: c"1".as_ptr(),
        get_extension: Some(no_host_extension),
        request_restart: None,
        request_process: None,
        request_callback: None,
    };
    let wrapper = unsafe { Wrapper::<QueueProbe>::new(&host).unwrap() };
    let plugin = wrapper.clap_plugin.as_ptr();
    let id =
        |name| unsafe { Wrapper::<QueueProbe>::custom_extension_param_id(plugin, name) }.unwrap();
    let (gain, mix) = (id("gain"), id("mix"));
    assert!(unsafe { Wrapper::<QueueProbe>::init(plugin) });
    let value = |param_hash, clap_plain_value| OutputParamEvent::SetValue {
        param_hash,
        clap_plain_value,
    };
    for event in [
        OutputParamEvent::BeginGesture { param_hash: gain },
        value(gain, 0.75),
        value(mix, 0.125),
        value(gain, 0.625),
        OutputParamEvent::EndGesture { param_hash: gain },
    ] {
        assert!(wrapper.push_output_event(event));
    }
    let mut output = Output {
        limit: 1,
        events: Vec::with_capacity(5),
    };
    let callbacks = clap_output_events {
        ctx: ptr::from_mut(&mut output).cast(),
        try_push: Some(try_push),
    };
    unsafe { Wrapper::<QueueProbe>::ext_params_flush(plugin, ptr::null(), &callbacks) };
    assert!(wrapper.rejected_parameter_event.borrow().is_some());

    let mut state = wrapper.get_state_object();
    state.params.remove("mix");
    state.params.insert("gain".into(), ParamValue::F32(0.25));
    assert!(wrapper.set_state_inner(&mut state));
    output.limit = 5;
    unsafe { Wrapper::<QueueProbe>::ext_params_flush(plugin, ptr::null(), &callbacks) };
    let mut current = 0.0;
    assert!(unsafe { Wrapper::<QueueProbe>::ext_params_get_value(plugin, gain, &mut current) });
    assert_eq!(current, 0.25);
    assert!(unsafe { Wrapper::<QueueProbe>::ext_params_get_value(plugin, mix, &mut current) });
    assert_eq!(current, 0.125);
    assert_eq!(
        output.events,
        [
            (0, CLAP_EVENT_PARAM_GESTURE_BEGIN),
            (0, CLAP_EVENT_PARAM_VALUE),
            (0, CLAP_EVENT_PARAM_GESTURE_END),
        ]
    );

    assert!(wrapper.push_output_event(value(gain, 0.875)));
    unsafe { Wrapper::<QueueProbe>::ext_params_flush(plugin, ptr::null(), &callbacks) };
    assert!(unsafe { Wrapper::<QueueProbe>::ext_params_get_value(plugin, gain, &mut current) });
    assert_eq!(current, 0.875);
}
