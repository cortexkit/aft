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
//!
//! A slice is held compactly: a whole repository's graph is decoded at once
//! by commands that walk it, so the rows must cost bytes, not hundreds of
//! bytes. Every string a row names is stored once in the slice's sorted
//! string table, and rows hold indexes into it. Rows are columns of
//! integers, each column bit-packed to the width its largest value needs.
//! Ids (`view:<path>:<ordinal>` for references, the node ids) are not
//! stored as text; they are formatted from the path and integers when a
//! reply needs them. Readers go through the accessors and the small view
//! structs ([`NodeView`], [`RefView`], [`EdgeView`]).
//!
//! The slice is also stored, as a record next to the file's fragment
//! ([`FileSlice::encode`], [`FileSlice::decode`]), so reading a file's
//! slice does not have to read and convert its content record.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;

use super::callgraph::FileResolution;
use super::graph_rows::{macro_body_call, references_by_key};
use crate::callgraph_store::join::{BlobRefKind, BlobSymbol, ParseBlob, ResolutionStatus};
use crate::callgraph_store::{
    MACRO_BODY_CALL_REF_KIND, MACRO_MENTION_REF_KIND, PROVENANCE_NAME_MATCH, PROVENANCE_TREESITTER,
    PROVENANCE_TYPE_MATCH,
};

/// A node id as the views store spells it: `vp` and 16 lowercase hex
/// digits for a symbol, `vt` and 16 for a file's top level
/// ([`crate::callgraph_store::view_node_id`]). Ordered as the spelled ids
/// sort: every symbol id before every top-level id, then by digest, whose
/// fixed-width lowercase hex sorts as the number does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct NodeId {
    top: bool,
    digest: u64,
}

impl NodeId {
    /// The id `text` spells, or `None` when it is not a views node id.
    #[must_use]
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let top = match text.get(..2)? {
            "vp" => false,
            "vt" => true,
            _ => return None,
        };
        let hex = &text[2..];
        // `from_str_radix` would also take uppercase digits and a sign,
        // which no id contains; only the exact spelling round-trips.
        if hex.len() != 16 || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return None;
        }
        Some(Self {
            top,
            digest: u64::from_str_radix(hex, 16).ok()?,
        })
    }

    /// The id of the symbol `scoped_name` of `kind` in the file at `path`.
    fn of(path: &str, kind: &str, scoped_name: &str) -> Self {
        let id = crate::callgraph_store::view_node_id(path, kind, scoped_name);
        Self::parse(&id).expect("view_node_id spells a views node id")
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let class = if self.top { "vt" } else { "vp" };
        write!(f, "{class}{:016x}", self.digest)
    }
}

/// A node of a slice by position: below the slice's node count, a symbol
/// that survives in the file (its index in the node columns); at or above
/// it, an id that ordinary resolution links to although no surviving symbol
/// has it (a symbol another one replaced). Such an id is still a row's
/// caller or target, as it was in the views database, but has no node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct NodeHandle(u32);

/// How an edge's target node is found in the target file's slice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TargetLookup {
    /// By scoped name, else by plain name, as ordinary resolution links.
    Ordinary,
    /// By scoped name only, the first such symbol, as method-call sites
    /// link.
    Scoped,
}

/// One column of unsigned integers, each stored in `width` bits, the
/// fewest that hold the column's largest value, packed into words.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Packed {
    len: u32,
    width: u8,
    words: Vec<u64>,
}

/// Words that hold `len` values of `width` bits.
fn words_for(len: usize, width: u8) -> usize {
    (len * usize::from(width)).div_ceil(64)
}

impl Packed {
    fn of(values: &[u64]) -> Self {
        let largest = values.iter().copied().max().unwrap_or(0);
        let width = u8::try_from(64 - largest.leading_zeros()).expect("at most 64 bits");
        let mut words = vec![0u64; words_for(values.len(), width)];
        let bits = usize::from(width);
        if bits > 0 {
            for (index, value) in values.iter().enumerate() {
                let bit = index * bits;
                let (word, shift) = (bit / 64, bit % 64);
                words[word] |= value << shift;
                if shift + bits > 64 {
                    words[word + 1] |= value >> (64 - shift);
                }
            }
        }
        Self {
            len: u32::try_from(values.len()).expect("fewer than 2^32 rows in a file"),
            width,
            words,
        }
    }

    fn len(&self) -> usize {
        self.len as usize
    }

    #[inline]
    fn get(&self, index: usize) -> u64 {
        debug_assert!(index < self.len());
        let bits = usize::from(self.width);
        if bits == 0 {
            return 0;
        }
        let bit = index * bits;
        let (word, shift) = (bit / 64, bit % 64);
        let mut value = self.words[word] >> shift;
        if shift + bits > 64 {
            value |= self.words[word + 1] << (64 - shift);
        }
        if bits == 64 {
            value
        } else {
            value & ((1u64 << bits) - 1)
        }
    }

    /// A value the column holds as a `u32`: an index or a position, which
    /// assembly stored and decoding checked.
    #[inline]
    fn get32(&self, index: usize) -> u32 {
        u32::try_from(self.get(index)).expect("a 32-bit column holds 32-bit values")
    }

    /// An optional value: stored as 0 for none, otherwise the value plus
    /// one.
    #[inline]
    fn get_option(&self, index: usize) -> Option<u32> {
        match self.get(index) {
            0 => None,
            value => {
                Some(u32::try_from(value - 1).expect("an optional column holds 32-bit values"))
            }
        }
    }

    fn values(&self) -> impl Iterator<Item = u64> + '_ {
        (0..self.len()).map(|index| self.get(index))
    }

    /// The position of `key` in this column, which holds keys in ascending
    /// order without repeats.
    fn search(&self, key: u64) -> Option<usize> {
        let (mut low, mut high) = (0, self.len());
        while low < high {
            let middle = low + (high - low) / 2;
            match self.get(middle).cmp(&key) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => return Some(middle),
            }
        }
        None
    }
}

/// Every distinct string a slice's rows name, once, in ascending order, so
/// a string's index orders as the string does and a lookup by text is a
/// binary search.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Strings {
    text: String,
    /// Where each string ends in `text`.
    ends: Packed,
}

impl Strings {
    fn of(sorted: &[&str]) -> Self {
        let mut text = String::with_capacity(sorted.iter().map(|s| s.len()).sum());
        let mut ends = Vec::with_capacity(sorted.len());
        for string in sorted {
            text.push_str(string);
            ends.push(text.len() as u64);
        }
        Self {
            text,
            ends: Packed::of(&ends),
        }
    }

    fn len(&self) -> usize {
        self.ends.len()
    }

    #[inline]
    fn get(&self, index: u32) -> &str {
        let index = index as usize;
        let start = if index == 0 {
            0
        } else {
            self.ends.get(index - 1) as usize
        };
        &self.text[start..self.ends.get(index) as usize]
    }

    fn find(&self, value: &str) -> Option<u32> {
        let (mut low, mut high) = (0u32, u32::try_from(self.len()).ok()?);
        while low < high {
            let middle = low + (high - low) / 2;
            match self.get(middle).cmp(value) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => return Some(middle),
            }
        }
        None
    }
}

/// Reference kinds by their code in a slice.
const REF_KINDS: [&str; 10] = [
    "call",
    "value_ref",
    "construction",
    "callback_ref",
    "import",
    "module",
    "reexport",
    "export_alias",
    MACRO_MENTION_REF_KIND,
    MACRO_BODY_CALL_REF_KIND,
];

/// Edge kinds by their code in a slice: `call`, `callback_ref` or
/// `macro_body_call`; for a construction, `construction`, which a query
/// reads as `call` when the target is a function or method and as
/// `value_ref` otherwise.
const EDGE_KINDS: [&str; 4] = [
    "call",
    "callback_ref",
    MACRO_BODY_CALL_REF_KIND,
    "construction",
];

/// Edge provenances by their code in a slice: ordinary resolution's, then
/// the method-call resolver's (`fragment::PROVENANCES` lists the same).
const PROVENANCES: [&str; 5] = [
    PROVENANCE_TREESITTER,
    "exact",
    "dispatch",
    PROVENANCE_NAME_MATCH,
    PROVENANCE_TYPE_MATCH,
];

fn code_of(table: &[&str], value: &str) -> u64 {
    table
        .iter()
        .position(|known| *known == value)
        .map(|code| code as u64)
        .unwrap_or_else(|| panic!("{value} is not listed among a slice's codes"))
}

const EXPORTED: u64 = 1;
const DEFAULT_EXPORT: u64 = 1 << 1;
const ENTRY_POINT: u64 = 1 << 2;
const TEST_KNOWN: u64 = 1 << 3;
const TEST: u64 = 1 << 4;
const PROGRAM_START: u64 = 1 << 5;

const RESOLVED: u64 = 1 << 4;
const SCOPED_LOOKUP: u64 = 1 << 2;

/// A slice's symbols that survive the database's replacement rules, in the
/// order they were inserted. Strings are indexes into the string table.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Nodes {
    name: Packed,
    scoped_name: Packed,
    kind: Packed,
    signature: Packed,
    entry_attribute: Packed,
    start_line: Packed,
    start_col: Packed,
    end_line: Packed,
    range_ordinal: Packed,
    flags: Packed,
}

/// A slice's references, in the order of their ids
/// (`view:<path>:<ordinal>`), as the views database listed them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Refs {
    ordinal: Packed,
    /// The caller's node handle, optional.
    caller: Packed,
    short_name: Packed,
    full_ref: Packed,
    line: Packed,
    byte_start: Packed,
    byte_end: Packed,
    /// The kind's code, and [`RESOLVED`].
    code: Packed,
    /// The few references with a local name: their positions, ascending,
    /// and the names.
    local_at: Packed,
    local_name: Packed,
    /// The references with a target: their positions, ascending, and the
    /// target's file and symbol.
    target_at: Packed,
    target_file: Packed,
    target_symbol: Packed,
}

/// A slice's edges, in the order of their ids, as the views database
/// listed them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Edges {
    /// The position of the edge's reference, optional: a method-call site's
    /// edges name their site's reference even when the file has no
    /// reference with that ordinal.
    reference: Packed,
    source: Packed,
    target_file: Packed,
    target_symbol: Packed,
    line: Packed,
    /// The kind's code, [`SCOPED_LOOKUP`], and the provenance's code above
    /// those.
    code: Packed,
}

/// A file's slice of the call graph.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct FileSlice {
    path: String,
    language: String,
    record_key: String,
    strings: Strings,
    /// The id of every node handle: the surviving nodes', then the ids
    /// without a node.
    ids: Vec<u64>,
    /// 1 where the id at that handle is a top-level id.
    id_top: Packed,
    nodes: Nodes,
    refs: Refs,
    edges: Edges,
    /// Scoped name, else plain name, to node handle, as ordinary
    /// resolution links: string indexes ascending, and their handles.
    ordinary_name: Packed,
    ordinary_node: Packed,
    /// Scoped name to the first surviving node with it.
    scoped_name: Packed,
    scoped_node: Packed,
}

/// One surviving symbol of a slice.
#[derive(Clone, Copy)]
pub(crate) struct NodeView<'s> {
    slice: &'s FileSlice,
    index: usize,
}

/// One reference of a slice.
#[derive(Clone, Copy)]
pub(crate) struct RefView<'s> {
    slice: &'s FileSlice,
    index: usize,
}

/// One edge of a slice.
#[derive(Clone, Copy)]
pub(crate) struct EdgeView<'s> {
    slice: &'s FileSlice,
    index: usize,
}

impl<'s> NodeView<'s> {
    fn string(&self, column: &Packed) -> &'s str {
        self.slice.strings.get(column.get32(self.index))
    }

    fn flag(&self, flag: u64) -> bool {
        self.slice.nodes.flags.get(self.index) & flag != 0
    }

    pub(crate) fn handle(&self) -> NodeHandle {
        NodeHandle(u32::try_from(self.index).expect("fewer than 2^32 nodes"))
    }

    pub(crate) fn id(&self) -> NodeId {
        self.slice.node_id(self.handle())
    }

    pub(crate) fn name(&self) -> &'s str {
        self.string(&self.slice.nodes.name)
    }

    pub(crate) fn scoped_name(&self) -> &'s str {
        self.string(&self.slice.nodes.scoped_name)
    }

    pub(crate) fn kind(&self) -> &'s str {
        self.string(&self.slice.nodes.kind)
    }

    pub(crate) fn signature(&self) -> Option<&'s str> {
        self.slice
            .nodes
            .signature
            .get_option(self.index)
            .map(|index| self.slice.strings.get(index))
    }

    pub(crate) fn entry_attribute(&self) -> Option<&'s str> {
        self.slice
            .nodes
            .entry_attribute
            .get_option(self.index)
            .map(|index| self.slice.strings.get(index))
    }

    pub(crate) fn start_line(&self) -> u32 {
        self.slice.nodes.start_line.get32(self.index)
    }

    pub(crate) fn start_col(&self) -> u32 {
        self.slice.nodes.start_col.get32(self.index)
    }

    pub(crate) fn end_line(&self) -> u32 {
        self.slice.nodes.end_line.get32(self.index)
    }

    pub(crate) fn range_ordinal(&self) -> u32 {
        self.slice.nodes.range_ordinal.get32(self.index)
    }

    pub(crate) fn exported(&self) -> bool {
        self.flag(EXPORTED)
    }

    pub(crate) fn is_default_export(&self) -> bool {
        self.flag(DEFAULT_EXPORT)
    }

    pub(crate) fn entry_point(&self) -> bool {
        self.flag(ENTRY_POINT)
    }

    pub(crate) fn is_test(&self) -> Option<bool> {
        self.flag(TEST_KNOWN).then(|| self.flag(TEST))
    }

    /// Whether the program starts here, as the language defines it. For
    /// Rust this is `fn main` at the top of the file; whether the file is a
    /// binary target's root is checked when a root is labelled.
    pub(crate) fn program_start(&self) -> bool {
        self.flag(PROGRAM_START)
    }
}

impl<'s> RefView<'s> {
    fn optional_string(&self, column: &Packed) -> Option<&'s str> {
        column
            .get_option(self.index)
            .map(|index| self.slice.strings.get(index))
    }

    /// The reference's position in its slice.
    pub(crate) fn index(&self) -> u32 {
        u32::try_from(self.index).expect("fewer than 2^32 references")
    }

    /// The reference's ordinal in its file, the order it was written in.
    pub(crate) fn ordinal(&self) -> u32 {
        self.slice.refs.ordinal.get32(self.index)
    }

    /// `view:<path>:<ordinal>`, the reference's id in the views database.
    pub(crate) fn ref_id(&self) -> String {
        format!("view:{}:{}", self.slice.path, self.ordinal())
    }

    pub(crate) fn caller(&self) -> Option<NodeHandle> {
        self.slice
            .refs
            .caller
            .get_option(self.index)
            .map(NodeHandle)
    }

    pub(crate) fn kind(&self) -> &'static str {
        REF_KINDS[(self.slice.refs.code.get(self.index) & 0xF) as usize]
    }

    pub(crate) fn short_name(&self) -> Option<&'s str> {
        self.optional_string(&self.slice.refs.short_name)
    }

    /// The short name's index in the slice's string table, to compare with
    /// [`FileSlice::find_string`]'s.
    pub(crate) fn short_name_index(&self) -> Option<u32> {
        self.slice.refs.short_name.get_option(self.index)
    }

    pub(crate) fn full_ref(&self) -> Option<&'s str> {
        self.optional_string(&self.slice.refs.full_ref)
    }

    /// The name the reference binds locally; for a call written in a
    /// `macro_rules!` template, the macro.
    pub(crate) fn local_name(&self) -> Option<&'s str> {
        let refs = &self.slice.refs;
        refs.local_at
            .search(self.index as u64)
            .map(|at| self.slice.strings.get(refs.local_name.get32(at)))
    }

    pub(crate) fn line(&self) -> u32 {
        self.slice.refs.line.get32(self.index)
    }

    pub(crate) fn byte_start(&self) -> u64 {
        self.slice.refs.byte_start.get(self.index)
    }

    pub(crate) fn byte_end(&self) -> u64 {
        self.slice.refs.byte_end.get(self.index)
    }

    pub(crate) fn resolved(&self) -> bool {
        self.slice.refs.code.get(self.index) & RESOLVED != 0
    }

    /// The target's file and symbol, unless a method-call site replaced the
    /// reference's ordinary resolution.
    pub(crate) fn target(&self) -> Option<(&'s str, &'s str)> {
        let refs = &self.slice.refs;
        refs.target_at.search(self.index as u64).map(|at| {
            (
                self.slice.strings.get(refs.target_file.get32(at)),
                self.slice.strings.get(refs.target_symbol.get32(at)),
            )
        })
    }
}

impl<'s> EdgeView<'s> {
    /// The position of the edge's reference in the slice, when the file has
    /// that reference.
    pub(crate) fn reference(&self) -> Option<RefView<'s>> {
        self.slice
            .edges
            .reference
            .get_option(self.index)
            .map(|index| self.slice.reference(index as usize))
    }

    /// The reference position, as [`RefView::index`] gives it.
    pub(crate) fn reference_index(&self) -> Option<u32> {
        self.slice.edges.reference.get_option(self.index)
    }

    pub(crate) fn source(&self) -> NodeHandle {
        NodeHandle(self.slice.edges.source.get32(self.index))
    }

    pub(crate) fn target_file(&self) -> &'s str {
        self.slice
            .strings
            .get(self.slice.edges.target_file.get32(self.index))
    }

    pub(crate) fn target_symbol(&self) -> &'s str {
        self.slice
            .strings
            .get(self.slice.edges.target_symbol.get32(self.index))
    }

    /// Whether the edge targets `symbol` in `file`, with both given as
    /// indexes into this slice's string table ([`FileSlice::find_string`]).
    pub(crate) fn targets(&self, file: u32, symbol: u32) -> bool {
        self.slice.edges.target_file.get32(self.index) == file
            && self.slice.edges.target_symbol.get32(self.index) == symbol
    }

    pub(crate) fn kind(&self) -> &'static str {
        EDGE_KINDS[(self.slice.edges.code.get(self.index) & 0x3) as usize]
    }

    pub(crate) fn lookup(&self) -> TargetLookup {
        if self.slice.edges.code.get(self.index) & SCOPED_LOOKUP == 0 {
            TargetLookup::Ordinary
        } else {
            TargetLookup::Scoped
        }
    }

    pub(crate) fn provenance(&self) -> &'static str {
        PROVENANCES[(self.slice.edges.code.get(self.index) >> 3) as usize]
    }

    pub(crate) fn line(&self) -> u32 {
        self.slice.edges.line.get32(self.index)
    }
}

impl FileSlice {
    pub(crate) fn path(&self) -> &str {
        &self.path
    }

    pub(crate) fn language(&self) -> &str {
        &self.language
    }

    pub(crate) fn record_key(&self) -> &str {
        &self.record_key
    }

    /// The surviving symbols, in the order they were inserted.
    pub(crate) fn nodes(&self) -> impl ExactSizeIterator<Item = NodeView<'_>> + '_ {
        (0..self.nodes.name.len()).map(|index| NodeView { slice: self, index })
    }

    /// The references, in the order of their ids.
    pub(crate) fn references(&self) -> impl ExactSizeIterator<Item = RefView<'_>> + '_ {
        (0..self.refs.ordinal.len()).map(|index| RefView { slice: self, index })
    }

    /// The reference at `index`, a position [`RefView::index`] gave.
    pub(crate) fn reference(&self, index: usize) -> RefView<'_> {
        RefView { slice: self, index }
    }

    /// The edges, in the order of their ids.
    pub(crate) fn edges(&self) -> impl ExactSizeIterator<Item = EdgeView<'_>> + '_ {
        (0..self.edges.source.len()).map(|index| EdgeView { slice: self, index })
    }

    /// The index of `value` in this slice's string table, when a row names
    /// it. Rows that name a string hold this index, so a filter on a string
    /// compares integers once the index is found.
    pub(crate) fn find_string(&self, value: &str) -> Option<u32> {
        self.strings.find(value)
    }

    /// The surviving symbol at `handle`; `None` for an id without a node.
    pub(crate) fn node(&self, handle: NodeHandle) -> Option<NodeView<'_>> {
        let index = handle.0 as usize;
        (index < self.nodes.name.len()).then_some(NodeView { slice: self, index })
    }

    /// The id at `handle`.
    pub(crate) fn node_id(&self, handle: NodeHandle) -> NodeId {
        let index = handle.0 as usize;
        NodeId {
            top: self.id_top.get(index) != 0,
            digest: self.ids[index],
        }
    }

    /// The handle with the id `id` spells, when this slice has it.
    pub(crate) fn handle_of(&self, id: &str) -> Option<NodeHandle> {
        let id = NodeId::parse(id)?;
        (0..self.ids.len())
            .find(|index| self.ids[*index] == id.digest && (self.id_top.get(*index) != 0) == id.top)
            .map(|index| NodeHandle(u32::try_from(index).expect("fewer than 2^32 ids")))
    }

    /// The node a row targeting `symbol` in this file links to.
    pub(crate) fn target_node(&self, symbol: &str, lookup: TargetLookup) -> Option<NodeHandle> {
        let name = u64::from(self.strings.find(symbol)?);
        match lookup {
            TargetLookup::Ordinary => self
                .ordinary_name
                .search(name)
                .map(|at| NodeHandle(self.ordinary_node.get32(at))),
            TargetLookup::Scoped => self
                .scoped_name
                .search(name)
                .map(|at| NodeHandle(self.scoped_node.get32(at))),
        }
    }

    /// The kind of the surviving node at `handle`.
    pub(crate) fn node_kind(&self, handle: NodeHandle) -> Option<&str> {
        self.node(handle).map(|node| node.kind())
    }

    /// Heap bytes by part, for a measurement probe: the string table, nodes
    /// (with every id), references, edges, the lookup arrays, and counts of
    /// nodes, references, resolved references and edges.
    #[doc(hidden)]
    pub(crate) fn probe_breakdown(&self) -> [usize; 9] {
        use aft_store::heap::HeapBytes as _;
        let mut seen = aft_store::heap::Seen::new();
        [
            self.strings.heap_bytes(&mut seen),
            self.nodes.heap_bytes(&mut seen)
                + self.ids.heap_bytes(&mut seen)
                + self.id_top.heap_bytes(&mut seen),
            self.refs.heap_bytes(&mut seen),
            self.edges.heap_bytes(&mut seen),
            self.ordinary_name.heap_bytes(&mut seen)
                + self.ordinary_node.heap_bytes(&mut seen)
                + self.scoped_name.heap_bytes(&mut seen)
                + self.scoped_node.heap_bytes(&mut seen),
            self.nodes.name.len(),
            self.refs.ordinal.len(),
            self.references().filter(RefView::resolved).count(),
            self.edges.source.len(),
        ]
    }

    /// A slice of `path` with no symbols, references or edges.
    #[cfg(test)]
    pub(crate) fn empty_for_test(path: &str) -> Self {
        Self {
            path: path.to_string(),
            language: "rust".to_string(),
            ..Self::default()
        }
    }
}

/// A file's symbols that survive the database's replacement rules, with
/// their ids, in the order they were inserted: a symbol replaces any
/// earlier one with its id or with its source range, and the replacement
/// counts as inserted last. Fills `ordinary` with every symbol as written,
/// whether or not the database kept its row: ordinary resolution links
/// through them all.
fn surviving_nodes<'p>(
    path: &str,
    parse: &'p ParseBlob,
    ordinary: &mut HashMap<&'p str, NodeId>,
) -> Vec<(&'p BlobSymbol, NodeId)> {
    let mut rows = BTreeMap::<NodeId, (u64, &BlobSymbol)>::new();
    let mut by_range = HashMap::<(u32, u32, u32, u32, u32), NodeId>::new();
    let range = |symbol: &BlobSymbol| {
        (
            symbol.start_line,
            symbol.start_col,
            symbol.end_line,
            symbol.end_col,
            symbol.ordinal,
        )
    };
    for (sequence, symbol) in (0u64..).zip(&parse.symbols) {
        let id = NodeId::of(path, &symbol.kind, &symbol.scoped_name);
        if let Some((_, old)) = rows.remove(&id) {
            by_range.remove(&range(old));
        }
        if let Some(other) = by_range.remove(&range(symbol)) {
            rows.remove(&other);
        }
        by_range.insert(range(symbol), id);
        rows.insert(id, (sequence, symbol));
        ordinary.insert(&symbol.scoped_name, id);
        ordinary.entry(&symbol.name).or_insert(id);
    }
    let mut surviving = rows
        .into_iter()
        .map(|(id, (sequence, symbol))| (sequence, symbol, id))
        .collect::<Vec<_>>();
    surviving.sort_by_key(|(sequence, ..)| *sequence);
    surviving
        .into_iter()
        .map(|(_, symbol, id)| (symbol, id))
        .collect()
}

/// A reference as assembly collects it, before the slice is packed.
struct RefRow<'p> {
    ordinal: u32,
    caller: Option<NodeId>,
    kind: u64,
    short_name: Option<&'p str>,
    full_ref: Option<&'p str>,
    local_name: Option<&'p str>,
    line: u32,
    byte_start: u64,
    byte_end: u64,
    resolved: bool,
    target: Option<(String, String)>,
}

/// An edge as assembly collects it, before the slice is packed.
struct EdgeRow {
    ref_id: String,
    source: NodeId,
    target_file: String,
    target_symbol: String,
    kind: u64,
    line: u32,
    provenance: u64,
    lookup: TargetLookup,
}

const fn ref_kind_code(kind: BlobRefKind) -> u64 {
    match kind {
        BlobRefKind::Call => 0,
        BlobRefKind::ValueRef => 1,
        BlobRefKind::Construction => 2,
        BlobRefKind::CallbackRef => 3,
        BlobRefKind::Import => 4,
        BlobRefKind::Module => 5,
        BlobRefKind::Reexport => 6,
        BlobRefKind::ExportAlias => 7,
        BlobRefKind::MacroMention => 8,
    }
}

/// The code of `macro_body_call` in [`REF_KINDS`].
const MACRO_BODY_CALL_CODE: u64 = 9;

impl FileSlice {
    /// Assembles the slice of the file at `path`.
    #[must_use]
    pub(crate) fn build(
        path: &str,
        record_key: &str,
        parse: &ParseBlob,
        resolution: &FileResolution,
    ) -> Self {
        let mut ordinary = HashMap::<&str, NodeId>::new();
        let surviving = surviving_nodes(path, parse, &mut ordinary);
        let mut scoped = HashMap::<&str, NodeId>::new();
        for (symbol, id) in &surviving {
            scoped.entry(symbol.scoped_name.as_str()).or_insert(*id);
        }

        // References and edges keyed by their ids, as the views database
        // listed them, with its replacement rules: a later row for the same
        // reference replaces the earlier one.
        let references = references_by_key(parse);
        let mut refs = BTreeMap::<String, RefRow<'_>>::new();
        let mut edges = BTreeMap::<String, EdgeRow>::new();
        let mut rows = resolution.rows.iter().collect::<Vec<_>>();
        rows.sort();
        for row in rows {
            let Some(reference) = references.get(&row.ref_ordinal) else {
                continue;
            };
            let ref_id = format!("view:{path}:{}", row.ref_ordinal);
            let macro_body = macro_body_call(row.kind, reference);
            let caller = reference
                .caller_symbol
                .as_deref()
                .and_then(|symbol| ordinary.get(symbol))
                .copied();
            let target = row
                .target_path
                .as_ref()
                .and_then(|target| String::from_utf8(target.clone()).ok())
                .zip(row.target_symbol.clone());
            if let (Some(source), Some((target_file, target_symbol)), true) = (
                caller,
                target.clone(),
                matches!(
                    row.kind,
                    BlobRefKind::Call | BlobRefKind::Construction | BlobRefKind::CallbackRef
                ),
            ) {
                edges.insert(
                    format!("edge:{ref_id}"),
                    EdgeRow {
                        ref_id: ref_id.clone(),
                        source,
                        target_file,
                        target_symbol,
                        kind: match row.kind {
                            BlobRefKind::Construction => 3,
                            BlobRefKind::CallbackRef => 1,
                            _ if macro_body => 2,
                            _ => 0,
                        },
                        line: reference.line,
                        provenance: 0,
                        lookup: TargetLookup::Ordinary,
                    },
                );
            }
            refs.insert(
                ref_id,
                RefRow {
                    ordinal: row.ref_ordinal,
                    caller,
                    kind: if macro_body {
                        MACRO_BODY_CALL_CODE
                    } else {
                        ref_kind_code(row.kind)
                    },
                    short_name: reference.short_name.as_deref(),
                    full_ref: reference.full_ref.as_deref(),
                    local_name: if macro_body {
                        reference.caller_symbol.as_deref()
                    } else {
                        reference.local_name.as_deref()
                    },
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
            edges.remove(&format!("edge:{ref_id}"));
            if let Some(reference) = refs.get_mut(&ref_id) {
                reference.resolved = !resolution.targets.is_empty();
                reference.target = None;
            }
            let Some(caller) = site
                .caller
                .as_deref()
                .and_then(|symbol| scoped.get(symbol))
                .copied()
            else {
                continue;
            };
            for target in &resolution.targets {
                edges.insert(
                    format!(
                        "dispatch:{path}:{}:{}:{}",
                        site.ordinal, target.file, target.symbol
                    ),
                    EdgeRow {
                        ref_id: ref_id.clone(),
                        source: caller,
                        target_file: target.file.clone(),
                        target_symbol: target.symbol.clone(),
                        kind: 0,
                        line: site.line,
                        provenance: code_of(&PROVENANCES, target.provenance),
                        lookup: TargetLookup::Scoped,
                    },
                );
            }
        }
        Self::pack(
            path,
            record_key,
            &parse.language,
            &parse.entry_point_symbols,
            &surviving,
            ordinary,
            scoped,
            &refs,
            &edges,
        )
    }

    /// Packs assembled rows into columns over one sorted string table.
    #[allow(clippy::too_many_arguments)]
    fn pack(
        path: &str,
        record_key: &str,
        language: &str,
        entry_point_symbols: &[String],
        surviving: &[(&BlobSymbol, NodeId)],
        ordinary: HashMap<&str, NodeId>,
        scoped: HashMap<&str, NodeId>,
        refs: &BTreeMap<String, RefRow<'_>>,
        edges: &BTreeMap<String, EdgeRow>,
    ) -> Self {
        let mut ordinary = ordinary.into_iter().collect::<Vec<_>>();
        ordinary.sort_unstable_by_key(|(name, _)| *name);
        let mut scoped = scoped.into_iter().collect::<Vec<_>>();
        scoped.sort_unstable_by_key(|(name, _)| *name);

        // Handles: the surviving nodes in order, then the ids ordinary
        // resolution links to that no surviving node has, in name order.
        let mut handles = HashMap::<NodeId, u32>::new();
        let mut ids = Vec::new();
        let mut tops = Vec::new();
        let mut add = |id: NodeId, ids: &mut Vec<u64>| {
            *handles.entry(id).or_insert_with(|| {
                let handle = u32::try_from(ids.len()).expect("fewer than 2^32 ids");
                ids.push(id.digest);
                tops.push(u64::from(id.top));
                handle
            })
        };
        for (_, id) in surviving {
            add(*id, &mut ids);
        }
        let ordinary_nodes = ordinary
            .iter()
            .map(|(_, id)| add(*id, &mut ids))
            .collect::<Vec<_>>();
        let handle = |id: &NodeId| handles[id];

        let mut strings = BTreeSet::<&str>::new();
        for (symbol, _) in surviving {
            strings.extend([
                symbol.name.as_str(),
                symbol.scoped_name.as_str(),
                symbol.kind.as_str(),
            ]);
            strings.extend(symbol.signature.as_deref());
            strings.extend(symbol.entry_attribute.as_deref());
        }
        for reference in refs.values() {
            strings.extend(reference.short_name);
            strings.extend(reference.full_ref);
            strings.extend(reference.local_name);
            if let Some((file, symbol)) = &reference.target {
                strings.extend([file.as_str(), symbol.as_str()]);
            }
        }
        for edge in edges.values() {
            strings.extend([edge.target_file.as_str(), edge.target_symbol.as_str()]);
        }
        strings.extend(ordinary.iter().map(|(name, _)| *name));
        let sorted = strings.into_iter().collect::<Vec<_>>();
        let index = sorted
            .iter()
            .enumerate()
            .map(|(index, string)| {
                (
                    *string,
                    u32::try_from(index).expect("fewer than 2^32 strings"),
                )
            })
            .collect::<HashMap<_, _>>();
        let at = |string: &str| u64::from(index[string]);
        let optional = |string: Option<&str>| string.map_or(0, |string| at(string) + 1);

        let column = |values: Vec<u64>| Packed::of(&values);
        let nodes = Nodes {
            name: column(surviving.iter().map(|(s, _)| at(&s.name)).collect()),
            scoped_name: column(surviving.iter().map(|(s, _)| at(&s.scoped_name)).collect()),
            kind: column(surviving.iter().map(|(s, _)| at(&s.kind)).collect()),
            signature: column(
                surviving
                    .iter()
                    .map(|(s, _)| optional(s.signature.as_deref()))
                    .collect(),
            ),
            entry_attribute: column(
                surviving
                    .iter()
                    .map(|(s, _)| optional(s.entry_attribute.as_deref()))
                    .collect(),
            ),
            start_line: column(
                surviving
                    .iter()
                    .map(|(s, _)| u64::from(s.start_line))
                    .collect(),
            ),
            start_col: column(
                surviving
                    .iter()
                    .map(|(s, _)| u64::from(s.start_col))
                    .collect(),
            ),
            end_line: column(
                surviving
                    .iter()
                    .map(|(s, _)| u64::from(s.end_line))
                    .collect(),
            ),
            range_ordinal: column(
                surviving
                    .iter()
                    .map(|(s, _)| u64::from(s.ordinal))
                    .collect(),
            ),
            flags: column(
                surviving
                    .iter()
                    .map(|(symbol, _)| {
                        let entry_point = if language == "go"
                            && matches!(symbol.kind.as_str(), "variable" | "method")
                        {
                            entry_point_symbols.contains(&symbol.scoped_name)
                        } else {
                            symbol.exported
                        };
                        [
                            (symbol.exported, EXPORTED),
                            (symbol.is_default_export, DEFAULT_EXPORT),
                            (entry_point, ENTRY_POINT),
                            (symbol.is_test.is_some(), TEST_KNOWN),
                            (symbol.is_test == Some(true), TEST),
                            (symbol.program_start, PROGRAM_START),
                        ]
                        .into_iter()
                        .filter(|(set, _)| *set)
                        .fold(0, |flags, (_, flag)| flags | flag)
                    })
                    .collect(),
            ),
        };

        let positions = refs
            .keys()
            .enumerate()
            .map(|(position, id)| (id.as_str(), position as u64))
            .collect::<HashMap<_, _>>();
        let rows = refs.values().collect::<Vec<_>>();
        let (mut local_at, mut local_name) = (Vec::new(), Vec::new());
        let (mut target_at, mut target_file, mut target_symbol) =
            (Vec::new(), Vec::new(), Vec::new());
        for (position, reference) in rows.iter().enumerate() {
            if let Some(name) = reference.local_name {
                local_at.push(position as u64);
                local_name.push(at(name));
            }
            if let Some((file, symbol)) = &reference.target {
                target_at.push(position as u64);
                target_file.push(at(file));
                target_symbol.push(at(symbol));
            }
        }
        let refs = Refs {
            ordinal: column(rows.iter().map(|r| u64::from(r.ordinal)).collect()),
            caller: column(
                rows.iter()
                    .map(|r| r.caller.map_or(0, |id| u64::from(handle(&id)) + 1))
                    .collect(),
            ),
            short_name: column(rows.iter().map(|r| optional(r.short_name)).collect()),
            full_ref: column(rows.iter().map(|r| optional(r.full_ref)).collect()),
            line: column(rows.iter().map(|r| u64::from(r.line)).collect()),
            byte_start: column(rows.iter().map(|r| r.byte_start).collect()),
            byte_end: column(rows.iter().map(|r| r.byte_end).collect()),
            code: column(
                rows.iter()
                    .map(|r| r.kind | if r.resolved { RESOLVED } else { 0 })
                    .collect(),
            ),
            local_at: column(local_at),
            local_name: column(local_name),
            target_at: column(target_at),
            target_file: column(target_file),
            target_symbol: column(target_symbol),
        };

        let rows = edges.values().collect::<Vec<_>>();
        let edges = Edges {
            reference: column(
                rows.iter()
                    .map(|e| positions.get(e.ref_id.as_str()).map_or(0, |at| at + 1))
                    .collect(),
            ),
            source: column(rows.iter().map(|e| u64::from(handle(&e.source))).collect()),
            target_file: column(rows.iter().map(|e| at(&e.target_file)).collect()),
            target_symbol: column(rows.iter().map(|e| at(&e.target_symbol)).collect()),
            line: column(rows.iter().map(|e| u64::from(e.line)).collect()),
            code: column(
                rows.iter()
                    .map(|e| {
                        let lookup = match e.lookup {
                            TargetLookup::Ordinary => 0,
                            TargetLookup::Scoped => SCOPED_LOOKUP,
                        };
                        e.kind | lookup | (e.provenance << 3)
                    })
                    .collect(),
            ),
        };

        Self {
            path: path.to_string(),
            language: language.to_string(),
            record_key: record_key.to_string(),
            strings: Strings::of(&sorted),
            ids,
            id_top: column(tops),
            nodes,
            refs,
            edges,
            ordinary_name: column(ordinary.iter().map(|(name, _)| at(name)).collect()),
            ordinary_node: column(ordinary_nodes.into_iter().map(u64::from).collect()),
            scoped_name: column(scoped.iter().map(|(name, _)| at(name)).collect()),
            scoped_node: column(scoped.iter().map(|(_, id)| u64::from(handle(id))).collect()),
        }
    }
}

/// The format of a stored slice record ([`FileSlice::encode`]).
const RECORD_FORMAT: u8 = 1;

/// Why a stored slice record was refused. The bytes are damaged or were not
/// written by this build: either way the slice is not read from them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecordError(pub(crate) &'static str);

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "stored slice record: {}", self.0)
    }
}

/// Reads a record's little-endian fields in order.
struct Reader<'b> {
    bytes: &'b [u8],
}

impl<'b> Reader<'b> {
    fn take(&mut self, len: usize) -> Result<&'b [u8], RecordError> {
        if len > self.bytes.len() {
            return Err(RecordError("a field runs past the end"));
        }
        let (taken, rest) = self.bytes.split_at(len);
        self.bytes = rest;
        Ok(taken)
    }

    fn u8(&mut self) -> Result<u8, RecordError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, RecordError> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("four bytes"),
        ))
    }

    fn u64(&mut self) -> Result<u64, RecordError> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("eight bytes"),
        ))
    }

    fn string(&mut self) -> Result<String, RecordError> {
        let len = self.u32()? as usize;
        String::from_utf8(self.take(len)?.to_vec())
            .map_err(|_| RecordError("a string is not UTF-8"))
    }

    fn packed(&mut self) -> Result<Packed, RecordError> {
        let len = self.u32()?;
        let width = self.u8()?;
        if width > 64 {
            return Err(RecordError("a column is wider than 64 bits"));
        }
        let words = words_for(len as usize, width);
        // The count is checked against the bytes left before anything is
        // allocated for it.
        let bytes = self.take(
            words
                .checked_mul(8)
                .ok_or(RecordError("a column runs past the end"))?,
        )?;
        let words = bytes
            .chunks_exact(8)
            .map(|word| u64::from_le_bytes(word.try_into().expect("eight bytes")))
            .collect::<Vec<_>>();
        // Bits past the last value are zero as written, so equal columns
        // have equal words.
        let used = (len as usize * usize::from(width)) % 64;
        if used != 0 && words.last().is_some_and(|last| last >> used != 0) {
            return Err(RecordError("a column has bits past its last value"));
        }
        Ok(Packed { len, width, words })
    }
}

fn write_u32(out: &mut Vec<u8>, value: usize) {
    out.extend_from_slice(
        &u32::try_from(value)
            .expect("a slice's parts are under 4 GiB")
            .to_le_bytes(),
    );
}

fn write_string(out: &mut Vec<u8>, value: &str) {
    write_u32(out, value.len());
    out.extend_from_slice(value.as_bytes());
}

fn write_packed(out: &mut Vec<u8>, column: &Packed) {
    write_u32(out, column.len());
    out.push(column.width);
    for word in &column.words {
        out.extend_from_slice(&word.to_le_bytes());
    }
}

/// Checks that `column` has `len` values, each below `bound`.
fn check(column: &Packed, len: usize, bound: u64, what: &'static str) -> Result<(), RecordError> {
    if column.len() != len {
        return Err(RecordError(what));
    }
    // Every value the width allows is in bounds: nothing to read.
    if column.width < 64 && (1u64 << column.width) <= bound {
        return Ok(());
    }
    if column.values().all(|value| value < bound) {
        Ok(())
    } else {
        Err(RecordError(what))
    }
}

/// Checks that `column` holds ascending values without repeats.
fn check_ascending(column: &Packed, what: &'static str) -> Result<(), RecordError> {
    let mut values = column.values();
    let Some(mut previous) = values.next() else {
        return Ok(());
    };
    for value in values {
        if value <= previous {
            return Err(RecordError(what));
        }
        previous = value;
    }
    Ok(())
}

const U32_BOUND: u64 = 1 << 32;

impl FileSlice {
    /// The slice as a stored record: a format byte, a CRC-32 of everything
    /// after it, and the slice's columns. The path and record key are the
    /// record's key where it is stored, so they are not repeated in it.
    #[must_use]
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut body = Vec::new();
        write_string(&mut body, &self.language);
        write_string(&mut body, &self.strings.text);
        write_packed(&mut body, &self.strings.ends);
        write_u32(&mut body, self.ids.len());
        for id in &self.ids {
            body.extend_from_slice(&id.to_le_bytes());
        }
        for column in self.columns() {
            write_packed(&mut body, column);
        }
        let mut out = Vec::with_capacity(body.len() + 5);
        out.push(RECORD_FORMAT);
        out.extend_from_slice(&crc32fast::hash(&body).to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    /// Every column after the string table and the ids, in record order.
    fn columns(&self) -> [&Packed; 34] {
        let (n, r, e) = (&self.nodes, &self.refs, &self.edges);
        [
            &self.id_top,
            &n.name,
            &n.scoped_name,
            &n.kind,
            &n.signature,
            &n.entry_attribute,
            &n.start_line,
            &n.start_col,
            &n.end_line,
            &n.range_ordinal,
            &n.flags,
            &r.ordinal,
            &r.caller,
            &r.short_name,
            &r.full_ref,
            &r.line,
            &r.byte_start,
            &r.byte_end,
            &r.code,
            &r.local_at,
            &r.local_name,
            &r.target_at,
            &r.target_file,
            &r.target_symbol,
            &e.reference,
            &e.source,
            &e.target_file,
            &e.target_symbol,
            &e.line,
            &e.code,
            &self.ordinary_name,
            &self.ordinary_node,
            &self.scoped_name,
            &self.scoped_node,
        ]
    }

    /// Reads the slice of the file at `path`, whose content record has the
    /// key `record_key`, from the record [`Self::encode`] wrote.
    ///
    /// # Errors
    ///
    /// When the record fails its checksum, or is not a slice this build
    /// writes: a damaged record is refused, never read as other rows.
    pub(crate) fn decode(path: &str, record_key: &str, bytes: &[u8]) -> Result<Self, RecordError> {
        let mut input = Reader { bytes };
        if input.u8()? != RECORD_FORMAT {
            return Err(RecordError("unknown format"));
        }
        let checksum = input.u32()?;
        if crc32fast::hash(input.bytes) != checksum {
            return Err(RecordError("the checksum does not match"));
        }
        let language = input.string()?;
        let text = input.string()?;
        let ends = input.packed()?;
        let id_count = input.u32()? as usize;
        if id_count > input.bytes.len() / 8 {
            return Err(RecordError("the id count runs past the end"));
        }
        let ids = (0..id_count)
            .map(|_| input.u64())
            .collect::<Result<Vec<_>, _>>()?;
        let mut columns = Vec::with_capacity(34);
        for _ in 0..34 {
            columns.push(input.packed()?);
        }
        if !input.bytes.is_empty() {
            return Err(RecordError("bytes after the record"));
        }
        let mut columns = columns.into_iter();
        let mut next = || columns.next().expect("34 columns were read");
        let id_top = next();
        let nodes = Nodes {
            name: next(),
            scoped_name: next(),
            kind: next(),
            signature: next(),
            entry_attribute: next(),
            start_line: next(),
            start_col: next(),
            end_line: next(),
            range_ordinal: next(),
            flags: next(),
        };
        let refs = Refs {
            ordinal: next(),
            caller: next(),
            short_name: next(),
            full_ref: next(),
            line: next(),
            byte_start: next(),
            byte_end: next(),
            code: next(),
            local_at: next(),
            local_name: next(),
            target_at: next(),
            target_file: next(),
            target_symbol: next(),
        };
        let edges = Edges {
            reference: next(),
            source: next(),
            target_file: next(),
            target_symbol: next(),
            line: next(),
            code: next(),
        };
        let slice = Self {
            path: path.to_string(),
            language,
            record_key: record_key.to_string(),
            strings: Strings { text, ends },
            ids,
            id_top,
            nodes,
            refs,
            edges,
            ordinary_name: next(),
            ordinary_node: next(),
            scoped_name: next(),
            scoped_node: next(),
        };
        slice.validate()?;
        Ok(slice)
    }

    /// Checks what readers rely on without checking again: every index in
    /// range, every code known, every searched column ascending, and the
    /// string table sorted, so no read of a decoded slice can fail.
    fn validate(&self) -> Result<(), RecordError> {
        let strings = &self.strings;
        let string_count = strings.len() as u64;
        let ends_fit = strings.ends.values().try_fold(0u64, |start, end| {
            (start <= end
                && end <= strings.text.len() as u64
                && strings.text.is_char_boundary(end as usize))
            .then_some(end)
        });
        if ends_fit.is_none_or(|last| last != strings.text.len() as u64) {
            return Err(RecordError("the string table's bounds"));
        }
        if (1..strings.len())
            .any(|index| strings.get(index as u32 - 1) >= strings.get(index as u32))
        {
            return Err(RecordError("the string table is not sorted"));
        }
        let ids = self.ids.len();
        let ids_bound = ids as u64;
        check(&self.id_top, ids, 2, "id classes")?;

        let n = &self.nodes;
        let nodes = n.name.len();
        if nodes > ids {
            return Err(RecordError("more nodes than ids"));
        }
        for column in [&n.name, &n.scoped_name, &n.kind] {
            check(column, nodes, string_count, "a node's string")?;
        }
        for column in [&n.signature, &n.entry_attribute] {
            check(column, nodes, string_count + 1, "a node's optional string")?;
        }
        for column in [&n.start_line, &n.start_col, &n.end_line, &n.range_ordinal] {
            check(column, nodes, U32_BOUND, "a node's position")?;
        }
        check(&n.flags, nodes, PROGRAM_START << 1, "a node's flags")?;

        let r = &self.refs;
        let refs = r.ordinal.len();
        check(&r.ordinal, refs, U32_BOUND, "a reference's ordinal")?;
        check(&r.caller, refs, ids_bound + 1, "a reference's caller")?;
        check(&r.short_name, refs, string_count + 1, "a reference's name")?;
        check(
            &r.full_ref,
            refs,
            string_count + 1,
            "a reference's full name",
        )?;
        check(&r.line, refs, U32_BOUND, "a reference's line")?;
        // Any value is a valid byte offset.
        if r.byte_start.len() != refs || r.byte_end.len() != refs {
            return Err(RecordError("a reference's byte range"));
        }
        check(&r.code, refs, RESOLVED << 1, "a reference's code")?;
        if r.code
            .values()
            .any(|code| (code & 0xF) as usize >= REF_KINDS.len())
        {
            return Err(RecordError("a reference's kind"));
        }
        check(
            &r.local_at,
            r.local_at.len(),
            refs as u64,
            "a local name's reference",
        )?;
        check_ascending(&r.local_at, "local names out of order")?;
        check(
            &r.local_name,
            r.local_at.len(),
            string_count,
            "a local name",
        )?;
        check(
            &r.target_at,
            r.target_at.len(),
            refs as u64,
            "a target's reference",
        )?;
        check_ascending(&r.target_at, "targets out of order")?;
        check(
            &r.target_file,
            r.target_at.len(),
            string_count,
            "a target's file",
        )?;
        check(
            &r.target_symbol,
            r.target_at.len(),
            string_count,
            "a target's symbol",
        )?;

        let e = &self.edges;
        let edges = e.source.len();
        check(&e.reference, edges, refs as u64 + 1, "an edge's reference")?;
        check(&e.source, edges, ids_bound, "an edge's source")?;
        check(&e.target_file, edges, string_count, "an edge's target file")?;
        check(
            &e.target_symbol,
            edges,
            string_count,
            "an edge's target symbol",
        )?;
        check(&e.line, edges, U32_BOUND, "an edge's line")?;
        check(
            &e.code,
            edges,
            (PROVENANCES.len() as u64) << 3,
            "an edge's code",
        )?;

        check(
            &self.ordinary_name,
            self.ordinary_name.len(),
            string_count,
            "a linked name",
        )?;
        check_ascending(&self.ordinary_name, "linked names out of order")?;
        check(
            &self.ordinary_node,
            self.ordinary_name.len(),
            ids_bound,
            "a linked node",
        )?;
        check(
            &self.scoped_name,
            self.scoped_name.len(),
            string_count,
            "a scoped name",
        )?;
        check_ascending(&self.scoped_name, "scoped names out of order")?;
        check(
            &self.scoped_node,
            self.scoped_name.len(),
            nodes as u64,
            "a scoped node",
        )?;
        Ok(())
    }
}

aft_store::heap_bytes_none!(TargetLookup, NodeId, NodeHandle);
aft_store::heap_bytes_of_fields!(Packed { len, width, words });
aft_store::heap_bytes_of_fields!(Strings { text, ends });
aft_store::heap_bytes_of_fields!(Nodes {
    name,
    scoped_name,
    kind,
    signature,
    entry_attribute,
    start_line,
    start_col,
    end_line,
    range_ordinal,
    flags,
});
aft_store::heap_bytes_of_fields!(Refs {
    ordinal,
    caller,
    short_name,
    full_ref,
    line,
    byte_start,
    byte_end,
    code,
    local_at,
    local_name,
    target_at,
    target_file,
    target_symbol,
});
aft_store::heap_bytes_of_fields!(Edges {
    reference,
    source,
    target_file,
    target_symbol,
    line,
    code,
});
aft_store::heap_bytes_of_fields!(FileSlice {
    path,
    language,
    record_key,
    strings,
    ids,
    id_top,
    nodes,
    refs,
    edges,
    ordinary_name,
    ordinary_node,
    scoped_name,
    scoped_node,
});

#[cfg(test)]
mod tests {
    use super::*;

    /// The slices of a small Rust crate with ordinary calls, a method call
    /// the dispatch pass resolves, a call in a `macro_rules!` template, an
    /// import, a value reference, an unresolved call, and a symbol another
    /// one with the same id replaces.
    fn slices() -> Vec<FileSlice> {
        let root = tempfile::tempdir().unwrap();
        let write = |path: &str, text: &str| {
            let path = root.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write("Cargo.toml", "[package]\nname = \"demo\"\n");
        write(
            "src/lib.rs",
            "mod util;\nuse util::callee;\n\
             pub struct S;\nimpl S { pub fn run(&self) { callee(); } }\n\
             macro_rules! twice { () => { callee(); callee(); } }\n\
             pub fn caller() { callee(); let s = S; s.run(); let f = callee; f(); missing(); twice!(); }\n\
             fn dup() {}\nfn dup() {}\n",
        );
        write("src/util.rs", "pub fn callee() {}\n");
        let built = super::super::trace_measure::build(root.path(), 2).unwrap();
        built
            .resolved
            .iter()
            .map(|(path, resolution)| {
                let (key, parse) = &built.inputs.parses[path];
                FileSlice::build(path, key, parse, resolution)
            })
            .collect()
    }

    #[test]
    fn a_slice_reads_back_from_its_record_unchanged() {
        let slices = slices();
        let lib = slices
            .iter()
            .find(|slice| slice.path() == "src/lib.rs")
            .expect("the crate root has a slice");
        // The record has something of every part to carry.
        assert!(lib.nodes().len() > 3, "{lib:?}");
        assert!(lib.references().len() > 5, "{lib:?}");
        assert!(lib.edges().len() > 2, "{lib:?}");
        assert!(lib
            .references()
            .any(|reference| reference.target().is_some()));
        assert!(lib
            .references()
            .any(|reference| reference.local_name().is_some()));
        for slice in &slices {
            let bytes = slice.encode();
            let read = FileSlice::decode(slice.path(), slice.record_key(), &bytes).unwrap();
            assert_eq!(read, *slice, "{}", slice.path());
            // And again to the same bytes.
            assert_eq!(read.encode(), bytes);
        }
    }

    #[test]
    fn a_damaged_record_is_refused_never_read_as_other_rows() {
        let slices = slices();
        let lib = slices
            .iter()
            .find(|slice| slice.path() == "src/lib.rs")
            .unwrap();
        let bytes = lib.encode();
        let decode = |bytes: &[u8]| FileSlice::decode(lib.path(), lib.record_key(), bytes);
        // Any one bit flipped anywhere.
        for at in 0..bytes.len() {
            for bit in [0, 3, 7] {
                let mut damaged = bytes.clone();
                damaged[at] ^= 1 << bit;
                assert!(decode(&damaged).is_err(), "bit {bit} of byte {at} flipped");
            }
        }
        // Cut short, or with bytes after it.
        for len in [0, 1, 4, 5, bytes.len() / 2, bytes.len() - 1] {
            assert!(decode(&bytes[..len]).is_err(), "cut to {len} bytes");
        }
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(decode(&longer).is_err());
        // A record whose checksum matches but whose rows point outside the
        // slice, as a writer bug would store: refused by the row checks.
        let mut broken = lib.clone();
        let sources = broken.edges.source.values().collect::<Vec<_>>();
        let mut out_of_range = sources.clone();
        out_of_range[0] = broken.ids.len() as u64;
        broken.edges.source = Packed::of(&out_of_range);
        assert_eq!(
            FileSlice::decode(lib.path(), lib.record_key(), &broken.encode()),
            Err(RecordError("an edge's source"))
        );
        let mut unsorted = lib.clone();
        let mut names = unsorted.scoped_name.values().collect::<Vec<_>>();
        names.reverse();
        unsorted.scoped_name = Packed::of(&names);
        assert!(names.len() > 1);
        assert_eq!(
            FileSlice::decode(lib.path(), lib.record_key(), &unsorted.encode()),
            Err(RecordError("scoped names out of order"))
        );
    }

    #[test]
    fn packed_columns_hold_every_width() {
        for width in 0..=64u32 {
            let largest = if width == 0 {
                0
            } else {
                u64::MAX >> (64 - width)
            };
            let values = (0..131u64)
                .map(|index| {
                    largest.wrapping_sub(index.wrapping_mul(0x9E37_79B9_7F4A_7C15)) & largest
                })
                .chain([largest, 0])
                .collect::<Vec<_>>();
            let column = Packed::of(&values);
            assert_eq!(u32::from(column.width), width);
            assert_eq!(column.values().collect::<Vec<_>>(), values, "width {width}");
        }
    }

    #[test]
    fn node_ids_spell_and_order_as_the_views_store_does() {
        let ids = [
            crate::callgraph_store::view_node_id("src/a.rs", "function", "run"),
            crate::callgraph_store::view_node_id("src/a.rs", "method", "S::run"),
            crate::callgraph_store::view_node_id(
                "src/a.rs",
                "module",
                crate::callgraph_store::TOP_LEVEL_SYMBOL,
            ),
            crate::callgraph_store::view_node_id("src/b.rs", "function", "run"),
        ];
        let mut parsed = Vec::new();
        for id in &ids {
            let node = NodeId::parse(id).unwrap();
            assert_eq!(node.to_string(), *id);
            parsed.push(node);
        }
        let mut by_text = ids.to_vec();
        by_text.sort();
        parsed.sort();
        assert_eq!(
            parsed.iter().map(ToString::to_string).collect::<Vec<_>>(),
            by_text
        );
        for other in [
            "",
            "vp",
            "vq0123456789abcdef",
            "vp0123456789ABCDEF",
            "vp+123456789abcdef",
            "vp0123456789abcdef0",
        ] {
            assert_eq!(NodeId::parse(other), None, "{other}");
        }
    }
}
