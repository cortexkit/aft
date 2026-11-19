//! The rows a resolved checkout's call graph consists of: files, symbols
//! (nodes), references, edges, method-call sites and unknown-receiver
//! liveness.
//!
//! They are assembled from each file's parse and resolution by the rules the
//! views call-graph database was written with, so the two can be compared
//! row for row. Node ids come from the target file's own symbols at assembly
//! time; a file's resolution stores only target paths and symbol names.

use std::collections::{BTreeMap, HashMap};

use crate::callgraph_store::join::{BlobRef, BlobRefKind, ParseBlob};

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
    /// Whether the program starts here, as the language defines it (see
    /// `SliceNode::program_start`). Dead code does not read it.
    pub(crate) program_start: bool,
    /// The entry attribute extraction recorded on the symbol.
    pub(crate) entry_attribute: Option<String>,
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

/// The nodes table, with the rules a SQLite `INSERT OR REPLACE` applies: a
/// row replaces any row with its id or with its file and source range, and
/// a replacement is inserted last.
#[derive(Default)]
pub(crate) struct Nodes {
    pub(crate) rows: BTreeMap<String, (u64, NodeRow)>,
    by_range: HashMap<(String, u32, u32, u32, u32, u32), String>,
    next: u64,
}

impl Nodes {
    pub(crate) fn insert(&mut self, row: NodeRow) {
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

    /// `SELECT id FROM nodes WHERE file_path=? AND scoped_name=?`: the
    /// earliest inserted row that is still present.
    pub(crate) fn by_scoped_name(&self) -> HashMap<(&str, &str), &str> {
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

pub(crate) const fn ref_kind(kind: BlobRefKind) -> &'static str {
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
