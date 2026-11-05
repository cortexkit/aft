//! aft_inspect's dead-code analysis, read from the data layer.
//!
//! Dead code analyses a snapshot of the call graph: every file, exported
//! symbol, outbound call and entry point (`CallgraphSnapshot`). Today that
//! snapshot is projected from the call-graph database. When the process runs
//! the data layer and the root's checkout has published a call graph with no
//! changes pending, the snapshot comes from the checkout's view instead. In
//! every other case dead code projects it from the database as before, so a
//! change the view has not resolved yet is never answered from the older
//! publication.
//!
//! Only the snapshot's source changes. Unused exports and the Tier-1
//! categories do not read the call graph. Cached results keep their keys: a
//! cached result is keyed by the files it was computed from, and the view
//! describes the same files as the database it stands in for.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Instant;

use crate::data_layer::checkout::PlaneStatus;
use crate::data_layer::runtime::DataCheckout;
use crate::inspect::job::{canonicalize_normalized, CallgraphSnapshot};

/// The data-layer checkouts dead code reads, by the project root inspect
/// analyses. Empty unless the process runs the data layer. Holds each
/// checkout weakly: the request context owns it, and a root bound to another
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
        if canonicalize_normalized(checkout.root()) != project_root {
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
            .insert(project_root.to_path_buf(), Arc::downgrade(checkout));
    }

    /// The dead-code snapshot of `project_root` from its checkout's view,
    /// when the view is published and its call graph has no changes
    /// pending. `None` when no checkout is bound for the root, or the view
    /// cannot answer; dead code then projects the snapshot from today's
    /// database.
    pub(crate) fn snapshot(&self, project_root: &Path) -> Option<Arc<CallgraphSnapshot>> {
        let checkout = self
            .checkouts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(project_root)
            .and_then(Weak::upgrade)?;
        let Some(view) = checkout.view() else {
            crate::slog_info!(
                "tier2 dead_code: data layer not used for {}: nothing published yet",
                project_root.display()
            );
            return None;
        };
        let callgraph = view.status().callgraph;
        if callgraph != PlaneStatus::Ready {
            crate::slog_info!(
                "tier2 dead_code: data layer not used for {}: call graph {callgraph:?}",
                project_root.display()
            );
            return None;
        }
        let started = Instant::now();
        match view.dead_code_snapshot() {
            Ok(snapshot) => {
                crate::slog_info!(
                    "perf tier2_callgraph_snapshot: source=data_layer files={} exports={} edges={} entry_points={} ms={}",
                    snapshot.files.len(),
                    snapshot.exported_symbols.len(),
                    snapshot.outbound_calls.len()
                        + snapshot
                            .go_initializers
                            .iter()
                            .map(|group| group.edge_count())
                            .sum::<usize>(),
                    snapshot.entry_points.len(),
                    started.elapsed().as_millis()
                );
                Some(Arc::new(snapshot))
            }
            Err(error) => {
                crate::slog_warn!(
                    "tier2 dead_code: data layer not used for {}: {error}",
                    project_root.display()
                );
                None
            }
        }
    }
}
