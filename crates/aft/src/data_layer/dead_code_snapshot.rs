use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// Resolves the liveness roots among a snapshot's files. Inspect supplies the
/// inventory and manifest policy; the index owns only the resulting paths.
pub(crate) trait EntryPoints {
    fn resolve(&self, root: &Path, files: &[PathBuf]) -> BTreeSet<PathBuf>;
}

#[derive(Debug, Clone, Default)]
pub struct AnalyzeOptions {
    pub entry_points: Vec<PathBuf>,
    pub public_api_files: Vec<PathBuf>,
    pub executable_root_exports: BTreeMap<PathBuf, BTreeSet<String>>,
    /// Files already proven stale by the inspect freshness layer. These paths
    /// bypass the path metadata fast path so same-size/same-mtime edits are
    /// still re-read and content-hashed before facts are reused.
    pub force_reparse_files: Vec<PathBuf>,
    /// When true, imports/re-exports only make targets live after execution is
    /// reachable from entry/public files. Used by dead_code; unused_exports keeps
    /// the default import-usage semantics.
    pub entry_reachability: bool,
}

#[derive(Debug, Clone, Default)]
pub struct CallgraphSnapshot {
    pub generated_at: Option<SystemTime>,
    pub files: Vec<PathBuf>,
    pub exported_symbols: Vec<CallgraphExport>,
    pub outbound_calls: Vec<CallgraphOutboundCall>,
    pub go_initializers: Vec<GoInitializerGroup>,
    pub entry_points: BTreeSet<PathBuf>,
    pub entry_point_symbols: BTreeMap<PathBuf, BTreeSet<String>>,
    /// The data layer's call graph the snapshot was taken from. Dead code
    /// reads its calls from there one file at a time, so a snapshot from the
    /// data layer holds no call rows, marked roots or initializer groups of
    /// its own: only its files, exports and entry points.
    pub data_layer: Option<crate::data_layer::dead_code_graph::ViewCallGraph>,
}

/// All confirmed callers in one Go file initialize the same package set.
/// Keep the two sides separately instead of allocating their Cartesian product.
#[derive(Debug, Clone, Default)]
pub struct GoInitializerGroup {
    pub caller_file: PathBuf,
    pub callers: Vec<String>,
    pub targets: Vec<(Arc<PathBuf>, Arc<str>)>,
}

impl GoInitializerGroup {
    pub fn edge_count(&self) -> usize {
        self.callers.len() * self.targets.len()
            - self
                .callers
                .iter()
                .map(|caller| {
                    self.targets
                        .iter()
                        .filter(|(file, symbol)| {
                            file.as_ref() == &self.caller_file && symbol.as_ref() == caller
                        })
                        .count()
                })
                .sum::<usize>()
    }

    pub fn calls(&self) -> impl Iterator<Item = CallgraphOutboundCall> + '_ {
        self.callers.iter().flat_map(move |caller| {
            self.targets.iter().filter_map(move |(file, symbol)| {
                if file.as_ref() == &self.caller_file && symbol.as_ref() == caller {
                    return None;
                }
                Some(CallgraphOutboundCall {
                    caller_file: self.caller_file.clone(),
                    caller_symbol: caller.clone(),
                    target: format!("{}::{symbol}", file.display()),
                    line: 1,
                    provenance: "go_package_initializer".to_string(),
                })
            })
        })
    }
}

impl CallgraphSnapshot {
    /// Recreate the legacy edge sequence for byte-for-byte comparisons. The
    /// scanner retains initializer groups instead to avoid quadratic storage.
    pub fn expanded_outbound_calls(&self) -> Vec<CallgraphOutboundCall> {
        let mut calls = self.outbound_calls.clone();
        calls.extend(
            self.go_initializers
                .iter()
                .flat_map(GoInitializerGroup::calls),
        );
        calls.sort_by(|a, b| a.caller_file.as_os_str().cmp(b.caller_file.as_os_str()));
        calls
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallgraphExport {
    pub file: PathBuf,
    pub symbol: String,
    pub kind: String,
    pub line: u32,
}

pub(crate) const DISPATCHED_CALLEE_SEPARATOR: char = '\u{1f}';
pub(crate) const CALLGRAPH_PROVENANCE_TREESITTER: &str = "treesitter";
pub(crate) const CALLGRAPH_PROVENANCE_REEXPORT: &str = "reexport";

fn default_callgraph_outbound_provenance() -> String {
    CALLGRAPH_PROVENANCE_TREESITTER.to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallgraphOutboundCall {
    pub caller_file: PathBuf,
    pub caller_symbol: String,
    pub target: String,
    pub line: u32,
    #[serde(default = "default_callgraph_outbound_provenance")]
    pub provenance: String,
}

aft_store::heap_bytes_of_fields!(CallgraphSnapshot {
    generated_at,
    files,
    exported_symbols,
    outbound_calls,
    go_initializers,
    entry_points,
    entry_point_symbols,
    data_layer,
});
aft_store::heap_bytes_of_fields!(GoInitializerGroup {
    caller_file,
    callers,
    targets,
});
aft_store::heap_bytes_of_fields!(CallgraphExport {
    file,
    symbol,
    kind,
    line,
});
aft_store::heap_bytes_of_fields!(CallgraphOutboundCall {
    caller_file,
    caller_symbol,
    target,
    line,
    provenance,
});

pub(crate) const DEFAULT_EXPORT_MARKER_KIND: &str = "default_export";
