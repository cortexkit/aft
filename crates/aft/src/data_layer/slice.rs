//! One file's part of a checkout's call graph, assembled from its content
//! record and its fragment: its symbols (nodes), its references, and the
//! edges they make.
//!
//! Every row of the graph belongs to exactly one file: nodes to the file
//! that declares them, references and edges to the file that makes the call.
//! So a checkout's graph is the union of its files' slices, and an edit
//! replaces whole slices. What a row needs from another file (the target's
//! node id, and for a construction whether its target is a function or
//! method) is looked up in that file's slice when a query reads the row,
//! never stored in the caller's slice.
//!
//! The rows follow the rules the views database was written with, so
//! queries over slices answer exactly what queries over that database did.

use std::collections::{BTreeMap, HashMap};

use super::callgraph::FileResolution;
use super::graph_rows::{macro_body_call, references_by_key};
use crate::callgraph_store::join::{BlobRefKind, ParseBlob, ResolutionStatus};
use crate::callgraph_store::MACRO_BODY_CALL_REF_KIND;

/// One symbol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SliceNode {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) scoped_name: String,
    pub(crate) kind: String,
    pub(crate) start_line: u32,
    pub(crate) start_col: u32,
    pub(crate) end_line: u32,
    pub(crate) end_col: u32,
    pub(crate) range_ordinal: u32,
    pub(crate) signature: Option<String>,
    pub(crate) exported: bool,
    pub(crate) is_default_export: bool,
    pub(crate) entry_point: bool,
    pub(crate) is_test: Option<bool>,
}

/// One reference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SliceRef {
    pub(crate) ref_id: String,
    /// The reference's ordinal in its file, the order it was written in.
    pub(crate) ordinal: u32,
    pub(crate) caller_node: Option<String>,
    pub(crate) kind: &'static str,
    pub(crate) short_name: Option<String>,
    pub(crate) full_ref: Option<String>,
    pub(crate) module_path: Option<String>,
    pub(crate) import_kind: Option<String>,
    pub(crate) local_name: Option<String>,
    pub(crate) requested_name: Option<String>,
    pub(crate) namespace_alias: Option<String>,
    pub(crate) wildcard: bool,
    pub(crate) line: u32,
    pub(crate) byte_start: u64,
    pub(crate) byte_end: u64,
    pub(crate) resolved: bool,
    /// The target, unless a method-call site replaced the reference's
    /// ordinary resolution.
    pub(crate) target: Option<(String, String)>,
}

/// How an edge's target node is found in the target file's slice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TargetLookup {
    /// By scoped name, else by plain name, as ordinary resolution links.
    Ordinary,
    /// By scoped name only, the first such symbol, as method-call sites
    /// link.
    Scoped,
}

/// One edge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SliceEdge {
    pub(crate) edge_id: String,
    pub(crate) ref_id: String,
    pub(crate) source_node: String,
    pub(crate) target_file: String,
    pub(crate) target_symbol: String,
    /// `call`, `callback_ref` or `macro_body_call`; for a construction,
    /// `construction`, which a query reads as `call` when the target is a
    /// function or method and as `value_ref` otherwise.
    pub(crate) kind: &'static str,
    pub(crate) line: u32,
    pub(crate) provenance: &'static str,
    pub(crate) lookup: TargetLookup,
}

/// A file's slice of the call graph.
#[derive(Clone, Debug, Default)]
pub(crate) struct FileSlice {
    pub(crate) path: String,
    pub(crate) language: String,
    pub(crate) record_key: String,
    /// Symbols that survive the database's replacement rules, in the order
    /// they were inserted.
    pub(crate) nodes: Vec<SliceNode>,
    /// References, by reference id.
    pub(crate) refs: BTreeMap<String, SliceRef>,
    /// Edges, by edge id.
    pub(crate) edges: BTreeMap<String, SliceEdge>,
    /// Scoped name, else plain name, to node id, as ordinary resolution
    /// links.
    ordinary: HashMap<String, String>,
    /// Scoped name to the first surviving node id with it.
    scoped: HashMap<String, String>,
    kinds: HashMap<String, String>,
    /// Node id to its position in `nodes`.
    node_index: HashMap<String, usize>,
}

impl FileSlice {
    /// The node id a row targeting `symbol` in this file links to.
    pub(crate) fn target_node(&self, symbol: &str, lookup: TargetLookup) -> Option<&str> {
        match lookup {
            TargetLookup::Ordinary => self.ordinary.get(symbol),
            TargetLookup::Scoped => self.scoped.get(symbol),
        }
        .map(String::as_str)
    }

    /// The kind of the node with `id` in this file.
    pub(crate) fn node_kind(&self, id: &str) -> Option<&str> {
        self.kinds.get(id).map(String::as_str)
    }

    /// The node with `id`.
    pub(crate) fn node(&self, id: &str) -> Option<&SliceNode> {
        self.node_index.get(id).map(|index| &self.nodes[*index])
    }

    /// Assembles the slice of the file at `path`.
    #[must_use]
    pub(crate) fn build(
        path: &str,
        record_key: &str,
        parse: &ParseBlob,
        resolution: &FileResolution,
    ) -> Self {
        let mut slice = Self {
            path: path.to_string(),
            language: parse.language.clone(),
            record_key: record_key.to_string(),
            ..Self::default()
        };
        slice.insert_nodes(path, parse);
        let references = references_by_key(parse);
        let mut rows = resolution.rows.iter().collect::<Vec<_>>();
        rows.sort();
        for row in rows {
            let Some(reference) = references.get(&row.ref_ordinal) else {
                continue;
            };
            let ref_id = format!("view:{path}:{}", row.ref_ordinal);
            let macro_body = macro_body_call(row.kind, reference);
            let caller_node = reference
                .caller_symbol
                .as_ref()
                .and_then(|symbol| slice.ordinary.get(symbol))
                .cloned();
            let target = row
                .target_path
                .as_ref()
                .and_then(|target| String::from_utf8(target.clone()).ok())
                .zip(row.target_symbol.clone());
            if let (Some(source_node), Some((target_file, target_symbol)), true) = (
                caller_node.clone(),
                target.clone(),
                matches!(
                    row.kind,
                    BlobRefKind::Call | BlobRefKind::Construction | BlobRefKind::CallbackRef
                ),
            ) {
                let edge_id = format!("edge:{ref_id}");
                slice.edges.insert(
                    edge_id.clone(),
                    SliceEdge {
                        edge_id,
                        ref_id: ref_id.clone(),
                        source_node,
                        target_file,
                        target_symbol,
                        kind: match row.kind {
                            BlobRefKind::Construction => "construction",
                            BlobRefKind::CallbackRef => "callback_ref",
                            _ if macro_body => MACRO_BODY_CALL_REF_KIND,
                            _ => "call",
                        },
                        line: reference.line,
                        provenance: crate::callgraph_store::PROVENANCE_TREESITTER,
                        lookup: TargetLookup::Ordinary,
                    },
                );
            }
            slice.refs.insert(
                ref_id.clone(),
                SliceRef {
                    ref_id,
                    ordinal: row.ref_ordinal,
                    caller_node,
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
                    resolved: row.status == ResolutionStatus::Resolved,
                    target,
                },
            );
        }
        // Method-call sites replace their reference's ordinary edge and
        // target with the dispatch pass's.
        for kept in &resolution.sites {
            let site = &kept.site;
            let resolution = &kept.resolution;
            let ref_id = format!("view:{path}:{}", site.ordinal);
            slice.edges.remove(&format!("edge:{ref_id}"));
            if let Some(reference) = slice.refs.get_mut(&ref_id) {
                reference.resolved = !resolution.targets.is_empty();
                reference.target = None;
            }
            let Some(caller) = site
                .caller
                .as_deref()
                .and_then(|symbol| slice.scoped.get(symbol))
                .cloned()
            else {
                continue;
            };
            for target in &resolution.targets {
                let edge_id = format!(
                    "dispatch:{path}:{}:{}:{}",
                    site.ordinal, target.file, target.symbol
                );
                slice.edges.insert(
                    edge_id.clone(),
                    SliceEdge {
                        edge_id,
                        ref_id: ref_id.clone(),
                        source_node: caller.clone(),
                        target_file: target.file.clone(),
                        target_symbol: target.symbol.clone(),
                        kind: "call",
                        line: site.line,
                        provenance: target.provenance,
                        lookup: TargetLookup::Scoped,
                    },
                );
            }
        }
        slice
    }

    /// Inserts the file's symbols with the database's replacement rules: a
    /// symbol replaces any earlier one with its id or with its source range,
    /// and the replacement counts as inserted last.
    fn insert_nodes(&mut self, path: &str, parse: &ParseBlob) {
        let mut rows = BTreeMap::<String, (u64, SliceNode)>::new();
        let mut by_range = HashMap::<(u32, u32, u32, u32, u32), String>::new();
        for (sequence, symbol) in (0u64..).zip(&parse.symbols) {
            let id = crate::callgraph_store::view_node_id(path, &symbol.kind, &symbol.scoped_name);
            let node = SliceNode {
                id: id.clone(),
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
            };
            let range = (
                node.start_line,
                node.start_col,
                node.end_line,
                node.end_col,
                node.range_ordinal,
            );
            if let Some((_, old)) = rows.remove(&id) {
                by_range.remove(&(
                    old.start_line,
                    old.start_col,
                    old.end_line,
                    old.end_col,
                    old.range_ordinal,
                ));
            }
            if let Some(other) = by_range.remove(&range) {
                rows.remove(&other);
            }
            by_range.insert(range, id.clone());
            rows.insert(id.clone(), (sequence, node));
            // Ordinary resolution links through every symbol as written,
            // whether or not the database kept its row.
            self.ordinary.insert(symbol.scoped_name.clone(), id.clone());
            self.ordinary.entry(symbol.name.clone()).or_insert(id);
        }
        let mut surviving = rows.into_values().collect::<Vec<_>>();
        surviving.sort_by_key(|(sequence, _)| *sequence);
        for (_, node) in surviving {
            self.scoped
                .entry(node.scoped_name.clone())
                .or_insert_with(|| node.id.clone());
            self.kinds.insert(node.id.clone(), node.kind.clone());
            // A later duplicate id would not be found by the scan this
            // replaces, which returned the first; keep the first here too.
            self.node_index
                .entry(node.id.clone())
                .or_insert(self.nodes.len());
            self.nodes.push(node);
        }
    }
}

const fn ref_kind(kind: BlobRefKind) -> &'static str {
    match kind {
        BlobRefKind::Call => "call",
        BlobRefKind::ValueRef => "value_ref",
        BlobRefKind::Construction => "construction",
        BlobRefKind::CallbackRef => "callback_ref",
        BlobRefKind::Import => "import",
        BlobRefKind::Module => "module",
        BlobRefKind::Reexport => "reexport",
        BlobRefKind::ExportAlias => "export_alias",
    }
}
