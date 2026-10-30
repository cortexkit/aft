//! Subscription generation and observed gaps for steady-state refresh. This
//! does not prove that all writes before a cookie reached the driver, so it
//! cannot justify a bind based on Git/stat observations.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use super::InitialCoverage;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Continuity {
    pub generation: u64,
    pub gap: u64,
}
struct State {
    generation: u64,
    gap: AtomicU64,
    coverage: InitialCoverage,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SubscriptionStatus {
    pub continuity: Continuity,
    pub coverage: InitialCoverage,
}
pub(crate) struct Subscription {
    _state: Arc<State>,
    root: PathBuf,
}
impl Drop for Subscription {
    fn drop(&mut self) {
        let mut roots = roots()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if roots
            .get(&self.root)
            .is_some_and(|state| state.ptr_eq(&Arc::downgrade(&self._state)))
        {
            roots.remove(&self.root);
        }
    }
}
static NEXT: AtomicU64 = AtomicU64::new(1);
fn roots() -> &'static Mutex<BTreeMap<PathBuf, Weak<State>>> {
    static ROOTS: OnceLock<Mutex<BTreeMap<PathBuf, Weak<State>>>> = OnceLock::new();
    ROOTS.get_or_init(Default::default)
}

pub(crate) fn start(root: &Path, coverage: InitialCoverage) -> Subscription {
    let state = Arc::new(State {
        generation: NEXT.fetch_add(1, Ordering::Relaxed),
        gap: AtomicU64::new(0),
        coverage,
    });
    roots()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(root.to_path_buf(), Arc::downgrade(&state));
    Subscription {
        _state: state,
        root: root.to_path_buf(),
    }
}

fn retained(root: &Path) -> Option<Arc<State>> {
    let mut roots = roots()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match roots.get(root).and_then(Weak::upgrade) {
        Some(state) => Some(state),
        None => {
            roots.remove(root);
            None
        }
    }
}

pub(crate) fn current(root: &Path) -> Option<Continuity> {
    retained(root)
        .filter(|state| state.coverage.is_complete())
        .map(|state| Continuity {
            generation: state.generation,
            gap: state.gap.load(Ordering::Acquire),
        })
}

pub(crate) fn status(root: &Path) -> Option<SubscriptionStatus> {
    retained(root).map(|state| SubscriptionStatus {
        continuity: Continuity {
            generation: state.generation,
            gap: state.gap.load(Ordering::Acquire),
        },
        coverage: state.coverage.clone(),
    })
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
    fn incomplete_and_unproved_handles_never_grant_a_continuity_token() {
        let root = tempfile::tempdir().unwrap();
        let coverage =
            super::InitialCoverage::Incomplete(vec![crate::watcher_backend::CoverageGap {
                path: root.path().join("unreadable"),
                kind: crate::watcher_backend::CoverageGapKind::ListingFailed(
                    std::io::ErrorKind::PermissionDenied,
                ),
            }]);
        let partial = super::start(root.path(), coverage.clone());
        assert_eq!(super::status(root.path()).unwrap().coverage, coverage);
        assert!(super::current(root.path()).is_none());
        drop(partial);
        let _unproved = super::start(
            root.path(),
            crate::watcher_backend::initial_coverage_of(&()),
        );
        assert_eq!(
            super::status(root.path()).unwrap().coverage,
            super::InitialCoverage::Unproved
        );
        assert!(super::current(root.path()).is_none());
    }

    #[test]
    fn subscription_restart_stop_and_gap_change_the_continuity_token() {
        let root = tempfile::tempdir().unwrap();
        assert!(super::current(root.path()).is_none());
        let first = super::start(root.path(), super::InitialCoverage::Complete);
        let before = super::current(root.path()).unwrap();
        super::gap(root.path());
        assert_ne!(super::current(root.path()).unwrap(), before);
        let second = super::start(root.path(), super::InitialCoverage::Complete);
        let after = super::current(root.path()).unwrap();
        assert_ne!(before.generation, after.generation);
        drop(first);
        assert_eq!(super::current(root.path()), Some(after));
        drop(second);
        assert!(super::current(root.path()).is_none());
    }
}
