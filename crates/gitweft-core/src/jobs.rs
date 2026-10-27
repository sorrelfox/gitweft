//! Background jobs.
//!
//! One [`WorkerPool`] is shared by the whole application. Each open repository gets a
//! [`RepoJobs`], which puts every job into one of three [`Lane`]s:
//!
//! * [`Lane::Latest`]: a new job cancels the previous job with the same key
//!   (e.g. the diff of the selected file).
//! * [`Lane::Coalesce`]: at most one running and one pending job per key, and a new
//!   request replaces the pending one (e.g. a status refresh after a burst of file events).
//! * [`Lane::Write`]: a strict FIFO on a dedicated thread, for every mutation and
//!   network operation of the repository. Reads never wait for it.
//!
//! Results are delivered through [`JobHandle`], which the UI awaits from its own executor.

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::thread;

use gitweft_git::CancelToken;

pub type JobKey = &'static str;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    Latest(JobKey),
    Coalesce(JobKey),
    Write,
}

type Task = Box<dyn FnOnce() + Send + 'static>;

/// Threads that run read jobs for all repositories.
pub struct WorkerPool {
    tx: mpsc::Sender<Task>,
}

impl WorkerPool {
    /// A pool sized to the machine (2–8 threads).
    pub fn new() -> Arc<Self> {
        let workers = thread::available_parallelism()
            .map_or(2, |n| n.get())
            .clamp(2, 8);
        Self::with_workers(workers)
    }

    pub fn with_workers(workers: usize) -> Arc<Self> {
        let (tx, rx) = mpsc::channel::<Task>();
        let rx = Arc::new(Mutex::new(rx));
        for index in 0..workers.max(1) {
            let rx = Arc::clone(&rx);
            thread::Builder::new()
                .name(format!("gitweft-worker-{index}"))
                .spawn(move || {
                    loop {
                        // The guard is dropped before the task runs, so other workers can
                        // pick up jobs meanwhile.
                        let next = lock(&rx).recv();
                        match next {
                            Ok(task) => run_task(task),
                            Err(_) => break, // pool dropped
                        }
                    }
                })
                .expect("spawn worker thread");
        }
        Arc::new(Self { tx })
    }

    fn submit(&self, task: Task) {
        // Fails only if every worker has exited, which only happens on shutdown.
        let _ = self.tx.send(task);
    }
}

/// The result side of a spawned job.
pub struct JobHandle<T> {
    cancel: CancelToken,
    rx: async_channel::Receiver<T>,
}

impl<T> JobHandle<T> {
    pub fn cancel_token(&self) -> &CancelToken {
        &self.cancel
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// The job's result, or `None` if it was cancelled, superseded or panicked.
    pub async fn join(self) -> Option<T> {
        self.rx.recv().await.ok()
    }

    /// Blocking variant of [`JobHandle::join`], for tests and worker code.
    pub fn join_blocking(self) -> Option<T> {
        self.rx.recv_blocking().ok()
    }
}

/// Job lanes for one repository.
#[derive(Clone)]
pub struct RepoJobs {
    inner: Arc<Inner>,
}

struct Inner {
    pool: Arc<WorkerPool>,
    write_tx: mpsc::Sender<Task>,
    /// Write jobs queued or running (see [`RepoJobs::pending_writes`]).
    writes: AtomicUsize,
    latest: Mutex<HashMap<JobKey, CancelToken>>,
    coalesce: Mutex<HashMap<JobKey, CoalesceSlot>>,
}

#[derive(Default)]
struct CoalesceSlot {
    running: bool,
    pending: Option<Task>,
}

impl RepoJobs {
    pub fn new(pool: Arc<WorkerPool>) -> Self {
        let (write_tx, write_rx) = mpsc::channel::<Task>();
        thread::Builder::new()
            .name("gitweft-writer".into())
            .spawn(move || write_rx.into_iter().for_each(run_task))
            .expect("spawn writer thread");
        Self {
            inner: Arc::new(Inner {
                pool,
                write_tx,
                writes: AtomicUsize::new(0),
                latest: Mutex::default(),
                coalesce: Mutex::default(),
            }),
        }
    }

    /// Schedules `job`. It receives a [`CancelToken`] to poll (and to pass to gix or
    /// the git runner). A job cancelled before it starts never runs.
    pub fn spawn<T, F>(&self, lane: Lane, job: F) -> JobHandle<T>
    where
        T: Send + 'static,
        F: FnOnce(&CancelToken) -> T + Send + 'static,
    {
        let cancel = CancelToken::new();
        let (tx, rx) = async_channel::bounded(1);
        let token = cancel.clone();
        let task: Task = Box::new(move || {
            if token.is_cancelled() {
                return; // dropping `tx` resolves the handle to None
            }
            let result = job(&token);
            if !token.is_cancelled() {
                let _ = tx.send_blocking(result);
            }
        });

        match lane {
            Lane::Latest(key) => {
                if let Some(previous) = lock(&self.inner.latest).insert(key, cancel.clone()) {
                    previous.cancel();
                }
                self.inner.pool.submit(task);
            }
            Lane::Coalesce(key) => Inner::submit_coalesced(&self.inner, key, task),
            Lane::Write => {
                // Counted until it's done (or dropped unrun), so the UI can tell that a
                // change waits behind a long job.
                self.inner.writes.fetch_add(1, Ordering::SeqCst);
                let counted = Counted(self.inner.clone());
                let _ = self.inner.write_tx.send(Box::new(move || {
                    let _counted = counted;
                    task();
                }));
            }
        }
        JobHandle { cancel, rx }
    }

    /// Write jobs queued or running: more than one means a change is waiting behind another
    /// (a `git bisect run`, a slow fetch), in order.
    pub fn pending_writes(&self) -> usize {
        self.inner.writes.load(Ordering::SeqCst)
    }
}

/// One write job counted in [`Inner::writes`] until it's dropped: after it ran, panicked
/// or never ran.
struct Counted(Arc<Inner>);

impl Drop for Counted {
    fn drop(&mut self) {
        self.0.writes.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Inner {
    fn submit_coalesced(this: &Arc<Self>, key: JobKey, task: Task) {
        let mut slots = lock(&this.coalesce);
        let slot = slots.entry(key).or_default();
        if slot.running {
            // Replaces (and thereby drops) any older pending request for this key.
            slot.pending = Some(task);
            return;
        }
        slot.running = true;
        drop(slots);
        this.pool.submit(Self::coalesced(this, key, task));
    }

    /// Wraps `task` so that, once it finishes, the pending request for `key` (if any) runs.
    fn coalesced(this: &Arc<Self>, key: JobKey, task: Task) -> Task {
        let inner = Arc::clone(this);
        Box::new(move || {
            run_task(task);
            let next = {
                let mut slots = lock(&inner.coalesce);
                let slot = slots.entry(key).or_default();
                let next = slot.pending.take();
                slot.running = next.is_some();
                next
            };
            if let Some(next) = next {
                inner.pool.submit(Self::coalesced(&inner, key, next));
            }
        })
    }
}

fn run_task(task: Task) {
    if catch_unwind(AssertUnwindSafe(task)).is_err() {
        eprintln!("gitweft: a background job panicked");
    }
}

/// Locks a mutex, recovering from poisoning (a panicking job never holds these locks).
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// A job that blocks its worker until `release` is sent.
    fn gate() -> (mpsc::Sender<()>, impl FnOnce(&CancelToken) + Send + 'static) {
        let (release, wait) = mpsc::channel::<()>();
        (release, move |_: &CancelToken| {
            let _ = wait.recv_timeout(Duration::from_secs(5));
        })
    }

    #[test]
    fn latest_cancels_previous_job() {
        let jobs = RepoJobs::new(WorkerPool::with_workers(2));
        let first = jobs.spawn(Lane::Latest("diff"), |cancel| {
            while !cancel.is_cancelled() {
                thread::sleep(Duration::from_millis(5));
            }
            "stale"
        });
        let second = jobs.spawn(Lane::Latest("diff"), |_| "fresh");
        assert_eq!(first.join_blocking(), None);
        assert_eq!(second.join_blocking(), Some("fresh"));
    }

    #[test]
    fn latest_keys_are_independent() {
        let jobs = RepoJobs::new(WorkerPool::with_workers(2));
        let a = jobs.spawn(Lane::Latest("a"), |_| 1);
        let b = jobs.spawn(Lane::Latest("b"), |_| 2);
        assert_eq!((a.join_blocking(), b.join_blocking()), (Some(1), Some(2)));
    }

    #[test]
    fn coalesce_keeps_one_running_and_latest_pending() {
        let jobs = RepoJobs::new(WorkerPool::with_workers(2));
        let runs = Arc::new(AtomicUsize::new(0));
        let (release, blocker) = gate();

        let running = jobs.spawn(Lane::Coalesce("status"), blocker);
        thread::sleep(Duration::from_millis(50)); // let it start
        let counted = |runs: &Arc<AtomicUsize>, value| {
            let runs = Arc::clone(runs);
            move |_: &CancelToken| {
                runs.fetch_add(1, Ordering::SeqCst);
                value
            }
        };
        let superseded = jobs.spawn(Lane::Coalesce("status"), counted(&runs, 1));
        let latest = jobs.spawn(Lane::Coalesce("status"), counted(&runs, 2));
        release.send(()).unwrap();

        assert_eq!(running.join_blocking(), Some(()));
        assert_eq!(superseded.join_blocking(), None);
        assert_eq!(latest.join_blocking(), Some(2));
        assert_eq!(
            runs.load(Ordering::SeqCst),
            1,
            "superseded request must not run"
        );

        // The slot is free again afterwards.
        assert_eq!(
            jobs.spawn(Lane::Coalesce("status"), |_| 3).join_blocking(),
            Some(3)
        );
    }

    #[test]
    fn writes_run_in_order_one_at_a_time() {
        let jobs = RepoJobs::new(WorkerPool::with_workers(4));
        let log = Arc::new(Mutex::new(Vec::new()));
        let active = Arc::new(AtomicUsize::new(0));
        let handles: Vec<_> = (0..20)
            .map(|i| {
                let (log, active) = (Arc::clone(&log), Arc::clone(&active));
                jobs.spawn(Lane::Write, move |_| {
                    assert_eq!(
                        active.fetch_add(1, Ordering::SeqCst),
                        0,
                        "writes overlapped"
                    );
                    thread::sleep(Duration::from_millis(1));
                    lock(&log).push(i);
                    active.fetch_sub(1, Ordering::SeqCst);
                })
            })
            .collect();
        for handle in handles {
            assert_eq!(handle.join_blocking(), Some(()));
        }
        assert_eq!(*lock(&log), (0..20).collect::<Vec<_>>());
    }

    #[test]
    fn pending_writes_count_the_queue_until_each_is_done() {
        let jobs = RepoJobs::new(WorkerPool::with_workers(1));
        assert_eq!(jobs.pending_writes(), 0);
        let (release, blocker) = gate();
        let long = jobs.spawn(Lane::Write, blocker);
        let waiting = jobs.spawn(Lane::Write, |_| 7);
        // The long one running, the next one waiting behind it.
        assert_eq!(jobs.pending_writes(), 2);
        release.send(()).unwrap();
        assert_eq!(long.join_blocking(), Some(()));
        assert_eq!(waiting.join_blocking(), Some(7));
        // A job that panics is done too.
        let panicking = jobs.spawn(Lane::Write, |_| -> u8 { panic!("a failing job") });
        assert_eq!(panicking.join_blocking(), None);
        // The count drops just after each result is sent.
        for _ in 0..200 {
            if jobs.pending_writes() == 0 {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(jobs.pending_writes(), 0);
    }

    #[test]
    fn cancelled_before_start_never_runs() {
        let jobs = RepoJobs::new(WorkerPool::with_workers(1));
        let (release, blocker) = gate();
        let busy = jobs.spawn(Lane::Latest("busy"), blocker);
        let ran = Arc::new(AtomicUsize::new(0));
        let ran_in_job = Arc::clone(&ran);
        let queued = jobs.spawn(Lane::Latest("queued"), move |_| {
            ran_in_job.fetch_add(1, Ordering::SeqCst);
        });
        queued.cancel();
        release.send(()).unwrap();
        assert_eq!(busy.join_blocking(), Some(()));
        assert_eq!(queued.join_blocking(), None);
        assert_eq!(ran.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_panicking_job_does_not_kill_its_worker() {
        let jobs = RepoJobs::new(WorkerPool::with_workers(1));
        let panicked = jobs.spawn(Lane::Latest("a"), |_| -> u32 { panic!("boom") });
        assert_eq!(panicked.join_blocking(), None);
        assert_eq!(
            jobs.spawn(Lane::Latest("b"), |_| 7).join_blocking(),
            Some(7)
        );
    }
}
