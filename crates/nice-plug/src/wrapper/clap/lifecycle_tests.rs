use std::cell::Cell;
use std::ptr;

thread_local! {
    static RELEASES: Cell<usize> = const { Cell::new(0) };
    static FLUSH_REQUESTS: Cell<usize> = const { Cell::new(0) };
    static MAIN_CALLBACKS: Cell<usize> = const { Cell::new(0) };
    static RESETS: Cell<usize> = const { Cell::new(0) };
    static STATE_CONTENTION: Cell<Option<StateContention>> = const { Cell::new(None) };
}

struct StateContention {
    plugin: *const clap_plugin,
    process: *const clap_process,
    flush_events: *const clap_input_events,
    gain: u32,
}

use crate::prelude::{
    AudioIOLayout, AuxiliaryBuffers, Buffer, ClapFeature, FloatParam, FloatRange, Param, ParamPtr,
    Params, ProcessContext, ProcessStatus,
};
use clap_sys::audio_buffer::clap_audio_buffer;
use clap_sys::version::CLAP_VERSION;

use super::*;

struct TestParams(FloatParam);

impl Default for TestParams {
    fn default() -> Self {
        Self(FloatParam::new(
            "Gain",
            0.5,
            FloatRange::Linear { min: 0.0, max: 1.0 },
        ))
    }
}

// SAFETY: The test wrapper retains the Arc containing this parameter.
unsafe impl Params for TestParams {
    fn param_map(&self) -> Vec<(String, ParamPtr, String)> {
        vec![("gain".to_owned(), self.0.as_ptr(), String::new())]
    }
}

#[derive(Default)]
struct TestPlugin(Arc<TestParams>);

impl Plugin for TestPlugin {
    const NAME: &'static str = "Lifecycle test";
    const VENDOR: &'static str = "nice-plug";
    const URL: &'static str = "";
    const EMAIL: &'static str = "";
    const VERSION: &'static str = "1.0.0";
    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[AudioIOLayout {
        main_output_channels: NonZeroU32::new(2),
        ..AudioIOLayout::const_default()
    }];
    const MIDI_INPUT: MidiConfig = MidiConfig::Basic;
    type SysExMessage = ();
    type BackgroundTask = ();
    #[cfg(not(feature = "editor"))]
    type Editor = ();
    #[cfg(feature = "editor")]
    type Editor = TestEditor;

    fn params(&self) -> Arc<dyn Params> {
        self.0.clone()
    }

    fn process(
        &mut self,
        _: &mut Buffer,
        _: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        while let Some(event) = context.next_event() {
            if matches!(event, NoteEvent::NoteOff { timing: 0, .. }) {
                RELEASES.set(RELEASES.get() + 1);
            }
        }
        ProcessStatus::Normal
    }

    fn reset(&mut self) {
        RESETS.set(RESETS.get() + 1);
    }

    fn filter_state(_: &mut PluginState) {
        if let Some(contention) = STATE_CONTENTION.take() {
            // SAFETY: The test keeps these pointers valid until the synchronous load returns.
            unsafe {
                assert_eq!(
                    Wrapper::<TestPlugin>::process(contention.plugin, contention.process),
                    CLAP_PROCESS_CONTINUE
                );
                Wrapper::<TestPlugin>::ext_params_flush(
                    contention.plugin,
                    contention.flush_events,
                    ptr::null(),
                );
                assert_eq!(MAIN_CALLBACKS.get(), 1);
                Wrapper::<TestPlugin>::on_main_thread(contention.plugin);
                assert_eq!(FLUSH_REQUESTS.get(), 0);
                let mut gain = 0.0;
                assert!(Wrapper::<TestPlugin>::ext_params_get_value(
                    contention.plugin,
                    contention.gain,
                    &mut gain
                ));
                assert_eq!(gain, 0.5);
            }
        }
    }

    #[cfg(feature = "editor")]
    fn editor(&mut self, _: crate::prelude::AsyncExecutor<Self>) -> Option<Self::Editor> {
        Some(TestEditor)
    }
}

impl ClapPlugin for TestPlugin {
    const CLAP_ID: &'static str = "org.nice-plug.wrapper-lifecycle-test";
    const CLAP_DESCRIPTION: Option<&'static str> = None;
    const CLAP_MANUAL_URL: Option<&'static str> = None;
    const CLAP_SUPPORT_URL: Option<&'static str> = None;
    const CLAP_FEATURES: &'static [ClapFeature] = &[ClapFeature::AudioEffect];
}

const HOST: clap_host = clap_host {
    clap_version: CLAP_VERSION,
    host_data: ptr::null_mut(),
    name: c"Test host".as_ptr(),
    vendor: c"nice-plug".as_ptr(),
    url: c"".as_ptr(),
    version: c"1.0".as_ptr(),
    get_extension: Some(no_extension),
    request_restart: Some(no_host_action),
    request_process: Some(no_host_action),
    request_callback: Some(no_host_action),
};

unsafe extern "C" fn no_extension(_: *const clap_host, _: *const c_char) -> *const c_void {
    ptr::null()
}

unsafe extern "C" fn no_host_action(_: *const clap_host) {}

#[test]
fn editor_state_access_times_out_if_an_inflight_audio_callback_does_not_exit() {
    let access = StateAccess::default();
    let active_audio = access.try_audio().unwrap();
    let result = access.with_control(Duration::from_millis(5), || panic!("state applied twice"));
    assert_eq!(result, None::<()>);
    drop(active_audio);
    assert!(access.try_audio().is_some());
    assert_eq!(access.with_control(Duration::ZERO, || 42), Some(42));
}

#[test]
fn a_pending_request_excludes_new_audio_and_does_not_wait_after_apply() {
    let access = StateAccess::default();
    let active_audio = access.try_audio().unwrap();
    thread::scope(|scope| {
        let request = scope.spawn(|| {
            access.with_control(Duration::from_secs(1), || {
                assert!(access.try_audio().is_none());
                assert!(access.with_control(Duration::ZERO, || ()).is_none());
                42
            })
        });
        let started = Instant::now();
        while !access.control_pending.load(Ordering::Acquire) {
            assert!(started.elapsed() < Duration::from_millis(500));
            thread::yield_now();
        }
        assert!(access.try_audio().is_none());
        drop(active_audio);
        assert_eq!(request.join().unwrap(), Some(42));
    });
    assert!(access.try_audio().is_some());
}

#[test]
fn state_contention_silences_audio_and_replays_releases_and_parameter_flushes() {
    // SAFETY: The host is static and all callbacks and buffers below have the required lifetimes.
    unsafe {
        let host = clap_host {
            get_extension: Some(params_extension),
            request_callback: Some(record_callback),
            ..HOST
        };
        let wrapper = Wrapper::<TestPlugin>::new(&host).unwrap();
        let plugin = wrapper.clap_plugin.as_ptr();
        RELEASES.set(0);
        FLUSH_REQUESTS.set(0);
        MAIN_CALLBACKS.set(0);
        assert!(Wrapper::<TestPlugin>::init(plugin));
        assert!(Wrapper::<TestPlugin>::activate(plugin, 48_000.0, 1, 4));
        assert!(Wrapper::<TestPlugin>::start_processing(plugin));
        let id = Wrapper::<TestPlugin>::custom_extension_param_id(plugin, "gain").unwrap();
        let event = clap_event_param_value {
            header: clap_event_header {
                size: mem::size_of::<clap_event_param_value>() as u32,
                time: 0,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_PARAM_VALUE,
                flags: 0,
            },
            param_id: id,
            cookie: ptr::null_mut(),
            note_id: -1,
            port_index: -1,
            channel: -1,
            key: -1,
            value: 0.25,
        };
        let events = clap_input_events {
            ctx: ptr::from_ref(&event).cast_mut().cast(),
            size: Some(one_input_event),
            get: Some(input_event),
        };
        let release = clap_event_note {
            header: clap_event_header {
                size: mem::size_of::<clap_event_note>() as u32,
                time: 3,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_NOTE_OFF,
                flags: 0,
            },
            note_id: 42,
            port_index: 0,
            channel: 0,
            key: 60,
            velocity: 0.0,
        };
        let releases = clap_input_events {
            ctx: ptr::from_ref(&release).cast_mut().cast(),
            size: Some(one_input_event),
            get: Some(input_event),
        };
        let mut left = [2.0_f32; 5];
        let mut right = [2.0_f32; 5];
        let mut channels = [left.as_mut_ptr(), right.as_mut_ptr()];
        let mut output = clap_audio_buffer {
            data32: channels.as_mut_ptr(),
            data64: ptr::null_mut(),
            channel_count: 2,
            latency: 0,
            constant_mask: 3,
        };
        let mut process = clap_process {
            steady_time: -1,
            frames_count: 4,
            transport: ptr::null(),
            audio_inputs: ptr::null(),
            audio_outputs: &mut output,
            audio_inputs_count: 0,
            audio_outputs_count: 1,
            in_events: &releases,
            out_events: ptr::null(),
        };

        STATE_CONTENTION.set(Some(StateContention {
            plugin,
            process: &process,
            flush_events: &events,
            gain: id,
        }));
        let mut state = wrapper.get_state_object();
        assert!(wrapper.set_state_inner(&mut state));
        assert_eq!(left, [0.0, 0.0, 0.0, 0.0, 2.0]);
        assert_eq!(right, left);
        assert_eq!(output.constant_mask, 0);
        assert!(MAIN_CALLBACKS.get() >= 2);
        Wrapper::<TestPlugin>::on_main_thread(plugin);
        assert_eq!(FLUSH_REQUESTS.get(), 1);
        Wrapper::<TestPlugin>::ext_params_flush(plugin, ptr::null(), ptr::null());
        let mut gain = 0.0;
        assert!(Wrapper::<TestPlugin>::ext_params_get_value(
            plugin, id, &mut gain
        ));
        assert_eq!(gain, 0.25);
        assert_eq!(RELEASES.get(), 0);
        process.in_events = ptr::null();
        assert_ne!(
            Wrapper::<TestPlugin>::process(plugin, &process),
            CLAP_PROCESS_ERROR
        );
        assert_eq!(RELEASES.get(), 1);
        assert_ne!(
            Wrapper::<TestPlugin>::process(plugin, &process),
            CLAP_PROCESS_ERROR
        );
        assert_eq!(RELEASES.get(), 1);

        let overflowing_releases = clap_input_events {
            size: Some(overflow_input_events),
            get: Some(repeated_input_event),
            ..releases
        };
        let previous_resets = RESETS.get();
        process.in_events = &overflowing_releases;
        wrapper
            .state_access
            .with_control(Duration::ZERO, || {
                assert_eq!(
                    Wrapper::<TestPlugin>::process(plugin, &process),
                    CLAP_PROCESS_CONTINUE
                );
            })
            .unwrap();
        process.in_events = ptr::null();
        assert_ne!(
            Wrapper::<TestPlugin>::process(plugin, &process),
            CLAP_PROCESS_ERROR
        );
        assert_eq!(RESETS.get(), previous_resets + 1);
        assert_eq!(RELEASES.get(), 1);
        assert!(wrapper.deferred_releases.is_empty());
        Wrapper::<TestPlugin>::stop_processing(plugin);
        Wrapper::<TestPlugin>::deactivate(plugin);
    }
}

const HOST_PARAMS: clap_host_params = clap_host_params {
    rescan: Some(ignore_rescan),
    clear: None,
    request_flush: Some(record_flush),
};

unsafe extern "C" fn params_extension(_: *const clap_host, id: *const c_char) -> *const c_void {
    if unsafe { CStr::from_ptr(id) } == CLAP_EXT_PARAMS {
        ptr::from_ref(&HOST_PARAMS).cast()
    } else {
        ptr::null()
    }
}

unsafe extern "C" fn ignore_rescan(_: *const clap_host, _: u32) {}

unsafe extern "C" fn record_flush(_: *const clap_host) {
    FLUSH_REQUESTS.set(FLUSH_REQUESTS.get() + 1);
}

unsafe extern "C" fn record_callback(_: *const clap_host) {
    MAIN_CALLBACKS.set(MAIN_CALLBACKS.get() + 1);
}

#[cfg(feature = "editor")]
#[test]
fn busy_editor_state_is_retried_and_newer_host_state_supersedes_it() {
    use nice_plug_core::plugin::ParamValue;

    let wrapper = unsafe { Wrapper::<TestPlugin>::new(&HOST).unwrap() };
    let mut editor_state = wrapper.get_state_object();
    editor_state
        .params
        .insert("gain".into(), ParamValue::F32(0.25));
    let active_audio = wrapper.state_access.try_audio().unwrap();
    wrapper.apply_or_queue_editor_state(wrapper.next_revision(), editor_state, Duration::ZERO);
    assert!(wrapper.pending_editor_state.lock().is_some());
    drop(active_audio);

    let plugin = wrapper.clap_plugin.as_ptr();
    unsafe { Wrapper::<TestPlugin>::on_main_thread(plugin) };
    assert!(wrapper.pending_editor_state.lock().is_none());
    assert!(matches!(
        wrapper.get_state_object().params.get("gain"),
        Some(ParamValue::F32(0.25))
    ));

    let editor_state = wrapper.get_state_object();
    let active_audio = wrapper.state_access.try_audio().unwrap();
    wrapper.apply_or_queue_editor_state(wrapper.next_revision(), editor_state, Duration::ZERO);
    drop(active_audio);

    let mut host_state = wrapper.get_state_object();
    host_state
        .params
        .insert("gain".into(), ParamValue::F32(0.75));
    assert!(wrapper.set_state_inner(&mut host_state));
    unsafe { Wrapper::<TestPlugin>::on_main_thread(plugin) };
    assert!(wrapper.pending_editor_state.lock().is_none());
    assert!(matches!(
        wrapper.get_state_object().params.get("gain"),
        Some(ParamValue::F32(0.75))
    ));
}

unsafe extern "C" fn overflow_input_events(_: *const clap_input_events) -> u32 {
    DEFERRED_EVENT_QUEUE_CAPACITY as u32 + 1
}

unsafe extern "C" fn repeated_input_event(
    list: *const clap_input_events,
    _: u32,
) -> *const clap_event_header {
    unsafe { (*list).ctx.cast() }
}

unsafe extern "C" fn one_input_event(_: *const clap_input_events) -> u32 {
    1
}

unsafe extern "C" fn input_event(
    list: *const clap_input_events,
    index: u32,
) -> *const clap_event_header {
    if index == 0 {
        // SAFETY: The test owns both the stream and its event for the duration of the call.
        unsafe { (*list).ctx.cast() }
    } else {
        ptr::null()
    }
}

#[cfg(feature = "editor")]
struct TestEditor;

#[cfg(feature = "editor")]
impl Editor for TestEditor {
    type Handle = ();

    fn spawn(
        &self,
        _: Option<nice_plug_core::editor::ParentWindowHandle>,
        _: bool,
        _: Option<f64>,
        _: GuiContext,
        host: Option<nice_plug_core::editor::HostMethods>,
    ) -> Result<SpawnedEditor<Self::Handle>, Box<dyn std::error::Error>> {
        use nice_plug_core::editor::dpi::{PhysicalSize, Size};
        let mut host = host.unwrap();
        assert!(
            host.callbacks
                .request_resize(Size::new(PhysicalSize::new(100, 100)), 1.0)
                .is_err()
        );
        host.callbacks.destroyed();
        Ok(SpawnedEditor {
            handle: (),
            window: (),
        })
    }

    fn size(&self) -> nice_plug_core::editor::dpi::NativeSize<u32> {
        nice_plug_core::editor::dpi::NativeSize::new(100, 100)
    }
}

#[cfg(feature = "editor")]
#[test]
fn editor_create_works_without_the_optional_host_gui_extension() {
    // SAFETY: The host is static and the wrapper is initialized before the editor is created.
    unsafe {
        let wrapper = Wrapper::<TestPlugin>::new(&HOST).unwrap();
        let plugin = &*wrapper.clap_plugin.borrow();
        assert!(Wrapper::<TestPlugin>::init(plugin));
        assert!(wrapper.host_gui.borrow().is_none());
        let mut api = ptr::null();
        let mut floating = true;
        assert!(Wrapper::<TestPlugin>::ext_gui_get_preferred_api(
            plugin,
            &mut api,
            &mut floating
        ));
        // create temporarily adopts the same Arc count that the factory lends to CLAP.
        let factory_ref = Arc::into_raw(Arc::clone(&wrapper));
        assert!(Wrapper::<TestPlugin>::ext_gui_create(plugin, api, floating));
        Wrapper::<TestPlugin>::ext_gui_destroy(plugin);
        drop(Arc::from_raw(factory_ref));
    }
}
