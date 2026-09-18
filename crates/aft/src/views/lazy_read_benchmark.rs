//! Opt-in manifest-join reader and benchmark driver for the lazy-read investigation.
//!
//! This module is compiled only when the manual benchmark cfg is supplied. It is
//! deliberately separate from live navigation so measurements cannot change
//! product ranking or query behavior.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
use std::io::Read;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::Serialize;

use crate::callgraph_store::join::{
    BlobRefKind, CallgraphBlob, ManifestBlobReader, ManifestJoinError, ResolutionStatus,
};
use crate::callgraph_store::{
    ReadonlyCallGraphStore, StoreCallSite, StoreCallersResult, StoreImpactResult, StoreNode,
};
use crate::inspect::job::{CallgraphSnapshot, DISPATCHED_CALLEE_SEPARATOR};
use crate::parser::LangId;
use crate::symbols::SymbolKind;
use crate::views::{ManifestEntry, ViewStore};

const QUERY_DEPTH: usize = 3;
const SYMBOLS_PER_STRATUM: usize = 18;
const BENCHMARK_RUNS: usize = 3;
const PROVENANCE_TREESITTER: &str = "treesitter+resolver";
const TOP_LEVEL_SYMBOL: &str = "<top-level>";

type BenchResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Clone, Debug)]
struct LazyNode {
    id: String,
    file: String,
    symbol: String,
    name: String,
    kind: String,
    line: u32,
    end_line: u32,
    start_col: u32,
    signature: Option<String>,
    exported: bool,
    is_default_export: bool,
    is_entry_point: bool,
    lang: LangId,
}

#[derive(Clone, Debug)]
struct LazyCallSite {
    caller_id: String,
    ref_id: String,
    value: ComparableCallSite,
}

#[derive(Clone, Debug)]
struct LazyRef {
    ref_id: String,
    caller_file: String,
    caller_symbol: Option<String>,
    short_name: Option<String>,
    full_ref: Option<String>,
    kind: BlobRefKind,
    target_file: Option<String>,
    target_symbol: Option<String>,
    target_node_name: Option<String>,
    line: u32,
    byte_start: usize,
    byte_end: usize,
}

#[derive(Debug)]
struct LazyGeneration {
    project_root: PathBuf,
    nodes: HashMap<String, LazyNode>,
    nodes_by_file: HashMap<String, Vec<String>>,
    direct_callers: HashMap<(String, String), Vec<LazyCallSite>>,
    refs: Vec<LazyRef>,
    files: Vec<String>,
}

#[derive(Debug)]
struct GenerationCache {
    capacity: NonZeroUsize,
    order: VecDeque<String>,
    entries: HashMap<String, Arc<LazyGeneration>>,
}

impl GenerationCache {
    fn new(capacity: NonZeroUsize) -> Self {
        Self {
            capacity,
            order: VecDeque::new(),
            entries: HashMap::new(),
        }
    }

    fn get(&mut self, generation: &str) -> Option<Arc<LazyGeneration>> {
        let value = self.entries.get(generation)?.clone();
        self.order.retain(|key| key != generation);
        self.order.push_back(generation.to_string());
        Some(value)
    }

    fn insert(&mut self, generation: String, value: Arc<LazyGeneration>) {
        self.order.retain(|key| key != &generation);
        self.order.push_back(generation.clone());
        self.entries.insert(generation, value);
        while self.entries.len() > self.capacity.get() {
            if let Some(evicted) = self.order.pop_front() {
                self.entries.remove(&evicted);
            }
        }
    }

    fn clear(&mut self) {
        self.order.clear();
        self.entries.clear();
    }
}

/// Measurement-only reader that resolves immutable blob payloads through a
/// manifest and retains at most a fixed number of assembled generations.
#[derive(Debug)]
pub struct LazyManifestJoinReader {
    project_root: PathBuf,
    storage: PathBuf,
    scope: String,
    family: String,
    generation: String,
    cache: Mutex<GenerationCache>,
}

impl LazyManifestJoinReader {
    pub fn open(
        project_root: PathBuf,
        storage: PathBuf,
        scope: String,
        family: String,
        generation: String,
        generation_capacity: NonZeroUsize,
    ) -> Self {
        Self {
            project_root,
            storage,
            scope,
            family,
            generation,
            cache: Mutex::new(GenerationCache::new(generation_capacity)),
        }
    }

    pub fn clear_generation_cache(&self) {
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    pub fn callers_bytes(&self, file: &Path, symbol: &str, depth: usize) -> BenchResult<Vec<u8>> {
        let graph = self.generation()?;
        let result = graph.callers(file, symbol, depth)?;
        Ok(serde_json::to_vec(&result)?)
    }

    pub fn impact_bytes(&self, file: &Path, symbol: &str, depth: usize) -> BenchResult<Vec<u8>> {
        let graph = self.generation()?;
        let result = graph.impact(file, symbol, depth)?;
        Ok(serde_json::to_vec(&result)?)
    }

    pub fn projection_bytes(&self) -> BenchResult<Vec<u8>> {
        let graph = self.generation()?;
        graph.projection_bytes()
    }

    fn generation(&self) -> BenchResult<Arc<LazyGeneration>> {
        if let Some(cached) = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&self.generation)
        {
            return Ok(cached);
        }

        let built = Arc::new(LazyGeneration::build(
            &self.project_root,
            &self.storage,
            &self.scope,
            &self.family,
            &self.generation,
        )?);
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cached) = cache.get(&self.generation) {
            return Ok(cached);
        }
        cache.insert(self.generation.clone(), built.clone());
        Ok(built)
    }
}

struct BlobReader {
    connection: Connection,
    decoded: RefCell<HashMap<String, Arc<CallgraphBlob>>>,
}

impl BlobReader {
    fn open(path: &Path) -> Result<Self, rusqlite::Error> {
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        Ok(Self {
            connection,
            decoded: RefCell::new(HashMap::new()),
        })
    }

    fn read_payload(&self, full_key: &str) -> Result<Option<Vec<u8>>, ManifestJoinError> {
        let Some(key) = decode_manifest_full_key(full_key) else {
            return Ok(None);
        };
        self.connection
            .query_row(
                "SELECT payload FROM blob_payloads WHERE full_key = ?1",
                [key],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| ManifestJoinError::InvalidBlob(error.to_string()))
    }

    fn read_decoded(
        &self,
        full_key: &str,
    ) -> Result<Option<Arc<CallgraphBlob>>, ManifestJoinError> {
        if let Some(blob) = self.decoded.borrow().get(full_key) {
            return Ok(Some(blob.clone()));
        }
        let Some(payload) = self.read_payload(full_key)? else {
            return Ok(None);
        };
        let blob = Arc::new(CallgraphBlob::from_bytes(&payload)?);
        self.decoded
            .borrow_mut()
            .insert(full_key.to_string(), blob.clone());
        Ok(Some(blob))
    }
}

impl ManifestBlobReader for BlobReader {
    fn read_callgraph_blob(&self, full_key: &str) -> Result<Option<Vec<u8>>, ManifestJoinError> {
        self.read_payload(full_key)
    }

    fn read_callgraph_blob_decoded(
        &self,
        full_key: &str,
    ) -> Result<Option<Arc<CallgraphBlob>>, ManifestJoinError> {
        self.read_decoded(full_key)
    }
}

impl LazyGeneration {
    fn build(
        project_root: &Path,
        storage: &Path,
        scope: &str,
        family: &str,
        generation: &str,
    ) -> BenchResult<Self> {
        let view = ViewStore::open(storage, scope)?;
        let manifest = view.load_manifest(generation)?;
        let blob_path = storage.join("blobs").join(family).join("callgraph.sqlite");
        let reader = BlobReader::open(&blob_path)?;
        let joined = crate::callgraph_store::join::JoinResult::from_manifest(&manifest, &reader)?;

        let mut nodes = HashMap::new();
        let mut nodes_by_file = HashMap::<String, Vec<String>>::new();
        let mut symbol_lookup = HashMap::<String, HashMap<String, String>>::new();
        let mut parse_by_file = BTreeMap::<String, Arc<CallgraphBlob>>::new();
        let mut files = Vec::new();

        for (path, entry) in manifest.entries() {
            let ManifestEntry::Regular {
                planes,
                resolution_input,
                ..
            } = entry
            else {
                continue;
            };
            let Some(key) = planes.callgraph.as_deref() else {
                continue;
            };
            let Some(blob) = reader.read_decoded(key)? else {
                return Err(format!("missing manifest callgraph blob {key}").into());
            };
            let Some(parse) = blob.parse() else {
                continue;
            };
            let file = String::from_utf8(path.as_bytes().to_vec())?;
            files.push(file.clone());
            let lang =
                crate::parser::detect_language(Path::new(&file)).unwrap_or(LangId::TypeScript);
            let lookup = symbol_lookup.entry(file.clone()).or_default();
            let file_nodes = nodes_by_file.entry(file.clone()).or_default();
            for symbol in &parse.symbols {
                let id = format!("view:{file}:{}:{}", symbol.scoped_name, symbol.ordinal);
                let node = LazyNode {
                    id: id.clone(),
                    file: file.clone(),
                    symbol: symbol.scoped_name.clone(),
                    name: symbol.name.clone(),
                    kind: symbol.kind.clone(),
                    line: symbol.start_line.saturating_add(1),
                    end_line: symbol.end_line.saturating_add(1),
                    start_col: symbol.start_col,
                    signature: symbol.signature.clone(),
                    exported: symbol.exported,
                    is_default_export: symbol.is_default_export,
                    is_entry_point: symbol.exported,
                    lang,
                };
                nodes.insert(id.clone(), node);
                file_nodes.push(id.clone());
                lookup.insert(symbol.scoped_name.clone(), id.clone());
                lookup.entry(symbol.name.clone()).or_insert(id);
            }
            if !resolution_input {
                parse_by_file.insert(file, blob);
            }
        }
        files.sort();

        let mut edges_by_ref = BTreeMap::<String, ((String, String), LazyCallSite)>::new();
        let mut refs_by_id = BTreeMap::<String, LazyRef>::new();
        for row in joined.rows {
            let caller_file = String::from_utf8(row.caller_path)?;
            let Some(parse) = parse_by_file
                .get(&caller_file)
                .and_then(|blob| blob.parse())
            else {
                continue;
            };
            let Some(reference) = parse
                .refs
                .iter()
                .find(|reference| reference.ordinal == row.ref_ordinal)
            else {
                continue;
            };
            let caller_node_id = reference
                .caller_symbol
                .as_ref()
                .and_then(|symbol| symbol_lookup.get(&caller_file)?.get(symbol))
                .cloned();
            let target_file = row
                .target_path
                .and_then(|path| String::from_utf8(path).ok());
            let target_symbol = row.target_symbol;
            let target_node_id = target_file
                .as_ref()
                .zip(target_symbol.as_ref())
                .and_then(|(file, symbol)| symbol_lookup.get(file)?.get(symbol))
                .cloned();
            let target_node_name = target_node_id
                .as_ref()
                .and_then(|id| nodes.get(id))
                .map(|node| node.name.clone());
            let ref_id = format!("view:{caller_file}:{}", row.ref_ordinal);

            if row.kind == BlobRefKind::Call {
                if let (Some(caller_id), Some(target_file), Some(target_symbol)) = (
                    caller_node_id.as_ref(),
                    target_file.as_ref(),
                    target_symbol.as_ref(),
                ) {
                    let caller = nodes
                        .get(caller_id)
                        .ok_or_else(|| format!("missing caller node {caller_id}"))?;
                    let target = target_node_id.as_ref().and_then(|id| nodes.get(id));
                    let site = ComparableCallSite {
                        caller: ComparableNode::from_lazy(caller),
                        target_file: target_file.clone(),
                        target_symbol: target_symbol.clone(),
                        target: target.map(ComparableNode::from_lazy),
                        line: reference.line,
                        byte_start: reference.byte_start,
                        byte_end: reference.byte_end,
                        resolved: row.status == ResolutionStatus::Resolved,
                        provenance: PROVENANCE_TREESITTER.to_string(),
                    };
                    edges_by_ref.insert(
                        ref_id.clone(),
                        (
                            (target_file.clone(), target_symbol.clone()),
                            LazyCallSite {
                                caller_id: caller_id.clone(),
                                ref_id: ref_id.clone(),
                                value: site,
                            },
                        ),
                    );
                }
            }

            refs_by_id.insert(
                ref_id.clone(),
                LazyRef {
                    ref_id,
                    caller_file,
                    caller_symbol: caller_node_id
                        .as_ref()
                        .and_then(|id| nodes.get(id))
                        .map(|node| node.name.clone()),
                    short_name: reference.short_name.clone(),
                    full_ref: reference.full_ref.clone(),
                    kind: row.kind,
                    target_file,
                    target_symbol,
                    target_node_name,
                    line: reference.line,
                    byte_start: reference.byte_start,
                    byte_end: reference.byte_end,
                },
            );
        }

        let mut direct_callers = HashMap::<(String, String), Vec<LazyCallSite>>::new();
        for (_, (target, site)) in edges_by_ref {
            direct_callers.entry(target).or_default().push(site);
        }
        let refs = refs_by_id.into_values().collect();
        for sites in direct_callers.values_mut() {
            sites.sort_by(|left, right| {
                left.caller_id
                    .cmp(&right.caller_id)
                    .then_with(|| left.value.byte_start.cmp(&right.value.byte_start))
                    .then_with(|| left.value.line.cmp(&right.value.line))
                    .then_with(|| left.ref_id.cmp(&right.ref_id))
            });
        }

        Ok(Self {
            project_root: project_root.to_path_buf(),
            nodes,
            nodes_by_file,
            direct_callers,
            refs,
            files,
        })
    }

    fn node_for(&self, file: &Path, symbol: &str) -> BenchResult<&LazyNode> {
        let file = file.to_string_lossy();
        let qualified = symbol.contains("::");
        let mut candidates = self
            .nodes_by_file
            .get(file.as_ref())
            .into_iter()
            .flatten()
            .filter_map(|id| self.nodes.get(id))
            .filter(|node| node.symbol == symbol || (!qualified && node.name == symbol))
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| {
            left.symbol
                .cmp(&right.symbol)
                .then_with(|| left.line.cmp(&right.line))
                .then_with(|| left.start_col.cmp(&right.start_col))
        });
        match candidates.as_slice() {
            [node] => Ok(node),
            [] => Err(format!("symbol '{symbol}' not found in {file}").into()),
            _ => Err(format!("symbol '{symbol}' is ambiguous in {file}").into()),
        }
    }

    fn direct(&self, file: &str, symbol: &str) -> &[LazyCallSite] {
        self.direct_callers
            .get(&(file.to_string(), symbol.to_string()))
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    fn callers(&self, file: &Path, symbol: &str, depth: usize) -> BenchResult<ComparableCallers> {
        let target = self.node_for(file, symbol)?;
        let mut visited = HashSet::new();
        let mut callers = Vec::new();
        let mut depth_limited = false;
        let mut truncated = 0;
        self.collect_callers(
            &target.file,
            &target.symbol,
            depth.max(1),
            0,
            &mut visited,
            &mut callers,
            &mut depth_limited,
            &mut truncated,
        );
        Ok(ComparableCallers {
            target: ComparableNode::from_lazy(target),
            callers,
            scanned_files: self.files.len(),
            depth_limited,
            truncated,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn collect_callers(
        &self,
        file: &str,
        symbol: &str,
        max_depth: usize,
        current_depth: usize,
        visited: &mut HashSet<(String, String)>,
        result: &mut Vec<ComparableCallSite>,
        depth_limited: &mut bool,
        truncated: &mut usize,
    ) {
        if current_depth >= max_depth {
            let omitted = self.direct(file, symbol).len();
            if omitted > 0 {
                *depth_limited = true;
                *truncated += omitted;
            }
            return;
        }
        if !visited.insert((file.to_string(), symbol.to_string())) {
            return;
        }
        for site in self.direct(file, symbol) {
            result.push(site.value.clone());
            if current_depth + 1 < max_depth {
                self.collect_callers(
                    &site.value.caller.file,
                    &site.value.caller.symbol,
                    max_depth,
                    current_depth + 1,
                    visited,
                    result,
                    depth_limited,
                    truncated,
                );
            } else {
                let omitted = self
                    .direct(&site.value.caller.file, &site.value.caller.symbol)
                    .len();
                if omitted > 0 {
                    *depth_limited = true;
                    *truncated += omitted;
                }
            }
        }
    }

    fn impact(&self, file: &Path, symbol: &str, depth: usize) -> BenchResult<ComparableImpact> {
        let callers = self.callers(file, symbol, depth)?;
        let target_lang = self.node_for(file, symbol)?.lang;
        let parameters = callers
            .target
            .signature
            .as_deref()
            .map(|signature| crate::callgraph::extract_parameters(signature, target_lang))
            .unwrap_or_default();
        let mut source_lines = HashMap::<String, Option<Vec<String>>>::new();
        let mut enriched = Vec::with_capacity(callers.callers.len());
        for site in callers.callers {
            let lines = source_lines
                .entry(site.caller.file.clone())
                .or_insert_with(|| {
                    fs::read_to_string(self.project_root.join(&site.caller.file))
                        .ok()
                        .map(|source| source.lines().map(|line| line.trim().to_string()).collect())
                });
            let call_expression = lines
                .as_ref()
                .and_then(|lines| lines.get(site.line.saturating_sub(1) as usize))
                .cloned();
            let caller_lang = crate::parser::detect_language(Path::new(&site.caller.file))
                .unwrap_or(LangId::TypeScript);
            let caller_parameters = site
                .caller
                .signature
                .as_deref()
                .map(|signature| crate::callgraph::extract_parameters(signature, caller_lang))
                .unwrap_or_default();
            enriched.push(ComparableImpactCaller {
                signature: site.caller.signature.clone(),
                is_entry_point: site.caller.is_entry_point,
                call_expression,
                parameters: caller_parameters,
                site,
            });
        }
        Ok(ComparableImpact {
            target: callers.target,
            parameters,
            callers: enriched,
            depth_limited: callers.depth_limited,
            truncated: callers.truncated,
        })
    }

    fn projection_bytes(&self) -> BenchResult<Vec<u8>> {
        let mut path_cache = HashMap::new();
        let resolve_path = |file: &str, cache: &mut HashMap<String, PathBuf>| {
            cache
                .entry(file.to_string())
                .or_insert_with(|| {
                    crate::inspect::job::canonicalize_normalized(&self.project_root.join(file))
                })
                .clone()
        };

        let file_paths = self
            .files
            .iter()
            .map(|file| resolve_path(file, &mut path_cache))
            .collect::<Vec<_>>();
        let files = file_paths
            .iter()
            .map(|file| path_text(file))
            .collect::<Vec<_>>();

        let mut ordered_nodes = self.nodes.values().collect::<Vec<_>>();
        ordered_nodes.sort_by(|left, right| {
            left.file
                .cmp(&right.file)
                .then_with(|| left.line.cmp(&right.line))
                .then_with(|| left.name.cmp(&right.name))
                .then_with(|| left.kind.cmp(&right.kind))
                .then_with(|| left.id.cmp(&right.id))
        });
        let mut exported_symbols = Vec::new();
        let mut entry_point_symbols = BTreeMap::<String, BTreeSet<String>>::new();
        for node in ordered_nodes {
            let file = path_text(&resolve_path(&node.file, &mut path_cache));
            if node.exported {
                exported_symbols.push(ComparableExport {
                    file: file.clone(),
                    symbol: node.name.clone(),
                    kind: node.kind.clone(),
                    line: node.line,
                });
            }
            if node.is_entry_point {
                if let Some(kind) = symbol_kind_from_label(&node.kind) {
                    if !crate::callgraph::is_entry_point(
                        &node.symbol,
                        &kind,
                        node.exported,
                        node.lang,
                    ) {
                        let roots = entry_point_symbols.entry(file.clone()).or_default();
                        roots.insert(node.name.clone());
                        if node.symbol != node.name {
                            roots.insert(node.symbol.clone());
                        }
                    }
                }
            }
            if node.is_default_export {
                exported_symbols.push(ComparableExport {
                    file,
                    symbol: node.name.clone(),
                    kind: crate::inspect::scanners::DEFAULT_EXPORT_MARKER_KIND.to_string(),
                    line: node.line,
                });
            }
        }

        let mut refs = self
            .refs
            .iter()
            .filter(|reference| matches!(reference.kind, BlobRefKind::Call | BlobRefKind::ValueRef))
            .collect::<Vec<_>>();
        refs.sort_by(|left, right| {
            left.caller_file
                .cmp(&right.caller_file)
                .then_with(|| left.caller_symbol.cmp(&right.caller_symbol))
                .then_with(|| left.line.cmp(&right.line))
                .then_with(|| left.byte_start.cmp(&right.byte_start))
                .then_with(|| left.byte_end.cmp(&right.byte_end))
                .then_with(|| left.ref_id.cmp(&right.ref_id))
        });
        let mut outbound_calls = Vec::with_capacity(refs.len());
        for reference in refs {
            let short_name = reference
                .short_name
                .as_deref()
                .or(reference.full_ref.as_deref())
                .unwrap_or_default();
            let mut target = match (
                reference.target_file.as_deref(),
                reference
                    .target_node_name
                    .as_deref()
                    .or(reference.target_symbol.as_deref()),
            ) {
                (Some(target_file), Some(target_symbol)) => format!(
                    "{}::{target_symbol}",
                    resolve_path(target_file, &mut path_cache).display()
                ),
                _ => short_name.to_string(),
            };
            if reference
                .full_ref
                .as_deref()
                .is_some_and(|full_ref| is_method_dispatch_callee(full_ref, short_name))
            {
                target.push(DISPATCHED_CALLEE_SEPARATOR);
                target.push_str(reference.full_ref.as_deref().unwrap_or_default());
            }
            outbound_calls.push(ComparableOutboundCall {
                caller_file: path_text(&resolve_path(&reference.caller_file, &mut path_cache)),
                caller_symbol: reference
                    .caller_symbol
                    .clone()
                    .unwrap_or_else(|| TOP_LEVEL_SYMBOL.to_string()),
                target,
                line: reference.line,
                provenance: PROVENANCE_TREESITTER.to_string(),
            });
        }

        let resolved_entry_points = crate::inspect::resolve_entry_points(&self.project_root);
        let entry_points = file_paths
            .iter()
            .filter(|file| resolved_entry_points.is_entry_point(file))
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|file| path_text(&file))
            .collect();

        let projection = ComparableProjection {
            files,
            exported_symbols,
            outbound_calls,
            entry_points,
            entry_point_symbols: entry_point_symbols
                .into_iter()
                .map(|(file, symbols)| (file, symbols.into_iter().collect()))
                .collect(),
        };
        Ok(serde_json::to_vec(&projection)?)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct ComparableNode {
    file: String,
    symbol: String,
    name: String,
    kind: String,
    line: u32,
    end_line: u32,
    signature: Option<String>,
    exported: bool,
    is_entry_point: bool,
    lang: String,
}

impl ComparableNode {
    fn from_lazy(node: &LazyNode) -> Self {
        Self {
            file: node.file.clone(),
            symbol: node.symbol.clone(),
            name: node.name.clone(),
            kind: node.kind.clone(),
            line: node.line,
            end_line: node.end_line,
            signature: node.signature.clone(),
            exported: node.exported,
            is_entry_point: node.is_entry_point,
            lang: lang_label(node.lang).to_string(),
        }
    }

    fn from_store(node: &StoreNode) -> Self {
        Self {
            file: node.file.clone(),
            symbol: node.symbol.clone(),
            name: node.name.clone(),
            kind: node.kind.clone(),
            line: node.line,
            end_line: node.end_line,
            signature: node.signature.clone(),
            exported: node.exported,
            is_entry_point: node.is_entry_point,
            lang: lang_label(node.lang).to_string(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct ComparableCallSite {
    caller: ComparableNode,
    target_file: String,
    target_symbol: String,
    target: Option<ComparableNode>,
    line: u32,
    byte_start: usize,
    byte_end: usize,
    resolved: bool,
    provenance: String,
}

impl ComparableCallSite {
    fn from_store(site: &StoreCallSite) -> Self {
        Self {
            caller: ComparableNode::from_store(&site.caller),
            target_file: site.target_file.clone(),
            target_symbol: site.target_symbol.clone(),
            target: site.target.as_ref().map(ComparableNode::from_store),
            line: site.line,
            byte_start: site.byte_start,
            byte_end: site.byte_end,
            resolved: site.resolved,
            provenance: site.provenance.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct ComparableCallers {
    target: ComparableNode,
    callers: Vec<ComparableCallSite>,
    scanned_files: usize,
    depth_limited: bool,
    truncated: usize,
}

impl ComparableCallers {
    fn from_store(result: &StoreCallersResult) -> Self {
        Self {
            target: ComparableNode::from_store(&result.target),
            callers: result
                .callers
                .iter()
                .map(ComparableCallSite::from_store)
                .collect(),
            scanned_files: result.scanned_files,
            depth_limited: result.depth_limited,
            truncated: result.truncated,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct ComparableImpactCaller {
    site: ComparableCallSite,
    signature: Option<String>,
    is_entry_point: bool,
    call_expression: Option<String>,
    parameters: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct ComparableImpact {
    target: ComparableNode,
    parameters: Vec<String>,
    callers: Vec<ComparableImpactCaller>,
    depth_limited: bool,
    truncated: usize,
}

impl ComparableImpact {
    fn from_store(result: &StoreImpactResult) -> Self {
        Self {
            target: ComparableNode::from_store(&result.target),
            parameters: result.parameters.clone(),
            callers: result
                .callers
                .iter()
                .map(|caller| ComparableImpactCaller {
                    site: ComparableCallSite::from_store(&caller.site),
                    signature: caller.signature.clone(),
                    is_entry_point: caller.is_entry_point,
                    call_expression: caller.call_expression.clone(),
                    parameters: caller.parameters.clone(),
                })
                .collect(),
            depth_limited: result.depth_limited,
            truncated: result.truncated,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct ComparableExport {
    file: String,
    symbol: String,
    kind: String,
    line: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct ComparableOutboundCall {
    caller_file: String,
    caller_symbol: String,
    target: String,
    line: u32,
    provenance: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct ComparableProjection {
    files: Vec<String>,
    exported_symbols: Vec<ComparableExport>,
    outbound_calls: Vec<ComparableOutboundCall>,
    entry_points: Vec<String>,
    entry_point_symbols: BTreeMap<String, Vec<String>>,
}

impl ComparableProjection {
    fn from_store(snapshot: &CallgraphSnapshot) -> Self {
        Self {
            files: snapshot.files.iter().map(|path| path_text(path)).collect(),
            exported_symbols: snapshot
                .exported_symbols
                .iter()
                .map(|export| ComparableExport {
                    file: path_text(&export.file),
                    symbol: export.symbol.clone(),
                    kind: export.kind.clone(),
                    line: export.line,
                })
                .collect(),
            outbound_calls: snapshot
                .outbound_calls
                .iter()
                .map(|call| ComparableOutboundCall {
                    caller_file: path_text(&call.caller_file),
                    caller_symbol: call.caller_symbol.clone(),
                    target: call.target.clone(),
                    line: call.line,
                    provenance: call.provenance.clone(),
                })
                .collect(),
            entry_points: snapshot
                .entry_points
                .iter()
                .map(|path| path_text(path))
                .collect(),
            entry_point_symbols: snapshot
                .entry_point_symbols
                .iter()
                .map(|(file, symbols)| {
                    (path_text(file), symbols.iter().cloned().collect::<Vec<_>>())
                })
                .collect(),
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
struct SelectedSymbol {
    stratum: String,
    file: String,
    symbol: String,
    in_degree: usize,
}

#[derive(Clone, Debug, Serialize)]
struct SampleStats {
    samples: usize,
    p50_us: u64,
    p95_us: u64,
    max_us: u64,
}

#[derive(Clone, Debug, Serialize)]
struct TimingRow {
    run: usize,
    arm: String,
    query_kind: String,
    stratum: String,
    stats: SampleStats,
}

#[derive(Clone, Debug, Serialize)]
struct ArmRun {
    run: usize,
    arm: String,
    load_average_1m: f64,
    bytes_read: Option<u64>,
    projection_us: u64,
}

#[derive(Clone, Debug, Serialize)]
struct SpreadRow {
    arm: String,
    query_kind: String,
    stratum: String,
    p50_us_min: u64,
    p50_us_max: u64,
    p95_us_min: u64,
    p95_us_max: u64,
    max_us_min: u64,
    max_us_max: u64,
}

#[derive(Debug, Serialize)]
struct BenchmarkReport {
    machine: String,
    binary_sha256: String,
    binary_path: String,
    subject_sha: String,
    generation: String,
    initial_load_average_1m: f64,
    require_idle: bool,
    provisional_loaded_host: bool,
    runs: usize,
    query_depth: usize,
    selection: Vec<SelectedSymbol>,
    timing_rows: Vec<TimingRow>,
    arm_runs: Vec<ArmRun>,
    spread: Vec<SpreadRow>,
    verdict: String,
}

#[derive(Clone)]
struct BaselineResults {
    callers: Vec<u8>,
    impact: Vec<u8>,
}

/// Runs the ignored release-profile benchmark over artifacts named by
/// `AFT_HUNT_ROOT` and `AFT_HUNT_STORAGE`.
pub fn run_profile_benchmark_from_env() -> BenchResult<()> {
    if cfg!(debug_assertions) {
        return Err("the lazy-read benchmark must run with --release".into());
    }
    let root = PathBuf::from(std::env::var_os("AFT_HUNT_ROOT").ok_or("AFT_HUNT_ROOT is required")?);
    let storage =
        PathBuf::from(std::env::var_os("AFT_HUNT_STORAGE").ok_or("AFT_HUNT_STORAGE is required")?);
    let load_average_1m = load_average_1m()?;
    let require_idle = std::env::var_os("AFT_LAZY_READ_REQUIRE_IDLE").is_some();
    eprintln!("views_lazy_read load_average_1m={load_average_1m:.2}");
    if require_idle && load_average_1m > 3.0 {
        return Err(format!(
            "refusing to record benchmark: one-minute load average {load_average_1m:.2} exceeds 3.00"
        )
        .into());
    }
    if load_average_1m > 3.0 {
        eprintln!(
            "views_lazy_read provisional_loaded_host=true absolute_thresholds_decidable=false"
        );
    }

    let family = crate::search_index::artifact_cache_key(&root);
    let scope = crate::path_identity::project_scope_key(&root);
    let view = ViewStore::open(&storage, &scope)?;
    let generation = view
        .current_generation()?
        .ok_or("drill artifact view has no current generation")?;
    let pin = Some(Arc::new(crate::pins::QueryPin::acquire(
        view.view_dir(),
        &generation,
    )?));
    let materialized = ReadonlyCallGraphStore::open_manifest_view(
        root.clone(),
        family.clone(),
        view.view_dir().to_path_buf(),
        &generation,
        pin,
    )?;
    let lazy = LazyManifestJoinReader::open(
        root.clone(),
        storage.clone(),
        scope,
        family,
        generation.clone(),
        NonZeroUsize::new(2).expect("nonzero cache capacity"),
    );
    let selection = select_symbols(materialized.sqlite_path())?;
    eprintln!(
        "views_lazy_read selection={}",
        serde_json::to_string(&selection)?
    );

    let mut timing_rows = Vec::new();
    let mut arm_runs = Vec::new();
    for run in 1..=BENCHMARK_RUNS {
        let (baseline, rows, arm) = run_materialized_arm(run, &materialized, &selection)?;
        timing_rows.extend(rows);
        arm_runs.push(arm);

        let (rows, arm) = run_lazy_arm(run, "lazy_cold", true, &lazy, &selection, &baseline)?;
        timing_rows.extend(rows);
        arm_runs.push(arm);

        let (rows, arm) = run_lazy_arm(run, "lazy_warm", false, &lazy, &selection, &baseline)?;
        timing_rows.extend(rows);
        arm_runs.push(arm);
    }

    let spread = summarize_spread(&timing_rows);
    let provisional_loaded_host =
        load_average_1m > 3.0 || arm_runs.iter().any(|arm| arm.load_average_1m > 3.0);
    let verdict = closing_rule_verdict(&timing_rows, provisional_loaded_host);
    let binary_path = std::env::current_exe()?;
    let report = BenchmarkReport {
        machine: machine_description(),
        binary_sha256: sha256_file(&binary_path)?,
        binary_path: path_text(&binary_path),
        subject_sha: command_output("git", &["-C", &path_text(&root), "rev-parse", "HEAD"]),
        generation,
        initial_load_average_1m: load_average_1m,
        require_idle,
        provisional_loaded_host,
        runs: BENCHMARK_RUNS,
        query_depth: QUERY_DEPTH,
        selection,
        timing_rows,
        arm_runs,
        spread,
        verdict,
    };
    print_report(&report)?;
    Ok(())
}

fn run_materialized_arm(
    run: usize,
    reader: &ReadonlyCallGraphStore,
    selection: &[SelectedSymbol],
) -> BenchResult<(
    HashMap<SelectedSymbol, BaselineResults>,
    Vec<TimingRow>,
    ArmRun,
)> {
    let load_average_1m = load_average_1m()?;
    eprintln!("views_lazy_read run={run} arm=materialized load_average_1m={load_average_1m:.2}");
    let before = process_bytes_read();
    let mut timings = BTreeMap::<(String, String), Vec<u64>>::new();
    let mut baseline = HashMap::new();
    for selected in selection {
        let path = Path::new(&selected.file);
        let started = Instant::now();
        let callers = reader.callers_of(path, &selected.symbol, QUERY_DEPTH)?;
        record_timing(
            &mut timings,
            "callers",
            &selected.stratum,
            started.elapsed(),
        );
        let callers = serde_json::to_vec(&ComparableCallers::from_store(&callers))?;

        let started = Instant::now();
        let impact = reader.impact_of(path, &selected.symbol, QUERY_DEPTH)?;
        record_timing(&mut timings, "impact", &selected.stratum, started.elapsed());
        let impact = serde_json::to_vec(&ComparableImpact::from_store(&impact))?;
        baseline.insert(selected.clone(), BaselineResults { callers, impact });
    }
    let started = Instant::now();
    let (_, projection, _, _) =
        crate::callgraph_store::project_dead_code_snapshot_from_view(reader)?;
    let projection_us = duration_us(started.elapsed());
    let projection_bytes = serde_json::to_vec(&ComparableProjection::from_store(&projection))?;
    baseline.insert(
        projection_key(),
        BaselineResults {
            callers: projection_bytes,
            impact: Vec::new(),
        },
    );
    let after = process_bytes_read();
    Ok((
        baseline,
        timing_rows(run, "materialized", timings),
        ArmRun {
            run,
            arm: "materialized".to_string(),
            load_average_1m,
            bytes_read: byte_delta(before, after),
            projection_us,
        },
    ))
}

fn run_lazy_arm(
    run: usize,
    arm: &str,
    cold: bool,
    reader: &LazyManifestJoinReader,
    selection: &[SelectedSymbol],
    baseline: &HashMap<SelectedSymbol, BaselineResults>,
) -> BenchResult<(Vec<TimingRow>, ArmRun)> {
    let load_average_1m = load_average_1m()?;
    eprintln!("views_lazy_read run={run} arm={arm} load_average_1m={load_average_1m:.2}");
    reader.clear_generation_cache();
    let before = process_bytes_read();
    let mut timings = BTreeMap::<(String, String), Vec<u64>>::new();
    for selected in selection {
        let path = Path::new(&selected.file);
        if cold {
            reader.clear_generation_cache();
        }
        let started = Instant::now();
        let callers = reader.callers_bytes(path, &selected.symbol, QUERY_DEPTH)?;
        record_timing(
            &mut timings,
            "callers",
            &selected.stratum,
            started.elapsed(),
        );
        assert_equal(
            run,
            arm,
            "callers",
            selected,
            &baseline[selected].callers,
            &callers,
        )?;

        if cold {
            reader.clear_generation_cache();
        }
        let started = Instant::now();
        let impact = reader.impact_bytes(path, &selected.symbol, QUERY_DEPTH)?;
        record_timing(&mut timings, "impact", &selected.stratum, started.elapsed());
        assert_equal(
            run,
            arm,
            "impact",
            selected,
            &baseline[selected].impact,
            &impact,
        )?;
    }
    if cold {
        reader.clear_generation_cache();
    }
    let started = Instant::now();
    let projection = reader.projection_bytes()?;
    let projection_us = duration_us(started.elapsed());
    let expected = &baseline[&projection_key()].callers;
    assert_bytes("tier2_projection", expected, &projection)?;
    let after = process_bytes_read();
    Ok((
        timing_rows(run, arm, timings),
        ArmRun {
            run,
            arm: arm.to_string(),
            load_average_1m,
            bytes_read: byte_delta(before, after),
            projection_us,
        },
    ))
}

fn select_symbols(database: &Path) -> BenchResult<Vec<SelectedSymbol>> {
    let connection = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut statement = connection.prepare(
        "SELECT n.file_path, n.scoped_name, COUNT(e.edge_id) AS in_degree
         FROM nodes n
         JOIN files f ON f.path = n.file_path
         LEFT JOIN edges e
           ON e.kind = 'call'
          AND e.target_file = n.file_path
          AND e.target_symbol = n.scoped_name
         WHERE (SELECT COUNT(*) FROM nodes duplicate
                WHERE duplicate.file_path = n.file_path
                  AND duplicate.scoped_name = n.scoped_name) = 1
         GROUP BY n.file_path, n.scoped_name
         HAVING COUNT(e.edge_id) > 0
         ORDER BY in_degree, n.file_path, n.scoped_name",
    )?;
    let candidates = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?.max(0) as usize,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let third = candidates.len() / 3;
    if third < SYMBOLS_PER_STRATUM {
        return Err(format!(
            "need at least {} unambiguous called symbols, found {}",
            SYMBOLS_PER_STRATUM * 3,
            candidates.len()
        )
        .into());
    }
    let strata = [
        ("low", &candidates[..third]),
        ("mid", &candidates[third..third * 2]),
        ("high", &candidates[third * 2..]),
    ];
    let mut selected = Vec::with_capacity(SYMBOLS_PER_STRATUM * strata.len());
    for (stratum, candidates) in strata {
        for index in evenly_spaced_indices(candidates.len(), SYMBOLS_PER_STRATUM) {
            let (file, symbol, in_degree) = &candidates[index];
            selected.push(SelectedSymbol {
                stratum: stratum.to_string(),
                file: file.clone(),
                symbol: symbol.clone(),
                in_degree: *in_degree,
            });
        }
    }
    Ok(selected)
}

fn evenly_spaced_indices(len: usize, count: usize) -> Vec<usize> {
    if count == 1 {
        return vec![len / 2];
    }
    (0..count)
        .map(|index| index * (len - 1) / (count - 1))
        .collect()
}

fn record_timing(
    timings: &mut BTreeMap<(String, String), Vec<u64>>,
    query_kind: &str,
    stratum: &str,
    elapsed: Duration,
) {
    timings
        .entry((query_kind.to_string(), stratum.to_string()))
        .or_default()
        .push(duration_us(elapsed));
}

fn timing_rows(
    run: usize,
    arm: &str,
    timings: BTreeMap<(String, String), Vec<u64>>,
) -> Vec<TimingRow> {
    timings
        .into_iter()
        .map(|((query_kind, stratum), samples)| TimingRow {
            run,
            arm: arm.to_string(),
            query_kind,
            stratum,
            stats: sample_stats(samples),
        })
        .collect()
}

fn sample_stats(mut samples: Vec<u64>) -> SampleStats {
    samples.sort_unstable();
    SampleStats {
        samples: samples.len(),
        p50_us: percentile(&samples, 50),
        p95_us: percentile(&samples, 95),
        max_us: *samples.last().unwrap_or(&0),
    }
}

fn percentile(samples: &[u64], percentile: usize) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    let rank = (percentile * samples.len()).div_ceil(100);
    samples[rank.saturating_sub(1).min(samples.len() - 1)]
}

fn summarize_spread(rows: &[TimingRow]) -> Vec<SpreadRow> {
    let mut grouped = BTreeMap::<(String, String, String), Vec<&SampleStats>>::new();
    for row in rows {
        grouped
            .entry((row.arm.clone(), row.query_kind.clone(), row.stratum.clone()))
            .or_default()
            .push(&row.stats);
    }
    grouped
        .into_iter()
        .map(|((arm, query_kind, stratum), values)| SpreadRow {
            arm,
            query_kind,
            stratum,
            p50_us_min: values.iter().map(|value| value.p50_us).min().unwrap_or(0),
            p50_us_max: values.iter().map(|value| value.p50_us).max().unwrap_or(0),
            p95_us_min: values.iter().map(|value| value.p95_us).min().unwrap_or(0),
            p95_us_max: values.iter().map(|value| value.p95_us).max().unwrap_or(0),
            max_us_min: values.iter().map(|value| value.max_us).min().unwrap_or(0),
            max_us_max: values.iter().map(|value| value.max_us).max().unwrap_or(0),
        })
        .collect()
}

fn closing_rule_verdict(rows: &[TimingRow], provisional_loaded_host: bool) -> String {
    let worst_p95 = |arm: &str, kind: &str| {
        rows.iter()
            .filter(|row| row.arm == arm && row.query_kind == kind)
            .map(|row| row.stats.p95_us)
            .max()
            .unwrap_or(u64::MAX)
    };
    let callers_p95 = worst_p95("lazy_cold", "callers");
    let impact_p95 = worst_p95("lazy_cold", "impact");
    if provisional_loaded_host {
        let materialized_callers = worst_p95("materialized", "callers");
        let materialized_impact = worst_p95("materialized", "impact");
        let callers_ratio = callers_p95 as f64 / materialized_callers.max(1) as f64;
        let impact_ratio = impact_p95 as f64 / materialized_impact.max(1) as f64;
        let direction = if callers_ratio > 1.0 || impact_ratio > 1.0 {
            "relative evidence favors materialized navigation"
        } else {
            "relative evidence favors lazy navigation"
        };
        return format!(
            "provisional loaded-host result: {direction} (lazy-cold/materialized p95 callers={callers_ratio:.2}x impact={impact_ratio:.2}x); absolute 234/228 ms closing thresholds are not decidable"
        );
    }
    if callers_p95 <= 234_000 && impact_p95 <= 228_000 {
        format!(
            "refs/edges go lazy (worst lazy-cold p95 callers={callers_p95}us impact={impact_p95}us)"
        )
    } else if callers_p95 >= 500_000 || impact_p95 >= 500_000 {
        format!(
            "materialized navigation stays (worst lazy-cold p95 callers={callers_p95}us impact={impact_p95}us)"
        )
    } else {
        format!(
            "owner decision required (worst lazy-cold p95 callers={callers_p95}us impact={impact_p95}us)"
        )
    }
}

fn print_report(report: &BenchmarkReport) -> BenchResult<()> {
    eprintln!("| run | arm | kind | stratum | n | p50 ms | p95 ms | max ms |");
    eprintln!("|---:|---|---|---|---:|---:|---:|---:|");
    for row in &report.timing_rows {
        eprintln!(
            "| {} | {} | {} | {} | {} | {:.3} | {:.3} | {:.3} |",
            row.run,
            row.arm,
            row.query_kind,
            row.stratum,
            row.stats.samples,
            row.stats.p50_us as f64 / 1_000.0,
            row.stats.p95_us as f64 / 1_000.0,
            row.stats.max_us as f64 / 1_000.0,
        );
    }
    eprintln!("| run | arm | load average (1m) | bytes read | projection ms |");
    eprintln!("|---:|---|---:|---:|---:|");
    for arm in &report.arm_runs {
        eprintln!(
            "| {} | {} | {:.2} | {} | {:.3} |",
            arm.run,
            arm.arm,
            arm.load_average_1m,
            arm.bytes_read
                .map(|bytes| bytes.to_string())
                .unwrap_or_else(|| "unavailable".to_string()),
            arm.projection_us as f64 / 1_000.0,
        );
    }
    eprintln!("views_lazy_read verdict={}", report.verdict);
    if let Some(path) = std::env::var_os("AFT_LAZY_READ_REPORT") {
        let path = PathBuf::from(path);
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, serde_json::to_vec_pretty(report)?)?;
    }
    println!(
        "AFT_LAZY_READ_BENCHMARK_JSON={}",
        serde_json::to_string(report)?
    );
    Ok(())
}

fn assert_equal(
    run: usize,
    arm: &str,
    query_kind: &str,
    selected: &SelectedSymbol,
    expected: &[u8],
    actual: &[u8],
) -> BenchResult<()> {
    assert_bytes(
        &format!(
            "run={run} arm={arm} kind={query_kind} file={} symbol={}",
            selected.file, selected.symbol
        ),
        expected,
        actual,
    )
}

fn assert_bytes(label: &str, expected: &[u8], actual: &[u8]) -> BenchResult<()> {
    if expected == actual {
        return Ok(());
    }
    Err(format!(
        "byte equality failed for {label}: expected_len={} actual_len={} expected_blake3={} actual_blake3={}{}",
        expected.len(),
        actual.len(),
        blake3::hash(expected).to_hex(),
        blake3::hash(actual).to_hex(),
        json_difference(expected, actual),
    )
    .into())
}

fn json_difference(expected: &[u8], actual: &[u8]) -> String {
    let (Ok(serde_json::Value::Object(expected)), Ok(serde_json::Value::Object(actual))) = (
        serde_json::from_slice::<serde_json::Value>(expected),
        serde_json::from_slice::<serde_json::Value>(actual),
    ) else {
        return String::new();
    };
    let keys = expected
        .keys()
        .chain(actual.keys())
        .collect::<BTreeSet<_>>();
    let mut differences = Vec::new();
    for key in keys {
        let left = expected.get(key);
        let right = actual.get(key);
        if left == right {
            continue;
        }
        let count = |value: Option<&serde_json::Value>| {
            value
                .and_then(serde_json::Value::as_array)
                .map(Vec::len)
                .unwrap_or(0)
        };
        let digest = |value: Option<&serde_json::Value>| {
            value
                .and_then(|value| serde_json::to_vec(value).ok())
                .map(|bytes| blake3::hash(&bytes).to_hex().to_string())
                .unwrap_or_default()
        };
        let first = match (
            left.and_then(serde_json::Value::as_array),
            right.and_then(serde_json::Value::as_array),
        ) {
            (Some(left), Some(right)) => left
                .iter()
                .zip(right)
                .position(|(left, right)| left != right)
                .map(|index| {
                    let left = serde_json::to_string(&left[index]).unwrap_or_default();
                    let right = serde_json::to_string(&right[index]).unwrap_or_default();
                    format!(
                        " first={index} expected={} actual={}",
                        truncate(&left, 240),
                        truncate(&right, 240)
                    )
                })
                .unwrap_or_default(),
            _ => String::new(),
        };
        differences.push(format!(
            "{key}:expected_count={} actual_count={} expected_hash={} actual_hash={}{}",
            count(left),
            count(right),
            digest(left),
            digest(right),
            first,
        ));
    }
    format!(" components=[{}]", differences.join("; "))
}

fn truncate(value: &str, max_bytes: usize) -> String {
    let end = value
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= max_bytes)
        .last()
        .unwrap_or(0);
    if value.len() <= max_bytes {
        value.to_string()
    } else {
        format!("{}…", &value[..end])
    }
}

fn projection_key() -> SelectedSymbol {
    SelectedSymbol {
        stratum: "projection".to_string(),
        file: String::new(),
        symbol: String::new(),
        in_degree: 0,
    }
}

fn decode_manifest_full_key(value: &str) -> Option<Vec<u8>> {
    if value.len() != 64 {
        return None;
    }
    (0..value.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&value[index..index + 2], 16).ok())
        .collect()
}

fn duration_us(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn byte_delta(before: Option<u64>, after: Option<u64>) -> Option<u64> {
    before
        .zip(after)
        .map(|(before, after)| after.saturating_sub(before))
}

#[cfg(target_os = "macos")]
fn process_bytes_read() -> Option<u64> {
    unsafe extern "C" {
        fn proc_pid_rusage(
            pid: libc::c_int,
            flavor: libc::c_int,
            buffer: *mut libc::c_void,
        ) -> libc::c_int;
    }
    let mut buffer = [0_u8; 512];
    let result = unsafe {
        proc_pid_rusage(
            std::process::id() as libc::c_int,
            4,
            buffer.as_mut_ptr().cast(),
        )
    };
    if result != 0 {
        return None;
    }
    // Darwin rusage_info_v4 field 16 is ri_diskio_bytesread.
    Some(u64::from_ne_bytes(
        buffer[16 + 16 * 8..16 + 17 * 8].try_into().ok()?,
    ))
}

#[cfg(target_os = "linux")]
fn process_bytes_read() -> Option<u64> {
    fs::read_to_string("/proc/self/io")
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("read_bytes: ")?.parse().ok())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn process_bytes_read() -> Option<u64> {
    None
}

fn load_average_1m() -> BenchResult<f64> {
    let mut averages = [0.0_f64; 3];
    let count = unsafe { libc::getloadavg(averages.as_mut_ptr(), averages.len() as libc::c_int) };
    if count < 1 {
        return Err("getloadavg did not return a one-minute sample".into());
    }
    Ok(averages[0])
}

fn machine_description() -> String {
    let uname = command_output("uname", &["-a"]);
    let cpu = command_output("sysctl", &["-n", "machdep.cpu.brand_string"]);
    if cpu.is_empty() {
        uname
    } else {
        format!("{uname}; {cpu}")
    }
}

fn command_output(command: &str, args: &[&str]) -> String {
    std::process::Command::new(command)
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_default()
}

fn sha256_file(path: &Path) -> BenchResult<String> {
    use sha2::{Digest, Sha256};
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

fn is_method_dispatch_callee(full_callee: &str, callee_name: &str) -> bool {
    let full_callee = full_callee.trim();
    if !full_callee.contains('.') || full_callee == callee_name.trim() {
        return false;
    }
    full_callee
        .rsplit('.')
        .next()
        .map(|segment| segment.trim().trim_start_matches('?') == callee_name.trim())
        .unwrap_or(false)
}

fn symbol_kind_from_label(label: &str) -> Option<SymbolKind> {
    match label {
        "function" => Some(SymbolKind::Function),
        "kernel" => Some(SymbolKind::Kernel),
        "class" => Some(SymbolKind::Class),
        "method" => Some(SymbolKind::Method),
        "struct" => Some(SymbolKind::Struct),
        "interface" => Some(SymbolKind::Interface),
        "enum" => Some(SymbolKind::Enum),
        "type_alias" => Some(SymbolKind::TypeAlias),
        "variable" => Some(SymbolKind::Variable),
        "heading" => Some(SymbolKind::Heading),
        "file_summary" => Some(SymbolKind::FileSummary),
        _ => None,
    }
}

const fn lang_label(lang: LangId) -> &'static str {
    match lang {
        LangId::TypeScript => "typescript",
        LangId::Tsx => "tsx",
        LangId::JavaScript => "javascript",
        LangId::Python => "python",
        LangId::Rust => "rust",
        LangId::Go => "go",
        LangId::C => "c",
        LangId::Cpp => "cpp",
        LangId::Cuda => "cuda",
        LangId::Metal => "metal",
        LangId::Zig => "zig",
        LangId::CSharp => "csharp",
        LangId::Bash => "bash",
        LangId::Html => "html",
        LangId::Markdown => "markdown",
        LangId::Solidity => "solidity",
        LangId::Scss => "scss",
        LangId::Vue => "vue",
        LangId::Json => "json",
        LangId::Scala => "scala",
        LangId::Java => "java",
        LangId::Ruby => "ruby",
        LangId::Kotlin => "kotlin",
        LangId::Swift => "swift",
        LangId::Php => "php",
        LangId::Lua => "lua",
        LangId::Perl => "perl",
        LangId::Yaml => "yaml",
        LangId::Pascal => "pascal",
        LangId::R => "r",
        LangId::Groovy => "groovy",
        LangId::ObjC => "objc",
        LangId::Toml => "toml",
    }
}
