//! Index-facing views of a parent folder's repositories. Runtime owns the
//! sessions and search engines; queries borrow only their published state.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};

/// The child repositories found below a parent folder.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Discovery {
    /// Canonical parent folder.
    pub root: PathBuf,
    /// Served children, canonical, in byte order of their paths.
    pub children: Vec<PathBuf>,
    /// Children past the cap, in the same order. They are not served.
    pub skipped: Vec<PathBuf>,
    /// Top-level entries of the parent folder that hold files outside every child.
    pub outside: Vec<PathBuf>,
    /// The walk stopped at its directory-entry budget.
    pub entry_limit_reached: bool,
}

/// The index planes requested before configure disables the parent's own indexes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RequestedPlanes {
    pub trigram: bool,
    pub semantic: bool,
    pub callgraph: bool,
}

/// One child plane as seen by queries.
pub(crate) enum Plane<T> {
    Loading,
    Ready(Arc<T>),
    Gap(String),
}

impl<T> Clone for Plane<T> {
    fn clone(&self) -> Self {
        match self {
            Self::Loading => Self::Loading,
            Self::Ready(value) => Self::Ready(Arc::clone(value)),
            Self::Gap(reason) => Self::Gap(reason.clone()),
        }
    }
}

impl<T> Plane<T> {
    pub(crate) fn ready(&self) -> Option<&Arc<T>> {
        match self {
            Self::Ready(value) => Some(value),
            _ => None,
        }
    }

    pub(crate) fn gap_reason(&self, plane: &str) -> Option<String> {
        match self {
            Self::Loading => Some(format!("{plane} index of this repository is still loading")),
            Self::Ready(_) => None,
            Self::Gap(reason) => Some(format!("{plane}: {reason}")),
        }
    }
}

/// Project-wide aggregates that a parent can read without owning inspect's
/// database or exposing its job keys to the index.
pub trait InspectCacheView: Send + Sync {
    fn project_aggregate(
        &self,
        category: crate::inspect_category::InspectCategory,
        config: &crate::config::Config,
    ) -> Result<Option<serde_json::Value>, String>;
}

/// Published state of a child, independent of its runtime search engine.
pub struct Child {
    pub root: PathBuf,
    pub relative: PathBuf,
    pub(crate) inspect: RwLock<Option<Arc<dyn InspectCacheView>>>,
}

impl Child {
    pub(crate) fn new(parent: &Path, root: PathBuf) -> Self {
        let relative = root
            .strip_prefix(parent)
            .map(Path::to_path_buf)
            .unwrap_or_else(|_| root.clone());
        Self {
            root,
            relative,
            inspect: RwLock::new(None),
        }
    }

    pub fn display(&self) -> String {
        self.relative.to_string_lossy().replace('\\', "/")
    }
}

/// The published parent state index consumers may observe. The runtime keeps
/// ownership of holders, workers and child search contexts behind this view.
pub trait ParentView: Send + Sync {
    fn root(&self) -> &Path;
    fn planes(&self) -> RequestedPlanes;
    fn children(&self) -> Vec<Arc<Child>>;
    fn discovery(&self) -> Option<Arc<Discovery>>;
    fn stopped(&self) -> bool;
    fn note_child_failure(&self, child: &Child, plane: &'static str, error: &str) -> String;
}

fn views() -> &'static Mutex<HashMap<PathBuf, Weak<dyn ParentView>>> {
    static VIEWS: OnceLock<Mutex<HashMap<PathBuf, Weak<dyn ParentView>>>> = OnceLock::new();
    VIEWS.get_or_init(Default::default)
}

pub(crate) fn publish(session: Arc<dyn ParentView>) {
    views()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(session.root().to_path_buf(), Arc::downgrade(&session));
}

pub(crate) fn withdraw(session: &dyn ParentView) {
    let mut views = views()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if views
        .get(session.root())
        .is_some_and(|view| std::ptr::addr_eq(view.as_ptr(), session as *const dyn ParentView))
    {
        views.remove(session.root());
    }
}

/// A weak publication never keeps a runtime session or its readers alive.
pub fn session_for_root(root: &Path) -> Option<Arc<dyn ParentView>> {
    let exact = {
        let views = views()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if views.is_empty() {
            return None;
        }
        views.get(root).cloned()
    };
    let view = match exact {
        Some(view) => view,
        // Canonicalizing touches the filesystem, which can stall on a slow
        // mount; it runs unlocked so other lookups, publications and
        // withdrawals never wait for it.
        None => {
            #[cfg(test)]
            tests::before_canonicalize(root);
            let canonical = std::fs::canonicalize(root).ok()?;
            views()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&canonical)
                .cloned()?
        }
    };
    let session = view.upgrade()?;
    (!session.stopped()).then_some(session)
}

pub(crate) fn display_relative(root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    let text = relative.to_string_lossy().replace('\\', "/");
    if text.is_empty() {
        ".".into()
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    type Gate = (PathBuf, mpsc::Sender<()>, mpsc::Receiver<()>);

    /// A lookup for the gated path signals the test on entering the
    /// canonicalizing fallback, then waits until the test releases it.
    fn gate() -> &'static Mutex<Option<Gate>> {
        static GATE: OnceLock<Mutex<Option<Gate>>> = OnceLock::new();
        GATE.get_or_init(Mutex::default)
    }

    pub(super) fn before_canonicalize(root: &Path) {
        let mut gate = gate()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if gate.as_ref().is_some_and(|(gated, _, _)| gated == root) {
            let (_, entered, release) = gate.take().expect("checked above");
            drop(gate);
            entered.send(()).unwrap();
            release.recv().unwrap();
        }
    }

    struct FakeParent(PathBuf);

    impl ParentView for FakeParent {
        fn root(&self) -> &Path {
            &self.0
        }
        fn planes(&self) -> RequestedPlanes {
            RequestedPlanes {
                trigram: false,
                semantic: false,
                callgraph: false,
            }
        }
        fn children(&self) -> Vec<Arc<Child>> {
            Vec::new()
        }
        fn discovery(&self) -> Option<Arc<Discovery>> {
            None
        }
        fn stopped(&self) -> bool {
            false
        }
        fn note_child_failure(&self, _child: &Child, _plane: &'static str, error: &str) -> String {
            error.to_string()
        }
    }

    /// A registry miss canonicalizes its path, which can stall on a slow
    /// mount. Meanwhile an exact hit and a withdrawal for another parent
    /// must still complete.
    #[test]
    fn a_stalled_alias_lookup_does_not_block_other_parents() {
        let folder = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(folder.path()).unwrap();
        let first: Arc<dyn ParentView> = Arc::new(FakeParent(canonical.join("first")));
        let second: Arc<dyn ParentView> = Arc::new(FakeParent(canonical.join("second")));
        publish(Arc::clone(&first));
        publish(Arc::clone(&second));
        let alias = canonical.join("not-registered");
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        *gate()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some((alias.clone(), entered_tx, release_rx));
        let stalled = std::thread::spawn(move || session_for_root(&alias).is_none());
        entered_rx.recv().unwrap();

        // The stalled lookup holds its gate until this thread reports back.
        let (done_tx, done_rx) = mpsc::channel();
        let first_root = first.root().to_path_buf();
        let other = std::thread::spawn(move || {
            let hit = session_for_root(&first_root).is_some();
            withdraw(second.as_ref());
            done_tx.send(hit).unwrap();
        });
        let finished = done_rx.recv_timeout(std::time::Duration::from_secs(30));
        release_tx.send(()).unwrap();
        assert_eq!(
            finished,
            Ok(true),
            "an exact hit and a withdrawal waited for another lookup's canonicalization"
        );
        other.join().unwrap();
        assert!(stalled.join().unwrap());
        withdraw(first.as_ref());
    }
}
