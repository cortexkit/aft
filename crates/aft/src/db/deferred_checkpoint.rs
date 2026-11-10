//! WAL checkpoints that a commit asked for but must not wait on.
//!
//! SQLite runs a PASSIVE checkpoint inside whichever commit pushes the WAL
//! past its autocheckpoint threshold (see `tracked_wal_hook`). The checkpoint
//! copies the WAL's pages into the database file and syncs both files, which
//! under heavy disk write load can take seconds, all while the committing
//! thread holds its connection's mutex. A commit made while a caller waits,
//! such as the undo-history mirror of an edit, hands its checkpoint here
//! instead (see `defer_wal_checkpoints`). One worker thread runs each one on
//! a short-lived connection of its own, so it holds neither the committing
//! connection nor its mutex, and a PASSIVE checkpoint never blocks the
//! database's readers or writers.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, OnceLock};

use rusqlite::OpenFlags;

use super::file_identity::{self, FileIdentity};
use super::lifecycle::{SqliteStore, TrackedConnection, WalCheckpointMode};
use crate::write_ledger::Domain as WriteDomain;

struct Request {
    path: PathBuf,
    identity: FileIdentity,
}

struct Queue {
    pending: Mutex<VecDeque<Request>>,
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

/// Ask the worker to checkpoint the database at `path`, but only while the
/// file there is still `identity`, the file the asking connection uses. A
/// request for a database that already has one waiting is dropped: the
/// waiting checkpoint will copy every frame committed before it runs.
pub(crate) fn request(path: &Path, identity: FileIdentity) {
    let queue = queue();
    let mut pending = queue
        .pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if pending.iter().any(|waiting| waiting.path == path) {
        return;
    }
    pending.push_back(Request {
        path: path.to_path_buf(),
        identity,
    });
    queue.ready.notify_one();
}

fn run(queue: &'static Queue) {
    loop {
        let request = {
            let mut pending = queue
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            loop {
                if let Some(request) = pending.pop_front() {
                    break request;
                }
                pending = queue
                    .ready
                    .wait(pending)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        };
        checkpoint(&request);
        #[cfg(test)]
        test_hooks::note_finished(&request.path);
    }
}

fn checkpoint(request: &Request) {
    let connection = {
        // Opening and the checks before it are serialized with AFT's own
        // database removals and replacements.
        let _guard = file_identity::filesystem_guard();
        // A different file at the same path (the database was deleted and
        // recreated) would share SQLite's per-path WAL-index with the file
        // the asking connection still has open; opening it would truncate
        // the index under that connection.
        if file_identity::identity_of(&request.path) != Some(request.identity) {
            return;
        }
        // Never be the only connection: there is no live WAL to checkpoint,
        // and closing the last connection runs SQLite's own checkpoint.
        if file_identity::open_connections(&request.path) == 0 {
            return;
        }
        // No SQLITE_OPEN_CREATE: a database removed since the request is
        // left removed.
        match TrackedConnection::open_path_with_flags(
            &request.path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            SqliteStore::AftDb,
        ) {
            Ok(connection) => connection,
            Err(error) => {
                crate::slog_warn!(
                    "deferred WAL checkpoint of {} skipped: {error}",
                    request.path.display()
                );
                return;
            }
        }
    };
    #[cfg(test)]
    test_hooks::wait_at_gate();
    // The same sync policy the database's writers use, so the checkpoint
    // syncs exactly what an inline one would.
    if let Err(error) = connection.pragma_update(None, "synchronous", "NORMAL") {
        crate::slog_warn!(
            "deferred WAL checkpoint of {} skipped: {error}",
            request.path.display()
        );
        return;
    }
    if let Err(error) = connection.checkpoint_wal_as(WalCheckpointMode::Passive, WriteDomain::AftDb)
    {
        crate::slog_warn!(
            "deferred WAL checkpoint of {} failed: {error}",
            request.path.display()
        );
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

    pub(super) fn note_finished(path: &Path) {
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

    /// Hold every deferred checkpoint after it opens its connection until
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
