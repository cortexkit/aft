//! Project dead-code inputs from one caller’s rows.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;

use super::super::OUTBOUND_KINDS;
use super::rows::{GraphRows, TargetNodes};
use super::{path_key, GoPackages, Paths};
use crate::callgraph_store::dead_code_projection::{
    is_method_dispatch_callee, is_resolved_edge, symbol_kind_from_label,
    UNKNOWN_RECEIVER_LIVE_PROVENANCE, UNRESOLVED_VALUE_PATH_PROVENANCE,
};
use crate::callgraph_store::{
    lang_from_label, PROVENANCE_TREESITTER, PROVENANCE_VALUE_REF, TOP_LEVEL_SYMBOL,
};
use crate::data_layer::dead_code_snapshot::{
    CallgraphExport, CallgraphOutboundCall, GoInitializerGroup, DEFAULT_EXPORT_MARKER_KIND,
    DISPATCHED_CALLEE_SEPARATOR,
};
use crate::data_layer::graph_rows::{EdgeRow, NodeRow, RefRow};

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

pub(super) fn exported_symbols(rows: &GraphRows, paths: &mut Paths<'_>) -> Vec<CallgraphExport> {
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

pub(super) fn entry_point_symbols(rows: &GraphRows) -> BTreeSet<String> {
    let mut roots = BTreeSet::new();
    for node in nodes_in_file_order(rows, |node| node.entry_point) {
        let Some(kind) = symbol_kind_from_label(&node.kind) else {
            continue;
        };
        let Some(lang) = lang_from_label(&rows.language) else {
            continue;
        };
        if crate::callgraph::is_entry_point(&node.scoped_name, &kind, node.exported, lang) {
            continue;
        }
        roots.insert(node.name.clone());
        if node.scoped_name != node.name {
            roots.insert(node.scoped_name.clone());
        }
    }
    // Members an unknown receiver may reach are not roots here: they are
    // listed with their call sites among the outbound calls, since a site in
    // test code keeps them alive for tests only.
    roots
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

    fn target_symbol(&self, nodes: &impl TargetNodes) -> Option<std::borrow::Cow<'_, str>> {
        // `COALESCE(tn.name, r.target_symbol, e.target_symbol)`: the edge's
        // target node's plain name first.
        self.edge
            .and_then(|edge| nodes.name(&edge.target_file, edge.target_node.as_deref()?))
            .map(std::borrow::Cow::Owned)
            .or(self
                .reference
                .target_symbol
                .as_deref()
                .map(std::borrow::Cow::Borrowed))
            .or_else(|| {
                self.edge
                    .map(|edge| std::borrow::Cow::Borrowed(edge.target_symbol.as_str()))
            })
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

pub(super) fn outbound_calls(
    rows: &GraphRows,
    paths: &mut Paths<'_>,
    targets: &impl TargetNodes,
) -> Vec<CallgraphOutboundCall> {
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
            if let Some(call) = outbound_call(rows, paths, &row, targets) {
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
    targets: &impl TargetNodes,
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
        match (row.target_file(), row.target_symbol(targets)) {
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

pub(super) fn parent(path: &str) -> &Path {
    Path::new(path).parent().unwrap_or(Path::new(""))
}

pub(super) fn go_initializers(
    file: &str,
    rows: &GraphRows,
    paths: &mut Paths<'_>,
    packages: &GoPackages,
) -> Option<GoInitializerGroup> {
    if rows.language != "go" {
        return None;
    }
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
            node.scoped_name == "<go-package-init>"
                || (node.kind == "function" && node.scoped_name == "main")
                || calls_out.contains(node.id.as_str())
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
    if callers.is_empty() {
        return None;
    }
    let mut dirs = BTreeSet::from([parent(file)]);
    dirs.extend(rows.refs.values().filter_map(|reference| {
        (reference.kind == "import" && reference.status == "resolved")
            .then(|| reference.target_file.as_deref().map(parent))
            .flatten()
    }));
    let mut targets = Vec::new();
    for (target, symbols) in dirs.iter().filter_map(|dir| packages.get(*dir)).flatten() {
        if !file.ends_with("_test.go") && target.ends_with("_test.go") {
            continue;
        }
        let path = Arc::new(paths.resolve(target));
        targets.extend(
            symbols
                .iter()
                .map(|symbol| (Arc::clone(&path), Arc::clone(symbol))),
        );
    }
    Some(GoInitializerGroup {
        caller_file: paths.resolve(file),
        callers: callers
            .into_iter()
            .map(|node| node.scoped_name.clone())
            .collect(),
        targets,
    })
}
