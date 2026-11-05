//! aft_inspect's dead-code analysis, read from the data layer.
//!
//! Dead code analyses a snapshot of the call graph: every file, exported
//! symbol, outbound call and entry point (`CallgraphSnapshot`). Today that
//! snapshot is projected from a call-graph database: the checkout's view
//! database with views on, the legacy store with views off. When the process
//! runs the data layer, views are on, and the root's checkout has published a
//! call graph with no changes pending, the snapshot comes from the checkout's
//! view instead. In every other case dead code projects it from the database
//! as before, so a change the view has not resolved yet is never answered
//! from the older publication.
//!
//! The data layer stands in only for the views database, whose snapshot it
//! reproduces. The legacy store answers differently on some checkouts, so
//! with views off dead code keeps it, and no result computed from the data
//! layer reaches the legacy store's caches.
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use aft_store::ids::{Digest128, FamilyId};

    use crate::data_layer::checkout::{Checkout, DataLayer};
    use crate::inspect::job::CallgraphSnapshot;

    /// Each snapshot part as one line per item, so two snapshots compare as
    /// sets of items.
    fn parts(snapshot: &CallgraphSnapshot) -> [(&'static str, Vec<String>); 6] {
        fn lines<T: std::fmt::Debug>(items: impl IntoIterator<Item = T>) -> Vec<String> {
            items.into_iter().map(|item| format!("{item:?}")).collect()
        }
        [
            ("files", lines(&snapshot.files)),
            ("exported_symbols", lines(&snapshot.exported_symbols)),
            ("outbound_calls", lines(&snapshot.outbound_calls)),
            ("entry_points", lines(&snapshot.entry_points)),
            (
                "entry_point_symbols",
                lines(snapshot.entry_point_symbols.iter().flat_map(|(file, symbols)| {
                    symbols.iter().map(move |symbol| (file, symbol))
                })),
            ),
            ("go_initializers", lines(&snapshot.go_initializers)),
        ]
    }

    /// A diagnostic for the inspect gate, not a check: on the checkout named
    /// by `AFT_INSPECT_GATE_ROOT`, builds today's legacy call-graph database
    /// and the data layer's checkout, and prints each dead-code snapshot part
    /// that differs between the database's projection and the data layer's
    /// snapshot, with the items found on one side only; then every item dead
    /// code reports from one snapshot and not the other. Run it with
    /// `--ignored --nocapture`.
    #[test]
    #[ignore = "a diagnostic run on a named checkout"]
    fn print_how_dead_code_snapshots_differ_on_a_checkout() {
        let Some(root) = std::env::var_os("AFT_INSPECT_GATE_ROOT") else {
            return;
        };
        let root = crate::inspect::job::canonicalize_normalized(std::path::Path::new(&root));
        let storage = tempfile::tempdir().expect("storage");
        let files = crate::callgraph::walk_project_files(&root).collect::<Vec<_>>();
        // Not `<dir>/callgraph`: that names a legacy harness partition, which
        // today's store refuses to write into.
        let store = crate::callgraph_store::CallGraphStore::open(
            storage.path().join("today-store"),
            root.clone(),
        )
        .expect("open today's database");
        store.cold_build(&files).expect("build today's database");
        let today = crate::callgraph_store::project_dead_code_snapshot(store.sqlite_path())
            .expect("project today's snapshot");
        let threads = std::thread::available_parallelism().map_or(4, usize::from);
        let checkout = Checkout::bind(
            Arc::new(DataLayer::new(256 << 20, threads)),
            &storage.path().join("data"),
            FamilyId::from_digest(Digest128::of(b"inspect gate")),
            &root,
        )
        .expect("bind the checkout");
        checkout.build().expect("build the checkout");
        let ours = checkout
            .view()
            .expect("published")
            .dead_code_snapshot()
            .expect("the data layer's snapshot");
        for ((name, theirs), (_, mine)) in parts(&today).into_iter().zip(parts(&ours)) {
            let theirs = theirs.iter().collect::<BTreeSet<_>>();
            let mine = mine.iter().collect::<BTreeSet<_>>();
            let only_today = theirs.difference(&mine).collect::<Vec<_>>();
            let only_ours = mine.difference(&theirs).collect::<Vec<_>>();
            println!(
                "{name}: {} items only today, {} only in the data layer",
                only_today.len(),
                only_ours.len()
            );
            for line in only_today.iter().take(12) {
                println!("  today:      {line}");
            }
            for line in only_ours.iter().take(12) {
                println!("  data layer: {line}");
            }
        }

        // What dead code reports from each snapshot, every item. JavaScript
        // and TypeScript read the Oxc engine's facts in a real pass and are
        // left out here, where neither side has them.
        let dead = |snapshot: CallgraphSnapshot| {
            let job = crate::inspect::InspectJob {
                job_id: 1,
                key: crate::inspect::JobKey::for_project_category(
                    crate::inspect::InspectCategory::DeadCode,
                ),
                category: crate::inspect::InspectCategory::DeadCode,
                scope_files: files.clone(),
                project_root: root.clone(),
                inspect_dir: storage.path().join("inspect"),
                config: Arc::new(crate::config::Config {
                    project_root: Some(root.clone()),
                    ..crate::config::Config::default()
                }),
                symbol_cache: Arc::new(std::sync::RwLock::new(crate::parser::SymbolCache::new())),
                inspect_writer: true,
                callgraph_writer: true,
                callgraph_snapshot: Some(Arc::new(snapshot)),
            };
            let aggregate = crate::inspect::scanners::dead_code::run_dead_code_scan(&job)
                .outcome
                .expect("dead-code scan")
                .aggregate;
            let mut found = BTreeSet::new();
            for (list, label) in [("items", "dead"), ("test_only_items", "test-only")] {
                for item in aggregate[list].as_array().into_iter().flatten() {
                    let file = item["file"].as_str().unwrap_or_default();
                    if [".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs"]
                        .iter()
                        .any(|suffix| file.ends_with(suffix))
                    {
                        continue;
                    }
                    found.insert(format!(
                        "{label} {file}::{} ({}, line {})",
                        item["symbol"].as_str().unwrap_or_default(),
                        item["kind"].as_str().unwrap_or_default(),
                        item["line"]
                    ));
                }
            }
            found
        };
        let theirs = dead(today);
        let mine = dead(ours);
        println!(
            "dead code: {} items from today's snapshot, {} from the data layer's",
            theirs.len(),
            mine.len()
        );
        for item in theirs.difference(&mine) {
            println!("  only today:      {item}");
        }
        for item in mine.difference(&theirs) {
            println!("  only data layer: {item}");
        }
    }
}
