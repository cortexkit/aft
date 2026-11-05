//! Per-checkout write intent for opt-in checkout views.
//!
//! [`crate::write_intent::record_paths`] brackets every AFT mutation; this
//! module records the paths it names into each registered checkout's
//! `Mutex<LiveDelta>`. An intent is deliberately not cleared when a command
//! fails: only a strict reconciliation (a full walk that reads every path's
//! bytes again) under that mutex may resolve it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use super::snapshot::LiveDelta;
use super::RelPath;

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

/// Whether any checkout view is still registered. With no listener and no
/// registered view, a mutation skips classifying its paths.
pub(crate) fn has_roots() -> bool {
    let mut roots = registrations().lock().unwrap_or_else(|e| e.into_inner());
    roots.retain(|entry| entry.delta.strong_count() != 0);
    !roots.is_empty()
}

/// The registered checkouts one mutation touches, with the paths it named
/// under each. Dropping it after the mutation records the intents again,
/// marks each checkout for a rewalk and releases its active count.
#[derive(Default)]
pub(crate) struct CheckoutIntents {
    entries: Vec<(Registration, Vec<PathBuf>)>,
}

/// Record `paths` (each with whether it is a directory) into every registered
/// checkout that contains one of them, and mark those checkouts active until
/// the returned value drops.
pub(crate) fn begin(paths: &[(&Path, bool)]) -> CheckoutIntents {
    let mut roots = registrations().lock().unwrap_or_else(|e| e.into_inner());
    roots.retain(|entry| entry.delta.strong_count() != 0);
    let entries = roots.clone();
    drop(roots);
    let mut checkouts = CheckoutIntents::default();
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
        checkouts.entries.push((entry.clone(), relevant));
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
    checkouts
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

impl Drop for CheckoutIntents {
    fn drop(&mut self) {
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

// This test needs a symlinked checkout root, so it runs only on Unix.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

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
        let guard = crate::write_intent::record_paths([path.as_path()]);
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
}
