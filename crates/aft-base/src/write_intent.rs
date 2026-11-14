//! Synchronous write intent around every AFT filesystem mutation.
//!
//! An intent is deliberately not cleared when a command fails: formatting,
//! partial writes and rollback can all change bytes before the error returns.
//! Only whoever indexes the paths may resolve it, by reading their bytes again.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

/// Notifications bracket an AFT mutation: Before precedes any filesystem write;
/// After runs when the guard drops, after formatting or rollback even on error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WritePhase {
    Before,
    After,
}

/// Directory operations invalidate descendants, not just one indexed path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WriteIntent {
    Paths(Vec<PathBuf>),
    Directory(PathBuf),
}

/// The runtime uses this to invalidate trigram, semantic and callgraph data
/// under its shared revision/snapshot lock. Listeners filter paths to their
/// owning root. Callbacks must not write files, which would recurse into intent.
pub trait WriteIntentListener: Send + Sync {
    fn record_change(&self, change: &WriteIntent, phase: WritePhase);
}

fn listeners() -> &'static Mutex<Vec<Weak<dyn WriteIntentListener>>> {
    static LISTENERS: OnceLock<Mutex<Vec<Weak<dyn WriteIntentListener>>>> = OnceLock::new();
    LISTENERS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Register a weak listener reference, so dropping the driver stops callbacks.
pub fn register_listener(listener: &Arc<dyn WriteIntentListener>) {
    let mut registered = listeners().lock().unwrap_or_else(|e| e.into_inner());
    registered.retain(|listener| listener.strong_count() != 0);
    registered.push(Arc::downgrade(listener));
}

fn notify(
    registered: &[Weak<dyn WriteIntentListener>],
    changes: &[WriteIntent],
    phase: WritePhase,
) {
    for listener in registered {
        if let Some(listener) = listener.upgrade() {
            for change in changes {
                listener.record_change(change, phase);
            }
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static CLASSIFICATION_STATS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn is_directory(path: &Path) -> bool {
    #[cfg(any(test, feature = "test-support"))]
    CLASSIFICATION_STATS.with(|count| count.set(count.get() + 1));
    path.is_dir()
}

/// Called before the first possible filesystem mutation, not after success.
/// Directory intents invalidate the whole watcher epoch: their descendants
/// (including newly created names) cannot safely be enumerated from an index.
pub fn record_paths<'a>(paths: impl IntoIterator<Item = &'a Path>) -> IntentGuard {
    let listener_snapshot = {
        let mut registered = listeners().lock().unwrap_or_else(|e| e.into_inner());
        registered.retain(|listener| listener.strong_count() != 0);
        registered.clone()
    };
    if listener_snapshot.is_empty() {
        return IntentGuard {
            listeners: Vec::new(),
            changes: Vec::new(),
        };
    }
    let paths = paths
        .into_iter()
        .map(|path| (path, is_directory(path)))
        .collect::<Vec<_>>();
    let mut changes = paths
        .iter()
        .filter(|(_, is_dir)| *is_dir)
        .map(|(path, _)| WriteIntent::Directory(path.to_path_buf()))
        .collect::<Vec<_>>();
    let files = paths
        .iter()
        .filter(|(_, is_dir)| !*is_dir)
        .map(|(path, _)| path.to_path_buf())
        .collect::<Vec<_>>();
    if !files.is_empty() {
        changes.push(WriteIntent::Paths(files));
    }
    notify(&listener_snapshot, &changes, WritePhase::Before);
    IntentGuard {
        listeners: listener_snapshot,
        changes,
    }
}

/// Keeps concurrent reconciliation from resolving pending intent or marking
/// the watcher healthy while a filesystem mutation is still running.
#[must_use]
pub struct IntentGuard {
    listeners: Vec<Weak<dyn WriteIntentListener>>,
    changes: Vec<WriteIntent>,
}

impl Drop for IntentGuard {
    fn drop(&mut self) {
        notify(&self.listeners, &self.changes, WritePhase::After);
    }
}
