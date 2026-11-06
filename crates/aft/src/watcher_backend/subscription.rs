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

#[cfg(test)]
mod tests {

}
