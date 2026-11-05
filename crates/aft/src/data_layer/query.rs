//! The call graph's queries over file slices.
//!
//! Each query reads the slices of the files it needs, found through a few
//! lookups a [`SliceSource`] provides (which files call a target, which use
//! a name), and answers with the filters and order today's SQLite queries
//! used, so commands built on them answer the same. Where a row needs
//! another file's data (a target's node, whether a construction targets a
//! function), it reads that file's slice.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::slice::{FileSlice, SliceEdge, SliceNode, SliceRef};
use crate::callgraph::{self, TraceToSymbolCandidate};
use crate::callgraph_store::{
    lang_from_label, lang_label, normalize_file_path, relative_path, CallGraphRead,
    CallGraphStoreError, EntryFacts, Result, StoreCallSite, StoreCallersResult, StoreImpactResult,
    StoreNode, StoreRefSite, StoreUnresolvedCall, StoredEdge, UNRESOLVED_CALL_SITE_COUNT_LIMIT,
};
use crate::error::AftError;
use crate::parser::LangId;

/// Where queries find slices and the lookups that narrow which to read.
pub(crate) trait SliceSource {
    fn slice(&self, path: &str) -> Option<Arc<FileSlice>>;
    /// Every file with an edge to `target_symbol` in `target_file`.
    fn caller_files(&self, target_file: &str, target_symbol: &str) -> BTreeSet<String>;
    /// Every file with a reference of this short name.
    fn files_with_ref_name(&self, short_name: &str) -> BTreeSet<String>;
    /// Every file declaring a symbol with this name, scoped name, or (Rust)
    /// scoped name after ` for `.
    fn files_with_symbol_name(&self, name: &str) -> BTreeSet<String>;
    /// Every file parsed as `language`.
    fn files_of_language(&self, language: &str) -> BTreeSet<String>;
    /// Every file, in path order.
    fn files(&self) -> Vec<String>;
}

impl<T: SliceSource + ?Sized> SliceSource for Arc<T> {
    fn slice(&self, path: &str) -> Option<Arc<FileSlice>> {
        (**self).slice(path)
    }
    fn caller_files(&self, target_file: &str, target_symbol: &str) -> BTreeSet<String> {
        (**self).caller_files(target_file, target_symbol)
    }
    fn files_with_ref_name(&self, short_name: &str) -> BTreeSet<String> {
        (**self).files_with_ref_name(short_name)
    }
    fn files_with_symbol_name(&self, name: &str) -> BTreeSet<String> {
        (**self).files_with_symbol_name(name)
    }
    fn files_of_language(&self, language: &str) -> BTreeSet<String> {
        (**self).files_of_language(language)
    }
    fn files(&self) -> Vec<String> {
        (**self).files()
    }
}

/// Slices kept in memory, with their lookups.
#[derive(Clone, Debug, Default)]
pub(crate) struct MemorySlices {
    slices: BTreeMap<String, Arc<FileSlice>>,
    callers: HashMap<(String, String), BTreeSet<String>>,
    ref_names: HashMap<String, BTreeSet<String>>,
    symbol_names: HashMap<String, BTreeSet<String>>,
    languages: HashMap<String, BTreeSet<String>>,
}

impl MemorySlices {
    /// Adds or replaces the slice of its file.
    pub(crate) fn insert(&mut self, slice: FileSlice) {
        self.remove(&slice.path.clone());
        let path = slice.path.clone();
        for edge in slice.edges.values() {
            self.callers
                .entry((edge.target_file.clone(), edge.target_symbol.clone()))
                .or_default()
                .insert(path.clone());
        }
        for reference in slice.refs.values() {
            if let Some(name) = &reference.short_name {
                self.ref_names
                    .entry(name.clone())
                    .or_default()
                    .insert(path.clone());
            }
        }
        for name in symbol_keys(&slice) {
            self.symbol_names
                .entry(name)
                .or_default()
                .insert(path.clone());
        }
        self.languages
            .entry(slice.language.clone())
            .or_default()
            .insert(path.clone());
        self.slices.insert(path, Arc::new(slice));
    }

    /// Removes the slice of `path`.
    pub(crate) fn remove(&mut self, path: &str) {
        let Some(slice) = self.slices.remove(path) else {
            return;
        };
        for edge in slice.edges.values() {
            let key = (edge.target_file.clone(), edge.target_symbol.clone());
            if let Some(files) = self.callers.get_mut(&key) {
                files.remove(path);
                if files.is_empty() {
                    self.callers.remove(&key);
                }
            }
        }
        for reference in slice.refs.values() {
            if let Some(files) = reference
                .short_name
                .as_ref()
                .and_then(|name| self.ref_names.get_mut(name))
            {
                files.remove(path);
            }
        }
        for name in symbol_keys(&slice) {
            if let Some(files) = self.symbol_names.get_mut(&name) {
                files.remove(path);
            }
        }
        if let Some(files) = self.languages.get_mut(&slice.language) {
            files.remove(path);
        }
    }
}

/// The names a slice's symbols can be found by.
pub(crate) fn symbol_keys(slice: &FileSlice) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    for node in &slice.nodes {
        keys.insert(node.name.clone());
        keys.insert(node.scoped_name.clone());
        if slice.language == "rust" {
            if let Some((_, implementation)) = node.scoped_name.split_once(" for ") {
                keys.insert(implementation.to_string());
            }
        }
    }
    keys
}

impl SliceSource for MemorySlices {
    fn slice(&self, path: &str) -> Option<Arc<FileSlice>> {
        self.slices.get(path).cloned()
    }
    fn caller_files(&self, target_file: &str, target_symbol: &str) -> BTreeSet<String> {
        self.callers
            .get(&(target_file.to_string(), target_symbol.to_string()))
            .cloned()
            .unwrap_or_default()
    }
    fn files_with_ref_name(&self, short_name: &str) -> BTreeSet<String> {
        self.ref_names.get(short_name).cloned().unwrap_or_default()
    }
    fn files_with_symbol_name(&self, name: &str) -> BTreeSet<String> {
        self.symbol_names.get(name).cloned().unwrap_or_default()
    }
    fn files_of_language(&self, language: &str) -> BTreeSet<String> {
        self.languages.get(language).cloned().unwrap_or_default()
    }
    fn files(&self) -> Vec<String> {
        self.slices.keys().cloned().collect()
    }
}

/// A call-graph reader over slices.
pub(crate) struct SliceReader<S> {
    source: S,
    project_root: PathBuf,
    project_key: String,
}

impl<S: SliceSource> SliceReader<S> {
    pub(crate) fn new(source: S, project_root: PathBuf, project_key: String) -> Self {
        Self {
            source,
            project_root,
            project_key,
        }
    }

    pub(crate) fn source(&self) -> &S {
        &self.source
    }

    fn rel(&self, file_rel: &Path) -> Result<String> {
        let absolute = normalize_file_path(&self.project_root, file_rel)?;
        Ok(relative_path(&self.project_root, &absolute))
    }
}

fn store_node(slice: &FileSlice, node: &SliceNode) -> StoreNode {
    StoreNode::from_parts(
        node.id.clone(),
        slice.path.clone(),
        node.scoped_name.clone(),
        node.name.clone(),
        node.kind.clone(),
        node.start_line.saturating_add(1),
        node.end_line.saturating_add(1),
        node.signature.clone(),
        node.exported,
        node.entry_point,
        lang_from_label(&slice.language).unwrap_or(LangId::TypeScript),
    )
    .with_entry(EntryFacts {
        is_test: node.is_test,
        program_start: node.program_start,
        entry_attribute: node.entry_attribute.clone(),
        default_export: node.is_default_export,
    })
}

/// The scoped name after ` for `, with generic arguments removed.
fn implementation_name(scoped: &str) -> String {
    let suffix = scoped
        .split_once(" for ")
        .map_or(scoped, |(_, implementation)| implementation);
    let mut depth = 0usize;
    suffix
        .chars()
        .filter(|ch| match ch {
            '<' => {
                depth += 1;
                false
            }
            '>' if depth > 0 => {
                depth -= 1;
                false
            }
            _ => depth == 0,
        })
        .collect()
}

/// Whether `node` matches a symbol query as the database's node queries do.
fn matches_symbol(slice: &FileSlice, node: &SliceNode, symbol: &str) -> bool {
    if symbol.contains("::") {
        node.scoped_name == symbol
            || (slice.language == "rust"
                && node
                    .scoped_name
                    .split_once(" for ")
                    .is_some_and(|(_, implementation)| implementation == symbol))
    } else {
        node.scoped_name == symbol || node.name == symbol
    }
}

/// `ORDER BY scoped_name, start_line, start_col`.
fn node_order(a: &SliceNode, b: &SliceNode) -> std::cmp::Ordering {
    (&a.scoped_name, a.start_line, a.start_col).cmp(&(&b.scoped_name, b.start_line, b.start_col))
}

impl<S: SliceSource> SliceReader<S> {
    fn nodes_in_file(&self, file: &str, symbol: &str) -> Vec<StoreNode> {
        let Some(slice) = self.source.slice(file) else {
            return Vec::new();
        };
        let mut nodes = slice
            .nodes
            .iter()
            .filter(|node| matches_symbol(&slice, node, symbol))
            .collect::<Vec<_>>();
        nodes.sort_by(|a, b| node_order(a, b));
        let matched = nodes
            .into_iter()
            .map(|node| store_node(&slice, node))
            .collect::<Vec<_>>();
        if symbol.contains("::") && matched.is_empty() {
            let short = symbol.rsplit("::").next().unwrap_or(symbol);
            return self
                .nodes_in_file(file, short)
                .into_iter()
                .filter(|node| {
                    node.lang == LangId::Rust && implementation_name(&node.symbol) == symbol
                })
                .collect();
        }
        matched
    }

    fn all_nodes_matching(&self, symbol: &str) -> Vec<StoreNode> {
        let mut nodes = Vec::new();
        for file in self.source.files_with_symbol_name(symbol) {
            let Some(slice) = self.source.slice(&file) else {
                continue;
            };
            let mut found = slice
                .nodes
                .iter()
                .filter(|node| matches_symbol(&slice, node, symbol))
                .collect::<Vec<_>>();
            found.sort_by(|a, b| node_order(a, b));
            nodes.extend(found.into_iter().map(|node| store_node(&slice, node)));
        }
        nodes
    }

    /// Nodes of `file` whose scoped name is exactly `symbol`, ordered as
    /// the batched node lookup orders them.
    fn nodes_scoped_exactly(&self, file: &str, symbol: &str) -> Vec<StoreNode> {
        let Some(slice) = self.source.slice(file) else {
            return Vec::new();
        };
        let mut nodes = slice
            .nodes
            .iter()
            .filter(|node| node.scoped_name == symbol)
            .collect::<Vec<_>>();
        nodes.sort_by(|a, b| {
            (
                &a.scoped_name,
                a.start_line,
                a.end_line,
                a.start_col,
                a.range_ordinal,
            )
                .cmp(&(
                    &b.scoped_name,
                    b.start_line,
                    b.end_line,
                    b.start_col,
                    b.range_ordinal,
                ))
        });
        nodes
            .into_iter()
            .map(|node| store_node(&slice, node))
            .collect()
    }

    /// The node an edge or reference targets, when its file is indexed and
    /// the node survives there.
    fn target_of(
        &self,
        file: &str,
        symbol: &str,
        lookup: super::slice::TargetLookup,
    ) -> (Option<String>, Option<StoreNode>) {
        let Some(slice) = self.source.slice(file) else {
            return (None, None);
        };
        let Some(id) = slice.target_node(symbol, lookup) else {
            return (None, None);
        };
        let node = slice.node(id).map(|node| store_node(&slice, node));
        (Some(id.to_string()), node)
    }

    /// An edge's kind as stored: a construction is a call when its target
    /// is a function or method, otherwise a value reference.
    fn edge_kind(&self, edge: &SliceEdge) -> &'static str {
        if edge.kind != "construction" {
            return edge.kind;
        }
        let target_is_callable = self.source.slice(&edge.target_file).is_some_and(|slice| {
            slice
                .target_node(&edge.target_symbol, edge.lookup)
                .and_then(|id| slice.node_kind(id))
                .is_some_and(|kind| matches!(kind, "function" | "method"))
        });
        if target_is_callable {
            "call"
        } else {
            "value_ref"
        }
    }

    /// Direct callers of one target, in the database's order for one target.
    fn callers_of_target(
        &self,
        target_file: &str,
        target_symbol: &str,
    ) -> Vec<(CallerKey, StoreCallSite)> {
        let mut sites = Vec::new();
        for file in self.source.caller_files(target_file, target_symbol) {
            let Some(slice) = self.source.slice(&file) else {
                continue;
            };
            for edge in slice.edges.values() {
                if edge.target_file != target_file || edge.target_symbol != target_symbol {
                    continue;
                }
                let kind = self.edge_kind(edge);
                if !matches!(kind, "call" | "callback_ref") {
                    continue;
                }
                let Some(reference) = slice.refs.get(&edge.ref_id) else {
                    continue;
                };
                let Some(source) = slice.node(&edge.source_node) else {
                    continue;
                };
                let (target_node, target) = self.target_of(target_file, target_symbol, edge.lookup);
                sites.push((
                    CallerKey {
                        source_node: edge.source_node.clone(),
                        byte_start: reference.byte_start,
                        line: reference.line,
                        ref_id: reference.ref_id.clone(),
                        target_node: target_node.clone(),
                        provenance: edge.provenance,
                        kind,
                    },
                    StoreCallSite {
                        kind: kind.to_string(),
                        caller_is_test: source.is_test,
                        caller: store_node(&slice, source),
                        target_file: target_file.to_string(),
                        target_symbol: target_symbol.to_string(),
                        target,
                        line: edge.line,
                        byte_start: usize::try_from(reference.byte_start).unwrap_or(usize::MAX),
                        byte_end: usize::try_from(reference.byte_end).unwrap_or(usize::MAX),
                        resolved: reference.resolved,
                        provenance: edge.provenance.to_string(),
                    },
                ));
            }
        }
        sites.sort_by(|a, b| a.0.cmp(&b.0));
        sites
    }

    /// Edges whose source is `node`, of kind `call`, in the database's
    /// outgoing order, with their target nodes.
    fn outgoing_of(&self, node: &StoreNode) -> Vec<(OutgoingKey, StoreCallSite)> {
        let Some(slice) = self.source.slice(&node.file) else {
            return Vec::new();
        };
        let mut calls = Vec::new();
        for edge in slice.edges.values() {
            if edge.source_node != node.node_id() || self.edge_kind(edge) != "call" {
                continue;
            }
            let Some(reference) = slice.refs.get(&edge.ref_id) else {
                continue;
            };
            let (target_node, target) =
                self.target_of(&edge.target_file, &edge.target_symbol, edge.lookup);
            calls.push((
                OutgoingKey {
                    byte_start: reference.byte_start,
                    line: reference.line,
                    ref_id: reference.ref_id.clone(),
                    target_file: edge.target_file.clone(),
                    target_symbol: edge.target_symbol.clone(),
                    target_node,
                    provenance: edge.provenance,
                },
                StoreCallSite {
                    kind: "call".to_string(),
                    caller_is_test: None,
                    caller: node.clone(),
                    target_file: edge.target_file.clone(),
                    target_symbol: edge.target_symbol.clone(),
                    target,
                    line: edge.line,
                    byte_start: usize::try_from(reference.byte_start).unwrap_or(usize::MAX),
                    byte_end: usize::try_from(reference.byte_end).unwrap_or(usize::MAX),
                    resolved: reference.resolved,
                    provenance: edge.provenance.to_string(),
                },
            ));
        }
        calls.sort_by(|a, b| a.0.cmp(&b.0));
        calls
    }

    /// Whether a `call` edge exists for the reference `ref_id` of `slice`.
    fn has_call_edge(&self, slice: &FileSlice, ref_id: &str) -> bool {
        slice
            .edges
            .values()
            .any(|edge| edge.ref_id == ref_id && self.edge_kind(edge) == "call")
    }

    /// Unresolved call or construction references of `language`, one per
    /// caller file and byte span, as the unresolved-site queries group them.
    fn unresolved_spans(
        &self,
        language: &str,
        name: Option<&str>,
        member_call: bool,
    ) -> Vec<(String, u64, u64, Vec<SliceRef>, Arc<FileSlice>)> {
        let mut spans = Vec::new();
        self.visit_unresolved_spans(
            language,
            name,
            member_call,
            |file, span, references, slice| {
                spans.push((
                    file.to_string(),
                    span.0,
                    span.1,
                    references.to_vec(),
                    Arc::clone(slice),
                ));
                ControlFlow::Continue(())
            },
        );
        spans
    }

    /// Calls `visit` with each span [`Self::unresolved_spans`] lists, in
    /// the same order, until it breaks. Reads files one at a time, so a
    /// caller that stops early reads no further.
    fn visit_unresolved_spans(
        &self,
        language: &str,
        name: Option<&str>,
        member_call: bool,
        mut visit: impl FnMut(&str, (u64, u64), &[SliceRef], &Arc<FileSlice>) -> ControlFlow<()>,
    ) {
        // The named count matches the name exactly, an empty one included;
        // the method-site query treats an empty name as every site.
        let name = name.filter(|name| !(member_call && name.is_empty()));
        let files = match name {
            Some(name) => self
                .source
                .files_with_ref_name(name)
                .intersection(&self.source.files_of_language(language))
                .cloned()
                .collect::<BTreeSet<_>>(),
            _ => self.source.files_of_language(language),
        };
        for file in files {
            let Some(slice) = self.source.slice(&file) else {
                continue;
            };
            // Each reference's edges, so the call-edge check below reads only
            // the reference's own.
            let mut edges_by_ref = HashMap::<&str, Vec<&SliceEdge>>::new();
            for edge in slice.edges.values() {
                edges_by_ref
                    .entry(edge.ref_id.as_str())
                    .or_default()
                    .push(edge);
            }
            let mut groups = BTreeMap::<(u64, u64), Vec<SliceRef>>::new();
            for reference in slice.refs.values() {
                if !matches!(reference.kind, "call" | "construction") || reference.resolved {
                    continue;
                }
                if let Some(name) = name {
                    if reference.short_name.as_deref() != Some(name) {
                        continue;
                    }
                    if member_call
                        && !reference
                            .full_ref
                            .as_deref()
                            .is_some_and(|full| full.contains('.'))
                    {
                        continue;
                    }
                }
                let has_call_edge = edges_by_ref
                    .get(reference.ref_id.as_str())
                    .is_some_and(|edges| edges.iter().any(|edge| self.edge_kind(edge) == "call"));
                if has_call_edge {
                    continue;
                }
                groups
                    .entry((reference.byte_start, reference.byte_end))
                    .or_default()
                    .push(reference.clone());
            }
            for (span, references) in groups {
                if visit(&file, span, &references, &slice).is_break() {
                    return;
                }
            }
        }
    }
}

/// The database's order for direct callers of one target.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct CallerKey {
    source_node: String,
    byte_start: u64,
    line: u32,
    ref_id: String,
    target_node: Option<String>,
    provenance: &'static str,
    kind: &'static str,
}

/// The database's order for outgoing calls of one node.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct OutgoingKey {
    byte_start: u64,
    line: u32,
    ref_id: String,
    target_file: String,
    target_symbol: String,
    target_node: Option<String>,
    provenance: &'static str,
}

fn unused(operation: &str) -> CallGraphStoreError {
    CallGraphStoreError::Unavailable(format!(
        "{operation} is composed by the call-graph command adapter from the store's direct lookups"
    ))
}

impl<S: SliceSource> CallGraphRead for SliceReader<S> {
    fn project_root(&self) -> &Path {
        &self.project_root
    }

    fn project_key(&self) -> &str {
        &self.project_key
    }

    fn sqlite_path(&self) -> &Path {
        Path::new("")
    }

    fn is_current(&self) -> bool {
        true
    }

    fn edge_snapshot(&self) -> Result<BTreeSet<StoredEdge>> {
        let mut edges = BTreeSet::new();
        for file in self.source.files() {
            let Some(slice) = self.source.slice(&file) else {
                continue;
            };
            for edge in slice.edges.values() {
                let Some(source) = slice.node(&edge.source_node) else {
                    continue;
                };
                edges.insert(StoredEdge {
                    source_file: file.clone(),
                    source_symbol: source.scoped_name.clone(),
                    target_file: edge.target_file.clone(),
                    target_symbol: edge.target_symbol.clone(),
                    kind: self.edge_kind(edge).to_string(),
                    line: edge.line,
                });
            }
        }
        Ok(edges)
    }

    fn indexed_file_count(&self) -> Result<usize> {
        Ok(self.source.files().len())
    }

    fn node_for(&self, file_rel: &Path, symbol: &str) -> Result<StoreNode> {
        let rel = self.rel(file_rel)?;
        let candidates = self.nodes_in_file(&rel, symbol);
        match candidates.as_slice() {
            [candidate] => Ok(candidate.clone()),
            [] => Err(AftError::SymbolNotFound {
                name: symbol.to_string(),
                file: rel,
            }
            .into()),
            _ => Err(AftError::AmbiguousSymbol {
                name: symbol.to_string(),
                candidates: candidates
                    .iter()
                    .map(|candidate| candidate.symbol.clone())
                    .collect(),
            }
            .into()),
        }
    }

    fn nodes_for(&self, file_rel: &Path, symbol: &str) -> Result<Vec<StoreNode>> {
        let rel = self.rel(file_rel)?;
        Ok(self.nodes_in_file(&rel, symbol))
    }

    fn nodes_for_stored(&self, file: &str, symbol: &str) -> Result<Vec<StoreNode>> {
        Ok(self.nodes_in_file(file, symbol))
    }

    fn stored_content_hash(&self, file: &str) -> Result<Option<String>> {
        Ok(self
            .source
            .slice(file)
            .map(|slice| slice.record_key.clone()))
    }

    fn nodes_matching(&self, symbol: &str) -> Result<Vec<StoreNode>> {
        Ok(self.all_nodes_matching(symbol))
    }

    fn direct_callers_of(&self, file_rel: &Path, symbol: &str) -> Result<Vec<StoreCallSite>> {
        let rel = self.rel(file_rel)?;
        Ok(self
            .callers_of_target(&rel, symbol)
            .into_iter()
            .map(|(_, site)| site)
            .collect())
    }

    fn direct_callers_for_symbols(
        &self,
        targets: &[(String, String)],
    ) -> Result<HashMap<(String, String), Vec<StoreCallSite>>> {
        Ok(targets
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|target| {
                let sites = self
                    .callers_of_target(&target.0, &target.1)
                    .into_iter()
                    .map(|(_, site)| site)
                    .collect();
                (target, sites)
            })
            .collect())
    }

    fn direct_caller_counts_of(
        &self,
        targets: &[(String, String)],
    ) -> Result<HashMap<(String, String), usize>> {
        Ok(targets
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|target| {
                let groups = self
                    .callers_of_target(&target.0, &target.1)
                    .into_iter()
                    .map(|(key, site)| (site.caller.file, site.line, key.kind))
                    .collect::<BTreeSet<_>>();
                (target, groups.len())
            })
            .collect())
    }

    fn outgoing_calls_for_symbols(
        &self,
        sources: &[(String, String)],
    ) -> Result<HashMap<(String, String), Vec<StoreCallSite>>> {
        let mut by_source = HashMap::new();
        for source in sources.iter().cloned().collect::<BTreeSet<_>>() {
            let nodes = self.nodes_scoped_exactly(&source.0, &source.1);
            let mut calls = Vec::new();
            // Each source node's calls in its outgoing order, nodes in the
            // order the node lookup returns them.
            for node in &nodes {
                calls.extend(self.outgoing_of(node).into_iter().map(|(_, call)| call));
            }
            // Calls whose target node the edge did not name take the first
            // node of the target's file and scoped name.
            for call in &mut calls {
                if call.target.is_none() {
                    call.target = self
                        .nodes_scoped_exactly(&call.target_file, &call.target_symbol)
                        .into_iter()
                        .next();
                }
            }
            by_source.insert(source, calls);
        }
        Ok(by_source)
    }

    fn callers_of(&self, _: &Path, _: &str, _: usize) -> Result<StoreCallersResult> {
        Err(unused("callers_of"))
    }

    fn impact_of(&self, _: &Path, _: &str, _: usize) -> Result<StoreImpactResult> {
        Err(unused("impact_of"))
    }

    fn outgoing_calls_of(&self, node: &StoreNode) -> Result<Vec<StoreCallSite>> {
        Ok(self
            .outgoing_of(node)
            .into_iter()
            .map(|(_, call)| call)
            .collect())
    }

    fn resolved_self_calls_of(&self, node: &StoreNode) -> Result<Vec<StoreCallSite>> {
        let Some(slice) = self.source.slice(&node.file) else {
            return Ok(Vec::new());
        };
        let mut calls = Vec::new();
        for reference in slice.refs.values() {
            let targets_itself = reference
                .target
                .as_ref()
                .is_some_and(|(file, symbol)| *file == node.file && *symbol == node.symbol);
            if reference.caller_node.as_deref() != Some(node.node_id())
                || reference.kind != "call"
                || !reference.resolved
                || !targets_itself
                || self.has_call_edge(&slice, &reference.ref_id)
            {
                continue;
            }
            let (_, target) = self.target_of(
                &node.file,
                &node.symbol,
                super::slice::TargetLookup::Ordinary,
            );
            calls.push((
                (
                    reference.byte_start,
                    reference.line,
                    reference.ref_id.clone(),
                ),
                StoreCallSite {
                    kind: "call".to_string(),
                    caller_is_test: None,
                    caller: node.clone(),
                    target_file: node.file.clone(),
                    target_symbol: node.symbol.clone(),
                    target,
                    line: reference.line,
                    byte_start: usize::try_from(reference.byte_start).unwrap_or(usize::MAX),
                    byte_end: usize::try_from(reference.byte_end).unwrap_or(usize::MAX),
                    resolved: true,
                    provenance: crate::callgraph_store::PROVENANCE_TREESITTER.to_string(),
                },
            ));
        }
        calls.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(calls.into_iter().map(|(_, call)| call).collect())
    }

    fn unresolved_calls_of(&self, node: &StoreNode) -> Result<Vec<StoreUnresolvedCall>> {
        let Some(slice) = self.source.slice(&node.file) else {
            return Ok(Vec::new());
        };
        let mut calls = slice
            .refs
            .values()
            .filter(|reference| {
                reference.caller_node.as_deref() == Some(node.node_id())
                    && reference.kind == "call"
                    && !reference.resolved
                    && !self.has_call_edge(&slice, &reference.ref_id)
            })
            .map(|reference| {
                (
                    (
                        reference.byte_start,
                        reference.line,
                        reference.ref_id.clone(),
                    ),
                    StoreUnresolvedCall {
                        caller: node.clone(),
                        symbol: reference
                            .short_name
                            .clone()
                            .or_else(|| reference.full_ref.clone())
                            .unwrap_or_default(),
                        full_ref: reference.full_ref.clone(),
                        line: reference.line,
                        byte_start: usize::try_from(reference.byte_start).unwrap_or(usize::MAX),
                        byte_end: usize::try_from(reference.byte_end).unwrap_or(usize::MAX),
                    },
                )
            })
            .collect::<Vec<_>>();
        calls.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(calls.into_iter().map(|(_, call)| call).collect())
    }

    fn ref_sites_named(
        &self,
        kind: &str,
        short_name: &str,
        limit: usize,
    ) -> Result<Vec<StoreRefSite>> {
        let mut sites = Vec::new();
        for file in self.source.files_with_ref_name(short_name) {
            let Some(slice) = self.source.slice(&file) else {
                continue;
            };
            for reference in slice.refs.values() {
                if reference.short_name.as_deref() != Some(short_name) || reference.kind != kind {
                    continue;
                }
                let caller = reference
                    .caller_node
                    .as_deref()
                    .and_then(|id| slice.node(id));
                sites.push((
                    (file.clone(), reference.line, reference.ref_id.clone()),
                    StoreRefSite {
                        is_test: caller.and_then(|node| node.is_test),
                        file: file.clone(),
                        caller_symbol: caller.map(|node| node.scoped_name.clone()),
                        line: reference.line,
                        local_name: reference.local_name.clone(),
                    },
                ));
            }
        }
        sites.sort_by(|a, b| a.0.cmp(&b.0));
        sites.truncate(limit);
        Ok(sites.into_iter().map(|(_, site)| site).collect())
    }

    fn unresolved_method_sites_named(
        &self,
        name: &str,
        language: LangId,
    ) -> Result<Vec<StoreRefSite>> {
        Ok(self
            .unresolved_spans(lang_label(language), Some(name), true)
            .into_iter()
            .map(|(file, _, _, references, slice)| {
                // One row per span. Several references share a span when a
                // call inside a nested function is also credited to the
                // functions around it. The extractor writes the innermost
                // caller's reference first, and the row names it, as the
                // database's GROUP BY did when it read a group in insertion
                // order.
                let first = references
                    .iter()
                    .min_by_key(|reference| reference.ordinal)
                    .expect("a span has a reference");
                let caller = first.caller_node.as_deref().and_then(|id| slice.node(id));
                StoreRefSite {
                    is_test: caller.and_then(|node| node.is_test),
                    file,
                    caller_symbol: caller.map(|node| node.scoped_name.clone()),
                    line: first.line,
                    local_name: None,
                }
            })
            .collect())
    }

    fn unresolved_call_site_count(&self, language: LangId, include_tests: bool) -> Result<usize> {
        Ok(self.count_unresolved(language, include_tests, None))
    }

    fn unresolved_call_site_count_named(
        &self,
        name: &str,
        language: LangId,
        include_tests: bool,
    ) -> Result<usize> {
        Ok(self.count_unresolved(language, include_tests, Some(name)))
    }

    fn call_tree(&self, _: &Path, _: &str, _: usize) -> Result<callgraph::CallTreeNode> {
        Err(unused("call_tree"))
    }

    fn trace_to(&self, _: &Path, _: &str, _: usize) -> Result<callgraph::TraceToResult> {
        Err(unused("trace_to"))
    }

    fn trace_to_symbol_candidates(&self, to_symbol: &str) -> Result<Vec<TraceToSymbolCandidate>> {
        let mut by_file = BTreeMap::<String, u32>::new();
        for node in self.all_nodes_matching(to_symbol) {
            by_file
                .entry(node.file)
                .and_modify(|line| *line = (*line).min(node.line))
                .or_insert(node.line);
        }
        Ok(by_file
            .into_iter()
            .map(|(file, line)| TraceToSymbolCandidate { file, line })
            .collect())
    }

    fn trace_to_symbol(
        &self,
        _: &Path,
        _: &str,
        _: &str,
        _: Option<&Path>,
        _: usize,
    ) -> Result<callgraph::TraceToSymbolResult> {
        Err(unused("trace_to_symbol"))
    }
}

impl<S: SliceSource> SliceReader<S> {
    /// Counts unresolved spans up to the limit, and stops reading files
    /// once it is reached, as the database query stops at its `LIMIT`.
    fn count_unresolved(&self, language: LangId, include_tests: bool, name: Option<&str>) -> usize {
        let mut count = 0;
        self.visit_unresolved_spans(
            lang_label(language),
            name,
            false,
            |file, _, references, slice| {
                let counted = include_tests
                    || references.iter().any(|reference| {
                        let caller_is_test = reference
                            .caller_node
                            .as_deref()
                            .and_then(|id| slice.node(id))
                            .and_then(|node| node.is_test)
                            .unwrap_or(false);
                        !caller_is_test && !crate::inspect::job::is_test_file(file)
                    });
                if counted {
                    count += 1;
                    if count == UNRESOLVED_CALL_SITE_COUNT_LIMIT {
                        return ControlFlow::Break(());
                    }
                }
                ControlFlow::Continue(())
            },
        );
        count
    }
}
