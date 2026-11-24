use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use crate::data_layer::runtime::DataCheckout;
use crate::path_identity::{canonicalize_normalized, normalize_path};

/// Every checkout this process has bound, for the TODO count: it runs on
/// the inspect pool with only its job's root, and reads the root's
/// checkout, or for a folder of repositories its children's. Held weakly:
/// whoever bound a checkout owns it.
static BOUND: Mutex<Vec<Weak<DataCheckout>>> = Mutex::new(Vec::new());

/// Normalized roots the data layer never binds and folders of repositories,
/// as their last configure left them.
static ROOTS: Mutex<Vec<(PathBuf, RootKind)>> = Mutex::new(Vec::new());

/// How the data layer serves a configured root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[doc(hidden)]
pub enum RootKind {
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
#[doc(hidden)]
pub fn note_root(root: &Path, kind: RootKind) {
    let root = normalize_path(root);
    let mut roots = ROOTS.lock().unwrap_or_else(PoisonError::into_inner);
    roots.retain(|(held, _)| *held != root);
    roots.push((root, kind));
}

#[doc(hidden)]
pub fn root_kind(root: &Path) -> RootKind {
    ROOTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .find(|(held, _)| held == root)
        .map_or(RootKind::Own, |(_, kind)| *kind)
}

/// Pin the live checkouts newest first without holding the registry lock afterwards.
#[doc(hidden)]
pub fn newest_bound_checkouts() -> Vec<Arc<DataCheckout>> {
    BOUND
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .rev()
        .filter_map(Weak::upgrade)
        .collect()
}

/// Return the checkout and resolved root pairs used by the TODO count.
#[doc(hidden)]
pub fn bound_checkout_roots() -> Vec<(PathBuf, Arc<DataCheckout>)> {
    BOUND
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .filter_map(Weak::upgrade)
        .map(|checkout| (canonicalize_normalized(checkout.root()), checkout))
        .collect()
}
