//! Parent folders on the data layer.
//!
//! A session bound to a folder of repositories that is not a repository
//! itself builds nothing of its own (see [`crate::views::parent`]). When
//! the process runs the data layer, each child repository that session
//! found gets a checkout of its own, bound the first time a query reaches
//! it, and grep, glob and the call-graph commands answer from the
//! children's views (`views::parent::query`), as does `aft_search`
//! (`views::parent::engine`), whose meaning lane reads each child's meaning
//! plane without ever filling it ([`search_children`]). The parent folder's
//! own root binds no checkout.
//!
//! The children are bound with the parent root's executor, so each child's
//! builds and refreshes run as the parent root's maintenance, as jobs of
//! their own: while the parent root is unbound the executor runs none of
//! them, and an unbind cancels them at their checkpoints. The parent's
//! watcher passes each changed path to the child that holds it.
//!
//! A query never waits for a child's first build: a child with nothing
//! published yet is named as still loading. A call-graph query waits for
//! the edits recorded since a child published its call graph, as a query
//! of a single checkout does.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::config::SemanticBackendConfig;
use crate::context::AppContext;
use crate::data_layer::checkout::{CheckoutView, PlaneStatus, Planes};
use crate::data_layer::runtime::{DataCheckout, Fresh};
use crate::executor::RootMaintenanceExecutor;
use crate::semantic_index::SemanticIndexFingerprint;
use crate::views::parent::Plane;

/// One child's checkout, bound on first use. Each child has a lock of its
/// own, so a slow bind holds up only the queries of that child.
type Slot = Arc<parking_lot::Mutex<Option<Arc<DataCheckout>>>>;

/// The data-layer checkouts of the child repositories of the parent folder
/// a context is bound to.
#[derive(Default)]
pub(crate) struct ChildCheckouts {
    inner: parking_lot::Mutex<Children>,
}

#[derive(Default)]
struct Children {
    /// The parent folder the checkouts belong to.
    parent: Option<PathBuf>,
    /// By child root.
    slots: HashMap<PathBuf, Slot>,
}

impl ChildCheckouts {
    /// Drops every child checkout unless they belong to `parent`: the
    /// parent folder the context is bound to now, or `None` when it is
    /// bound to anything else.
    pub(crate) fn keep_only(&self, parent: Option<&Path>) {
        let mut inner = self.inner.lock();
        if inner.parent.as_deref() != parent {
            inner.slots.clear();
            inner.parent = parent.map(Path::to_path_buf);
        }
    }

    /// The slot of child `child` of `parent`; asking for another parent's
    /// child drops the children of the one before.
    fn slot(&self, parent: &Path, child: &Path) -> Slot {
        let mut inner = self.inner.lock();
        if inner.parent.as_deref() != Some(parent) {
            inner.slots.clear();
            inner.parent = Some(parent.to_path_buf());
        }
        Arc::clone(inner.slots.entry(child.to_path_buf()).or_default())
    }

    /// The child checkouts bound so far. Waits for a bind in progress, as
    /// the root's own checkout does.
    fn bound(&self) -> Vec<Arc<DataCheckout>> {
        let slots = self
            .inner
            .lock()
            .slots
            .values()
            .cloned()
            .collect::<Vec<_>>();
        slots
            .iter()
            .filter_map(|slot| slot.lock().clone())
            .collect()
    }

    /// Records the watcher's changed `paths` (absolute) in the child that
    /// holds each one. Child repositories never nest: discovery does not
    /// look inside one.
    pub(crate) fn note_paths(&self, paths: &[PathBuf]) {
        for checkout in self.bound() {
            let held = paths
                .iter()
                .filter(|path| path.starts_with(checkout.root()))
                .cloned()
                .collect::<Vec<_>>();
            if !held.is_empty() {
                checkout.note_paths(&held);
            }
        }
    }

    /// Has every bound child compare its files with its state again, after
    /// the parent's watcher lost events.
    pub(crate) fn rescan(&self) {
        for checkout in self.bound() {
            checkout.rescan();
        }
    }

    /// Runs the children's background work on `executor` from now on: the
    /// parent root's executor, installed after some children were bound.
    pub(crate) fn set_executor(&self, executor: &RootMaintenanceExecutor) {
        for checkout in self.bound() {
            checkout.set_executor(executor.clone());
        }
    }
}

/// Binds the checkout of `child` into `slot` unless it is bound already.
///
/// # Errors
///
/// Why the child's checkout could not be bound.
fn bind(
    slot: &Slot,
    child: &Path,
    storage: &Path,
    executor: Option<RootMaintenanceExecutor>,
    planes: Planes,
) -> Result<Arc<DataCheckout>, String> {
    let mut slot = slot.lock();
    if let Some(checkout) = slot.as_ref() {
        return Ok(Arc::clone(checkout));
    }
    // A failed bind is not retried before its backoff ends: the runtime
    // answers with the same error until then.
    let checkout = DataCheckout::open_planes(child, storage, executor, planes)
        .map_err(|error| error.to_string())?;
    *slot = Some(Arc::clone(&checkout));
    Ok(checkout)
}

/// The checkouts of `children` (child roots of the parent folder `parent`
/// that `ctx` is bound to), binding the ones not bound yet, several at
/// once: a bind of a published checkout compares its files with the
/// publication first.
fn checkouts(
    ctx: &AppContext,
    parent: &Path,
    children: &[&Path],
) -> Vec<Result<Arc<DataCheckout>, String>> {
    use rayon::prelude::*;
    let storage = ctx.storage_dir();
    let executor = ctx.data_executor();
    // Not the parent context's own index switches: configure turns those
    // off for a folder of repositories, which builds nothing itself. A plane
    // the user switched off never reaches here; the parent route answers it
    // as switched off before asking any child.
    let planes = Planes::ALL;
    let slots = children
        .iter()
        .map(|child| ctx.data_children().slot(parent, child))
        .collect::<Vec<_>>();
    slots
        .par_iter()
        .zip(children.par_iter())
        .map(|(slot, child)| bind(slot, child, &storage, executor.clone(), planes))
        .collect()
}

/// Each child's text plane for grep and glob, in the order of `children`:
/// its view once the text plane covers every path, still loading before
/// that, or the reason it cannot answer.
pub(crate) fn text_planes(
    ctx: &AppContext,
    parent: &Path,
    children: &[&Path],
) -> Vec<Plane<CheckoutView>> {
    checkouts(ctx, parent, children)
        .into_iter()
        .map(|checkout| match checkout {
            Ok(checkout) => text_plane(&checkout),
            Err(error) => Plane::Gap(error),
        })
        .collect()
}

fn text_plane(checkout: &DataCheckout) -> Plane<CheckoutView> {
    match checkout
        .view()
        .filter(|view| view.status().text == PlaneStatus::Ready)
    {
        Some(view) => Plane::Ready(Arc::new(view)),
        None => not_ready(checkout),
    }
}

/// A child whose view cannot answer yet: still loading, or the reason its
/// last build failed while no build runs.
fn not_ready(checkout: &DataCheckout) -> Plane<CheckoutView> {
    // Starts a build the executor refused while the parent root was
    // unbound; one already queued, running or waiting to be tried again
    // is left as it is.
    checkout.request_refresh();
    let building = checkout.checkout().status().building;
    match checkout.last_failure() {
        Some(failure) if !building => Plane::Gap(failure.error),
        _ => Plane::Loading,
    }
}

/// The call-graph plane of child `child` of the parent folder `parent`
/// that `ctx` is bound to: a view whose call graph covers every recorded
/// change, still loading, or the reason it cannot answer.
pub(crate) fn callgraph_plane(
    ctx: &AppContext,
    parent: &Path,
    child: &Path,
) -> Plane<CheckoutView> {
    let checkout = match checkouts(ctx, parent, &[child]).pop() {
        Some(Ok(checkout)) => checkout,
        Some(Err(error)) => return Plane::Gap(error),
        None => return Plane::Loading,
    };
    // Waits for the edits recorded since the child's call graph was
    // published, as a single checkout's query does, but never for a
    // child's first build.
    let published = checkout
        .view()
        .is_some_and(|view| view.status().callgraph_published);
    let wait = if published {
        crate::data_layer::tools::callgraph::READ_WAIT
    } else {
        Duration::ZERO
    };
    match checkout.fresh_view(wait) {
        Fresh::Ready(view) => Plane::Ready(Arc::new(view)),
        Fresh::Building => Plane::Loading,
        Fresh::Failed(reason) => Plane::Gap(reason),
    }
}

/// One child repository as a parent folder's `aft_search` reads it.
pub(crate) struct ChildSearch {
    /// Its checkout, which the child's search engine answers from; `None`
    /// when it could not be bound, and `text` says why.
    pub(crate) checkout: Option<Arc<DataCheckout>>,
    /// Its text plane, as grep reads it.
    pub(crate) text: Plane<CheckoutView>,
    /// Why its meaning search has nothing to answer from, when it has not.
    pub(crate) meaning_gap: Option<String>,
}

/// The words a parent folder's `aft_search` names a child with, when the
/// child's repository store holds no vectors for any of its files.
pub(crate) const NO_VECTORS: &str = "no semantic view has been published for this repository yet";

/// Each of `children` (child roots of the parent folder `parent` that `ctx`
/// is bound to) for the parent's `aft_search`, in the order of `children`:
/// its checkout, bound if it is not yet, and its text plane. With
/// `meaning` (the session's semantic configuration and its model's
/// identity), the child's meaning plane is made read only: it reads the
/// vectors the repository's store holds, whichever checkout embedded
/// them, and never fills, since semantic search is not admitted for a
/// repository no session has opened.
pub(crate) fn search_children(
    ctx: &AppContext,
    parent: &Path,
    children: &[&Path],
    meaning: Option<(&SemanticBackendConfig, &SemanticIndexFingerprint)>,
) -> Vec<ChildSearch> {
    checkouts(ctx, parent, children)
        .into_iter()
        .map(|checkout| match checkout {
            Ok(checkout) => ChildSearch {
                text: text_plane(&checkout),
                meaning_gap: meaning
                    .and_then(|(config, fingerprint)| meaning_gap(&checkout, config, fingerprint)),
                checkout: Some(checkout),
            },
            Err(error) => ChildSearch {
                checkout: None,
                text: Plane::Gap(error),
                meaning_gap: None,
            },
        })
        .collect()
}

/// Makes `checkout`'s meaning plane read only for `config`'s model
/// (identified by `fingerprint`), and says why it has nothing to answer
/// from, when it has not: no vectors stored for any of its files, or the
/// store could not be read. A checkout with nothing published yet is the
/// text plane's gap.
fn meaning_gap(
    checkout: &DataCheckout,
    config: &SemanticBackendConfig,
    fingerprint: &SemanticIndexFingerprint,
) -> Option<String> {
    use crate::data_layer::meaning::MeaningState;
    if let Err(error) = checkout.read_meaning_only(config, fingerprint) {
        return Some(format!("semantic view unavailable: {error}"));
    }
    match checkout.meaning_status() {
        MeaningState::Serving {
            failure: Some(failure),
            ..
        } => Some(format!("semantic view unreadable: {}", failure.message())),
        MeaningState::Serving { snapshot, .. } => (snapshot.chunks() == 0
            && crate::data_layer::meaning::waiting(snapshot.coverage()))
        .then(|| NO_VECTORS.to_string()),
        MeaningState::Blocked(failure) => {
            Some(format!("semantic view unreadable: {}", failure.message()))
        }
        MeaningState::Off | MeaningState::Unpublished | MeaningState::Starting => None,
    }
}

#[cfg(test)]
impl ChildCheckouts {
    /// Puts `checkout` in place as the bound checkout of `child`, so a test
    /// can give one child a state of its choosing.
    pub(crate) fn insert_for_test(&self, parent: &Path, child: &Path, checkout: Arc<DataCheckout>) {
        *self.slot(parent, child).lock() = Some(checkout);
    }

    /// The child checkouts bound so far.
    pub(crate) fn bound_for_test(&self) -> Vec<Arc<DataCheckout>> {
        self.bound()
    }
}

#[cfg(test)]
mod tests;
