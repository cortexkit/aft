use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use crate::data_layer::runtime::DataCheckout;
use crate::inspect::job::normalize_path;

/// Every checkout this process has bound, for the TODO count: it runs on
/// the inspect pool with only its job's root, and reads the root's
/// checkout, or for a folder of repositories its children's. Held weakly:
/// whoever bound a checkout owns it.
pub(crate) static BOUND: Mutex<Vec<Weak<DataCheckout>>> = Mutex::new(Vec::new());

/// Roots the data layer never binds (see `AppContext::data_layer_withheld`)
/// and folders of repositories, as their last configure left them,
/// normalized.
static ROOTS: Mutex<Vec<(PathBuf, RootKind)>> = Mutex::new(Vec::new());

/// How the data layer serves a configured root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RootKind {
    /// The root gets a checkout of its own.
    Own,
    /// A folder of repositories: each repository in it gets a checkout.
    Repositories,
    /// The data layer never binds the root.
    Withheld,
}

/// Lets the TODO count read `checkout`.
pub(crate) fn note_bound(checkout: &Arc<DataCheckout>) {
    let mut bound = BOUND.lock().unwrap_or_else(PoisonError::into_inner);
    bound.retain(|held| held.strong_count() > 0);
    bound.push(Arc::downgrade(checkout));
}

/// Records how the data layer serves `root`, so the TODO count of a root
/// that never gets a checkout says why instead of waiting for one, and a
/// folder of repositories counts its repositories'.
pub(crate) fn note_root(root: &Path, kind: RootKind) {
    let root = normalize_path(root);
    let mut roots = ROOTS.lock().unwrap_or_else(PoisonError::into_inner);
    roots.retain(|(held, _)| *held != root);
    roots.push((root, kind));
}

pub(crate) fn root_kind(root: &Path) -> RootKind {
    ROOTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .find(|(held, _)| held == root)
        .map_or(RootKind::Own, |(_, kind)| *kind)
}
