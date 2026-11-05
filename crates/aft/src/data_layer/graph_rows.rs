//! The rows a resolved checkout's call graph consists of: files, symbols
//! (nodes), references, edges, method-call sites and unknown-receiver
//! liveness.
//!
//! They are assembled from each file's parse and resolution by the rules the
//! views call-graph database was written with, so the two can be compared
//! row for row. Node ids come from the target file's own symbols at assembly
//! time; a file's resolution stores only target paths and symbol names.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::callgraph::{CheckoutInputs, FileResolution};
use crate::callgraph_store::join::{BlobRef, BlobRefKind, ParseBlob, ResolutionStatus};
use crate::callgraph_store::{MACRO_BODY_CALL_REF_KIND, PROVENANCE_TREESITTER};

/// One symbol.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct NodeRow {
    pub(crate) id: String,
    pub(crate) file_path: String,
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
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct RefRow {
    pub(crate) ref_id: String,
    pub(crate) caller_node: Option<String>,
    pub(crate) caller_file: String,
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
    pub(crate) status: &'static str,
    pub(crate) target_node: Option<String>,
    pub(crate) target_file: Option<String>,
    pub(crate) target_symbol: Option<String>,
}

/// One edge.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct EdgeRow {
    pub(crate) edge_id: String,
    pub(crate) ref_id: String,
    pub(crate) source_node: String,
    pub(crate) target_node: Option<String>,
    pub(crate) target_file: String,
    pub(crate) target_symbol: String,
    pub(crate) kind: &'static str,
    pub(crate) line: u32,
    pub(crate) provenance: String,
}

/// One method-call site's counts.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct SiteRow {
    pub(crate) caller_file: String,
    pub(crate) ordinal: u32,
    pub(crate) language: String,
    pub(crate) unresolved: usize,
    pub(crate) dynamic: usize,
    pub(crate) external: usize,
}

/// Every row of a checkout's call graph.
#[derive(Debug, Default)]
pub(crate) struct GraphRows {
    /// Path, record key and language of each parsed file.
    pub(crate) files: BTreeMap<String, (String, String)>,
    pub(crate) nodes: BTreeMap<String, NodeRow>,
    /// Node ids in the order the database stores them: insertion order,
    /// with a replaced row moved to the end.
    pub(crate) node_order: Vec<String>,
    /// Each edge's place in insertion order; a query that joins a reference
    /// to its edges reads them in this order.
    pub(crate) edge_sequence: HashMap<String, u64>,
    pub(crate) refs: BTreeMap<String, RefRow>,
    pub(crate) edges: BTreeMap<String, EdgeRow>,
    pub(crate) sites: BTreeMap<(String, u32), SiteRow>,
    /// Members an unknown receiver keeps alive: caller file, target file,
    /// target symbol.
    pub(crate) unknown_live: BTreeSet<(String, String, String)>,
}

/// The nodes table, with the rules a SQLite `INSERT OR REPLACE` applies: a
/// row replaces any row with its id or with its file and source range, and
/// a replacement is inserted last.
#[derive(Default)]
struct Nodes {
    rows: BTreeMap<String, (u64, NodeRow)>,
    by_range: HashMap<(String, u32, u32, u32, u32, u32), String>,
    next: u64,
}

impl Nodes {
    fn insert(&mut self, row: NodeRow) {
        let range = (
            row.file_path.clone(),
            row.start_line,
            row.start_col,
            row.end_line,
            row.end_col,
            row.range_ordinal,
        );
        if let Some((_, old)) = self.rows.remove(&row.id) {
            self.by_range.remove(&(
                old.file_path,
                old.start_line,
                old.start_col,
                old.end_line,
                old.end_col,
                old.range_ordinal,
            ));
        }
        if let Some(other) = self.by_range.remove(&range) {
            self.rows.remove(&other);
        }
        self.by_range.insert(range, row.id.clone());
        self.rows.insert(row.id.clone(), (self.next, row));
        self.next += 1;
    }

    fn kind(&self, id: &str) -> Option<&str> {
        self.rows.get(id).map(|(_, row)| row.kind.as_str())
    }

    /// `SELECT id FROM nodes WHERE file_path=? AND scoped_name=?`: the
    /// earliest inserted row that is still present.
    fn by_scoped_name(&self) -> HashMap<(&str, &str), &str> {
        let mut first = HashMap::<(&str, &str), (u64, &str)>::new();
        for (sequence, row) in self.rows.values() {
            let key = (row.file_path.as_str(), row.scoped_name.as_str());
            match first.get(&key) {
                Some((earlier, _)) if earlier < sequence => {}
                _ => {
                    first.insert(key, (*sequence, row.id.as_str()));
                }
            }
        }
        first.into_iter().map(|(key, (_, id))| (key, id)).collect()
    }
}

/// Assembles the rows of a whole checkout.
pub(crate) fn assemble(
    inputs: &CheckoutInputs,
    resolved: &BTreeMap<String, FileResolution>,
) -> GraphRows {
    let mut rows = GraphRows::default();
    let mut nodes = Nodes::default();
    // Per file: scoped name to node id (a later symbol wins), then plain
    // name to node id (the first symbol wins).
    let mut lookup = HashMap::<&str, HashMap<String, String>>::new();
    for (path, (key, parse)) in &inputs.parses {
        rows.files
            .insert(path.clone(), (key.clone(), parse.language.clone()));
        let file_nodes = lookup.entry(path.as_str()).or_default();
        for symbol in &parse.symbols {
            let id = crate::callgraph_store::view_node_id(path, &symbol.kind, &symbol.scoped_name);
            nodes.insert(NodeRow {
                id: id.clone(),
                file_path: path.clone(),
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
            });
            file_nodes.insert(symbol.scoped_name.clone(), id.clone());
            file_nodes.entry(symbol.name.clone()).or_insert(id);
        }
    }

    // References and their ordinary edges, in the order the join's rows
    // sort.
    let references = inputs
        .parses
        .iter()
        .map(|(path, (_, parse))| (path.as_str(), references_by_key(parse)))
        .collect::<HashMap<_, _>>();
    let mut derived = resolved
        .values()
        .flat_map(|file| file.rows.iter())
        .collect::<Vec<_>>();
    derived.sort();
    for row in derived {
        let Ok(caller_path) = std::str::from_utf8(&row.caller_path) else {
            continue;
        };
        let Some(reference) = references
            .get(caller_path)
            .and_then(|by_key| by_key.get(&row.ref_ordinal))
        else {
            continue;
        };
        let target_path = row
            .target_path
            .as_ref()
            .and_then(|path| String::from_utf8(path.clone()).ok());
        let caller_node = reference
            .caller_symbol
            .as_ref()
            .and_then(|symbol| lookup.get(caller_path)?.get(symbol))
            .cloned();
        let target_node = target_path
            .as_deref()
            .zip(row.target_symbol.as_deref())
            .and_then(|(path, symbol)| lookup.get(path)?.get(symbol))
            .cloned();
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
                        .and_then(|id| nodes.kind(id))
                        .is_some_and(|kind| matches!(kind, "function" | "method")) =>
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

    // Method-call sites, file by file in path order, sites in extraction
    // order: each replaces its reference's ordinary edge and target.
    let by_scoped_name = nodes.by_scoped_name();
    for (path, file) in resolved {
        let language = &inputs.parses[path].1.language;
        for kept in &file.sites {
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
            rows.sites.insert(
                (path.clone(), site.ordinal),
                SiteRow {
                    caller_file: path.clone(),
                    ordinal: site.ordinal,
                    language: language.clone(),
                    unresolved: resolution.unresolved,
                    dynamic: resolution.dynamic,
                    external: resolution.external,
                },
            );
            for (target_file, target_symbol) in &resolution.protected {
                rows.unknown_live.insert((
                    path.clone(),
                    target_file.clone(),
                    target_symbol.clone(),
                ));
            }
            let Some(caller) = site
                .caller
                .as_deref()
                .and_then(|symbol| by_scoped_name.get(&(path.as_str(), symbol)))
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
                        target_node: by_scoped_name
                            .get(&(target.file.as_str(), target.symbol.as_str()))
                            .map(|id| (*id).to_string()),
                        target_file: target.file.clone(),
                        target_symbol: target.symbol.clone(),
                        kind: "call",
                        line: site.line,
                        provenance: target.provenance.to_string(),
                    },
                );
            }
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

/// Each reference of a parse by its reference key; the later of two
/// references sharing a key wins.
pub(crate) fn references_by_key(parse: &ParseBlob) -> HashMap<u32, &BlobRef> {
    parse
        .reference_keys()
        .into_iter()
        .zip(&parse.refs)
        .collect()
}

/// Whether a reference of `kind` is a call written directly in a
/// `macro_rules!` template, outside any item the template defines: its
/// caller is the macro itself, named `name!`. The views database stored
/// such a call as a `macro_body_call` reference that names the macro in
/// `local_name`, with an edge of the same kind. Caller queries leave that
/// edge out; the callers command lists the template and every invocation of
/// the macro instead, by name.
pub(crate) fn macro_body_call(kind: BlobRefKind, reference: &BlobRef) -> bool {
    kind == BlobRefKind::Call
        && reference
            .caller_symbol
            .as_deref()
            .is_some_and(|caller| caller.ends_with('!'))
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
        BlobRefKind::MacroMention => crate::callgraph_store::MACRO_MENTION_REF_KIND,
    }
}
