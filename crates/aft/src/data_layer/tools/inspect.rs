//! aft_inspect's dead-code analysis, read from the data layer.
//!
//! Dead code analyses a snapshot of the call graph: every file, exported
//! symbol, outbound call and entry point (`CallgraphSnapshot`). It comes
//! from the root's checkout once its call graph is published with no
//! changes pending; until then dead code reports the call graph as
//! building, so a change not resolved yet is never answered from an older
//! publication.
//!
//! Unused exports and the Tier-1 categories do not read the call graph.
//! Cached results are keyed by the files they were computed from.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Instant;

use crate::data_layer::runtime::DataCheckout;
use crate::inspect::job::{canonicalize_normalized, normalize_path, CallgraphSnapshot};

/// How long a dead-code run waits for recorded edits to reach the call
/// graph, as the call-graph commands do.
const SNAPSHOT_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// What [`DeadCodeSources::snapshot`] found for a root.
pub(crate) enum DeadCodeSnapshot {
    Ready(Arc<CallgraphSnapshot>),
    /// No checkout is bound for the root yet, or its call graph is building
    /// or catching up with edits.
    Building,
    /// The call graph failed to build, refresh or read.
    Failed(String),
}

/// The data-layer checkouts dead code reads, by the project root inspect
/// analyses. Holds each checkout weakly: the request context owns it, and a root bound to another
/// checkout drops the old one.
#[derive(Default)]
pub(crate) struct DeadCodeSources {
    checkouts: Mutex<HashMap<PathBuf, Weak<DataCheckout>>>,
}

impl DeadCodeSources {
    /// Lets dead code at `project_root` read its snapshot from `checkout`.
    /// A checkout of another folder is not used: its call graph would
    /// describe different files.
    pub(crate) fn bind(&self, project_root: &Path, checkout: &Arc<DataCheckout>) {
        if canonicalize_normalized(checkout.root()) != normalize_path(project_root) {
            crate::slog_info!(
                "tier2 dead_code: data layer not used for {}: its checkout is of {}",
                project_root.display(),
                checkout.root().display()
            );
            return;
        }
        self.checkouts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(normalize_path(project_root), Arc::downgrade(checkout));
    }

    /// The checkout bound for `project_root`. Keys are normalized, so a root
    /// spelled with the Windows verbatim prefix (`\\?\C:\...`, what
    /// `std::fs::canonicalize` returns there) finds the same checkout as
    /// its plain spelling.
    fn checkout(&self, project_root: &Path) -> Option<Arc<DataCheckout>> {
        self.checkouts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&normalize_path(project_root))
            .and_then(Weak::upgrade)
    }

    /// The inventory of `project_root` from its bound checkout's current
    /// view, when the view can stand in for a walk of the folder (see
    /// [`InspectInventory::from_checkout`]); `None` sends the caller to the
    /// walk.
    pub(crate) fn inventory(
        &self,
        project_root: &Path,
    ) -> Option<crate::inspect::inventory::InspectInventory> {
        let checkout = self.checkout(project_root)?;
        let view = checkout.view()?;
        crate::inspect::inventory::InspectInventory::from_checkout(project_root, &view)
    }

    /// Whether `project_root` has no call graph to analyse yet: no checkout
    /// is bound for it, or its first call graph is not published. Checked
    /// without waiting, before a run opens the inspect cache and before a
    /// cached result is served, so neither a run nor a cached answer stands
    /// in for a graph that doesn't exist yet. A published graph with edits
    /// pending is not busy: [`Self::snapshot`] brings it up to date.
    pub(crate) fn callgraph_busy(&self, project_root: &Path) -> bool {
        self.checkout(project_root)
            .and_then(|checkout| checkout.view())
            .is_none_or(|view| !view.status().callgraph_published)
    }

    /// The dead-code snapshot of `project_root` from its checkout's view,
    /// once the view is published and its call graph has no changes
    /// pending.
    pub(crate) fn snapshot(&self, project_root: &Path) -> DeadCodeSnapshot {
        let Some(checkout) = self.checkout(project_root) else {
            return DeadCodeSnapshot::Building;
        };
        // The call graph is refreshed lazily, so ask for the edits recorded
        // since the last refresh to be resolved, as any exact reader does.
        let view = match checkout.fresh_view(SNAPSHOT_WAIT) {
            crate::data_layer::runtime::Fresh::Ready(view) => view,
            crate::data_layer::runtime::Fresh::Building => {
                let state = checkout.view().map_or_else(
                    || "nothing published yet".to_string(),
                    |view| format!("call graph {:?}", view.status().callgraph),
                );
                crate::slog_info!(
                    "tier2 dead_code: data layer still building for {}: {state}",
                    project_root.display()
                );
                return DeadCodeSnapshot::Building;
            }
            crate::data_layer::runtime::Fresh::Failed(reason) => {
                crate::slog_warn!(
                    "tier2 dead_code: data layer call graph failed for {}: {reason}",
                    project_root.display()
                );
                return DeadCodeSnapshot::Failed(reason);
            }
        };
        let started = Instant::now();
        // Each publication's dead-code graph is kept beside the family's
        // store, so later rollups over the same publication read it.
        let section_dir = checkout.checkout().family_dir().join("dead-code");
        match view.dead_code_snapshot_in(Some(section_dir)) {
            Ok(snapshot) => {
                crate::slog_info!(
                    "perf tier2_callgraph_snapshot: source=data_layer files={} entry_points={} ms={}",
                    snapshot.files.len(),
                    snapshot.entry_points.len(),
                    started.elapsed().as_millis()
                );
                DeadCodeSnapshot::Ready(snapshot)
            }
            Err(error) => {
                crate::slog_warn!(
                    "tier2 dead_code: data layer snapshot failed for {}: {error}",
                    project_root.display()
                );
                DeadCodeSnapshot::Failed(error.to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {}
