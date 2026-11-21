#[macro_use]
mod util;

mod context;
mod descriptor;
pub mod features;
mod wrapper;

use crate::wrapper::clap::features::ClapFeature;

/// Re-export for the macro
pub use self::descriptor::PluginDescriptor;
pub use self::wrapper::Wrapper;
pub use clap_sys::entry::clap_plugin_entry;
pub use clap_sys::factory::plugin_factory::{CLAP_PLUGIN_FACTORY_ID, clap_plugin_factory};
pub use clap_sys::host::clap_host;
pub use clap_sys::plugin::{clap_plugin, clap_plugin_descriptor};
pub use clap_sys::version::CLAP_VERSION;
use nice_plug_core::context::remote_controls::RemoteControlsContext;
use nice_plug_core::plugin::Plugin;
use parking_lot::Mutex;
use std::sync::Arc;

/// Owns the background workers for as long as this CLAP library is initialized. An instance can
/// close its task queue during destroy. The final deinit joins the workers before library unload.
#[doc(hidden)]
pub struct LibraryWorkers(Mutex<LibraryWorkerState>);

struct LibraryWorkerState {
    clients: usize,
    workers: Vec<Arc<dyn Send + Sync>>,
}

impl LibraryWorkers {
    pub const fn new() -> Self {
        Self(Mutex::new(LibraryWorkerState {
            clients: 0,
            workers: Vec::new(),
        }))
    }

    pub fn initialize(&self) {
        self.0.lock().clients += 1;
    }

    pub fn deinitialize(&self) {
        let workers = {
            let mut state = self.0.lock();
            if state.clients == 0 {
                return;
            }
            state.clients -= 1;
            if state.clients != 0 {
                return;
            }
            std::mem::take(&mut state.workers)
        };
        // Dropping the worker handles joins them outside the registry lock.
        drop(workers);
    }

    fn retain(&self, worker: Arc<dyn Send + Sync>) {
        let mut state = self.0.lock();
        if !state
            .workers
            .iter()
            .any(|current| Arc::ptr_eq(current, &worker))
        {
            state.workers.push(worker);
        }
    }
}

impl Default for LibraryWorkers {
    fn default() -> Self {
        Self::new()
    }
}

/// Provides auxiliary metadata needed for a CLAP plugin.
#[allow(unused_variables)]
pub trait ClapPlugin: Plugin {
    /// A unique ID that identifies this particular plugin. This is usually in reverse domain name
    /// notation, e.g. `com.manufacturer.plugin-name`.
    const CLAP_ID: &'static str;
    /// An optional short description for the plugin.
    const CLAP_DESCRIPTION: Option<&'static str>;
    /// The URL to the plugin's manual, if available.
    const CLAP_MANUAL_URL: Option<&'static str>;
    /// The URL to the plugin's support page, if available.
    const CLAP_SUPPORT_URL: Option<&'static str>;
    /// Keywords describing the plugin. The host may use this to classify the plugin in its plugin
    /// browser.
    const CLAP_FEATURES: &'static [ClapFeature];

    /// Deliver raw MIDI releases as `NoteEvent::MidiNoteOff`, so a plugin can distinguish
    /// them from CLAP note-offs whose voice ID is a wildcard.
    const CLAP_DISTINGUISH_MIDI_NOTE_OFF: bool = false;

    /// If set, this informs the host about the plugin's capabilities for polyphonic modulation.
    const CLAP_POLY_MODULATION_CONFIG: Option<PolyModulationConfig> = None;

    /// This function can be implemented to define plugin-specific [remote control
    /// pages](https://github.com/free-audio/clap/blob/main/include/clap/ext/draft/remote-controls.h)
    /// that the host can use to provide better hardware mapping for a plugin. See the linked
    /// extension for more information.
    fn remote_controls(&self, context: &mut impl RemoteControlsContext) {}

    /// Supplies an optional custom CLAP plugin extension after the wrapper's built-in dispatch.
    ///
    /// # Safety
    ///
    /// Implementations must be thread-safe and return null for unsupported IDs. Non-null pointers
    /// must point to immutable extension tables with the C layout and calling convention required
    /// by the requested ID. A table must remain valid until the plugin instance is destroyed.
    unsafe fn clap_custom_extension(_id: &std::ffi::CStr) -> *const std::ffi::c_void {
        std::ptr::null()
    }
}

/// Configuration for the plugin's polyphonic modulation options, if it supports .
pub struct PolyModulationConfig {
    /// The maximum number of voices this plugin will ever use. Call the context's
    /// `set_current_voice_capacity()` method during initialization or audio processing to set the
    /// polyphony limit.
    pub max_voice_capacity: u32,
    /// If set to `true`, then the host may send note events for the same channel and key, but using
    /// different voice IDs. Bitwig Studio, for instance, can use this to do voice stacking. After
    /// enabling this, you should always prioritize using voice IDs to map note events to voices.
    pub supports_overlapping_voices: bool,
}

/// Export one or more CLAP plugins from this library using the provided plugin types.
#[macro_export]
macro_rules! nice_export_clap {
    ($($plugin_ty:ty),+) => {
        // Earlier versions used a simple generic struct for this, but because we don't have
        // variadic generics (yet) we can't generate the struct for multiple plugin types without
        // macros. So instead we'll generate the implementation ad-hoc inside of this macro.
        #[doc(hidden)]
        mod clap {
            use $crate::wrapper::setup_logger;
            use $crate::wrapper::clap::{LibraryWorkers, PluginDescriptor, Wrapper};
            use $crate::wrapper::clap::{CLAP_PLUGIN_FACTORY_ID, clap_host, clap_plugin, clap_plugin_descriptor, clap_plugin_factory};
            use ::std::collections::HashSet;
            use ::std::ffi::{CStr, c_void};
            use ::std::os::raw::c_char;
            use ::std::sync::{Arc, OnceLock};

            // Because the `$plugin_ty`s are likely defined in the enclosing scope. This works even
            // if the types are not public because this is a child module.
            use super::*;

            static LIBRARY_WORKERS: LibraryWorkers = LibraryWorkers::new();

            const CLAP_PLUGIN_FACTORY: clap_plugin_factory = clap_plugin_factory {
                get_plugin_count: Some(get_plugin_count),
                get_plugin_descriptor: Some(get_plugin_descriptor),
                create_plugin: Some(create_plugin),
            };

            // Sneaky way to get the number of expanded elements
            const PLUGIN_COUNT: usize = [$(stringify!($plugin_ty)),+].len();

            // This is a type erased version of the information stored on the plugin types
            static PLUGIN_DESCRIPTORS: OnceLock<Option<[PluginDescriptor; PLUGIN_COUNT]>> =
                OnceLock::new();

            fn plugin_descriptors() -> &'static [PluginDescriptor] {
                PLUGIN_DESCRIPTORS.get_or_init(|| {
                    let descriptors = [$(PluginDescriptor::for_plugin::<$plugin_ty>()),+];
                    let mut ids = HashSet::new();
                    if descriptors.iter().all(|descriptor| ids.insert(descriptor.clap_id())) {
                        Some(descriptors)
                    } else {
                        $crate::nice_error!("Duplicate plugin IDs in nice_export_clap!()");
                        None
                    }
                }).as_ref().map_or(&[], |descriptors| descriptors.as_slice())
            }

            unsafe extern "C" fn get_plugin_count(_factory: *const clap_plugin_factory) -> u32 {
                plugin_descriptors().len() as u32
            }

            unsafe extern "C" fn get_plugin_descriptor (
                _factory: *const clap_plugin_factory,
                index: u32,
            ) -> *const clap_plugin_descriptor  {
                match plugin_descriptors().get(index as usize) {
                    Some(descriptor) => descriptor.clap_plugin_descriptor(),
                    None => ::std::ptr::null()
                }
            }

            unsafe extern "C" fn create_plugin (
                _factory: *const clap_plugin_factory,
                host: *const clap_host,
                plugin_id: *const c_char,
            ) -> *const clap_plugin  {
                if plugin_id.is_null() || host.is_null() {
                    return ::std::ptr::null();
                }
                let plugin_id_cstr = unsafe { CStr::from_ptr(plugin_id) };

                // This isn't great, but we'll just assume that `$plugin_ids` and the descriptors
                // are in the same order. We also can't directly enumerate over them with an index,
                // which is why we do things the way we do. Otherwise we could have used a tuple
                // instead.
                let descriptors = plugin_descriptors();
                if descriptors.is_empty() {
                    return ::std::ptr::null();
                }
                let mut descriptor_idx = 0;
                $({
                    let descriptor = &descriptors[descriptor_idx];
                    if plugin_id_cstr == descriptor.clap_id() {
                        // Arc does not have a convenient leak function like Box, so this gets a bit awkward
                        // This pointer gets turned into an Arc and its reference count decremented in
                        // [Wrapper::destroy()]
                        let Some(instance) = (unsafe { Wrapper::<$plugin_ty>::new(host) }) else {
                            return ::std::ptr::null();
                        };
                        instance.retain_background_worker(&LIBRARY_WORKERS);
                        return unsafe { (*Arc::into_raw(instance)).clap_plugin.as_ptr() };
                    }

                    descriptor_idx += 1;
                })+

                ::std::ptr::null()
            }

            pub extern "C" fn init(_plugin_path: *const c_char) -> bool {
                $({setup_logger::<$plugin_ty>();})+
                if plugin_descriptors().is_empty() {
                    return false;
                }
                LIBRARY_WORKERS.initialize();
                true
            }

            pub extern "C" fn deinit() {
                LIBRARY_WORKERS.deinitialize();
            }

            pub extern "C" fn get_factory(factory_id: *const c_char) -> *const c_void {
                if !factory_id.is_null()
                    && unsafe { CStr::from_ptr(factory_id) } == CLAP_PLUGIN_FACTORY_ID
                    && !plugin_descriptors().is_empty()
                {
                    &CLAP_PLUGIN_FACTORY as *const _ as *const c_void
                } else {
                    ::std::ptr::null()
                }
            }
        }

        /// The CLAP plugin's entry point.
        #[unsafe(no_mangle)]
        #[used]
        pub static clap_entry: $crate::wrapper::clap::clap_plugin_entry =
            $crate::wrapper::clap::clap_plugin_entry {
                clap_version: $crate::wrapper::clap::CLAP_VERSION,
                init: Some(self::clap::init),
                deinit: Some(self::clap::deinit),
                get_factory: Some(self::clap::get_factory),
            };
    };
}

#[cfg(test)]
mod library_worker_tests {
    use super::*;
    use crate::event_loop::{BackgroundThread, MainThreadExecutor};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    type Task = Box<dyn FnOnce() + Send>;

    struct Executor;

    impl MainThreadExecutor<Task> for Executor {
        fn execute(&self, task: Task, _: bool) {
            task();
        }
    }

    #[test]
    fn final_deinit_waits_for_the_running_worker() {
        const WATCHDOG: Duration = Duration::from_secs(10);

        let library = LibraryWorkers::new();
        library.initialize();
        let executor = Arc::new(Executor);
        let worker = BackgroundThread::get_or_create(Arc::downgrade(&executor));
        let worker_handle = worker.worker_handle();
        let worker_lifetime = Arc::downgrade(&worker_handle);
        library.retain(worker_handle);
        let (started_tx, started_rx) = mpsc::channel();
        let (finish_tx, finish_rx) = mpsc::channel();
        assert!(worker.schedule(Box::new(move || {
            started_tx.send(()).unwrap();
            finish_rx.recv_timeout(WATCHDOG).unwrap();
        })));
        started_rx.recv_timeout(WATCHDOG).unwrap();
        drop(worker);

        let (returned_tx, returned_rx) = mpsc::channel();
        let deinit = thread::spawn(move || {
            library.deinitialize();
            returned_tx.send(()).unwrap();
        });
        let deadline = Instant::now() + WATCHDOG;
        // A zero count means deinit has dropped the last handle and entered the worker's destructor.
        while worker_lifetime.strong_count() != 0 {
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        let result_while_blocked = returned_rx.recv_timeout(Duration::from_millis(50));
        finish_tx.send(()).unwrap();
        assert_eq!(result_while_blocked, Err(mpsc::RecvTimeoutError::Timeout));
        returned_rx.recv_timeout(WATCHDOG).unwrap();
        deinit.join().unwrap();
    }
}
