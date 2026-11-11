use super::{PROVENANCE_NAME_MATCH, PROVENANCE_TYPE_MATCH, PROVENANCE_VALUE_REF};
use crate::symbols::SymbolKind;
use std::time::Duration;

pub(crate) const UNRESOLVED_VALUE_PATH_PROVENANCE: &str = "unresolved_value_path";
/// Provenance of a projected outbound call that is not a call: a member an
/// unknown receiver at that line of the caller file may reach, which dead
/// code keeps alive (for tests only when the line is test code).
pub(crate) const UNKNOWN_RECEIVER_LIVE_PROVENANCE: &str = "unknown_receiver_live";
pub(crate) const MAX_DELTA_BYTES: usize = 256 * 1024;

/// How a dead-code snapshot was produced: by splicing the previous snapshot
/// with the caller delta journal, or by re-reading the whole store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProjectionKind {
    #[cfg(test)]
    Spliced,
    Full,
    /// The cached snapshot for the current revision was handed back without
    /// touching the store: no projection work at all.
    #[cfg(test)]
    Reused,
}

/// Why a projection took the path it did, plus the journal/corpus counts that
/// let an operator see the cost without re-deriving it from timings.
///
/// `reason` is `Some` only for `Full` projections; the reasons are `cold`
/// (no previous snapshot or a legacy store without a durable revision),
/// `journal_gap` (a missing or unparseable journal entry bridged the revisions),
/// and `splice_costlier` (the retained root timings predict a full pass is cheaper).
/// No `generation_changed` arm exists: the cache identity already pairs the
/// cold-build generation with the durable revision, so a generation change
/// surfaces as `cold` or a cache miss.
///
/// The journal records each write's callers already extended with their
/// dependents, so `changed_files` counts every file re-projected by a splice;
/// the caller/dependent split is not recoverable at read time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProjectionVerdict {
    pub kind: ProjectionKind,
    pub reason: Option<&'static str>,
    /// Bytes of inline or spill-backed journal entries read for a splice.
    pub journal_bytes: u64,
    /// Files in the journal delta, including a delta rejected as costlier than full.
    pub changed_files: usize,
}

/// Root-local running projection costs used to avoid a splice whose snapshot
/// fold is predicted to cost more than re-reading the complete store.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ProjectionCostEstimates {
    full_ns: u64,
    splice_ns_per_file: u64,
}

impl ProjectionCostEstimates {
    pub(crate) fn observe(&mut self, verdict: ProjectionVerdict, elapsed: Duration) {
        let elapsed_ns = elapsed.as_nanos().min(u128::from(u64::MAX)) as u64;
        match verdict.kind {
            ProjectionKind::Full => update_running_estimate(&mut self.full_ns, elapsed_ns),
            #[cfg(test)]
            ProjectionKind::Spliced if verdict.changed_files > 0 => {
                let per_file = elapsed_ns / verdict.changed_files as u64;
                update_running_estimate(&mut self.splice_ns_per_file, per_file.max(1));
            }
            #[cfg(test)]
            ProjectionKind::Spliced => {}
            #[cfg(test)]
            ProjectionKind::Reused => {}
        }
    }
}

fn update_running_estimate(estimate: &mut u64, observed: u64) {
    *estimate = if *estimate == 0 {
        observed.max(1)
    } else {
        estimate
            .saturating_mul(3)
            .saturating_add(observed)
            .saturating_add(2)
            / 4
    };
}

pub(crate) fn is_resolved_edge(status: &str, provenance: Option<&str>) -> bool {
    matches!(status, "resolved" | "resolved_local")
        || provenance.is_some_and(|provenance| {
            provenance == PROVENANCE_TYPE_MATCH
                || provenance == PROVENANCE_VALUE_REF
                || (provenance != PROVENANCE_NAME_MATCH
                    && (provenance.contains("treesitter") || provenance.contains("resolver")))
        })
}

pub(crate) fn is_method_dispatch_callee(full_callee: &str, callee_name: &str) -> bool {
    let full_callee = full_callee.trim();
    if !full_callee.contains('.') || full_callee == callee_name.trim() {
        return false;
    }

    full_callee
        .rsplit('.')
        .next()
        .map(|segment| segment.trim().trim_start_matches('?') == callee_name.trim())
        .unwrap_or(false)
}

pub(crate) fn symbol_kind_from_label(label: &str) -> Option<SymbolKind> {
    match label {
        "function" => Some(SymbolKind::Function),
        "kernel" => Some(SymbolKind::Kernel),
        "class" => Some(SymbolKind::Class),
        "method" => Some(SymbolKind::Method),
        "struct" => Some(SymbolKind::Struct),
        "interface" => Some(SymbolKind::Interface),
        "enum" => Some(SymbolKind::Enum),
        "type_alias" => Some(SymbolKind::TypeAlias),
        "variable" => Some(SymbolKind::Variable),
        "heading" => Some(SymbolKind::Heading),
        "file_summary" => Some(SymbolKind::FileSummary),
        _ => None,
    }
}

#[cfg(test)]
thread_local! {
    static FIXTURE_STORAGE: std::cell::RefCell<Vec<tempfile::TempDir>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// A fresh index-store call graph for scanner fixtures. Keep its files until
/// the test thread exits because the snapshot reads them lazily.
#[cfg(test)]
pub(crate) fn view_for_test(root: &std::path::Path) -> crate::data_layer::checkout::CheckoutView {
    let storage = tempfile::tempdir().expect("scanner fixture storage");
    let checkout = crate::data_layer::runtime::DataCheckout::open_planes(
        root,
        storage.path(),
        None,
        crate::data_layer::checkout::Planes {
            text: false,
            callgraph: true,
        },
    )
    .expect("bind scanner fixture");
    let view = match checkout.fresh_view(std::time::Duration::from_secs(60)) {
        crate::data_layer::runtime::Fresh::Ready(view) => view,
        crate::data_layer::runtime::Fresh::Building => {
            panic!("scanner fixture call graph did not publish")
        }
        crate::data_layer::runtime::Fresh::Failed(reason) => {
            panic!("scanner fixture call graph failed: {reason}")
        }
    };
    FIXTURE_STORAGE.with(|folders| folders.borrow_mut().push(storage));
    view
}

#[cfg(test)]
pub(crate) fn snapshot_for_test(root: &std::path::Path) -> crate::inspect::job::CallgraphSnapshot {
    view_for_test(root)
        .dead_code_snapshot(&crate::inspect::InspectEntryPoints)
        .expect("scanner fixture snapshot")
}
