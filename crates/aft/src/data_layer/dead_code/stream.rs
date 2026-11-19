//! Independent dead-code inputs, joined and compared one caller at a time.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::{path_key, Paths};
use crate::data_layer::checkout::{CheckoutView, ViewGraphFiles};
use crate::data_layer::dead_code_graph::{
    CallGraphFiles, DeadCodeGraph, FileProjection, SeedClass,
};
use crate::data_layer::dead_code_snapshot::{
    CallgraphExport, CallgraphOutboundCall, CallgraphSnapshot, EntryPoints, GoInitializerGroup,
};
use crate::data_layer::graph_rows::Nodes;

mod projection;
mod rows;
mod targets;
use projection::{entry_point_symbols, exported_symbols, go_initializers, outbound_calls, parent};
#[cfg(feature = "test-hooks")]
use rows::GraphRows;
use rows::{assemble, file_nodes};
use targets::Targets;

#[cfg(test)]
mod tests;

#[cfg(feature = "test-hooks")]
pub(crate) mod artifact;

pub(super) type GoPackages = BTreeMap<PathBuf, Vec<(String, Vec<Arc<str>>)>>;

pub(crate) struct Rows<'a> {
    root: &'a Path,
    source: ViewGraphFiles<'a>,
    entry_points: BTreeSet<PathBuf>,
    targets: Targets,
    packages: GoPackages,
    paths: parking_lot::Mutex<Paths<'a>>,
    /// Paths ordered by canonical file, then by the stored path string.
    canonical: Vec<(PathBuf, String)>,
}

#[derive(Debug)]
pub(crate) struct File {
    pub(crate) calls: Vec<CallgraphOutboundCall>,
    pub(crate) exports: Vec<CallgraphExport>,
    pub(crate) roots: BTreeSet<String>,
    pub(crate) go: Option<GoInitializerGroup>,
}

impl<'a> Rows<'a> {
    pub(crate) fn new(
        view: &'a CheckoutView,
        entry_points: &dyn EntryPoints,
    ) -> Result<Self, String> {
        let root = view.root();
        let source = view.dead_code_files();
        let mut paths = Paths {
            root,
            cache: HashMap::new(),
        };
        let mut targets = Targets::default();
        let mut packages = GoPackages::new();
        let mut canonical = Vec::new();
        for path in source.paths() {
            let parse = source.parse(&path)?;
            let (nodes, lookup) = file_nodes(&path, &parse);
            if parse.language == "go" {
                packages
                    .entry(parent(&path).to_path_buf())
                    .or_default()
                    .push((path.clone(), initializer_symbols(&nodes)));
            }
            targets.insert(path.clone(), nodes, lookup);
            canonical.push((paths.resolve(&path), path));
        }
        canonical
            .sort_by(|(left, l), (right, r)| path_key(left).cmp(path_key(right)).then(l.cmp(r)));
        let files = canonical
            .iter()
            .map(|(file, _)| file.clone())
            .collect::<Vec<_>>();
        Ok(Self {
            root,
            source,
            entry_points: entry_points.resolve(root, &files),
            targets,
            packages,
            paths: parking_lot::Mutex::new(paths),
            canonical,
        })
    }

    pub(crate) fn root(&self) -> &Path {
        self.root
    }

    pub(crate) fn paths(&self) -> &[(PathBuf, String)] {
        &self.canonical
    }

    pub(crate) fn metadata(&self) -> CallgraphSnapshot {
        let files: Vec<_> = self
            .canonical
            .iter()
            .map(|(file, _)| file.clone())
            .collect();
        CallgraphSnapshot {
            files,
            entry_points: self.entry_points.clone(),
            ..CallgraphSnapshot::default()
        }
    }

    pub(crate) fn file(&self, path: &str) -> Result<File, String> {
        let parse = self.source.parse(path)?;
        let resolution = self.source.resolution(path)?;
        let rows = assemble(path, &parse, &resolution, &self.targets);
        let mut paths = self.paths.lock();
        Ok(File {
            calls: outbound_calls(&rows, &mut paths, &self.targets),
            exports: exported_symbols(&rows, &mut paths),
            roots: entry_point_symbols(&rows),
            go: go_initializers(path, &rows, &mut paths, &self.packages),
        })
    }

    #[cfg(feature = "test-hooks")]
    pub(crate) fn exports(&self, path: &str) -> Result<Vec<CallgraphExport>, String> {
        let parse = self.source.parse(path)?;
        let (nodes, _) = file_nodes(path, &parse);
        let rows = GraphRows {
            nodes: nodes
                .rows
                .into_iter()
                .map(|(id, (_, node))| (id, node))
                .collect(),
            ..GraphRows::default()
        };
        Ok(exported_symbols(&rows, &mut self.paths.lock()))
    }

    #[cfg(any(feature = "test-hooks", test))]
    pub(crate) fn go_paths(&self) -> Vec<&str> {
        let mut paths = self
            .packages
            .values()
            .flatten()
            .map(|(path, _)| path.as_str())
            .collect::<Vec<_>>();
        paths.sort_unstable();
        paths
    }

    #[cfg(test)]
    pub(crate) fn fixture_snapshot(
        view: &'a CheckoutView,
        entry_points: &dyn EntryPoints,
    ) -> Result<CallgraphSnapshot, String> {
        let rows = Self::new(view, entry_points)?;
        let mut snapshot = rows.metadata();
        let expected = view
            .dead_code_snapshot_shared(entry_points)
            .map_err(|error| error.to_string())?;
        rows.compare(&expected, |file, facts| -> Result<(), String> {
            snapshot.outbound_calls.extend(facts.calls);
            snapshot.exported_symbols.extend(facts.exports);
            if !facts.roots.is_empty() {
                snapshot
                    .entry_point_symbols
                    .entry(file.to_path_buf())
                    .or_default()
                    .extend(facts.roots);
            }
            Ok(())
        })?;
        for path in rows.go_paths() {
            snapshot.go_initializers.extend(rows.file(path)?.go);
        }
        Ok(snapshot)
    }

    /// Neither the independent rows nor the graph projection retains a previous caller's
    /// calls. The visitor can export the checked file before it is dropped.
    pub(crate) fn compare<E: From<String>>(
        &self,
        snapshot: &CallgraphSnapshot,
        mut visit: impl FnMut(&Path, File) -> Result<(), E>,
    ) -> Result<(), E> {
        let metadata = self.metadata();
        if metadata.files != snapshot.files || metadata.entry_points != snapshot.entry_points {
            return Err("the snapshots' files, exports or entry points differ"
                .to_string()
                .into());
        }
        let mut graph = DeadCodeGraph::open_lazy(self.root, &self.source);
        graph.prepare_stream().map_err(E::from)?;
        for (absolute, path) in &self.canonical {
            let file = self.file(path).map_err(E::from)?;
            let (rollup, exports, roots) = graph.stream_file(path).map_err(E::from)?;
            compare_file(path, &file, rollup, exports, roots).map_err(E::from)?;
            #[cfg(test)]
            assert_eq!(
                graph.held_full_files(),
                0,
                "full symbols retained after {path}"
            );
            visit(absolute, file)?;
        }
        Ok(())
    }
}

fn initializer_symbols(nodes: &Nodes) -> Vec<Arc<str>> {
    let mut nodes = nodes
        .rows
        .values()
        .filter(|(_, node)| {
            node.kind == "variable"
                || node.scoped_name == "<go-package-init>"
                || (node.kind == "function" && node.scoped_name == "init")
        })
        .collect::<Vec<_>>();
    nodes.sort_by_key(|(ordinal, _)| *ordinal);
    nodes
        .into_iter()
        .map(|(_, node)| node.scoped_name.as_str().into())
        .collect()
}

fn compare_file(
    path: &str,
    rows: &File,
    rollup: FileProjection,
    exports: Vec<CallgraphExport>,
    roots: BTreeSet<String>,
) -> Result<(), String> {
    use crate::callgraph_store::dead_code_projection::UNKNOWN_RECEIVER_LIVE_PROVENANCE;
    use crate::callgraph_store::TOP_LEVEL_SYMBOL;

    let mismatch = |section| Err(format!("{path}: dead-code {section} differs from rollup"));
    if rows.exports != exports {
        return mismatch("exports");
    }
    if rows.roots != roots {
        return mismatch("entry_point_symbols");
    }
    if rows.go.as_ref().map(group) != rollup.go_initializers.as_ref().map(group) {
        return mismatch("go_initializers");
    }
    let mut expected = rows
        .calls
        .iter()
        .filter(|call| call.provenance != UNKNOWN_RECEIVER_LIVE_PROVENANCE)
        .map(call_key)
        .collect::<Vec<_>>();
    let mut actual = rollup
        .outbound_calls
        .iter()
        .map(call_key)
        .collect::<Vec<_>>();
    expected.sort_unstable();
    actual.sort_unstable();
    if expected != actual {
        return mismatch("calls");
    }
    let mut expected = rows
        .calls
        .iter()
        .filter(|call| call.provenance == UNKNOWN_RECEIVER_LIVE_PROVENANCE)
        .map(|call| (call.caller_symbol.as_str(), call.target.clone(), call.line))
        .collect::<Vec<_>>();
    let mut actual = rollup
        .unknown_live
        .iter()
        .map(|(file, symbol, class)| {
            let SeedClass::TestLine(line) = class else {
                unreachable!("the exactness projection retains every site line")
            };
            (
                TOP_LEVEL_SYMBOL,
                format!("{}::{symbol}", file.display()),
                *line,
            )
        })
        .collect::<Vec<_>>();
    expected.sort_unstable();
    actual.sort_unstable();
    if expected != actual {
        return mismatch("unknown receivers");
    }
    Ok(())
}

fn group(group: &GoInitializerGroup) -> (&Path, &[String], &[(Arc<PathBuf>, Arc<str>)]) {
    (&group.caller_file, &group.callers, &group.targets)
}

fn call_key(call: &CallgraphOutboundCall) -> (&Path, &str, u32, &str, &str) {
    (
        &call.caller_file,
        &call.caller_symbol,
        call.line,
        &call.target,
        &call.provenance,
    )
}
