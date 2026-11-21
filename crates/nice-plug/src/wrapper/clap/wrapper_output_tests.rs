use std::cell::Cell;
use std::ptr;

use crate::prelude::*;

use super::*;

struct ProbeParams {
    gain: FloatParam,
}

impl Default for ProbeParams {
    fn default() -> Self {
        Self {
            gain: FloatParam::new("Gain", 0.375, FloatRange::Linear { min: 0.0, max: 1.0 }),
        }
    }
}

// SAFETY: The pointer refers to a parameter owned by the ProbeParams Arc.
unsafe impl Params for ProbeParams {
    fn param_map(&self) -> Vec<(String, ParamPtr, String)> {
        vec![("gain".into(), self.gain.as_ptr(), String::new())]
    }
}

thread_local! {
    static OBSERVED_GAIN: Cell<Option<f32>> = const { Cell::new(None) };
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
        OBSERVED_GAIN.set(Some(self.params.gain.value()));
        assert!(
            context
                .try_send_event(NoteEvent::NoteOn {
                    timing: 5,
                    voice_id: VoiceID::Wildcard,
                    channel: Channel::Number(0),
                    key: Key::Number(60),
                    velocity: 1.0,
                })
                .is_ok()
        );
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
        assert!(wrapper.output_parameter_events.push(event).is_ok());
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
    assert!(
        wrapper
            .output_parameter_events
            .push(OutputParamEvent::SetValue {
                param_hash: id,
                clap_plain_value: 0.75,
            })
            .is_ok()
    );
    let mut output = Output {
        limit: 4,
        events: Vec::with_capacity(4),
    };
    let callbacks = clap_output_events {
        ctx: ptr::from_mut(&mut output).cast(),
        try_push: Some(try_push),
    };
    let process = clap_process {
        steady_time: 0,
        frames_count: 16,
        transport: ptr::null(),
        audio_inputs: ptr::null(),
        audio_outputs: ptr::null_mut(),
        audio_inputs_count: 0,
        audio_outputs_count: 0,
        in_events: ptr::null(),
        out_events: &callbacks,
    };
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
        [(0, CLAP_EVENT_PARAM_VALUE), (5, CLAP_EVENT_NOTE_ON)]
    );
    assert_eq!(OBSERVED_GAIN.get(), Some(0.375));
    let mut value = 0.0;
    assert!(unsafe { Wrapper::<QueueProbe>::ext_params_get_value(plugin, id, &mut value) });
    assert_eq!(value, 0.75);
}

unsafe extern "C" fn no_host_extension(_: *const clap_host, _: *const c_char) -> *const c_void {
    ptr::null()
}
