//! Subscription generation and observed gaps for steady-state refresh. This
//! does not prove that all writes before a cookie reached the driver, so it
//! cannot justify a bind based on Git/stat observations.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Continuity {
    pub generation: u64,
    pub gap: u64,
}
struct State {
    generation: u64,
    gap: AtomicU64,
}
pub(crate) struct Subscription {
    _state: Arc<State>,
}
static NEXT: AtomicU64 = AtomicU64::new(1);
fn roots() -> &'static Mutex<BTreeMap<PathBuf, Weak<State>>> {
    static ROOTS: OnceLock<Mutex<BTreeMap<PathBuf, Weak<State>>>> = OnceLock::new();
    ROOTS.get_or_init(Default::default)
}

pub(crate) fn start(root: &Path) -> Subscription {
    let state = Arc::new(State {
        generation: NEXT.fetch_add(1, Ordering::Relaxed),
        gap: AtomicU64::new(0),
    });
    roots()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(root.to_path_buf(), Arc::downgrade(&state));
    Subscription { _state: state }
}

pub(crate) fn current(root: &Path) -> Option<Continuity> {
    let mut roots = roots()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match roots.get(root).and_then(Weak::upgrade) {
        Some(state) => Some(Continuity {
            generation: state.generation,
            gap: state.gap.load(Ordering::Acquire),
        }),
        None => {
            roots.remove(root);
            None
        }
    }
}

pub(crate) fn gap(root: &Path) {
    if let Some(state) = roots()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(root)
        .and_then(Weak::upgrade)
    {
        state.gap.fetch_add(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn subscription_restart_stop_and_gap_change_the_continuity_token() {
        let root = tempfile::tempdir().unwrap();
        assert!(super::current(root.path()).is_none());
        let first = super::start(root.path());
        let before = super::current(root.path()).unwrap();
        super::gap(root.path());
        assert_ne!(super::current(root.path()).unwrap(), before);
        let second = super::start(root.path());
        let after = super::current(root.path()).unwrap();
        assert_ne!(before.generation, after.generation);
        drop(first);
        assert_eq!(super::current(root.path()), Some(after));
        drop(second);
        assert!(super::current(root.path()).is_none());
    }
}
