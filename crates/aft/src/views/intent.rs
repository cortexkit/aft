//! Synchronous write intent for opt-in checkout views.
//!
//! An intent is deliberately not cleared when a command fails: formatting,
//! partial writes and rollback can all change bytes before the error returns.
//! Only a strict reconciliation under the `Mutex<LiveDelta>` may resolve it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use super::snapshot::LiveDelta;
use super::RelPath;

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

#[derive(Clone)]
struct Registration {
    root: PathBuf,
    delta: Weak<Mutex<LiveDelta>>,
    active: Arc<std::sync::atomic::AtomicUsize>,
}

fn registrations() -> &'static Mutex<Vec<Registration>> {
    static ROOTS: OnceLock<Mutex<Vec<Registration>>> = OnceLock::new();
    ROOTS.get_or_init(|| Mutex::new(Vec::new()))
}

// Resolve the existing prefix so a write to a not-yet-created file routes
// through the same checkout identity as its symlinked parent directory.
fn path_identity(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => path_identity(parent).join(name),
        _ => path.to_path_buf(),
    }
}

/// Match event paths against the checkout identity, including Windows verbatim
/// prefixes and symlinked roots. Resolve the existing prefix rather than only
/// the whole path so deletions and not-yet-created descendants still route.
pub(super) fn relative_to_root(path: &Path, root: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    path_identity(path)
        .strip_prefix(path_identity(root))
        .ok()
        .map(Path::to_path_buf)
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

/// Register only roots routed to the new view plane. Legacy command behavior
/// and indexing remain unchanged when no view is registered.
pub fn register(root: &Path, delta: &Arc<Mutex<LiveDelta>>) {
    let mut roots = registrations().lock().unwrap_or_else(|e| e.into_inner());
    roots.retain(|entry| entry.delta.strong_count() != 0);
    roots.push(Registration {
        root: path_identity(root),
        delta: Arc::downgrade(delta),
        active: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    });
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
    let has_roots = {
        let mut roots = registrations().lock().unwrap_or_else(|e| e.into_inner());
        roots.retain(|entry| entry.delta.strong_count() != 0);
        !roots.is_empty()
    };
    if listener_snapshot.is_empty() && !has_roots {
        return IntentGuard {
            entries: Vec::new(),
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

    let mut roots = registrations().lock().unwrap_or_else(|e| e.into_inner());
    roots.retain(|entry| entry.delta.strong_count() != 0);
    let entries = roots.clone();
    drop(roots);
    let mut guard = IntentGuard {
        entries: Vec::new(),
        listeners: listener_snapshot,
        changes,
    };
    let normalized = paths
        .iter()
        .map(|(path, is_dir)| (path_identity(path), *is_dir))
        .collect::<Vec<_>>();
    for entry in entries {
        let Some(delta) = entry.delta.upgrade() else {
            continue;
        };
        let relevant = normalized
            .iter()
            .filter(|(path, _)| path.starts_with(&entry.root))
            .map(|(path, _)| path.to_path_buf())
            .collect::<Vec<_>>();
        if relevant.is_empty() {
            continue;
        }
        entry
            .active
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        guard.entries.push((entry.clone(), relevant));
        let mut delta = delta.lock().unwrap_or_else(|e| e.into_inner());
        for (path, is_dir) in &normalized {
            let Ok(relative) = path.strip_prefix(&entry.root) else {
                continue;
            };
            if relative.as_os_str().is_empty() || *is_dir {
                delta.bump_epoch();
                delta.set_watcher(super::snapshot::WatcherState::Reconciling);
            } else if let Ok(relative) = RelPath::from_os_path(relative) {
                delta.record_intent(relative);
            }
        }
    }
    guard
}

/// Keeps concurrent reconciliation from resolving pending intent or marking
/// the watcher healthy while a filesystem mutation is still running.
#[must_use]
pub struct IntentGuard {
    entries: Vec<(Registration, Vec<PathBuf>)>,
    listeners: Vec<Weak<dyn WriteIntentListener>>,
    changes: Vec<WriteIntent>,
}

pub fn active(root: &Path) -> bool {
    let root = path_identity(root);
    registrations()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .any(|entry| {
            entry.root == root && entry.active.load(std::sync::atomic::Ordering::SeqCst) != 0
        })
}

impl Drop for IntentGuard {
    fn drop(&mut self) {
        notify(&self.listeners, &self.changes, WritePhase::After);
        for (entry, paths) in &self.entries {
            if let Some(delta) = entry.delta.upgrade() {
                let mut delta = delta.lock().unwrap_or_else(|e| e.into_inner());
                for path in paths {
                    if let Ok(relative) = path.strip_prefix(&entry.root) {
                        if let Ok(relative) = RelPath::from_os_path(relative) {
                            delta.record_intent(relative);
                        }
                    }
                }
                // A command may have removed or created an entire directory.
                // Mark current membership for a later rewalk rather than
                // trusting the directory entries recorded before the command.
                delta.bump_epoch();
                delta.set_watcher(super::snapshot::WatcherState::Reconciling);
            }
            entry
                .active
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn symlink_roots_route_intents_for_not_yet_created_paths() {
        let directory = tempfile::tempdir().unwrap();
        let real = directory.path().join("real");
        let alias = directory.path().join("alias");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let manifest =
            crate::views::manifest_v2::ManifestV2::new(crate::views::manifest_v2::ManifestHeader {
                producers: crate::views::manifest_v2::Producers {
                    trigram: "test".into(),
                    semantic: None,
                    callgraph: "test".into(),
                },
                head_tree: None,
                ignore_fingerprint: None,
                segment: None,
            });
        let delta = Arc::new(Mutex::new(LiveDelta::new(Arc::new(
            crate::views::snapshot::OpenGeneration::new("base", manifest, None),
        ))));
        register(&real, &delta);
        let path = alias.join("new.rs");
        let guard = record_paths([path.as_path()]);
        assert!(active(&real));
        assert!(active(&alias));
        assert_eq!(
            delta
                .lock()
                .unwrap()
                .snapshot()
                .pending_intent()
                .next()
                .unwrap()
                .as_bytes(),
            b"new.rs"
        );
        drop(guard);
        assert!(!active(&real));
        assert!(!active(&alias));
    }

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
