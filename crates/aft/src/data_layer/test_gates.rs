//! Root-scoped rendezvous for publication/fill interleaving tests.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum Point {
    BuildPublication,
    DeltaPublication,
    FillSurvey,
}

#[derive(Default)]
struct Gate {
    state: Mutex<(bool, bool)>,
    changed: Condvar,
}
static GATES: LazyLock<Mutex<HashMap<(PathBuf, Point), Arc<Gate>>>> = LazyLock::new(Mutex::default);

pub(crate) struct Hold {
    root: PathBuf,
    point: Point,
    gate: Arc<Gate>,
}

pub(crate) fn hold(root: &Path, point: Point) -> Hold {
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
    pub(crate) fn wait_entered(&self) {
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
