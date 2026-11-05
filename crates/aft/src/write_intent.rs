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

#[cfg(test)]
thread_local! {
    static CLASSIFICATION_STATS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn is_directory(path: &Path) -> bool {
    #[cfg(test)]
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
    let has_roots = crate::views::intent::has_roots();
    if listener_snapshot.is_empty() && !has_roots {
        return IntentGuard {
            listeners: Vec::new(),
            changes: Vec::new(),
            _checkouts: Default::default(),
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
    let checkouts = crate::views::intent::begin(&paths);
    IntentGuard {
        listeners: listener_snapshot,
        changes,
        _checkouts: checkouts,
    }
}

/// Keeps concurrent reconciliation from resolving pending intent or marking
/// the watcher healthy while a filesystem mutation is still running.
#[must_use]
pub struct IntentGuard {
    listeners: Vec<Weak<dyn WriteIntentListener>>,
    changes: Vec<WriteIntent>,
    /// Dropped after this guard's `drop` has sent the After notification, so
    /// the checkout views settle their intents after every listener has seen it.
    _checkouts: crate::views::intent::CheckoutIntents,
}

impl Drop for IntentGuard {
    fn drop(&mut self) {
        notify(&self.listeners, &self.changes, WritePhase::After);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Events = Arc<Mutex<Vec<(WriteIntent, WritePhase, Vec<u8>)>>>;
    struct Listener {
        root: PathBuf,
        events: Events,
    }
    impl WriteIntentListener for Listener {
        fn record_change(&self, change: &WriteIntent, phase: WritePhase) {
            let paths = match change {
                WriteIntent::Paths(paths) => paths.clone(),
                WriteIntent::Directory(path) => vec![path.clone()],
            };
            if paths.iter().any(|path| path.starts_with(&self.root)) {
                let bytes = paths
                    .first()
                    .and_then(|path| std::fs::read(path).ok())
                    .unwrap_or_default();
                self.events
                    .lock()
                    .unwrap()
                    .push((change.clone(), phase, bytes));
            }
        }
    }

    #[test]
    fn intent_paths_are_classified_once() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("new.rs");
        let listener: Arc<dyn WriteIntentListener> = Arc::new(Listener {
            root: root.path().to_path_buf(),
            events: Arc::new(Mutex::new(Vec::new())),
        });
        register_listener(&listener);
        CLASSIFICATION_STATS.with(|count| count.set(0));
        let guard = record_paths([path.as_path()]);
        assert_eq!(CLASSIFICATION_STATS.with(|count| count.get()), 1);
        drop(guard);
    }

    #[test]
    fn composite_listener_sees_before_after_directory_and_unregisters_on_drop() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("a.txt");
        std::fs::write(&path, b"old").unwrap();
        let events: Events = Arc::new(Mutex::new(Vec::new()));
        let listener: Arc<dyn WriteIntentListener> = Arc::new(Listener {
            root: directory.path().to_path_buf(),
            events: events.clone(),
        });
        register_listener(&listener);
        crate::edit::write_format_validate(
            &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
            &path,
            "new",
            &crate::config::Config::default(),
            &serde_json::json!({}),
        )
        .unwrap();
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                (
                    WriteIntent::Paths(vec![path.clone()]),
                    WritePhase::Before,
                    b"old".to_vec()
                ),
                (
                    WriteIntent::Paths(vec![path.clone()]),
                    WritePhase::After,
                    b"new".to_vec()
                ),
            ]
        );
        events.lock().unwrap().clear();
        let tree = directory.path().join("tree");
        std::fs::create_dir(&tree).unwrap();
        let intent = record_paths([tree.as_path()]);
        std::fs::remove_dir(&tree).unwrap();
        drop(intent);
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                (
                    WriteIntent::Directory(tree.clone()),
                    WritePhase::Before,
                    Vec::new()
                ),
                (WriteIntent::Directory(tree), WritePhase::After, Vec::new()),
            ]
        );
        events.lock().unwrap().clear();
        let guard = record_paths([path.as_path()]);
        assert_eq!(events.lock().unwrap().len(), 1);
        drop(listener);
        drop(guard);
        crate::edit::write_format_validate(
            &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
            &path,
            "again",
            &crate::config::Config::default(),
            &serde_json::json!({}),
        )
        .unwrap();
        assert_eq!(
            events.lock().unwrap().len(),
            1,
            "a weak registration must not keep the listener alive"
        );
    }
}
