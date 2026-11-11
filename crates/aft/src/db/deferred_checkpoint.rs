//! Checkpoint through the owning connection, never a second SQLite handle.
//!
//! Commit hooks only queue work. Resident stores register a weak reference to
//! their connection mutex; the worker retries if a request currently owns it.
//! Short-lived stores hand over their connection at close instead. A stalled
//! reader or worker is reported as checkpoint lag, never repaired by doing
//! checkpoint I/O on the committing thread.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::lifecycle::TrackedConnection;

pub(crate) struct Request {
    path: PathBuf,
    queued: AtomicBool,
    work: Box<dyn Fn() -> bool + Send + Sync>,
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CheckpointRequest")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

enum Job {
    Checkpoint(Arc<Request>),
    Close(Box<dyn FnOnce() + Send>),
}

struct Queue {
    pending: Mutex<VecDeque<(Instant, Job)>>,
    ready: Condvar,
}

fn queue() -> &'static Queue {
    static QUEUE: OnceLock<Queue> = OnceLock::new();
    static WORKER: OnceLock<()> = OnceLock::new();
    let queue = QUEUE.get_or_init(|| Queue {
        pending: Mutex::new(VecDeque::new()),
        ready: Condvar::new(),
    });
    WORKER.get_or_init(|| {
        let spawned = std::thread::Builder::new()
            .name("aft-wal-checkpoint".to_string())
            .spawn(move || run(queue));
        if let Err(error) = spawned {
            crate::slog_warn!("WAL checkpoint worker could not start: {error}");
        }
    });
    queue
}

fn enqueue(job: Job, delay: Duration) {
    let queue = queue();
    queue
        .pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push_back((Instant::now() + delay, job));
    queue.ready.notify_one();
}

impl Request {
    pub(crate) fn schedule(self: &Arc<Self>) {
        self.schedule_after(Duration::ZERO);
    }

    fn schedule_after(self: &Arc<Self>, delay: Duration) {
        if !self.queued.swap(true, Ordering::AcqRel) {
            enqueue(Job::Checkpoint(Arc::clone(self)), delay);
        }
    }
}

/// Attach once, after putting the connection behind its resident owning lock.
/// The callback retains only a weak reference, so queued work cannot keep an
/// evicted store (or its connection) alive.
pub(crate) fn register(pool: &Arc<Mutex<TrackedConnection>>) {
    let weak = Arc::downgrade(pool);
    let conn = pool
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    conn.set_checkpoint_owner(Box::new(move || {
        let Some(pool) = weak.upgrade() else {
            return true;
        };
        let conn = match pool.try_lock() {
            Ok(conn) => conn,
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return false,
        };
        conn.checkpoint_in_background()
    }));
}

pub(crate) fn owner(path: &Path, work: Box<dyn Fn() -> bool + Send + Sync>) -> Arc<Request> {
    Arc::new(Request {
        path: path.to_path_buf(),
        queued: AtomicBool::new(false),
        work,
    })
}

pub(crate) fn close(work: impl FnOnce() + Send + 'static) {
    enqueue(Job::Close(Box::new(work)), Duration::ZERO);
}

fn run(queue: &'static Queue) {
    loop {
        let job = {
            let mut pending = queue
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            loop {
                let now = Instant::now();
                if let Some(index) = pending.iter().position(|(at, _)| *at <= now) {
                    break pending.remove(index).unwrap().1;
                }
                pending = if let Some(next) = pending.iter().map(|(at, _)| *at).min() {
                    queue
                        .ready
                        .wait_timeout(pending, next.saturating_duration_since(now))
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .0
                } else {
                    queue
                        .ready
                        .wait(pending)
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                };
            }
        };
        match job {
            Job::Close(work) => work(),
            Job::Checkpoint(request) => {
                #[cfg(test)]
                test_hooks::wait_at_gate();
                request.queued.store(false, Ordering::Release);
                if !(request.work)() {
                    request.schedule_after(Duration::from_millis(100));
                } else {
                    #[cfg(test)]
                    test_hooks::note_finished(&request.path);
                }
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod test_hooks {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Condvar, Mutex, OnceLock};
    use std::time::{Duration, Instant};

    #[derive(Default)]
    struct State {
        finished: HashMap<PathBuf, u64>,
        gate_closed: bool,
        waiting_at_gate: u64,
    }

    fn state() -> &'static (Mutex<State>, Condvar) {
        static STATE: OnceLock<(Mutex<State>, Condvar)> = OnceLock::new();
        STATE.get_or_init(Default::default)
    }

    pub(crate) fn note_finished(path: &Path) {
        let (lock, changed) = state();
        let mut state = lock.lock().unwrap();
        *state.finished.entry(path.to_path_buf()).or_default() += 1;
        changed.notify_all();
    }

    pub(super) fn wait_at_gate() {
        let (lock, changed) = state();
        let mut state = lock.lock().unwrap();
        state.waiting_at_gate += 1;
        changed.notify_all();
        while state.gate_closed {
            state = changed.wait(state).unwrap();
        }
        state.waiting_at_gate -= 1;
    }

    /// Hold every deferred checkpoint before it takes the owning lock until
    /// [`open_gate`]. One test at a time may close the gate.
    pub(crate) fn close_gate() {
        let (lock, _) = state();
        lock.lock().unwrap().gate_closed = true;
    }

    pub(crate) fn open_gate() {
        let (lock, changed) = state();
        lock.lock().unwrap().gate_closed = false;
        changed.notify_all();
    }

    /// Wait until a deferred checkpoint is held at the closed gate.
    pub(crate) fn wait_until_held(timeout: Duration) -> bool {
        let (lock, changed) = state();
        let deadline = Instant::now() + timeout;
        let mut state = lock.lock().unwrap();
        while state.waiting_at_gate == 0 {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            state = changed.wait_timeout(state, deadline - now).unwrap().0;
        }
        true
    }

    /// Deferred checkpoints of `path` finished so far.
    pub(crate) fn finished(path: &Path) -> u64 {
        let (lock, _) = state();
        lock.lock()
            .unwrap()
            .finished
            .get(path)
            .copied()
            .unwrap_or(0)
    }

    pub(crate) fn wait_closed(path: &Path, timeout: Duration) -> bool {
        let (lock, changed) = state();
        let deadline = Instant::now() + timeout;
        let mut state = lock.lock().unwrap();
        while crate::db::file_identity::open_connections(path) > 0 {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            state = changed.wait_timeout(state, deadline - now).unwrap().0;
        }
        true
    }

    /// Wait until more than `seen` deferred checkpoints of `path` finished.
    pub(crate) fn wait_finished_after(path: &Path, seen: u64, timeout: Duration) -> bool {
        let (lock, changed) = state();
        let deadline = Instant::now() + timeout;
        let mut state = lock.lock().unwrap();
        while state.finished.get(path).copied().unwrap_or(0) <= seen {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            state = changed.wait_timeout(state, deadline - now).unwrap().0;
        }
        true
    }
}
