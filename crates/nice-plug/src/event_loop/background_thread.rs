//! Used by the other [`EventLoop`][super::EventLoop] implementations to spawn threads for running
//! tasks in the background without blocking the GUI thread.
//!
//! This is essentially a slimmed down version of the `LinuxEventLoop`.

use anymap3::Entry;
use nice_plug_core::nice_trace;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Weak};
use std::thread::{self, JoinHandle};

use super::MainThreadExecutor;
use crate::util::permit_alloc;

/// See the module's documentation. This is a background thread that can be used to run tasks on.
/// The implementation shares a single thread between all of a plugin's instances hosted in the same
/// process.
pub(crate) struct BackgroundThread<T, E> {
    /// The executor and shutdown gate for this instance, shared with its queued tasks.
    executor: Arc<Executor<E>>,
    /// A thread that act as our worker thread. When [`schedule()`][Self::schedule()] is called,
    /// this thread will be woken up to execute the task on the executor. When the last worker
    /// thread handle gets dropped the thread is shut down.
    worker_thread: Arc<WorkerThread<T, E>>,
}

/// A handle for the singleton worker thread. This lets multiple instances of the same plugin share
/// a worker thread, and when the last instance gets dropped the worker thread gets terminated.
struct WorkerThread<T, E> {
    tasks_sender: Option<crossbeam::channel::Sender<(T, Arc<Executor<E>>)>>,
    /// The thread's join handle. Joined when the WorkerThread is dropped.
    join_handle: Option<JoinHandle<()>>,
}

struct Executor<E> {
    instance: Weak<E>,
    closed: AtomicBool,
    in_flight: Mutex<()>,
}

impl<T, E> BackgroundThread<T, E>
where
    T: Send + 'static,
    E: MainThreadExecutor<T> + 'static,
{
    pub fn get_or_create(executor: Weak<E>) -> Self {
        Self {
            executor: Arc::new(Executor {
                instance: executor,
                closed: AtomicBool::new(false),
                in_flight: Mutex::new(()),
            }),
            // The same worker thread can be shared by multiple instances. Lifecycle management
            // happens through reference counting.
            worker_thread: get_or_create_worker_thread(),
        }
    }

    pub fn schedule(&self, task: T) -> bool {
        if self.executor.closed.load(Ordering::Acquire) {
            return false;
        }
        // NOTE: This may check the current thread ID, which involves an allocation whenever this
        //       first happens on a new thread because of the way thread local storage works
        permit_alloc(|| {
            self.worker_thread
                .tasks_sender
                .as_ref()
                .expect("worker sender is present while handles are live")
                .try_send((task, Arc::clone(&self.executor)))
                .is_ok()
        })
    }

    /// Close this instance to new work and wait for its current task, if any, to finish.
    /// Call on the host's main thread before invalidating its instance-specific callbacks.
    pub fn quiesce(&self) {
        self.executor.closed.store(true, Ordering::Release);
        let _task = self.executor.in_flight.lock();
    }
}

// Rust does not allow us to use the `T` and `E` type variable in statics, so this is a
// workaround to have a singleton that also works if for whatever reason there are multiple `T`
// and `E`s in a single process (won't happen with normal plugin usage, but who knows).
static HANDLE_MAP: LazyLock<Mutex<anymap3::Map<dyn std::any::Any + Send>>> =
    LazyLock::new(|| Mutex::new(anymap3::Map::new()));

impl<T: Send + 'static, E: MainThreadExecutor<T> + 'static> WorkerThread<T, E> {
    fn spawn() -> Self {
        let (tasks_sender, tasks_receiver) =
            crossbeam::channel::bounded(super::TASK_QUEUE_CAPACITY);
        let join_handle = thread::Builder::new()
            .name(String::from("bg-worker"))
            .spawn(move || worker_thread(tasks_receiver))
            .expect("Could not spawn background worker thread");

        Self {
            join_handle: Some(join_handle),
            tasks_sender: Some(tasks_sender),
        }
    }
}

impl<T, E> Drop for WorkerThread<T, E> {
    fn drop(&mut self) {
        // Disconnecting the channel wakes the worker after existing entries are drained. This
        // does not require a free slot or wait for space on the worker's own thread.
        drop(self.tasks_sender.take());
        if let Some(join) = self.join_handle.take()
            && join.thread().id() != thread::current().id()
            && join.join().is_err()
        {
            nice_trace!("Background worker panicked");
        }
    }
}

/// Either acquire a handle for an existing worker thread or create one if it does not yet exists.
/// This allows multiple plugin instances to share a worker thread. Reference counting happens
/// automatically as part of this function and `WorkerThreadHandle`'s lifecycle.
fn get_or_create_worker_thread<T, E>() -> Arc<WorkerThread<T, E>>
where
    T: Send + 'static,
    E: MainThreadExecutor<T> + 'static,
{
    let mut handle_map = HANDLE_MAP.lock();

    match handle_map.entry::<Weak<WorkerThread<T, E>>>() {
        Entry::Occupied(mut entry) => {
            let weak = entry.get_mut();
            if let Some(arc) = weak.upgrade() {
                arc
            } else {
                let arc = Arc::new(WorkerThread::spawn());
                *weak = Arc::downgrade(&arc);
                arc
            }
        }
        Entry::Vacant(entry) => {
            let arc = Arc::new(WorkerThread::spawn());
            entry.insert(Arc::downgrade(&arc));
            arc
        }
    }
}

/// The worker thread used in [`EventLoop`] that executes incoming tasks on the event loop's
/// executor.
fn worker_thread<T, E>(tasks_receiver: crossbeam::channel::Receiver<(T, Arc<Executor<E>>)>)
where
    T: Send,
    E: MainThreadExecutor<T> + 'static,
{
    while let Ok((task, executor)) = tasks_receiver.recv() {
        let _task = executor.in_flight.lock();
        if !executor.closed.load(Ordering::Acquire)
            && let Some(instance) = executor.instance.upgrade()
        {
            instance.execute(task, true);
        }
        // An expired or closed executor only affects its own tasks. Other instances share
        // this worker and can continue using it.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    type Task = Box<dyn FnOnce() + Send>;

    struct TestExecutor;

    impl MainThreadExecutor<Task> for TestExecutor {
        fn execute(&self, task: Task, _: bool) {
            task();
        }
    }

    const TIMEOUT: Duration = Duration::from_secs(2);

    #[test]
    fn an_expired_instance_does_not_stop_the_shared_worker() {
        let expired = Arc::new(TestExecutor);
        let live = Arc::new(TestExecutor);
        let expired_worker = BackgroundThread::get_or_create(Arc::downgrade(&expired));
        let live_worker = BackgroundThread::get_or_create(Arc::downgrade(&live));
        assert!(Arc::ptr_eq(
            &expired_worker.worker_thread,
            &live_worker.worker_thread
        ));
        drop(expired);
        assert!(expired_worker.schedule(Box::new(|| panic!("expired task was run"))));

        let (tx, rx) = mpsc::channel();
        assert!(live_worker.schedule(Box::new(move || tx.send(()).unwrap())));
        rx.recv_timeout(TIMEOUT).unwrap();
    }

    #[test]
    fn quiesce_waits_for_only_the_closing_instances_active_task() {
        let first = Arc::new(TestExecutor);
        let peer = Arc::new(TestExecutor);
        let worker = Arc::new(BackgroundThread::get_or_create(Arc::downgrade(&first)));
        let peer_worker = BackgroundThread::get_or_create(Arc::downgrade(&peer));
        let (entered_tx, entered_rx) = mpsc::channel();
        let (leave_tx, leave_rx) = mpsc::channel();
        assert!(worker.schedule(Box::new(move || {
            entered_tx.send(()).unwrap();
            let _ = leave_rx.recv_timeout(TIMEOUT);
        })));
        entered_rx.recv_timeout(TIMEOUT).unwrap();
        let (cancelled_tx, cancelled_rx) = mpsc::channel();
        assert!(worker.schedule(Box::new(move || cancelled_tx.send(()).unwrap())));

        let (peer_entered_tx, peer_entered_rx) = mpsc::channel();
        let (peer_leave_tx, peer_leave_rx) = mpsc::channel();
        assert!(peer_worker.schedule(Box::new(move || {
            peer_entered_tx.send(()).unwrap();
            let _ = peer_leave_rx.recv_timeout(TIMEOUT);
        })));

        let (closed_tx, closed_rx) = mpsc::channel();
        let closing = Arc::clone(&worker);
        let join = thread::spawn(move || {
            closing.quiesce();
            closed_tx.send(()).unwrap();
        });
        let start = std::time::Instant::now();
        while !worker.executor.closed.load(Ordering::Acquire) {
            assert!(start.elapsed() < TIMEOUT);
            thread::yield_now();
        }
        assert!(closed_rx.try_recv().is_err());
        assert!(!worker.schedule(Box::new(|| panic!("closed task was queued"))));
        leave_tx.send(()).unwrap();

        peer_entered_rx.recv_timeout(TIMEOUT).unwrap();
        closed_rx.recv_timeout(TIMEOUT).unwrap();
        assert!(matches!(
            cancelled_rx.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
        peer_leave_tx.send(()).unwrap();
        join.join().unwrap();
    }
}
