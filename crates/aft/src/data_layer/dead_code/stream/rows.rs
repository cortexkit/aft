//! Assemble one caller’s rows using the views table rules.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::callgraph_store::join::{BlobRefKind, ParseBlob, ResolutionStatus};
use crate::callgraph_store::{MACRO_BODY_CALL_REF_KIND, PROVENANCE_TREESITTER};
use crate::data_layer::callgraph::FileResolution;
use crate::data_layer::graph_rows::{
    macro_body_call, ref_kind, references_by_key, EdgeRow, NodeRow, Nodes, RefRow,
};

/// The rows of one caller file.
#[derive(Debug, Default)]
pub(crate) struct GraphRows {
    pub(crate) language: String,
    pub(crate) nodes: BTreeMap<String, NodeRow>,
    /// Node ids in the order the database stores them: insertion order,
    /// with a replaced row moved to the end.
    pub(crate) node_order: Vec<String>,
    /// Each edge's place in insertion order; a query that joins a reference
    /// to its edges reads them in this order.
    pub(crate) edge_sequence: HashMap<String, u64>,
    pub(crate) refs: BTreeMap<String, RefRow>,
    pub(crate) edges: BTreeMap<String, EdgeRow>,
    /// Members an unknown receiver keeps alive: caller file, target file,
    /// target symbol, and the line of the call site (a site in test code
    /// keeps them alive for tests only).
    pub(crate) unknown_live: BTreeSet<(String, String, String, u32)>,
}

/// The symbol rows and ordinary reference lookup of one file.
pub(crate) fn file_nodes(path: &str, parse: &ParseBlob) -> (Nodes, HashMap<String, String>) {
    let mut nodes = Nodes::default();
    let mut lookup = HashMap::new();
    for symbol in &parse.symbols {
        let id = crate::callgraph_store::view_node_id(path, &symbol.kind, &symbol.scoped_name);
        nodes.insert(NodeRow {
            id: id.clone(),
            file_path: path.to_string(),
            name: symbol.name.clone(),
            scoped_name: symbol.scoped_name.clone(),
            kind: symbol.kind.clone(),
            start_line: symbol.start_line,
            start_col: symbol.start_col,
            end_line: symbol.end_line,
            end_col: symbol.end_col,
            range_ordinal: symbol.ordinal,
            signature: symbol.signature.clone(),
            exported: symbol.exported,
            is_default_export: symbol.is_default_export,
            entry_point: if parse.language == "go"
                && matches!(symbol.kind.as_str(), "variable" | "method")
            {
                parse.entry_point_symbols.contains(&symbol.scoped_name)
            } else {
                symbol.exported
            },
            is_test: symbol.is_test,
            program_start: symbol.program_start,
            entry_attribute: symbol.entry_attribute.clone(),
        });
        lookup.insert(symbol.scoped_name.clone(), id.clone());
        lookup.entry(symbol.name.clone()).or_insert(id);
    }
    (nodes, lookup)
}

/// Symbol facts that survive across files; reference and edge rows never do.
pub(crate) trait TargetNodes {
    fn lookup(&self, file: &str, symbol: &str) -> Option<String>;
    fn scoped(&self, file: &str, symbol: &str) -> Option<String>;
    fn name(&self, file: &str, id: &str) -> Option<String>;
    fn is_constructor(&self, file: &str, id: &str) -> bool;
}

/// Assembles one caller's rows with the same join and ordering as the
/// whole-checkout views tables.
pub(crate) fn assemble(
    path: &str,
    parse: &ParseBlob,
    resolved: &FileResolution,
    targets: &impl TargetNodes,
) -> GraphRows {
    let mut rows = GraphRows {
        language: parse.language.clone(),
        ..GraphRows::default()
    };
    let (nodes, lookup) = file_nodes(path, parse);
    let references = references_by_key(parse);
    let mut derived = resolved.rows.iter().collect::<Vec<_>>();
    derived.sort();
    for row in derived {
        let Ok(caller_path) = std::str::from_utf8(&row.caller_path) else {
            continue;
        };
        if caller_path != path {
            continue;
        }
        let Some(reference) = references.get(&row.ref_ordinal) else {
            continue;
        };
        let target_path = row
            .target_path
            .as_ref()
            .and_then(|path| String::from_utf8(path.clone()).ok());
        let caller_node = reference
            .caller_symbol
            .as_ref()
            .and_then(|symbol| lookup.get(symbol))
            .cloned();
        let target_node = target_path
            .as_deref()
            .zip(row.target_symbol.as_deref())
            .and_then(|(path, symbol)| targets.lookup(path, symbol));
        let ref_id = format!("view:{caller_path}:{}", row.ref_ordinal);
        let macro_body = macro_body_call(row.kind, reference);
        let edge_ends = (
            caller_node.clone(),
            target_path.clone(),
            row.target_symbol.clone(),
        );
        if let (Some(source_node), Some(target_file), Some(target_symbol), true) = (
            edge_ends.0,
            edge_ends.1,
            edge_ends.2,
            matches!(
                row.kind,
                BlobRefKind::Call | BlobRefKind::Construction | BlobRefKind::CallbackRef
            ),
        ) {
            // A construction is a call when it targets a function or method
            // (a constructor), and a value reference otherwise.
            let kind = match row.kind {
                BlobRefKind::Construction
                    if !target_node
                        .as_deref()
                        .is_some_and(|id| targets.is_constructor(&target_file, id)) =>
                {
                    "value_ref"
                }
                BlobRefKind::CallbackRef => "callback_ref",
                _ if macro_body => MACRO_BODY_CALL_REF_KIND,
                _ => "call",
            };
            rows.edge_sequence
                .insert(format!("edge:{ref_id}"), rows.edge_sequence.len() as u64);
            rows.edges.insert(
                format!("edge:{ref_id}"),
                EdgeRow {
                    edge_id: format!("edge:{ref_id}"),
                    ref_id: ref_id.clone(),
                    source_node,
                    target_node: target_node.clone(),
                    target_file,
                    target_symbol,
                    kind,
                    line: reference.line,
                    provenance: PROVENANCE_TREESITTER.to_string(),
                },
            );
        }
        rows.refs.insert(
            ref_id.clone(),
            RefRow {
                ref_id,
                caller_node,
                caller_file: caller_path.to_string(),
                kind: if macro_body {
                    MACRO_BODY_CALL_REF_KIND
                } else {
                    ref_kind(row.kind)
                },
                short_name: reference.short_name.clone(),
                full_ref: reference.full_ref.clone(),
                module_path: reference.module_path.clone(),
                import_kind: reference.import_kind.clone(),
                local_name: if macro_body {
                    reference.caller_symbol.clone()
                } else {
                    reference.local_name.clone()
                },
                requested_name: reference.requested_name.clone(),
                namespace_alias: reference.namespace_alias.clone(),
                wildcard: reference.wildcard,
                line: reference.line,
                byte_start: reference.byte_start as u64,
                byte_end: reference.byte_end as u64,
                status: if row.status == ResolutionStatus::Resolved {
                    "resolved"
                } else {
                    "unresolved"
                },
                target_node,
                target_file: target_path,
                target_symbol: row.target_symbol.clone(),
            },
        );
    }

    // Sites in extraction order: each replaces its reference's ordinary edge and target.
    let by_scoped_name = nodes.by_scoped_name();
    for kept in &resolved.sites {
        let site = &kept.site;
        let resolution = &kept.resolution;
        let ref_id = format!("view:{path}:{}", site.ordinal);
        // A reference's only ordinary edge is `edge:<ref id>`.
        let ordinary = format!("edge:{ref_id}");
        if rows.edges.get(&ordinary).is_some_and(|edge| {
            !matches!(
                edge.provenance.as_str(),
                "exact" | "dispatch" | "name_match"
            )
        }) {
            rows.edges.remove(&ordinary);
        }
        let status = if resolution.targets.is_empty() {
            "unresolved"
        } else {
            "resolved"
        };
        if let Some(reference) = rows.refs.get_mut(&ref_id) {
            reference.status = status;
            reference.target_node = None;
            reference.target_file = None;
            reference.target_symbol = None;
        }
        for (target_file, target_symbol) in &resolution.protected {
            rows.unknown_live.insert((
                path.to_owned(),
                target_file.clone(),
                target_symbol.clone(),
                site.line,
            ));
        }
        let Some(caller) = site
            .caller
            .as_deref()
            .and_then(|symbol| by_scoped_name.get(&(path, symbol)))
        else {
            continue;
        };
        for target in &resolution.targets {
            let edge_id = format!(
                "dispatch:{path}:{}:{}:{}",
                site.ordinal, target.file, target.symbol
            );
            rows.edge_sequence
                .insert(edge_id.clone(), rows.edge_sequence.len() as u64);
            rows.edges.insert(
                edge_id.clone(),
                EdgeRow {
                    edge_id,
                    ref_id: ref_id.clone(),
                    source_node: (*caller).to_string(),
                    target_node: targets.scoped(&target.file, &target.symbol),
                    target_file: target.file.clone(),
                    target_symbol: target.symbol.clone(),
                    kind: "call",
                    line: site.line,
                    provenance: target.provenance.to_string(),
                },
            );
        }
    }
    let mut order = nodes
        .rows
        .iter()
        .map(|(id, (sequence, _))| (*sequence, id.clone()))
        .collect::<Vec<_>>();
    order.sort_unstable();
    rows.node_order = order.into_iter().map(|(_, id)| id).collect();
    rows.nodes = nodes
        .rows
        .into_iter()
        .map(|(id, (_, row))| (id, row))
        .collect();
    rows
}
