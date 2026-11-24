//! Root-scoped rendezvous for publication/fill interleaving tests.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Point {
    BuildPublication,
    DeltaPublication,
    FillSurvey,
    #[cfg(feature = "wasm-grammars")]
    GrammarRetryRead,
    #[cfg(feature = "wasm-grammars")]
    GrammarSubscription,
}

#[cfg(feature = "wasm-grammars")]
static RETRY_FAILURES: LazyLock<Mutex<HashMap<PathBuf, Arc<std::sync::atomic::AtomicBool>>>> =
    LazyLock::new(Mutex::default);

#[cfg(feature = "wasm-grammars")]
pub fn fail_next_grammar_retry(root: &Path) -> Arc<std::sync::atomic::AtomicBool> {
    let failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    RETRY_FAILURES
        .lock()
        .unwrap()
        .insert(root.to_path_buf(), failed.clone());
    failed
}

#[cfg(feature = "wasm-grammars")]
pub(crate) fn fail_grammar_retry(root: &Path) -> bool {
    RETRY_FAILURES
        .lock()
        .unwrap()
        .remove(root)
        .is_some_and(|failed| {
            failed.store(true, std::sync::atomic::Ordering::Release);
            true
        })
}

#[derive(Default)]
struct Gate {
    state: Mutex<(bool, bool)>,
    changed: Condvar,
}
static GATES: LazyLock<Mutex<HashMap<(PathBuf, Point), Arc<Gate>>>> = LazyLock::new(Mutex::default);

pub struct Hold {
    root: PathBuf,
    point: Point,
    gate: Arc<Gate>,
}

pub fn hold(root: &Path, point: Point) -> Hold {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let gate = Arc::new(Gate::default());
    assert!(GATES
        .lock()
        .unwrap()
        .insert((root.clone(), point), Arc::clone(&gate))
        .is_none());
    Hold { root, point, gate }
}

impl Hold {
    pub fn wait_entered(&self) {
        let until = Instant::now() + Duration::from_secs(30);
        let mut state = self.gate.state.lock().unwrap();
        while !state.0 {
            let remaining = until.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "{:?} was not reached", self.point);
            state = self.gate.changed.wait_timeout(state, remaining).unwrap().0;
        }
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        GATES
            .lock()
            .unwrap()
            .remove(&(self.root.clone(), self.point));
        self.gate
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .1 = true;
        self.gate.changed.notify_all();
    }
}

pub(crate) fn arrive(root: &Path, point: Point) {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let gate = GATES.lock().unwrap().get(&(root, point)).cloned();
    if let Some(gate) = gate {
        let mut state = gate.state.lock().unwrap();
        state.0 = true;
        gate.changed.notify_all();
        while !state.1 {
            state = gate.changed.wait(state).unwrap();
        }
    }
}
