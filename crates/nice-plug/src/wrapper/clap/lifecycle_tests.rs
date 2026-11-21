use std::cell::Cell;
use std::ptr;

thread_local! {
    static RELEASES: Cell<usize> = const { Cell::new(0) };
    static PEDAL_RELEASES: Cell<usize> = const { Cell::new(0) };
    static VOICE_MODULATION: Cell<Option<f32>> = const { Cell::new(None) };
    static FLUSH_DURING_RESTORE: Cell<Option<(*const clap_plugin, *const clap_input_events)>> = const { Cell::new(None) };
    static FLUSH_REQUESTS: Cell<usize> = const { Cell::new(0) };
    static MAIN_CALLBACKS: Cell<usize> = const { Cell::new(0) };
    static RESCANS: Cell<usize> = const { Cell::new(0) };
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

struct TestParams {
    gain: FloatParam,
    other: FloatParam,
    fresh: FloatParam,
}

impl Default for TestParams {
    fn default() -> Self {
        let range = FloatRange::Linear { min: 0.0, max: 1.0 };
        Self {
            gain: FloatParam::new("Gain", 0.5, range).with_poly_modulation_id(0),
            other: FloatParam::new("Other", 0.5, range),
            fresh: FloatParam::new("Fresh", 0.5, range),
        }
    }
}

// SAFETY: The test wrapper retains the Arc containing this parameter.
unsafe impl Params for TestParams {
    fn param_map(&self) -> Vec<(String, ParamPtr, String)> {
        vec![
            ("gain".to_owned(), self.gain.as_ptr(), String::new()),
            ("other".to_owned(), self.other.as_ptr(), String::new()),
            ("fresh".to_owned(), self.fresh.as_ptr(), String::new()),
        ]
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
    const MIDI_INPUT: MidiConfig = MidiConfig::MidiCCs;
    type SysExMessage = ();
    type BackgroundTask = Box<dyn FnOnce() + Send>;
    #[cfg(not(feature = "editor"))]
    type Editor = ();
    #[cfg(feature = "editor")]
    type Editor = TestEditor;

    fn params(&self) -> Arc<dyn Params> {
        self.0.clone()
    }

    fn task_executor(&mut self) -> TaskExecutor<Self> {
        Box::new(|task| task())
    }

    fn process(
        &mut self,
        _: &mut Buffer,
        _: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        while let Some(event) = context.next_event() {
            match event {
                NoteEvent::NoteOff { timing: 0, .. } => RELEASES.set(RELEASES.get() + 1),
                NoteEvent::MidiCC {
                    timing: 0,
                    cc: 64,
                    value: 0.0,
                    ..
                } => {
                    PEDAL_RELEASES.set(PEDAL_RELEASES.get() + 1);
                }
                NoteEvent::PolyModulation {
                    timing: 0,
                    voice_id: 42,
                    poly_modulation_id: 0,
                    normalized_offset,
                } => VOICE_MODULATION.set(Some(normalized_offset)),
                _ => {}
            }
        }
        ProcessStatus::Normal
    }

    fn reset(&mut self) {
        RESETS.set(RESETS.get() + 1);
    }

    fn filter_state(_: &mut PluginState) {
        if let Some((plugin, events)) = FLUSH_DURING_RESTORE.take() {
            // SAFETY: The synchronous restore's caller still owns the plugin and event list.
            unsafe { Wrapper::<TestPlugin>::ext_params_flush(plugin, events, ptr::null()) };
        }
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
    fn editor(&mut self, executor: crate::prelude::AsyncExecutor<Self>) -> Option<Self::Editor> {
        Some(TestEditor { executor })
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

const WATCHDOG: Duration = Duration::from_secs(10);

fn wait_for_control_request(access: &StateAccess) {
    let started = Instant::now();
    while !access.control_pending.load(Ordering::Acquire) {
        assert!(
            started.elapsed() < WATCHDOG,
            "control request was not reserved"
        );
        thread::yield_now();
    }
}

#[test]
fn editor_state_access_times_out_if_an_inflight_audio_callback_does_not_exit() {
    let access = StateAccess::default();
    let active_audio = access.try_audio().unwrap();
    let result = access.with_control(Some(Duration::from_millis(5)), || {
        panic!("state applied twice")
    });
    assert_eq!(result, None::<()>);
    drop(active_audio);
    assert!(access.try_audio().is_some());
    assert_eq!(access.with_control(Some(Duration::ZERO), || 42), Some(42));
}

#[test]
fn a_pending_request_excludes_new_audio_and_does_not_wait_after_apply() {
    let access = StateAccess::default();
    let active_audio = access.try_audio().unwrap();
    thread::scope(|scope| {
        let request = scope.spawn(|| {
            access.with_control(None, || {
                assert!(access.try_audio().is_none());
                assert!(access.with_control(Some(Duration::ZERO), || ()).is_none());
                42
            })
        });
        wait_for_control_request(&access);
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
        PEDAL_RELEASES.set(0);
        VOICE_MODULATION.set(None);
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
        let pedal_release = clap_event_midi {
            header: clap_event_header {
                size: mem::size_of::<clap_event_midi>() as u32,
                time: 3,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_MIDI,
                flags: 0,
            },
            port_index: 0,
            data: [0xb0, 64, 0],
        };
        let modulation = clap_event_param_mod {
            header: clap_event_header {
                size: mem::size_of::<clap_event_param_mod>() as u32,
                time: 3,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_PARAM_MOD,
                flags: 0,
            },
            param_id: id,
            cookie: ptr::null_mut(),
            note_id: 42,
            port_index: 0,
            channel: 0,
            key: 60,
            amount: 0.125,
        };
        let skipped_events = [
            ptr::from_ref(&release).cast(),
            ptr::from_ref(&pedal_release).cast(),
            ptr::from_ref(&modulation).cast(),
        ];
        let releases = input_event_list(&skipped_events);
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
        assert_eq!(PEDAL_RELEASES.get(), 0);
        assert_eq!(VOICE_MODULATION.get(), None);
        process.in_events = ptr::null();
        assert_ne!(
            Wrapper::<TestPlugin>::process(plugin, &process),
            CLAP_PROCESS_ERROR
        );
        assert_eq!(RELEASES.get(), 1);
        assert_eq!(PEDAL_RELEASES.get(), 1);
        assert_eq!(VOICE_MODULATION.replace(None), Some(0.125));
        assert_ne!(
            Wrapper::<TestPlugin>::process(plugin, &process),
            CLAP_PROCESS_ERROR
        );
        assert_eq!(RELEASES.get(), 1);

        let overflowing_releases = clap_input_events {
            size: Some(overflow_input_events),
            get: Some(repeated_input_event),
            ctx: ptr::from_ref(&release).cast_mut().cast(),
        };
        let previous_resets = RESETS.get();
        process.in_events = &overflowing_releases;
        wrapper
            .state_access
            .with_control(Some(Duration::ZERO), || {
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

#[test]
fn deferred_host_values_respect_parameter_restore_cutoffs() {
    use nice_plug_core::plugin::ParamValue;

    // SAFETY: All event pointers and the static host remain valid for the synchronous callbacks.
    unsafe {
        let wrapper = Wrapper::<TestPlugin>::new(&HOST).unwrap();
        let plugin = wrapper.clap_plugin.as_ptr();
        assert!(Wrapper::<TestPlugin>::init(plugin));

        let value_event = |param_name, value| clap_event_param_value {
            header: clap_event_header {
                size: mem::size_of::<clap_event_param_value>() as u32,
                time: 0,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_PARAM_VALUE,
                flags: 0,
            },
            param_id: Wrapper::<TestPlugin>::custom_extension_param_id(plugin, param_name).unwrap(),
            cookie: ptr::null_mut(),
            note_id: -1,
            port_index: -1,
            channel: -1,
            key: -1,
            value,
        };
        let older_gain = value_event("gain", 0.25);
        let other = value_event("other", 0.375);
        let during_first_restore = [
            ptr::from_ref(&older_gain).cast(),
            ptr::from_ref(&other).cast(),
        ];
        let first_flush = input_event_list(&during_first_restore);
        let mut first_state = wrapper.get_state_object();
        FLUSH_DURING_RESTORE.set(Some((plugin, &first_flush)));
        assert!(wrapper.set_state_inner(&mut first_state));

        let fresh = value_event("fresh", 0.75);
        let during_second_restore = [ptr::from_ref(&fresh).cast()];
        let second_flush = input_event_list(&during_second_restore);
        let mut second_state = wrapper.get_state_object();
        second_state.params.remove("other");
        second_state
            .params
            .insert("gain".into(), ParamValue::F32(0.875));
        second_state
            .params
            .insert("fresh".into(), ParamValue::F32(0.125));
        FLUSH_DURING_RESTORE.set(Some((plugin, &second_flush)));
        assert!(wrapper.set_state_inner(&mut second_state));

        Wrapper::<TestPlugin>::ext_params_flush(plugin, ptr::null(), ptr::null());
        let params = wrapper.get_state_object().params;
        assert!(matches!(params["gain"], ParamValue::F32(0.875)));
        assert!(matches!(params["other"], ParamValue::F32(0.375)));
        assert!(matches!(params["fresh"], ParamValue::F32(0.75)));
    }
}

fn input_event_list<const N: usize>(headers: &[*const clap_event_header; N]) -> clap_input_events {
    unsafe extern "C" fn size<const N: usize>(_: *const clap_input_events) -> u32 {
        N as u32
    }
    unsafe extern "C" fn get<const N: usize>(
        list: *const clap_input_events,
        index: u32,
    ) -> *const clap_event_header {
        // SAFETY: This callback is used only with the header array borrowed in input_event_list.
        unsafe { &*(*list).ctx.cast::<[*const clap_event_header; N]>() }
            .get(index as usize)
            .copied()
            .unwrap_or(ptr::null())
    }
    clap_input_events {
        ctx: ptr::from_ref(headers).cast_mut().cast(),
        size: Some(size::<N>),
        get: Some(get::<N>),
    }
}

#[test]
fn sliced_transport_advances_bars_on_the_hosts_grid() {
    let context = clap_event_transport {
        header: clap_event_header {
            size: mem::size_of::<clap_event_transport>() as u32,
            time: 0,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_TRANSPORT,
            flags: 0,
        },
        flags: CLAP_TRANSPORT_IS_PLAYING
            | CLAP_TRANSPORT_HAS_BEATS_TIMELINE
            | CLAP_TRANSPORT_HAS_TIME_SIGNATURE,
        song_pos_beats: 3 * CLAP_BEATTIME_FACTOR,
        song_pos_seconds: 0,
        tempo: 120.0,
        tempo_inc: 0.0,
        loop_start_beats: 0,
        loop_end_beats: 0,
        loop_start_seconds: 0,
        loop_end_seconds: 0,
        bar_start: 3 * CLAP_BEATTIME_FACTOR,
        bar_number: 1,
        tsig_num: 4,
        tsig_denom: 4,
    };
    let mut transport = Transport::new(48_000.0);
    transport.playing = true;
    transport.time_sig_numerator = Some(4);
    transport.time_sig_denominator = Some(4);
    for (beats, expected_bar_start, expected_bar_number) in [
        (3.5, 3.0, 1),
        (6.99, 3.0, 1),
        (7.0, 7.0, 2),
        (11.25, 11.0, 3),
    ] {
        transport.pos_beats = Some(beats);
        advance_transport_bar(&mut transport, &context, 1);
        assert_eq!(transport.bar_start_pos_beats(), Some(expected_bar_start));
        assert_eq!(transport.bar_number(), Some(expected_bar_number));
    }
    transport.playing = false;
    advance_transport_bar(&mut transport, &context, 1);
    assert_eq!(transport.bar_start_pos_beats(), Some(3.0));
    assert_eq!(transport.bar_number(), Some(1));
}

const HOST_PARAMS: clap_host_params = clap_host_params {
    rescan: Some(record_rescan),
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

unsafe extern "C" fn record_rescan(_: *const clap_host, flags: u32) {
    assert_eq!(flags, CLAP_PARAM_RESCAN_VALUES);
    RESCANS.set(RESCANS.get() + 1);
}

unsafe extern "C" fn record_flush(_: *const clap_host) {
    FLUSH_REQUESTS.set(FLUSH_REQUESTS.get() + 1);
}

unsafe extern "C" fn record_callback(_: *const clap_host) {
    MAIN_CALLBACKS.set(MAIN_CALLBACKS.get() + 1);
}

#[cfg(feature = "editor")]
#[test]
fn editor_state_is_restored_before_returning_even_after_the_host_load_timeout() {
    use nice_plug_core::plugin::ParamValue;
    use std::sync::mpsc;

    let host = clap_host {
        get_extension: Some(params_extension),
        ..HOST
    };
    let wrapper = unsafe { Wrapper::<TestPlugin>::new(&host).unwrap() };
    let plugin = wrapper.clap_plugin.as_ptr();
    assert!(unsafe { Wrapper::<TestPlugin>::init(plugin) });
    let context = wrapper.clone().make_gui_context();
    RESCANS.set(0);
    let mut editor_state = context.get_state();
    editor_state
        .params
        .insert("gain".into(), ParamValue::F32(0.25));

    thread::scope(|scope| {
        let (audio_started_tx, audio_started_rx) = mpsc::channel();
        let access = &wrapper.state_access;
        scope.spawn(move || {
            let audio = access.try_audio().unwrap();
            audio_started_tx.send(()).unwrap();
            wait_for_control_request(access);
            thread::sleep(STATE_ACCESS_TIMEOUT + Duration::from_millis(50));
            drop(audio);
        });

        audio_started_rx.recv_timeout(WATCHDOG).unwrap();
        context.set_state(editor_state);
        assert_eq!(RESCANS.get(), 1);
        assert!(matches!(
            context.get_state().params.get("gain"),
            Some(ParamValue::F32(0.25))
        ));
    });
}

#[test]
fn destroying_the_last_instance_leaves_its_running_task_until_library_deinit() {
    #[cfg(feature = "editor")]
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;

    let library = LibraryWorkers::new();
    library.initialize();
    library.initialize();
    #[cfg(not(feature = "editor"))]
    let host = HOST;
    #[cfg(feature = "editor")]
    let callback_count = AtomicUsize::new(0);
    #[cfg(feature = "editor")]
    let host = clap_host {
        host_data: ptr::from_ref(&callback_count).cast_mut().cast(),
        request_callback: Some(record_background_callback),
        ..HOST
    };
    let wrapper = unsafe { Wrapper::<TestPlugin>::new(&host).unwrap() };
    wrapper.retain_background_worker(&library);
    #[cfg(feature = "editor")]
    let executor = wrapper
        .editor
        .borrow()
        .as_ref()
        .unwrap()
        .lock()
        .executor
        .clone();
    let weak_wrapper = Arc::downgrade(&wrapper);
    let (started_tx, started_rx) = mpsc::channel();
    let (finish_tx, finish_rx) = mpsc::channel();
    let (completed_tx, completed_rx) = mpsc::channel();
    assert!(
        wrapper.schedule_background(Task::<TestPlugin>::PluginTask(Box::new(move || {
            started_tx.send(()).unwrap();
            finish_rx.recv_timeout(WATCHDOG).unwrap();
            #[cfg(feature = "editor")]
            {
                executor.execute_gui(Box::new(|| panic!("task posted after destruction")));
                executor.execute_background(Box::new(|| panic!("task posted after destruction")));
            }
            completed_tx.send(()).unwrap();
        })))
    );
    started_rx.recv_timeout(WATCHDOG).unwrap();

    let (cancelled_tx, cancelled_rx) = mpsc::channel();
    assert!(
        wrapper.schedule_background(Task::<TestPlugin>::PluginTask(Box::new(move || {
            cancelled_tx.send(()).unwrap();
        })))
    );
    let plugin = wrapper.clap_plugin.as_ptr();
    // SAFETY: This transfers the last host-facing reference to the destroy callback.
    let _ = Arc::into_raw(wrapper);
    unsafe { Wrapper::<TestPlugin>::destroy(plugin) };
    assert!(weak_wrapper.upgrade().is_none());
    library.deinitialize();

    finish_tx.send(()).unwrap();
    library.deinitialize();
    completed_rx.recv_timeout(WATCHDOG).unwrap();
    assert!(matches!(
        cancelled_rx.recv_timeout(WATCHDOG),
        Err(mpsc::RecvTimeoutError::Disconnected)
    ));
    #[cfg(feature = "editor")]
    assert_eq!(callback_count.load(Ordering::Relaxed), 0);
}

#[cfg(feature = "editor")]
unsafe extern "C" fn record_background_callback(host: *const clap_host) {
    use std::sync::atomic::AtomicUsize;

    // SAFETY: This host contains a live counter for the duration of the test.
    let count = unsafe { &*((*host).host_data.cast::<AtomicUsize>()) };
    count.fetch_add(1, Ordering::Relaxed);
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
struct TestEditor {
    executor: crate::prelude::AsyncExecutor<TestPlugin>,
}

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
