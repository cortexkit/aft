//! The dead-code snapshot of a checkout's call graph, built from its rows.
//!
//! `aft_inspect` finds dead code and unused exports in a snapshot of the
//! call graph: every file, every exported symbol, every outbound call, the
//! symbols that keep themselves alive, and Go package initialization. Today
//! the snapshot is projected from the call-graph database by SQL
//! (`callgraph_store::dead_code_projection`). This builds the same snapshot
//! from the rows a resolved checkout consists of, by the same rules in the
//! same order, so the two can be compared value for value.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use super::graph_rows::{EdgeRow, GraphRows, NodeRow, RefRow};
use crate::callgraph_store::dead_code_projection::{
    is_method_dispatch_callee, is_resolved_edge, symbol_kind_from_label,
    UNKNOWN_RECEIVER_LIVE_PROVENANCE, UNRESOLVED_VALUE_PATH_PROVENANCE,
};
use crate::callgraph_store::{
    lang_from_label, PROVENANCE_TREESITTER, PROVENANCE_VALUE_REF, TOP_LEVEL_SYMBOL,
};
use crate::data_layer::dead_code_snapshot::DEFAULT_EXPORT_MARKER_KIND;
use crate::data_layer::dead_code_snapshot::{
    CallgraphExport, CallgraphOutboundCall, CallgraphSnapshot, GoInitializerGroup,
    DISPATCHED_CALLEE_SEPARATOR,
};
use crate::inspect::job::canonicalize_normalized;

/// The kinds of reference an outbound call can come from.
pub(crate) const OUTBOUND_KINDS: [&str; 5] = [
    "call",
    "construction",
    "callback_ref",
    "value_ref",
    "macro_body_call",
];

/// Store paths made absolute and canonical, as the snapshot names files.
pub(crate) struct Paths<'a> {
    pub(crate) root: &'a Path,
    pub(crate) cache: HashMap<String, PathBuf>,
}

impl Paths<'_> {
    pub(crate) fn resolve(&mut self, store_path: &str) -> PathBuf {
        if let Some(path) = self.cache.get(store_path) {
            return path.clone();
        }
        let path = Path::new(store_path);
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        let path = canonicalize_normalized(&absolute);
        self.cache.insert(store_path.to_string(), path.clone());
        path
    }
}

pub(crate) fn path_key(path: &Path) -> &[u8] {
    path.as_os_str().as_encoded_bytes()
}

/// The dead-code snapshot of the checkout at `root` whose call graph is
/// `rows`.
#[must_use]
pub(crate) fn snapshot(root: &Path, rows: &GraphRows) -> CallgraphSnapshot {
    let mut paths = Paths {
        root,
        cache: HashMap::new(),
    };
    let mut files = rows
        .files
        .keys()
        .map(|path| paths.resolve(path))
        .collect::<Vec<_>>();
    files.sort_by(|left, right| path_key(left).cmp(path_key(right)));
    let entry_points = {
        let resolved = crate::inspect::resolve_entry_points(root);
        files
            .iter()
            .filter(|file| resolved.is_entry_point(file))
            .cloned()
            .collect()
    };
    CallgraphSnapshot {
        generated_at: Some(SystemTime::now()),
        exported_symbols: exported_symbols(rows, &mut paths),
        outbound_calls: outbound_calls(rows, &mut paths),
        entry_point_symbols: entry_point_symbols(rows, &mut paths),
        go_initializers: go_initializers(rows, &mut paths),
        files,
        entry_points,
        data_layer: None,
    }
}

/// Nodes in `ORDER BY file_path, start_line, name, kind, id`.
fn nodes_in_file_order(rows: &GraphRows, keep: impl Fn(&NodeRow) -> bool) -> Vec<&NodeRow> {
    let mut nodes = rows
        .nodes
        .values()
        .filter(|node| keep(node))
        .collect::<Vec<_>>();
    nodes.sort_by(|left, right| {
        (
            &left.file_path,
            left.start_line,
            &left.name,
            &left.kind,
            &left.id,
        )
            .cmp(&(
                &right.file_path,
                right.start_line,
                &right.name,
                &right.kind,
                &right.id,
            ))
    });
    nodes
}

fn exported_symbols(rows: &GraphRows, paths: &mut Paths<'_>) -> Vec<CallgraphExport> {
    let mut exports = Vec::new();
    for node in nodes_in_file_order(rows, |node| node.exported || node.is_default_export) {
        let file = paths.resolve(&node.file_path);
        let line = node.start_line.saturating_add(1);
        if node.exported {
            exports.push(CallgraphExport {
                file: file.clone(),
                symbol: node.name.clone(),
                kind: node.kind.clone(),
                line,
            });
        }
        if node.is_default_export {
            exports.push(CallgraphExport {
                file,
                symbol: node.name.clone(),
                kind: DEFAULT_EXPORT_MARKER_KIND.to_string(),
                line,
            });
        }
    }
    exports.sort_by(|left, right| path_key(&left.file).cmp(path_key(&right.file)));
    exports
}

fn entry_point_symbols(
    rows: &GraphRows,
    paths: &mut Paths<'_>,
) -> BTreeMap<PathBuf, BTreeSet<String>> {
    let mut by_file = BTreeMap::<PathBuf, BTreeSet<String>>::new();
    for node in nodes_in_file_order(rows, |node| node.entry_point) {
        // The projection joins each node to its file for the language; a
        // node of a file the files table lacks is skipped.
        let Some((_, language)) = rows.files.get(&node.file_path) else {
            continue;
        };
        let Some(kind) = symbol_kind_from_label(&node.kind) else {
            continue;
        };
        let Some(lang) = lang_from_label(language) else {
            continue;
        };
        if crate::callgraph::is_entry_point(&node.scoped_name, &kind, node.exported, lang) {
            continue;
        }
        let roots = by_file.entry(paths.resolve(&node.file_path)).or_default();
        roots.insert(node.name.clone());
        if node.scoped_name != node.name {
            roots.insert(node.scoped_name.clone());
        }
    }
    // Members an unknown receiver may reach are not roots here: they are
    // listed with their call sites among the outbound calls, since a site in
    // test code keeps them alive for tests only.
    by_file
}

/// One row of the projection's outbound query: a reference, joined to each
/// of its edges of the same kind, or to none.
struct Outbound<'r> {
    reference: &'r RefRow,
    edge: Option<&'r EdgeRow>,
}

impl Outbound<'_> {
    fn target_file(&self) -> Option<&str> {
        self.reference
            .target_file
            .as_deref()
            .or_else(|| self.edge.map(|edge| edge.target_file.as_str()))
    }

    fn target_symbol<'n>(&'n self, rows: &'n GraphRows) -> Option<&'n str> {
        // `COALESCE(tn.name, r.target_symbol, e.target_symbol)`: the edge's
        // target node's plain name first.
        self.edge
            .and_then(|edge| edge.target_node.as_ref())
            .and_then(|id| rows.nodes.get(id))
            .map(|node| node.name.as_str())
            .or(self.reference.target_symbol.as_deref())
            .or_else(|| self.edge.map(|edge| edge.target_symbol.as_str()))
    }

    fn provenance(&self) -> &str {
        if matches!(self.reference.kind, "value_ref" | "callback_ref") {
            "value_ref"
        } else {
            self.edge
                .map_or(PROVENANCE_TREESITTER, |edge| edge.provenance.as_str())
        }
    }
}

fn outbound_calls(rows: &GraphRows, paths: &mut Paths<'_>) -> Vec<CallgraphOutboundCall> {
    let mut edges_by_ref = HashMap::<(&str, &str), Vec<&EdgeRow>>::new();
    for edge in rows.edges.values() {
        edges_by_ref
            .entry((edge.ref_id.as_str(), edge.kind))
            .or_default()
            .push(edge);
    }
    for edges in edges_by_ref.values_mut() {
        edges.sort_by_key(|edge| rows.edge_sequence.get(&edge.edge_id).copied());
    }
    let mut calls = Vec::new();
    for reference in rows.refs.values() {
        if !OUTBOUND_KINDS.contains(&reference.kind) {
            continue;
        }
        let edges = edges_by_ref.get(&(reference.ref_id.as_str(), reference.kind));
        let joined = match edges {
            Some(edges) => edges
                .iter()
                .map(|edge| Outbound {
                    reference,
                    edge: Some(edge),
                })
                .collect::<Vec<_>>(),
            None => vec![Outbound {
                reference,
                edge: None,
            }],
        };
        for row in joined {
            if let Some(call) = outbound_call(rows, paths, &row) {
                calls.push(call);
            }
        }
    }
    // Members an unknown receiver may reach, at the line of its call site;
    // the rollup keeps them alive as the data layer's graph does (see
    // `UNKNOWN_RECEIVER_LIVE_PROVENANCE`). They sort with the caller file's
    // top-level calls.
    for (caller_file, target_file, symbol, line) in &rows.unknown_live {
        let target_path = paths.resolve(target_file);
        calls.push((
            (
                caller_file.clone(),
                None,
                *line,
                0,
                0,
                format!("unknown:{target_file}:{symbol}"),
            ),
            CallgraphOutboundCall {
                caller_file: paths.resolve(caller_file),
                caller_symbol: TOP_LEVEL_SYMBOL.to_string(),
                target: format!("{}::{symbol}", target_path.display()),
                line: *line,
                provenance: UNKNOWN_RECEIVER_LIVE_PROVENANCE.to_string(),
            },
        ));
    }
    // The projection sorts by caller file, caller symbol (none first), line,
    // byte range and reference id, then by the resolved caller file.
    calls.sort_by(|left, right| left.0.cmp(&right.0));
    let mut calls = calls.into_iter().map(|(_, call)| call).collect::<Vec<_>>();
    calls.sort_by(|left, right| path_key(&left.caller_file).cmp(path_key(&right.caller_file)));
    calls
}

type OutboundKey = (String, Option<String>, u32, u64, u64, String);

fn outbound_call(
    rows: &GraphRows,
    paths: &mut Paths<'_>,
    row: &Outbound<'_>,
) -> Option<(OutboundKey, CallgraphOutboundCall)> {
    let reference = row.reference;
    let caller = reference
        .caller_node
        .as_ref()
        .and_then(|id| rows.nodes.get(id));
    let status = reference.status;
    let provenance = row.provenance();
    let resolved_status = matches!(status, "resolved" | "resolved_local");
    let full_ref = reference.full_ref.as_deref();
    let unresolved_value_path = provenance == PROVENANCE_VALUE_REF
        && !resolved_status
        && full_ref.is_some_and(|full_ref| full_ref.contains("::") || full_ref.contains('.'));
    if provenance == PROVENANCE_VALUE_REF && !resolved_status && !unresolved_value_path {
        return None;
    }
    let key = (
        reference.caller_file.clone(),
        caller.map(|node| node.name.clone()),
        reference.line,
        reference.byte_start,
        reference.byte_end,
        reference.ref_id.clone(),
    );
    let caller_symbol =
        caller.map_or_else(|| TOP_LEVEL_SYMBOL.to_string(), |node| node.name.clone());
    let unresolved_call_path =
        !resolved_status && full_ref.is_some_and(|full_ref| full_ref.contains("::"));
    let short_name = if unresolved_value_path || unresolved_call_path {
        full_ref.unwrap_or_default()
    } else {
        reference
            .short_name
            .as_deref()
            .or(full_ref)
            .unwrap_or_default()
    };
    let mut target = if is_resolved_edge(status, Some(provenance)) {
        match (row.target_file(), row.target_symbol(rows)) {
            (Some(target_file), Some(target_symbol)) => {
                let target_file = paths.resolve(target_file);
                format!("{}::{target_symbol}", target_file.display())
            }
            _ => short_name.to_string(),
        }
    } else {
        short_name.to_string()
    };
    if provenance != PROVENANCE_VALUE_REF
        && !reference.ref_id.starts_with("view:")
        && full_ref.is_some_and(|full_ref| is_method_dispatch_callee(full_ref, short_name))
    {
        target.push(DISPATCHED_CALLEE_SEPARATOR);
        target.push_str(full_ref.unwrap_or_default());
    }
    Some((
        key,
        CallgraphOutboundCall {
            caller_file: paths.resolve(&reference.caller_file),
            caller_symbol,
            target,
            line: reference.line,
            provenance: if unresolved_value_path {
                UNRESOLVED_VALUE_PATH_PROVENANCE.to_string()
            } else {
                provenance.to_string()
            },
        },
    ))
}

fn parent(path: &str) -> PathBuf {
    Path::new(path)
        .parent()
        .unwrap_or(Path::new(""))
        .to_path_buf()
}

fn go_initializers(rows: &GraphRows, paths: &mut Paths<'_>) -> Vec<GoInitializerGroup> {
    let go_files = rows
        .files
        .iter()
        .filter(|(_, (_, language))| language == "go")
        .map(|(path, _)| path.as_str())
        .collect::<Vec<_>>();
    let mut packages = BTreeMap::<PathBuf, Vec<&str>>::new();
    for path in &go_files {
        packages.entry(parent(path)).or_default().push(path);
    }
    let is_go = |file: &str| {
        rows.files
            .get(file)
            .is_some_and(|(_, language)| language == "go")
    };
    // A caller: the package initializer, `main`, or any symbol with a
    // resolved call or value reference to a known node.
    let calls_out = rows
        .refs
        .values()
        .filter(|reference| {
            matches!(reference.kind, "call" | "value_ref")
                && matches!(reference.status, "resolved" | "resolved_local")
                && reference.target_node.is_some()
        })
        .filter_map(|reference| reference.caller_node.as_deref())
        .collect::<BTreeSet<_>>();
    let mut callers = rows
        .nodes
        .values()
        .filter(|node| {
            is_go(&node.file_path)
                && (node.scoped_name == "<go-package-init>"
                    || (node.kind == "function" && node.scoped_name == "main")
                    || calls_out.contains(node.id.as_str()))
        })
        .collect::<Vec<_>>();
    // `ORDER BY file_path, scoped_name`; ties keep storage order.
    let position = rows
        .node_order
        .iter()
        .enumerate()
        .map(|(index, id)| (id.as_str(), index))
        .collect::<HashMap<_, _>>();
    callers.sort_by(|left, right| {
        (
            &left.file_path,
            &left.scoped_name,
            position[left.id.as_str()],
        )
            .cmp(&(
                &right.file_path,
                &right.scoped_name,
                position[right.id.as_str()],
            ))
    });
    // Package variables, initializers and `init` functions per file, in
    // storage order.
    let mut initializers = HashMap::<&str, Vec<&str>>::new();
    for id in &rows.node_order {
        let node = &rows.nodes[id];
        if node.kind == "variable"
            || node.scoped_name == "<go-package-init>"
            || (node.kind == "function" && node.scoped_name == "init")
        {
            initializers
                .entry(node.file_path.as_str())
                .or_default()
                .push(node.scoped_name.as_str());
        }
    }
    let mut imports = HashMap::<&str, BTreeSet<&str>>::new();
    for reference in rows.refs.values() {
        if reference.kind == "import" && reference.status == "resolved" {
            if let Some(target) = &reference.target_file {
                imports
                    .entry(reference.caller_file.as_str())
                    .or_default()
                    .insert(target);
            }
        }
    }
    let mut interned_files = BTreeMap::<&str, Arc<PathBuf>>::new();
    let mut interned_symbols = BTreeMap::<&str, Arc<str>>::new();
    let mut groups = BTreeMap::<String, GoInitializerGroup>::new();
    for caller in callers {
        let caller_file = caller.file_path.as_str();
        if !groups.contains_key(caller_file) {
            let mut dirs = vec![parent(caller_file)];
            for imported in imports.get(caller_file).into_iter().flatten() {
                dirs.push(parent(imported));
            }
            dirs.sort();
            dirs.dedup();
            let mut targets = Vec::new();
            for target_file in dirs.iter().filter_map(|dir| packages.get(dir)).flatten() {
                if !caller_file.ends_with("_test.go") && target_file.ends_with("_test.go") {
                    continue;
                }
                for symbol in initializers.get(target_file).into_iter().flatten() {
                    let file = interned_files
                        .entry(target_file)
                        .or_insert_with(|| Arc::new(paths.resolve(target_file)))
                        .clone();
                    let symbol = interned_symbols
                        .entry(symbol)
                        .or_insert_with(|| Arc::from(*symbol))
                        .clone();
                    targets.push((file, symbol));
                }
            }
            groups.insert(
                caller_file.to_string(),
                GoInitializerGroup {
                    caller_file: paths.resolve(caller_file),
                    callers: Vec::new(),
                    targets,
                },
            );
        }
        groups
            .get_mut(caller_file)
            .expect("inserted above")
            .callers
            .push(caller.scoped_name.clone());
    }
    groups.into_values().collect()
}
