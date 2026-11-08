//! The dead-code projection of a checkout's call graph, read one file at a
//! time.
//!
//! [`super::dead_code::snapshot`] projects dead code's view of a call graph
//! from [`GraphRows`](super::graph_rows::GraphRows), which hold every symbol,
//! reference and edge of the checkout at once: on a large repository that is
//! gigabytes of strings, and the snapshot it builds is gigabytes more. This
//! projection gives the same answers by the same rules, but it holds only
//! every file's symbols at once. A file's references are read when that file
//! is projected, so a caller can turn one file's outbound calls into its own
//! compact form and drop them before it projects the next file.
//!
//! Every rule here is the one `graph_rows::assemble` and `dead_code::snapshot`
//! apply, restricted to one caller file: node ids and node replacement, the
//! reference-to-edge join, method-call sites replacing ordinary edges, the
//! members an unknown receiver keeps alive, and Go package initialization.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::callgraph::FileResolution;
use super::dead_code::{path_key, Paths, OUTBOUND_KINDS};
use super::graph_rows::{
    macro_body_call, ref_kind, references_by_key, EdgeRow, NodeRow, Nodes, RefRow,
};
use crate::callgraph_store::dead_code_projection::{
    is_method_dispatch_callee, is_resolved_edge, symbol_kind_from_label,
    UNRESOLVED_VALUE_PATH_PROVENANCE,
};
use crate::callgraph_store::join::{BlobRefKind, ParseBlob, ResolutionStatus};
use crate::callgraph_store::{
    lang_from_label, MACRO_BODY_CALL_REF_KIND, PROVENANCE_TREESITTER, PROVENANCE_VALUE_REF,
    TOP_LEVEL_SYMBOL,
};
use crate::inspect::job::{
    CallgraphExport, CallgraphOutboundCall, GoInitializerGroup, DISPATCHED_CALLEE_SEPARATOR,
};
use crate::inspect::scanners::dead_code::reads::{record, Lookup};
use crate::inspect::scanners::DEFAULT_EXPORT_MARKER_KIND;

/// A checkout view's call graph, which a dead-code snapshot carries so the
/// rollup reads it one file at a time.
#[derive(Clone)]
pub struct ViewCallGraph {
    view: super::checkout::CheckoutView,
    /// Where the dead-code graph of each publication is kept once worked
    /// out, so a later rollup over the same publication reads it instead of
    /// materializing every file again. `None` keeps nothing.
    section_dir: Option<std::path::PathBuf>,
}

impl ViewCallGraph {
    pub(crate) fn new(
        view: super::checkout::CheckoutView,
        section_dir: Option<std::path::PathBuf>,
    ) -> Self {
        Self { view, section_dir }
    }

    /// The view's call graph, read file by file.
    pub(crate) fn files(&self) -> super::checkout::ViewGraphFiles<'_> {
        self.view.dead_code_files()
    }

    /// The exported symbols of `files` (absolute paths in the checkout), as
    /// a full dead-code snapshot would list them for those files; a file the
    /// call graph does not have has none.
    ///
    /// # Errors
    ///
    /// When a file's parse cannot be read.
    pub(crate) fn exported_symbols_of(
        &self,
        root: &Path,
        files: &[PathBuf],
    ) -> Result<Vec<CallgraphExport>, String> {
        let source = self.files();
        let mut paths = Paths {
            root,
            cache: HashMap::new(),
        };
        let mut exports = Vec::new();
        for file in files {
            let Ok(relative) = file.strip_prefix(root) else {
                continue;
            };
            let store_path = relative.to_string_lossy().replace('\\', "/");
            if !source.has_file(&store_path) {
                continue;
            }
            let parse = source.parse(&store_path)?;
            let indexed = IndexedFile::build(&store_path, &parse);
            exports.extend(file_exports(&indexed, paths.resolve(&store_path)));
        }
        Ok(exports)
    }

    /// The folder dead-code graphs of this view's family are kept in;
    /// `None` keeps nothing.
    pub(crate) fn section_dir(&self) -> Option<&Path> {
        self.section_dir.as_deref()
    }
}

impl std::fmt::Debug for ViewCallGraph {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ViewCallGraph")
    }
}

impl aft_store::heap::HeapBytes for ViewCallGraph {
    /// The view belongs to its checkout, which counts it.
    fn heap_bytes(&self, _seen: &mut aft_store::heap::Seen) -> usize {
        0
    }
}

/// Reads a call graph's parsed files on demand.
pub(crate) trait CallGraphFiles: Sync {
    /// Every parsed file, in path order.
    fn paths(&self) -> Vec<String>;
    /// Whether `path` is a parsed file.
    fn has_file(&self, path: &str) -> bool;
    /// Whether the checkout records `path` as a regular file rather than a
    /// symlink. The scan records a symlink as an entry of its own and never
    /// descends into a symlinked folder, so no folder below the root on a
    /// regular file's path is a symlink.
    fn is_regular_file(&self, path: &str) -> bool;
    /// `path`'s parse.
    ///
    /// # Errors
    ///
    /// When its stored record cannot be read.
    fn parse(&self, path: &str) -> Result<Arc<ParseBlob>, String>;
    /// `path`'s resolution.
    ///
    /// # Errors
    ///
    /// When its stored fragment cannot be read.
    fn resolution(&self, path: &str) -> Result<Arc<FileResolution>, String>;
    /// The dead-code facts `path`'s content record holds, `None` when it
    /// holds none.
    ///
    /// # Errors
    ///
    /// When its record cannot be read.
    fn dead_code_facts(&self, path: &str) -> Result<Option<Vec<u8>>, String>;
}

/// One symbol of a file, as the projection reads it.
#[derive(Debug)]
struct IndexedNode {
    id: Box<str>,
    name: Box<str>,
    scoped_name: Box<str>,
    kind: Box<str>,
    start_line: u32,
    exported: bool,
    is_default_export: bool,
    entry_point: bool,
}

/// One file's symbols: the nodes its stored rows keep and how references
/// name them.
#[derive(Debug)]
struct IndexedFile {
    language: Box<str>,
    /// The nodes that survive, in storage order.
    nodes: Vec<IndexedNode>,
    /// Each surviving node's position in `nodes`, by node id.
    by_id: HashMap<Box<str>, u32>,
    /// A name or scoped name to the node id references to it resolve to.
    /// The id may name a node another symbol's range replaced, which is a
    /// known node id with no row.
    lookup: HashMap<Box<str>, Box<str>>,
    /// A scoped name to the earliest stored node with it.
    by_scoped_name: HashMap<Box<str>, u32>,
}

impl IndexedFile {
    fn build(path: &str, parse: &ParseBlob) -> Self {
        // The same rows `graph_rows::assemble` inserts for this file, by its
        // `INSERT OR REPLACE` rules.
        let mut nodes = Nodes::default();
        let mut lookup = HashMap::<Box<str>, Box<str>>::new();
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
                signature: None,
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
                entry_attribute: None,
            });
            lookup.insert(symbol.scoped_name.as_str().into(), id.as_str().into());
            lookup
                .entry(symbol.name.as_str().into())
                .or_insert_with(|| id.as_str().into());
        }
        let by_scoped_name = nodes
            .by_scoped_name()
            .into_iter()
            .map(|((_, scoped), id)| (Box::<str>::from(scoped), Box::<str>::from(id)))
            .collect::<Vec<_>>();
        let mut order = nodes
            .rows
            .into_values()
            .map(|(sequence, row)| (sequence, row))
            .collect::<Vec<_>>();
        order.sort_unstable_by_key(|(sequence, _)| *sequence);
        let nodes = order
            .into_iter()
            .map(|(_, row)| IndexedNode {
                id: row.id.into(),
                name: row.name.into(),
                scoped_name: row.scoped_name.into(),
                kind: row.kind.into(),
                start_line: row.start_line,
                exported: row.exported,
                is_default_export: row.is_default_export,
                entry_point: row.entry_point,
            })
            .collect::<Vec<_>>();
        let by_id = nodes
            .iter()
            .enumerate()
            .map(|(index, node)| (node.id.clone(), index as u32))
            .collect::<HashMap<_, _>>();
        let by_scoped_name = by_scoped_name
            .into_iter()
            .filter_map(|(scoped, id)| Some((scoped, *by_id.get(&id)?)))
            .collect();
        lookup.shrink_to_fit();
        Self {
            language: parse.language.as_str().into(),
            nodes,
            by_id,
            lookup,
            by_scoped_name,
        }
    }

    fn node(&self, id: &str) -> Option<&IndexedNode> {
        self.by_id.get(id).map(|index| &self.nodes[*index as usize])
    }
}

/// Bytes `start..start + len` of a [`TargetIndex`]'s text.
type TextSpan = (u32, u32);

/// No node: a key that resolves to none in one of its two ways.
const NO_NODE: u32 = u32::MAX;

/// What a call into a file reads of the file: the node a name or scoped name
/// resolves to, the earliest stored node with a scoped name, and the plain
/// name and kind of such a node. Worked out from the file's
/// [`IndexedFile`], which is then dropped: a patched rollup that projects a
/// few files calls into hundreds, and holding each of those files' whole
/// symbol index cost hundreds of megabytes.
struct TargetIndex {
    /// The text of every key, node id, name and kind; spans index it.
    text: Box<str>,
    /// Each key, sorted by its text: the node id [`IndexedFile::lookup`]
    /// resolves it to and the node [`IndexedFile::by_scoped_name`] gives
    /// for it, as numbers into `ids` ([`NO_NODE`] for none).
    keys: Box<[(TextSpan, u32, u32)]>,
    /// Each node id a key names, sorted by its text, with the plain name and
    /// kind of the stored node it identifies (`None` for an id whose node
    /// another symbol's range replaced).
    ids: Box<[(TextSpan, Option<(TextSpan, TextSpan)>)]>,
}

impl TargetIndex {
    fn of(file: &IndexedFile) -> Self {
        fn span(text: &mut String, value: &str) -> TextSpan {
            let start = text.len() as u32;
            text.push_str(value);
            (start, value.len() as u32)
        }
        let mut text = String::new();
        // Every id a key resolves to, with its node when one is stored.
        let mut ids = BTreeMap::<&str, Option<&IndexedNode>>::new();
        for id in file.lookup.values() {
            ids.insert(&**id, file.node(id));
        }
        for index in file.by_scoped_name.values() {
            let node = &file.nodes[*index as usize];
            ids.insert(&*node.id, Some(node));
        }
        let numbers = ids
            .keys()
            .enumerate()
            .map(|(number, id)| (*id, number as u32))
            .collect::<HashMap<_, _>>();
        let ids_table = ids
            .iter()
            .map(|(id, node)| {
                let id = span(&mut text, id);
                let node =
                    node.map(|node| (span(&mut text, &node.name), span(&mut text, &node.kind)));
                (id, node)
            })
            .collect::<Vec<_>>();
        let mut keys = BTreeMap::<&str, (u32, u32)>::new();
        for (key, id) in &file.lookup {
            keys.entry(&**key).or_insert((NO_NODE, NO_NODE)).0 = numbers[&**id];
        }
        for (key, index) in &file.by_scoped_name {
            keys.entry(&**key).or_insert((NO_NODE, NO_NODE)).1 =
                numbers[&*file.nodes[*index as usize].id];
        }
        let keys = keys
            .into_iter()
            .map(|(key, (lookup, scoped))| (span(&mut text, key), lookup, scoped))
            .collect::<Vec<_>>();
        Self {
            text: text.into_boxed_str(),
            keys: keys.into_boxed_slice(),
            ids: ids_table.into_boxed_slice(),
        }
    }

    fn text(&self, (start, len): TextSpan) -> &str {
        &self.text[start as usize..(start + len) as usize]
    }

    fn key(&self, key: &str) -> Option<(u32, u32)> {
        let found = self
            .keys
            .binary_search_by(|(span, _, _)| self.text(*span).cmp(key))
            .ok()?;
        let (_, lookup, scoped) = self.keys[found];
        Some((lookup, scoped))
    }

    fn id(&self, number: u32) -> Option<&str> {
        (number != NO_NODE).then(|| self.text(self.ids[number as usize].0))
    }
}

/// A file's symbols as a call into it reads them: in full when the file's
/// own calls were projected too, else its [`TargetIndex`]. Both give the
/// same answers.
#[derive(Clone, Copy)]
enum Target<'g> {
    Full(&'g IndexedFile),
    Index(&'g TargetIndex),
}

impl<'g> Target<'g> {
    /// The node id references to `symbol` resolve to.
    fn lookup(self, symbol: &str) -> Option<&'g str> {
        match self {
            Self::Full(file) => file.lookup.get(symbol).map(|id| &**id),
            Self::Index(index) => index.id(index.key(symbol)?.0),
        }
    }

    /// The id of the earliest stored node with scoped name `symbol`.
    fn scoped(self, symbol: &str) -> Option<&'g str> {
        match self {
            Self::Full(file) => file
                .by_scoped_name
                .get(symbol)
                .map(|index| &*file.nodes[*index as usize].id),
            Self::Index(index) => index.id(index.key(symbol)?.1),
        }
    }

    /// The plain name and kind of the stored node `id`.
    fn node(self, id: &str) -> Option<(&'g str, &'g str)> {
        match self {
            Self::Full(file) => file.node(id).map(|node| (&*node.name, &*node.kind)),
            Self::Index(index) => {
                let found = index
                    .ids
                    .binary_search_by(|(span, _)| index.text(*span).cmp(id))
                    .ok()?;
                let (name, kind) = index.ids[found].1?;
                Some((index.text(name), index.text(kind)))
            }
        }
    }
}

/// One parsed file's symbols, each form read when first asked for.
#[derive(Default)]
struct FileCell {
    /// In full, for a file whose own calls are projected.
    full: std::sync::OnceLock<Option<IndexedFile>>,
    /// What calls into it read, for a file only called into.
    target: std::sync::OnceLock<Option<TargetIndex>>,
}

/// Every parsed file's symbols, and what the dead-code snapshot derives
/// from symbols alone. A file's symbols are read from its parse when first
/// asked for, so a rollup that reads only a few files (a patched one) parses
/// only those.
pub(crate) struct DeadCodeGraph<'f> {
    root: PathBuf,
    source: &'f dyn CallGraphFiles,
    /// Every parsed file, with its symbols once read.
    files: BTreeMap<String, FileCell>,
    /// Whether a file only called into is read into its [`TargetIndex`]
    /// alone (see [`Self::compact_targets`]).
    compact_targets: std::sync::atomic::AtomicBool,
    /// Go package directories to their files, in path order, once worked
    /// out (it reads every file's language).
    go_packages: std::sync::OnceLock<BTreeMap<PathBuf, Vec<String>>>,
    /// The first file whose parse could not be read; answers after it are
    /// incomplete.
    failure: parking_lot::Mutex<Option<String>>,
    paths: parking_lot::Mutex<HashMap<String, PathBuf>>,
    /// The root made canonical once, which [`Self::resolve`] joins regular
    /// files' paths to.
    canonical_root: std::sync::OnceLock<PathBuf>,
}

/// Whether [`resolve_store_path`] makes `store_path` canonical without
/// asking the file system: a plain relative path the checkout records as a
/// regular file.
fn resolves_lexically(source: &dyn CallGraphFiles, store_path: &str) -> bool {
    plain_relative(store_path) && source.is_regular_file(store_path)
}

/// `store_path` (a path of `source`'s checkout at `root`) made absolute and
/// canonical, as the snapshot names files. Canonicalizing costs several
/// system calls a path on macOS and opens a file handle on Windows, and a
/// rollup resolves every file: a regular file's path is joined to the root,
/// made canonical once in `canonical_root`, which gives the same path since
/// no folder below the root on it is a symlink (see
/// [`CallGraphFiles::is_regular_file`]). Any other path is canonicalized.
fn resolve_store_path(
    root: &Path,
    canonical_root: &std::sync::OnceLock<PathBuf>,
    source: &dyn CallGraphFiles,
    store_path: &str,
) -> PathBuf {
    if !resolves_lexically(source, store_path) {
        return Paths {
            root,
            cache: HashMap::new(),
        }
        .resolve(store_path);
    }
    let mut path = canonical_root
        .get_or_init(|| crate::inspect::job::canonicalize_normalized(root))
        .clone();
    for segment in store_path.split('/') {
        path.push(segment);
    }
    crate::inspect::job::normalize_path(&path)
}

/// Whether `path` is a relative path of plain segments: none empty, `.`,
/// `..`, or (on Windows) holding a separator or drive of its own.
fn plain_relative(path: &str) -> bool {
    !path.is_empty()
        && !Path::new(path).is_absolute()
        && path.split('/').all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && !(cfg!(windows) && segment.contains(['\\', ':']))
        })
}

/// What one caller file contributes to the dead-code snapshot.
#[derive(Debug, Default)]
pub(crate) struct FileProjection {
    /// Its outbound calls, as `snapshot` lists them for this caller file
    /// (in no particular order).
    pub(crate) outbound_calls: Vec<CallgraphOutboundCall>,
    /// Members an unknown receiver in this file keeps alive: target file
    /// (absolute, as the snapshot names files), symbol, and the line of the
    /// call site in this file (a site in test code keeps them alive for
    /// tests only).
    pub(crate) unknown_live: Vec<(PathBuf, String, u32)>,
    /// Its Go package initialization group, when it has callers.
    pub(crate) go_initializers: Option<GoInitializerGroup>,
}

/// The exported symbols of `file`, named `resolved`, in the snapshot's
/// order for that file: by line, name, kind and node id, a default export
/// after the plain export of the same node.
fn file_exports(file: &IndexedFile, resolved: PathBuf) -> Vec<CallgraphExport> {
    let mut nodes = file
        .nodes
        .iter()
        .filter(|node| node.exported || node.is_default_export)
        .collect::<Vec<_>>();
    nodes.sort_by(|left, right| {
        (&left.start_line, &left.name, &left.kind, &left.id).cmp(&(
            &right.start_line,
            &right.name,
            &right.kind,
            &right.id,
        ))
    });
    let mut exports = Vec::new();
    for node in nodes {
        let line = node.start_line.saturating_add(1);
        if node.exported {
            exports.push(CallgraphExport {
                file: resolved.clone(),
                symbol: node.name.to_string(),
                kind: node.kind.to_string(),
                line,
            });
        }
        if node.is_default_export {
            exports.push(CallgraphExport {
                file: resolved.clone(),
                symbol: node.name.to_string(),
                kind: DEFAULT_EXPORT_MARKER_KIND.to_string(),
                line,
            });
        }
    }
    exports
}

/// Every parsed file of `source` as a dead-code snapshot lists them:
/// absolute under `root`, canonical, and sorted.
pub(crate) fn snapshot_files_of(root: &Path, source: &dyn CallGraphFiles) -> Vec<PathBuf> {
    let canonical_root = std::sync::OnceLock::new();
    let mut files = source
        .paths()
        .iter()
        .map(|path| resolve_store_path(root, &canonical_root, source, path))
        .collect::<Vec<_>>();
    files.sort_by(|left, right| path_key(left).cmp(path_key(right)));
    files
}

impl<'f> DeadCodeGraph<'f> {
    /// Reads every file's symbols from `source`.
    ///
    /// # Errors
    ///
    /// When a file's parse cannot be read.
    pub(crate) fn open(root: &Path, source: &'f dyn CallGraphFiles) -> Result<Self, String> {
        use rayon::prelude::*;
        let graph = Self::open_lazy(root, source);
        graph.files.par_iter().for_each(|(path, _)| {
            let _ = graph.file(path);
        });
        match graph.failure() {
            Some(failure) => Err(failure),
            None => Ok(graph),
        }
    }

    /// Whether files only called into are read as their [`TargetIndex`]
    /// alone: on for a rollup that projects a few files, since their
    /// callees' full symbols would otherwise stay in memory for the whole
    /// rollup; off for one that projects every file, which reads every file
    /// in full anyway and would otherwise read most files twice.
    pub(crate) fn compact_targets(&self, compact: bool) {
        self.compact_targets
            .store(compact, std::sync::atomic::Ordering::Relaxed);
    }

    /// A graph of `source`'s files that reads each file's symbols when first
    /// asked for. A parse that cannot be read leaves the file without
    /// symbols and is reported by [`Self::failure`].
    pub(crate) fn open_lazy(root: &Path, source: &'f dyn CallGraphFiles) -> Self {
        Self {
            root: root.to_path_buf(),
            source,
            files: source
                .paths()
                .into_iter()
                .map(|path| (path, FileCell::default()))
                .collect(),
            compact_targets: std::sync::atomic::AtomicBool::new(false),
            go_packages: std::sync::OnceLock::new(),
            failure: parking_lot::Mutex::new(None),
            paths: parking_lot::Mutex::new(HashMap::new()),
            canonical_root: std::sync::OnceLock::new(),
        }
    }

    /// The first parse that could not be read, if any.
    pub(crate) fn failure(&self) -> Option<String> {
        self.failure.lock().clone()
    }

    /// Whether `path` is a parsed file.
    pub(crate) fn has_file(&self, path: &str) -> bool {
        self.files.contains_key(path)
    }

    /// `path`'s symbols, read from its parse the first time; `None` when it
    /// is not a parsed file or its parse cannot be read.
    fn file(&self, path: &str) -> Option<&IndexedFile> {
        self.files
            .get(path)?
            .full
            .get_or_init(|| match self.source.parse(path) {
                Ok(parse) => Some(IndexedFile::build(path, &parse)),
                Err(error) => {
                    self.failure.lock().get_or_insert(error);
                    None
                }
            })
            .as_ref()
    }

    /// The checkout's root.
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// Every parsed file, in path order (store paths, relative to the root).
    pub(crate) fn file_paths(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(String::as_str)
    }

    /// Whether [`Self::resolve`] makes `store_path` canonical without asking
    /// the file system (see [`resolve_store_path`]).
    pub(crate) fn resolves_lexically(&self, store_path: &str) -> bool {
        resolves_lexically(self.source, store_path)
    }

    /// `store_path` made absolute and canonical, as the snapshot names files
    /// (see [`resolve_store_path`]).
    pub(crate) fn resolve(&self, store_path: &str) -> PathBuf {
        if let Some(path) = self.paths.lock().get(store_path) {
            return path.clone();
        }
        let resolved =
            resolve_store_path(&self.root, &self.canonical_root, self.source, store_path);
        self.paths
            .lock()
            .insert(store_path.to_string(), resolved.clone());
        resolved
    }

    /// The exported symbols of `path`, in the snapshot's order for that
    /// file: by line, name, kind and node id, a default export after the
    /// plain export of the same node.
    pub(crate) fn exported_symbols(&self, path: &str) -> Vec<CallgraphExport> {
        let Some(file) = self.indexed(path) else {
            return Vec::new();
        };
        file_exports(file, self.resolve(path))
    }

    /// Every file's exported symbols, in the snapshot's order: by resolved
    /// file, then as [`Self::exported_symbols`] orders one file's.
    pub(crate) fn all_exported_symbols(&self) -> Vec<CallgraphExport> {
        let mut exports = self
            .files
            .keys()
            .flat_map(|path| self.exported_symbols(path))
            .collect::<Vec<_>>();
        exports.sort_by(|left, right| path_key(&left.file).cmp(path_key(&right.file)));
        exports
    }

    /// The symbols of `path` that keep themselves alive by their own marks
    /// (the snapshot's `entry_point_symbols` for the file, before the
    /// members unknown receivers keep alive are added).
    pub(crate) fn own_entry_point_symbols(&self, path: &str) -> BTreeSet<String> {
        let mut roots = BTreeSet::new();
        let Some(file) = self.indexed(path) else {
            return roots;
        };
        let mut nodes = file
            .nodes
            .iter()
            .filter(|node| node.entry_point)
            .collect::<Vec<_>>();
        nodes.sort_by(|left, right| {
            (&left.start_line, &left.name, &left.kind, &left.id).cmp(&(
                &right.start_line,
                &right.name,
                &right.kind,
                &right.id,
            ))
        });
        for node in nodes {
            let Some(kind) = symbol_kind_from_label(&node.kind) else {
                continue;
            };
            let Some(lang) = lang_from_label(&file.language) else {
                continue;
            };
            if crate::callgraph::is_entry_point(&node.scoped_name, &kind, node.exported, lang) {
                continue;
            }
            roots.insert(node.name.to_string());
            if node.scoped_name != node.name {
                roots.insert(node.scoped_name.to_string());
            }
        }
        roots
    }

    /// `path`'s language as its parse names it.
    pub(crate) fn language(&self, path: &str) -> Option<&str> {
        self.file(path).map(|file| &*file.language)
    }

    /// Every Go package folder's files, in path order. The first call reads
    /// every file's language, unless [`Self::seed_go_packages`] gave them.
    pub(crate) fn go_packages(&self) -> &BTreeMap<PathBuf, Vec<String>> {
        self.go_packages.get_or_init(|| {
            let mut go_packages = BTreeMap::<PathBuf, Vec<String>>::new();
            for path in self.files.keys() {
                if self.language(path) == Some("go") {
                    go_packages
                        .entry(parent(path))
                        .or_default()
                        .push(path.clone());
                }
            }
            go_packages
        })
    }

    /// Gives the Go packages worked out elsewhere (from a kept dead-code
    /// graph), so asking for one reads no file's language.
    pub(crate) fn seed_go_packages(&self, packages: BTreeMap<PathBuf, Vec<String>>) {
        let _ = self.go_packages.set(packages);
    }

    /// `kept` Go packages (of an earlier view) brought up to this graph for
    /// the `changed` files, whose languages are read; every other file is
    /// taken to be where `kept` has it. The result is also this graph's.
    pub(crate) fn patch_go_packages(
        &self,
        mut kept: BTreeMap<PathBuf, Vec<String>>,
        changed: &BTreeSet<String>,
    ) -> BTreeMap<PathBuf, Vec<String>> {
        for path in changed {
            let dir = parent(path);
            if let Some(files) = kept.get_mut(&dir) {
                files.retain(|file| file != path);
                if files.is_empty() {
                    kept.remove(&dir);
                }
            }
            if self.has_file(path) && self.language(path) == Some("go") {
                let files = kept.entry(dir).or_default();
                let at = files.partition_point(|file| file < path);
                files.insert(at, path.clone());
            }
        }
        self.seed_go_packages(kept.clone());
        kept
    }

    /// `path`'s symbols, recording the read for dead code's record of what
    /// each file's materialization depends on.
    fn indexed(&self, path: &str) -> Option<&IndexedFile> {
        record(Lookup::GraphFile, path);
        self.file(path)
    }

    /// `path`'s symbols as a call into it reads them, recording the read as
    /// [`Self::indexed`] does. A file whose symbols were read in full answers
    /// from them; any other is read into its [`TargetIndex`] alone.
    fn target(&self, path: &str) -> Option<Target<'_>> {
        if !self
            .compact_targets
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return self.indexed(path).map(Target::Full);
        }
        record(Lookup::GraphFile, path);
        let cell = self.files.get(path)?;
        if let Some(full) = cell.full.get() {
            return full.as_ref().map(Target::Full);
        }
        cell.target
            .get_or_init(|| match self.source.parse(path) {
                Ok(parse) => Some(TargetIndex::of(&IndexedFile::build(path, &parse))),
                Err(error) => {
                    self.failure.lock().get_or_insert(error);
                    None
                }
            })
            .as_ref()
            .map(Target::Index)
    }

    fn node(&self, file: &str, id: &str) -> Option<&IndexedNode> {
        self.indexed(file)?.node(id)
    }

    fn lookup(&self, file: &str, symbol: &str) -> Option<&str> {
        self.indexed(file)?.lookup.get(symbol).map(|id| &**id)
    }

    /// Members unknown receivers in caller file `path` keep alive: target
    /// file (absolute, as the snapshot names files), symbol and the line of
    /// the call site. Reads only the file's method-call sites, not the rest
    /// of its rows.
    ///
    /// # Errors
    ///
    /// When its resolution cannot be read.
    pub(crate) fn unknown_live(&self, path: &str) -> Result<Vec<(PathBuf, String, u32)>, String> {
        if self.indexed(path).is_none() {
            return Ok(Vec::new());
        }
        let resolution = self.source.resolution(path)?;
        let live = resolution
            .sites
            .iter()
            .flat_map(|kept| {
                kept.resolution
                    .protected
                    .iter()
                    .map(|(file, symbol)| (file.clone(), symbol.clone(), kept.site.line))
            })
            .collect::<BTreeSet<(String, String, u32)>>();
        Ok(live
            .into_iter()
            .map(|(file, symbol, line)| (self.resolve(&file), symbol, line))
            .collect())
    }

    /// Projects caller file `path`.
    ///
    /// # Errors
    ///
    /// When its parse or resolution cannot be read.
    pub(crate) fn project_file(&self, path: &str) -> Result<FileProjection, String> {
        let Some(file) = self.indexed(path) else {
            return Ok(FileProjection::default());
        };
        let parse = self.source.parse(path)?;
        let resolution = self.source.resolution(path)?;
        let rows = self.file_rows(path, &parse, &resolution);
        let mut projection = FileProjection::default();
        for (_, target_file, symbol, line) in &rows.unknown_live {
            projection
                .unknown_live
                .push((self.resolve(target_file), symbol.clone(), *line));
        }
        projection.outbound_calls = self.outbound_calls(&rows);
        if &*file.language == "go" {
            projection.go_initializers = self.go_initializers(path, file, &rows);
        }
        Ok(projection)
    }

    /// The rows `graph_rows::assemble` builds from caller file `path`.
    fn file_rows(&self, path: &str, parse: &ParseBlob, resolution: &FileResolution) -> FileRows {
        let mut rows = FileRows::default();
        let references = references_by_key(parse);
        let mut derived = resolution.rows.iter().collect::<Vec<_>>();
        derived.sort();
        for row in derived {
            let Ok(caller_path) = std::str::from_utf8(&row.caller_path) else {
                continue;
            };
            // A file's resolution holds only its own references; a row of
            // another caller file is that file's to project.
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
                .and_then(|symbol| self.lookup(caller_path, symbol))
                .map(str::to_string);
            let target_node = target_path
                .as_deref()
                .zip(row.target_symbol.as_deref())
                .and_then(|(path, symbol)| self.target(path)?.lookup(symbol))
                .map(str::to_string);
            let ref_id = format!("view:{caller_path}:{}", row.ref_ordinal);
            let macro_body = macro_body_call(row.kind, reference);
            if let (Some(source_node), Some(target_file), Some(target_symbol), true) = (
                caller_node.clone(),
                target_path.clone(),
                row.target_symbol.clone(),
                matches!(
                    row.kind,
                    BlobRefKind::Call | BlobRefKind::Construction | BlobRefKind::CallbackRef
                ),
            ) {
                // A construction is a call when it targets a function or
                // method (a constructor), and a value reference otherwise.
                let kind = match row.kind {
                    BlobRefKind::Construction
                        if !target_node
                            .as_deref()
                            .and_then(|id| self.target(&target_file)?.node(id))
                            .is_some_and(|(_, kind)| matches!(kind, "function" | "method")) =>
                    {
                        "value_ref"
                    }
                    BlobRefKind::CallbackRef => "callback_ref",
                    _ if macro_body => MACRO_BODY_CALL_REF_KIND,
                    _ => "call",
                };
                rows.insert_edge(EdgeRow {
                    edge_id: format!("edge:{ref_id}"),
                    ref_id: ref_id.clone(),
                    source_node,
                    target_node: target_node.clone(),
                    target_file,
                    target_symbol,
                    kind,
                    line: reference.line,
                    provenance: PROVENANCE_TREESITTER.to_string(),
                });
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
                    module_path: None,
                    import_kind: None,
                    local_name: None,
                    requested_name: None,
                    namespace_alias: None,
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

        // Method-call sites, in extraction order: each replaces its
        // reference's ordinary edge and target.
        for kept in &resolution.sites {
            let site = &kept.site;
            let site_resolution = &kept.resolution;
            let ref_id = format!("view:{path}:{}", site.ordinal);
            let ordinary = format!("edge:{ref_id}");
            if rows.edges.get(&ordinary).is_some_and(|(_, edge)| {
                !matches!(
                    edge.provenance.as_str(),
                    "exact" | "dispatch" | "name_match"
                )
            }) {
                rows.edges.remove(&ordinary);
            }
            let status = if site_resolution.targets.is_empty() {
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
            for (target_file, target_symbol) in &site_resolution.protected {
                rows.unknown_live.insert((
                    path.to_string(),
                    target_file.clone(),
                    target_symbol.clone(),
                    site.line,
                ));
            }
            let Some(caller) = site.caller.as_deref().and_then(|symbol| {
                let file = self.indexed(path)?;
                let index = file.by_scoped_name.get(symbol)?;
                Some(file.nodes[*index as usize].id.to_string())
            }) else {
                continue;
            };
            for target in &site_resolution.targets {
                let edge_id = format!(
                    "dispatch:{path}:{}:{}:{}",
                    site.ordinal, target.file, target.symbol
                );
                let target_node = self
                    .target(&target.file)
                    .and_then(|file| file.scoped(target.symbol.as_str()))
                    .map(str::to_string);
                rows.insert_edge(EdgeRow {
                    edge_id,
                    ref_id: ref_id.clone(),
                    source_node: caller.clone(),
                    target_node,
                    target_file: target.file.clone(),
                    target_symbol: target.symbol.clone(),
                    kind: "call",
                    line: site.line,
                    provenance: target.provenance.to_string(),
                });
            }
        }
        rows
    }

    fn outbound_calls(&self, rows: &FileRows) -> Vec<CallgraphOutboundCall> {
        let mut edges_by_ref = HashMap::<(&str, &str), Vec<&(u64, EdgeRow)>>::new();
        for edge in rows.edges.values() {
            edges_by_ref
                .entry((edge.1.ref_id.as_str(), edge.1.kind))
                .or_default()
                .push(edge);
        }
        for edges in edges_by_ref.values_mut() {
            edges.sort_by_key(|(sequence, _)| *sequence);
        }
        let mut calls = Vec::new();
        for reference in rows.refs.values() {
            if !OUTBOUND_KINDS.contains(&reference.kind) {
                continue;
            }
            match edges_by_ref.get(&(reference.ref_id.as_str(), reference.kind)) {
                Some(edges) => {
                    for (_, edge) in edges {
                        calls.extend(self.outbound_call(reference, Some(edge)));
                    }
                }
                None => calls.extend(self.outbound_call(reference, None)),
            }
        }
        calls
    }

    /// `dead_code::outbound_call` for one reference joined to one of its
    /// edges, or to none.
    fn outbound_call(
        &self,
        reference: &RefRow,
        edge: Option<&EdgeRow>,
    ) -> Option<CallgraphOutboundCall> {
        let caller = reference
            .caller_node
            .as_ref()
            .and_then(|id| self.node(&reference.caller_file, id));
        let status = reference.status;
        let provenance = if matches!(reference.kind, "value_ref" | "callback_ref") {
            "value_ref"
        } else {
            edge.map_or(PROVENANCE_TREESITTER, |edge| edge.provenance.as_str())
        };
        let resolved_status = matches!(status, "resolved" | "resolved_local");
        let full_ref = reference.full_ref.as_deref();
        let unresolved_value_path = provenance == PROVENANCE_VALUE_REF
            && !resolved_status
            && full_ref.is_some_and(|full_ref| full_ref.contains("::") || full_ref.contains('.'));
        if provenance == PROVENANCE_VALUE_REF && !resolved_status && !unresolved_value_path {
            return None;
        }
        let caller_symbol = caller.map_or_else(
            || TOP_LEVEL_SYMBOL.to_string(),
            |node| node.name.to_string(),
        );
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
        let target_file = reference
            .target_file
            .as_deref()
            .or_else(|| edge.map(|edge| edge.target_file.as_str()));
        // `COALESCE(tn.name, r.target_symbol, e.target_symbol)`: the edge's
        // target node's plain name first.
        let target_symbol = edge
            .and_then(|edge| {
                let id = edge.target_node.as_deref()?;
                self.target(&edge.target_file)?.node(id)
            })
            .map(|(name, _)| name)
            .or(reference.target_symbol.as_deref())
            .or_else(|| edge.map(|edge| edge.target_symbol.as_str()));
        let mut target = if is_resolved_edge(status, Some(provenance)) {
            match (target_file, target_symbol) {
                (Some(target_file), Some(target_symbol)) => {
                    let target_file = self.resolve(target_file);
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
        Some(CallgraphOutboundCall {
            caller_file: self.resolve(&reference.caller_file),
            caller_symbol,
            target,
            line: reference.line,
            provenance: if unresolved_value_path {
                UNRESOLVED_VALUE_PATH_PROVENANCE.to_string()
            } else {
                provenance.to_string()
            },
        })
    }

    /// `dead_code::go_initializers` for the one caller file `path`.
    fn go_initializers(
        &self,
        path: &str,
        file: &IndexedFile,
        rows: &FileRows,
    ) -> Option<GoInitializerGroup> {
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
        let mut callers = file
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, node)| {
                &*node.scoped_name == "<go-package-init>"
                    || (&*node.kind == "function" && &*node.scoped_name == "main")
                    || calls_out.contains(&*node.id)
            })
            .collect::<Vec<_>>();
        if callers.is_empty() {
            return None;
        }
        // `ORDER BY file_path, scoped_name`; ties keep storage order.
        callers.sort_by(|(left_position, left), (right_position, right)| {
            (&left.scoped_name, left_position).cmp(&(&right.scoped_name, right_position))
        });
        let imports = rows
            .refs
            .values()
            .filter(|reference| reference.kind == "import" && reference.status == "resolved")
            .filter_map(|reference| reference.target_file.as_deref())
            .collect::<BTreeSet<_>>();
        let mut dirs = vec![parent(path)];
        for imported in &imports {
            dirs.push(parent(imported));
        }
        dirs.sort();
        dirs.dedup();
        let mut interned_symbols = BTreeMap::<&str, Arc<str>>::new();
        let mut targets = Vec::new();
        for target_file in dirs
            .iter()
            .filter_map(|dir| {
                record(Lookup::GoPackage, &dir.to_string_lossy());
                self.go_packages().get(dir)
            })
            .flatten()
        {
            if !path.ends_with("_test.go") && target_file.ends_with("_test.go") {
                continue;
            }
            let Some(target) = self.indexed(target_file) else {
                continue;
            };
            let resolved = Arc::new(self.resolve(target_file));
            // Package variables, initializers and `init` functions, in
            // storage order.
            for node in &target.nodes {
                if &*node.kind == "variable"
                    || &*node.scoped_name == "<go-package-init>"
                    || (&*node.kind == "function" && &*node.scoped_name == "init")
                {
                    let symbol = interned_symbols
                        .entry(&node.scoped_name)
                        .or_insert_with(|| Arc::from(&*node.scoped_name))
                        .clone();
                    targets.push((Arc::clone(&resolved), symbol));
                }
            }
        }
        Some(GoInitializerGroup {
            caller_file: self.resolve(path),
            callers: callers
                .into_iter()
                .map(|(_, node)| node.scoped_name.to_string())
                .collect(),
            targets,
        })
    }
}

/// One caller file's references and edges, as `graph_rows::assemble` stores
/// them.
#[derive(Default)]
struct FileRows {
    refs: BTreeMap<String, RefRow>,
    /// Each edge with its place in insertion order: a query joining a
    /// reference to its edges reads them in this order.
    edges: BTreeMap<String, (u64, EdgeRow)>,
    next_edge: u64,
    /// Members unknown receivers keep alive: caller file, target file,
    /// target symbol and the line of the call site.
    unknown_live: BTreeSet<(String, String, String, u32)>,
}

impl FileRows {
    fn insert_edge(&mut self, edge: EdgeRow) {
        let sequence = self.next_edge;
        self.next_edge += 1;
        self.edges.insert(edge.edge_id.clone(), (sequence, edge));
    }
}

fn parent(path: &str) -> PathBuf {
    Path::new(path)
        .parent()
        .unwrap_or(Path::new(""))
        .to_path_buf()
}
