use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};

type Removal = (
    PathBuf,
    Sender<std::io::Result<()>>,
    Arc<super::registry::DbWriteOrder>,
);
const QUEUE_CAPACITY: usize = 64;

/// First wait before removing a task's scratch again after a failed attempt.
/// Each further failure doubles it, up to [`RETRY_MAX`].
const RETRY_BASE: Duration = Duration::from_secs(1);
/// Longest wait between removal attempts for one task's scratch. A removal
/// that keeps failing (read-only folders a build left behind, a file another
/// process still holds) is retried this often, not on every watchdog tick.
const RETRY_MAX: Duration = Duration::from_secs(5 * 60);

/// When a task's scratch removal may be attempted again after failures.
///
/// Every observer of a finished task (the watchdog pass twice a second, the
/// finished-task cleanup, replay) asks for its scratch to be removed. Without
/// spacing, a removal that fails the same way every time was rescheduled on
/// each of those calls, which kept the removal worker busy around the clock
/// for nothing.
#[derive(Debug, Default)]
pub(crate) struct RetryBackoff {
    failures: u32,
    not_before: Option<Instant>,
}

impl RetryBackoff {
    /// Whether a new removal attempt may be scheduled at `now`.
    pub(crate) fn ready(&self, now: Instant) -> bool {
        self.not_before.is_none_or(|not_before| now >= not_before)
    }

    /// Records a failed attempt that finished at `now`.
    pub(crate) fn failed(&mut self, now: Instant) {
        self.failures = self.failures.saturating_add(1);
        let doublings = self.failures.saturating_sub(1).min(16);
        let delay = RETRY_BASE.saturating_mul(1 << doublings).min(RETRY_MAX);
        self.not_before = Some(now + delay);
    }
}

/// The removal worker's queue. The worker sleeps on a condition variable while
/// the queue is empty, so an idle worker costs nothing and each request wakes
/// it once.
struct Queue {
    state: Mutex<QueueState>,
    ready: Condvar,
    capacity: usize,
}

#[derive(Default)]
struct QueueState {
    entries: VecDeque<Removal>,
    /// Set when the worker has stopped; nothing queued after that would run.
    closed: bool,
}

impl Queue {
    fn new(capacity: usize) -> Self {
        Self {
            state: Mutex::new(QueueState::default()),
            ready: Condvar::new(),
            capacity,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, QueueState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Queues a removal unless the queue is full or the worker has stopped.
    /// Never waits for the worker.
    fn try_push(&self, removal: Removal) -> bool {
        let mut state = self.lock();
        if state.closed || state.entries.len() >= self.capacity {
            return false;
        }
        state.entries.push_back(removal);
        drop(state);
        self.ready.notify_one();
        true
    }

    /// Waits until a removal is queued.
    fn pop(&self) -> Removal {
        let mut state = self.lock();
        loop {
            if let Some(removal) = state.entries.pop_front() {
                return removal;
            }
            state = self
                .ready
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    fn close(&self) {
        let mut state = self.lock();
        state.closed = true;
        // Dropping the queued result senders tells their observers that the
        // worker stopped, so they retry instead of waiting forever.
        state.entries.clear();
    }
}

fn run_worker(queue: &Queue) {
    /// Marks the queue closed however the worker exits.
    struct Close<'a>(&'a Queue);
    impl Drop for Close<'_> {
        fn drop(&mut self) {
            self.0.close();
        }
    }
    let _close = Close(queue);
    loop {
        let (path, result, owner) = queue.pop();
        let removed = std::panic::catch_unwind(|| remove(&path))
            .unwrap_or_else(|_| Err(std::io::Error::other("scratch cleanup panicked")));
        let _ = result.send(removed);
        // The task's ownership is released only once its result is published.
        drop(owner);
    }
}

/// Scratch deletion has its own single worker: the periodic maintenance lane
/// only runs once a minute, and timeout workers must remain free to kill tasks.
pub(super) fn schedule(
    path: &Path,
    owner: Arc<super::registry::DbWriteOrder>,
) -> Option<Receiver<std::io::Result<()>>> {
    static QUEUE: Mutex<Option<Arc<Queue>>> = Mutex::new(None);
    let queue = {
        let mut slot = QUEUE.lock().unwrap_or_else(|poison| poison.into_inner());
        if slot.is_none() {
            let queue = Arc::new(Queue::new(QUEUE_CAPACITY));
            let worker = Arc::clone(&queue);
            match std::thread::Builder::new()
                .name("aft-bash-scratch-cleanup".into())
                .spawn(move || run_worker(&worker))
            {
                Ok(_) => *slot = Some(queue),
                Err(error) => crate::slog_warn!("failed to start bash scratch cleanup: {error}"),
            }
        }
        slot.clone()?
    };
    schedule_on(&queue, path, owner)
}

fn schedule_on(
    queue: &Queue,
    path: &Path,
    owner: Arc<super::registry::DbWriteOrder>,
) -> Option<Receiver<std::io::Result<()>>> {
    let (tx, rx) = crossbeam_channel::bounded(1);
    // Full queues leave the durable scratch path untouched for a later pass or
    // the stale sweep. Never wait for the worker from a task observer.
    // Retain the task's process-wide ownership even if its registry unloads,
    // so persisted GC cannot remove the record while deletion is still running.
    queue
        .try_push((path.to_path_buf(), tx, owner))
        .then_some(rx)
}

pub(super) fn remove(path: &Path) -> std::io::Result<()> {
    #[cfg(all(test, unix))]
    {
        let gate = removal_gates().lock().unwrap().remove(path);
        if let Some((entered, release)) = gate {
            let _ = entered.send(());
            let _ = release.recv_timeout(std::time::Duration::from_secs(30));
        }
        let mut failing = failing_removals().lock().unwrap();
        if let Some(attempts) = failing.get_mut(path) {
            *attempts += 1;
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "injected scratch removal failure",
            ));
        }
    }
    crate::sandbox_spawn::cleanup_task_temp_dir(path)
}

/// Scratch paths whose removal fails on purpose, with the number of attempts.
#[cfg(all(test, unix))]
fn failing_removals() -> &'static Mutex<std::collections::HashMap<PathBuf, usize>> {
    static FAILING: std::sync::OnceLock<Mutex<std::collections::HashMap<PathBuf, usize>>> =
        std::sync::OnceLock::new();
    FAILING.get_or_init(Default::default)
}

/// Makes every removal of `path` fail until [`stop_failing_removal`], which
/// returns how many removals were attempted meanwhile.
#[cfg(all(test, unix))]
pub(super) fn fail_removal(path: &Path) {
    failing_removals()
        .lock()
        .unwrap()
        .insert(path.to_path_buf(), 0);
}

#[cfg(all(test, unix))]
pub(super) fn stop_failing_removal(path: &Path) -> usize {
    failing_removals().lock().unwrap().remove(path).unwrap_or(0)
}

#[cfg(all(test, unix))]
type RemovalGate = (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>);

#[cfg(all(test, unix))]
fn removal_gates(
) -> &'static std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, RemovalGate>> {
    static GATES: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, RemovalGate>>,
    > = std::sync::OnceLock::new();
    GATES.get_or_init(Default::default)
}

#[cfg(all(test, unix))]
pub(super) fn block_removal(
    path: &Path,
) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    removal_gates()
        .lock()
        .unwrap()
        .insert(path.to_path_buf(), (entered_tx, release_rx));
    (entered_rx, release_tx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bash_background_temp_cleanup_full_queue_defers_without_waiting() {
        let queue = Queue::new(1);
        let owner = Arc::new(super::super::registry::DbWriteOrder::default());
        let first = schedule_on(&queue, Path::new("first"), owner.clone()).unwrap();
        assert!(schedule_on(&queue, Path::new("deferred"), owner.clone()).is_none());
        let (path, result, _) = queue.pop();
        assert_eq!(path, Path::new("first"));
        result.send(Ok(())).unwrap();
        first.recv().unwrap().unwrap();
        let retry = schedule_on(&queue, Path::new("retry"), owner.clone()).unwrap();
        queue.close();
        assert!(
            retry.recv().is_err(),
            "a removal queued before the worker stopped kept its observer waiting"
        );
        assert!(schedule_on(&queue, Path::new("disconnected"), owner).is_none());
    }

    #[test]
    fn bash_background_temp_cleanup_retry_backoff_doubles_up_to_its_cap() {
        let start = Instant::now();
        let mut backoff = RetryBackoff::default();
        assert!(backoff.ready(start));
        let mut waits = Vec::new();
        for _ in 0..12 {
            backoff.failed(start);
            waits.push(backoff.not_before.unwrap() - start);
            assert!(!backoff.ready(start));
        }
        assert_eq!(waits[0], RETRY_BASE);
        assert_eq!(waits[1], RETRY_BASE * 2);
        assert_eq!(waits[2], RETRY_BASE * 4);
        assert_eq!(*waits.last().unwrap(), RETRY_MAX);
        assert!(backoff.ready(start + RETRY_MAX));
    }
}
