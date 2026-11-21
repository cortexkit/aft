//! Process-lifetime gaps between task JSON and its aft.db mirror.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Default)]
struct State {
    outstanding: AtomicU64,
    failed: AtomicBool,
}

fn stores() -> &'static Mutex<HashMap<PathBuf, Arc<State>>> {
    static STORES: OnceLock<Mutex<HashMap<PathBuf, Arc<State>>>> = OnceLock::new();
    STORES.get_or_init(Default::default)
}

fn state(storage: &Path) -> Arc<State> {
    let key = storage.canonicalize().unwrap_or_else(|_| storage.into());
    stores()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(key)
        .or_default()
        .clone()
}

/// A queued or in-flight mirror write. Dropping a ticket decrements the
/// store's outstanding-write counter. If unresolved, it also sets the sticky
/// failure flag, including on early returns or panics. A newer queued snapshot
/// may explicitly supersede an older one.
pub(crate) struct Ticket {
    state: Arc<State>,
    resolved: bool,
}

impl Ticket {
    pub(crate) fn new(storage: &Path) -> Self {
        let state = state(storage);
        state.outstanding.fetch_add(1, Ordering::SeqCst);
        Self {
            state,
            resolved: false,
        }
    }

    pub(crate) fn resolved(mut self) {
        self.resolved = true;
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        if !self.resolved {
            self.state.failed.store(true, Ordering::SeqCst);
        }
        self.state.outstanding.fetch_sub(1, Ordering::SeqCst);
    }
}

pub(crate) fn authoritative(storage: &Path) -> bool {
    let state = state(storage);
    state.outstanding.load(Ordering::SeqCst) == 0 && !state.failed.load(Ordering::SeqCst)
}

pub(crate) fn snapshot() -> serde_json::Value {
    let stores = stores()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    serde_json::json!({
        "outstanding": stores.values().map(|state| state.outstanding.load(Ordering::SeqCst)).sum::<u64>(),
        "failed": stores.values().any(|state| state.failed.load(Ordering::SeqCst)),
    })
}

#[cfg(test)]
pub(super) fn outstanding(storage: &Path) -> u64 {
    state(storage).outstanding.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_background_tasks_store_ticket_unwind_releases_count_and_keeps_gap() {
        let storage = tempfile::tempdir().unwrap();
        let result = std::panic::catch_unwind(|| {
            let _ticket = Ticket::new(storage.path());
            assert_eq!(outstanding(storage.path()), 1);
            panic!("mirror writer stopped");
        });
        assert!(result.is_err());
        assert_eq!(outstanding(storage.path()), 0);
        assert!(!authoritative(storage.path()));
    }
}
