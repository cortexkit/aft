//! Run checkpoints off the committing thread without monopolizing shared aft.db.
//!
//! Commit hooks only queue work. Shared aft.db uses a guarded, short-lived
//! SQLite connection so checkpoint I/O holds neither the shared connection nor
//! its mutex. Private stores use weak owning-lock callbacks, and short-lived
//! stores hand over their connection at close. A stalled reader or worker is
//! reported as lag, never repaired by checkpointing on the committing thread.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use rusqlite::OpenFlags;

use super::file_identity::{self, FileIdentity};
use super::lifecycle::{CheckpointStats, SqliteStore, TrackedConnection, WalCheckpointMode};
use crate::write_ledger::Domain as WriteDomain;

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
    let stats = conn.checkpoint_stats();
    #[cfg(test)]
    let path = PathBuf::from(conn.path().unwrap_or_default());
    conn.set_checkpoint_owner(Box::new(move || {
        #[cfg(test)]
        test_hooks::wait_at_gate(&path);
        let Some(pool) = weak.upgrade() else {
            return true;
        };
        let conn = match pool.try_lock() {
            Ok(conn) => conn,
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return false,
        };
        let started = Instant::now();
        let finished = conn.checkpoint_in_background();
        stats.record(started.elapsed());
        finished
    }));
}

/// Shared aft.db must remain available while its pages are copied and synced.
/// Keep its lifetime, but never take its mutex on the checkpoint worker.
pub(crate) fn register_shared(pool: &Arc<Mutex<TrackedConnection>>) {
    let weak = Arc::downgrade(pool);
    let conn = pool
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(path) = conn
        .path()
        .filter(|path| !path.is_empty() && *path != ":memory:")
    else {
        return;
    };
    let path = PathBuf::from(path);
    let Some(identity) = file_identity::identity_of(&path) else {
        return;
    };
    let stats = conn.checkpoint_stats();
    let feedback = conn.checkpoint_feedback();
    conn.set_checkpoint_owner(Box::new(move || {
        let Some(_resident) = weak.upgrade() else {
            return true;
        };
        let finished = checkpoint_shared(&path, identity, &stats);
        feedback(finished);
        finished
    }));
}

fn checkpoint_shared(path: &Path, identity: FileIdentity, stats: &CheckpointStats) -> bool {
    let connection = {
        // Opening and the checks before it are serialized with AFT's own
        // database removals and replacements.
        let _guard = file_identity::filesystem_guard();
        // A different file at the same path (the database was deleted and
        // recreated) would share SQLite's per-path WAL-index with the file
        // the asking connection still has open; opening it would truncate
        // the index under that connection.
        if file_identity::identity_of(path) != Some(identity) {
            return true;
        }
        // Never be the only connection: there is no live WAL to checkpoint,
        // and closing the last connection runs SQLite's own checkpoint.
        if file_identity::open_connections(path) == 0 {
            return true;
        }
        // No SQLITE_OPEN_CREATE: a database removed since the request is
        // left removed.
        match TrackedConnection::open_path_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            SqliteStore::AftDb,
        ) {
            Ok(connection) => connection,
            Err(error) => {
                crate::slog_warn!(
                    "deferred WAL checkpoint of {} skipped: {error}",
                    path.display()
                );
                return false;
            }
        }
    };
    #[cfg(test)]
    test_hooks::wait_at_gate(path);
    // The same sync policy the database's writers use, so the checkpoint
    // syncs exactly what an inline one would.
    if let Err(error) = connection.pragma_update(None, "synchronous", "NORMAL") {
        crate::slog_warn!(
            "deferred WAL checkpoint of {} skipped: {error}",
            path.display()
        );
        return false;
    }
    stats.record(Duration::ZERO);
    match connection.checkpoint_wal_as(WalCheckpointMode::Passive, WriteDomain::AftDb) {
        Ok(result) => result.busy == 0 && result.checkpointed_frames >= result.log_frames,
        Err(error) => {
            crate::slog_warn!(
                "deferred WAL checkpoint of {} failed: {error}",
                path.display()
            );
            false
        }
    }
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
mod tests {
    use super::*;

    #[test]
    fn shared_checkpoint_skips_unowned_replaced_and_removed_databases() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aft.db");
        let replacement_path = dir.path().join("replacement.db");
        let resident = TrackedConnection::open(&path, SqliteStore::AftDb).unwrap();
        let replacement = TrackedConnection::open(&replacement_path, SqliteStore::AftDb).unwrap();
        let identity = file_identity::identity_of(&path).unwrap();
        let other_identity = file_identity::identity_of(&replacement_path).unwrap();
        let stats = resident.checkpoint_stats();
        assert_ne!(identity, other_identity);
        assert!(checkpoint_shared(&path, other_identity, &stats));
        assert_eq!(file_identity::open_connections(&path), 1);
        drop(resident);
        assert!(checkpoint_shared(&path, identity, &stats));
        assert_eq!(file_identity::open_connections(&path), 0);
        drop(replacement);
        std::fs::remove_file(&path).unwrap();
        std::fs::rename(&replacement_path, &path).unwrap();
        assert!(checkpoint_shared(&path, identity, &stats));
        std::fs::remove_file(&path).unwrap();
        assert!(checkpoint_shared(&path, identity, &stats));
        assert!(
            !path.exists(),
            "a checkpoint request recreated a removed database"
        );
        assert_eq!(
            stats.snapshot().0,
            0,
            "a guarded-out request reached checkpoint I/O"
        );
    }
}

#[cfg(test)]
pub(crate) mod test_hooks {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};
    use std::time::{Duration, Instant};

    #[derive(Default)]
    struct State {
        finished: HashMap<PathBuf, u64>,
        gate_closed: bool,
        gate_path: PathBuf,
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

    pub(super) fn wait_at_gate(path: &Path) {
        let (lock, changed) = state();
        let mut state = lock.lock().unwrap();
        if !state.gate_closed || state.gate_path != path {
            return;
        }
        state.waiting_at_gate += 1;
        changed.notify_all();
        while state.gate_closed {
            state = changed.wait(state).unwrap();
        }
        state.waiting_at_gate -= 1;
    }

    pub(crate) struct Gate {
        _serial: MutexGuard<'static, ()>,
    }

    impl Drop for Gate {
        fn drop(&mut self) {
            open_gate();
        }
    }

    /// Hold this database's checkpoint before I/O, but after a shared worker
    /// opens its temporary handle. Unrelated stores remain free to drain.
    pub(crate) fn close_gate(path: &Path) -> Gate {
        static SERIAL: Mutex<()> = Mutex::new(());
        let serial = SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (lock, _) = state();
        let mut state = lock.lock().unwrap();
        state.gate_closed = true;
        state.gate_path = path.to_path_buf();
        Gate { _serial: serial }
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

    /// Wait until every close and checkpoint already due on the worker has run.
    /// A closed store hands its SQLite handle to the worker, so its descriptors
    /// stay open briefly after the owner returns; count descriptors only after
    /// this. A handle the worker never releases still shows up afterwards.
    pub(crate) fn drain(timeout: Duration) -> bool {
        let (done, finished) = std::sync::mpsc::channel();
        super::close(move || {
            let _ = done.send(());
        });
        finished.recv_timeout(timeout).is_ok()
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
