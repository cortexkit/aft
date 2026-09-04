//! Call graph engine: cross-file call resolution and forward traversal.
//!
//! Builds a lazy, worktree-scoped call graph that resolves calls across files
//! using import chains. Supports depth-limited forward traversal with cycle
//! detection.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, RwLock};

use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Serialize;
use serde_json::Value;
use tree_sitter::{Node, Parser};

use crate::callgraph_store::disk_facts::DiskFacts;
use crate::callgraph_store::facts::{byte_path, EntryKind, FactPaths};
#[cfg(test)]
use crate::calls::{call_node_kinds, extract_callee_name, extract_full_callee};
use crate::calls::{extract_calls_full, extract_rust_value_references};
#[cfg(test)]
use crate::edit::line_col_to_byte;
use crate::error::AftError;
use crate::imports::{self, ImportBlock};
use crate::parser::{detect_language, grammar_for, LangId};
use crate::symbols::{Range, Symbol, SymbolKind};

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

type WorkspacePackageCache = HashMap<(PathBuf, String), Option<PathBuf>>;
type RustCrateInfoCache = HashMap<PathBuf, Option<RustCrateInfo>>;
type RustWorkspaceCrateCache = HashMap<PathBuf, HashMap<String, RustCrateInfo>>;

static WORKSPACE_PACKAGE_CACHE: LazyLock<RwLock<WorkspacePackageCache>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));
static RUST_CRATE_INFO_CACHE: LazyLock<RwLock<RustCrateInfoCache>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));
static RUST_WORKSPACE_CRATE_CACHE: LazyLock<RwLock<RustWorkspaceCrateCache>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

// Cold builds retain one resolver memo across every staged reference window. The
// entry and retained-weight ceilings keep that speedup inside the bounded-build
// working-set contract: at most 9 MiB of estimated keys/values are retained.
const MODULE_RESOLUTION_MEMO_MAX_ENTRIES: usize = 32_768;
const MODULE_RESOLUTION_MEMO_MAX_RETAINED_BYTES: usize = 3 * 1024 * 1024;
const JSON_VALUE_MEMO_MAX_ENTRIES: usize = 8_192;
const JSON_VALUE_MEMO_MAX_RETAINED_BYTES: usize = 3 * 1024 * 1024;
const WORKSPACE_PACKAGE_MEMO_MAX_ENTRIES: usize = 2_048;
const WORKSPACE_PACKAGE_MEMO_MAX_RETAINED_BYTES: usize = 1024 * 1024;
const RUST_DECLARED_MODULE_MEMO_MAX_ENTRIES: usize = 8_192;
const RUST_DECLARED_MODULE_MEMO_MAX_RETAINED_BYTES: usize = 2 * 1024 * 1024;
const MEMO_ENTRY_OVERHEAD_BYTES: usize = 128;

type ModuleResolutionKey = (PathBuf, String);
type WorkspacePackageKey = (PathBuf, String);
type RustDeclaredModuleMap = HashMap<String, Option<String>>;

struct BoundedMemo<K, V> {
    entries: HashMap<K, V>,
    retained_weight: usize,
    max_entries: usize,
    max_retained_weight: usize,
}

impl<K: Eq + std::hash::Hash, V> BoundedMemo<K, V> {
    fn new(max_entries: usize, max_retained_weight: usize) -> Self {
        Self {
            entries: HashMap::new(),
            retained_weight: 0,
            max_entries,
            max_retained_weight,
        }
    }

    fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: std::borrow::Borrow<Q>,
        Q: Eq + std::hash::Hash + ?Sized,
    {
        self.entries.get(key)
    }

    fn insert(&mut self, key: K, value: V, retained_weight: usize) {
        if self.entries.contains_key(&key)
            || self.entries.len() >= self.max_entries
            || self.retained_weight.saturating_add(retained_weight) > self.max_retained_weight
        {
            return;
        }
        self.retained_weight += retained_weight;
        self.entries.insert(key, value);
    }
}

/// Snapshot-scoped filesystem memo used by the staged cold-build resolver.
///
/// The cold build reads one fixed corpus, so no watcher invalidation is needed:
/// the memo is dropped when publication completes or the build aborts.
pub(crate) struct ModuleResolutionMemo {
    enabled: bool,
    module_paths: RefCell<BoundedMemo<ModuleResolutionKey, Option<PathBuf>>>,
    json_values: RefCell<BoundedMemo<PathBuf, Option<Arc<Value>>>>,
    workspace_packages: RefCell<BoundedMemo<WorkspacePackageKey, Option<PathBuf>>>,
    rust_declared_modules: RefCell<BoundedMemo<String, Arc<RustDeclaredModuleMap>>>,
    #[cfg(test)]
    collect_metrics: bool,
    #[cfg(test)]
    module_computations: RefCell<HashMap<ModuleResolutionKey, usize>>,
    #[cfg(test)]
    json_probes: RefCell<HashMap<PathBuf, usize>>,
    #[cfg(test)]
    rust_declaration_parses: RefCell<HashMap<String, usize>>,
}

impl Default for ModuleResolutionMemo {
    fn default() -> Self {
        Self {
            enabled: true,
            module_paths: RefCell::new(BoundedMemo::new(
                MODULE_RESOLUTION_MEMO_MAX_ENTRIES,
                MODULE_RESOLUTION_MEMO_MAX_RETAINED_BYTES,
            )),
            json_values: RefCell::new(BoundedMemo::new(
                JSON_VALUE_MEMO_MAX_ENTRIES,
                JSON_VALUE_MEMO_MAX_RETAINED_BYTES,
            )),
            workspace_packages: RefCell::new(BoundedMemo::new(
                WORKSPACE_PACKAGE_MEMO_MAX_ENTRIES,
                WORKSPACE_PACKAGE_MEMO_MAX_RETAINED_BYTES,
            )),
            rust_declared_modules: RefCell::new(BoundedMemo::new(
                RUST_DECLARED_MODULE_MEMO_MAX_ENTRIES,
                RUST_DECLARED_MODULE_MEMO_MAX_RETAINED_BYTES,
            )),
            #[cfg(test)]
            collect_metrics: false,
            #[cfg(test)]
            module_computations: RefCell::new(HashMap::new()),
            #[cfg(test)]
            json_probes: RefCell::new(HashMap::new()),
            #[cfg(test)]
            rust_declaration_parses: RefCell::new(HashMap::new()),
        }
    }
}

impl ModuleResolutionMemo {
    fn resolve_module_path(
        &self,
        from_dir: &Path,
        module_path: &str,
        facts: &FactPaths<'_>,
    ) -> Option<PathBuf> {
        let key = (from_dir.to_path_buf(), module_path.to_string());
        if self.enabled {
            if let Some(cached) = self.module_paths.borrow().get(&key) {
                return cached.clone();
            }
        }

        self.note_module_computation(&key);
        let resolved = resolve_module_path_uncached(from_dir, module_path, Some(self), facts);
        if self.enabled {
            let retained_weight = module_resolution_entry_weight(&key, resolved.as_deref());
            self.module_paths
                .borrow_mut()
                .insert(key, resolved.clone(), retained_weight);
        }
        resolved
    }

    fn json_value(&self, path: &Path, facts: &FactPaths<'_>) -> Option<Arc<Value>> {
        if self.enabled {
            if let Some(cached) = self.json_values.borrow().get(path) {
                return cached.clone();
            }
        }

        self.note_json_probe(path);
        let parsed = facts
            .bytes(path)
            .and_then(|source| serde_json::from_slice(&source).ok())
            .map(Arc::new);
        if self.enabled {
            let retained_weight = MEMO_ENTRY_OVERHEAD_BYTES
                + path_retained_weight(path)
                + parsed
                    .as_deref()
                    .map(json_retained_weight)
                    .unwrap_or_default();
            self.json_values.borrow_mut().insert(
                path.to_path_buf(),
                parsed.clone(),
                retained_weight,
            );
        }
        parsed
    }

    pub(crate) fn rust_declared_module_target(
        &self,
        caller_file: &str,
        module_name: &str,
        populate: impl FnOnce() -> RustDeclaredModuleMap,
    ) -> Option<String> {
        if self.enabled {
            if let Some(cached) = self.rust_declared_modules.borrow().get(caller_file) {
                return cached.get(module_name).cloned().flatten();
            }
        }

        self.note_rust_declaration_parse(caller_file);
        let declared_modules = Arc::new(populate());
        let resolved = declared_modules.get(module_name).cloned().flatten();
        if self.enabled {
            let retained_weight = rust_declared_module_entry_weight(caller_file, &declared_modules);
            self.rust_declared_modules.borrow_mut().insert(
                caller_file.to_string(),
                declared_modules,
                retained_weight,
            );
        }
        resolved
    }

    fn workspace_package(&self, key: &WorkspacePackageKey) -> Option<Option<PathBuf>> {
        self.enabled
            .then(|| self.workspace_packages.borrow().get(key).cloned())
            .flatten()
    }

    fn remember_workspace_package(&self, key: WorkspacePackageKey, resolved: Option<PathBuf>) {
        if !self.enabled {
            return;
        }
        let retained_weight = module_resolution_entry_weight(&key, resolved.as_deref());
        self.workspace_packages
            .borrow_mut()
            .insert(key, resolved, retained_weight);
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(enabled: bool, collect_metrics: bool) -> Self {
        Self {
            enabled,
            collect_metrics,
            ..Self::default()
        }
    }

    #[cfg(test)]
    pub(crate) fn module_computations_for_test(&self) -> HashMap<ModuleResolutionKey, usize> {
        self.module_computations.borrow().clone()
    }

    #[cfg(test)]
    pub(crate) fn json_probes_for_test(&self) -> HashMap<PathBuf, usize> {
        self.json_probes.borrow().clone()
    }

    #[cfg(test)]
    pub(crate) fn rust_declaration_parses_for_test(&self) -> HashMap<String, usize> {
        self.rust_declaration_parses.borrow().clone()
    }

    #[cfg(test)]
    fn note_module_computation(&self, key: &ModuleResolutionKey) {
        if self.collect_metrics {
            *self
                .module_computations
                .borrow_mut()
                .entry(key.clone())
                .or_default() += 1;
        }
    }

    #[cfg(not(test))]
    fn note_module_computation(&self, _key: &ModuleResolutionKey) {}

    #[cfg(test)]
    fn note_json_probe(&self, path: &Path) {
        if self.collect_metrics {
            *self
                .json_probes
                .borrow_mut()
                .entry(path.to_path_buf())
                .or_default() += 1;
        }
    }

    #[cfg(not(test))]
    fn note_json_probe(&self, _path: &Path) {}

    #[cfg(test)]
    fn note_rust_declaration_parse(&self, caller_file: &str) {
        if self.collect_metrics {
            *self
                .rust_declaration_parses
                .borrow_mut()
                .entry(caller_file.to_string())
                .or_default() += 1;
        }
    }

    #[cfg(not(test))]
    fn note_rust_declaration_parse(&self, _caller_file: &str) {}
}

fn module_resolution_entry_weight(key: &(PathBuf, String), resolved: Option<&Path>) -> usize {
    MEMO_ENTRY_OVERHEAD_BYTES
        + path_retained_weight(&key.0)
        + key.1.len()
        + resolved.map(path_retained_weight).unwrap_or_default()
}

fn path_retained_weight(path: &Path) -> usize {
    path.to_string_lossy().len()
}

fn rust_declared_module_entry_weight(
    caller_file: &str,
    declared_modules: &RustDeclaredModuleMap,
) -> usize {
    MEMO_ENTRY_OVERHEAD_BYTES
        + caller_file.len()
        + declared_modules
            .iter()
            .map(|(module_name, target)| {
                MEMO_ENTRY_OVERHEAD_BYTES
                    + module_name.len()
                    + target.as_deref().map(str::len).unwrap_or_default()
            })
            .sum::<usize>()
}

fn json_retained_weight(value: &Value) -> usize {
    std::mem::size_of::<Value>()
        + match value {
            Value::Null | Value::Bool(_) | Value::Number(_) => 0,
            Value::String(value) => value.len(),
            Value::Array(values) => values.iter().map(json_retained_weight).sum(),
            Value::Object(values) => values
                .iter()
                .map(|(key, value)| {
                    key.len() + MEMO_ENTRY_OVERHEAD_BYTES + json_retained_weight(value)
                })
                .sum(),
        }
}

const TOP_LEVEL_SYMBOL: &str = "<top-level>";
const JS_TS_EXTENSIONS: &[&str] = &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"];
const JS_TS_INDEX_FILES: &[&str] = &[
    "index.ts",
    "index.tsx",
    "index.mts",
    "index.cts",
    "index.js",
    "index.jsx",
    "index.mjs",
    "index.cjs",
];

fn symbol_identity(symbol: &Symbol) -> String {
    if symbol.scope_chain.is_empty() {
        symbol.name.clone()
    } else {
        format!("{}::{}", symbol.scope_chain.join("::"), symbol.name)
    }
}

fn symbol_unqualified_name(symbol: &str) -> &str {
    symbol.rsplit("::").next().unwrap_or(symbol)
}

pub(crate) fn is_bare_callee(full_callee: &str, short_name: &str) -> bool {
    full_callee == short_name || (!full_callee.contains('.') && !full_callee.contains("::"))
}

fn symbol_query_candidates(file_data: &FileCallData, symbol_name: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut candidates = Vec::new();
    let qualified_query = symbol_name.contains("::");

    let mut consider = |candidate: &str| {
        let matches = if qualified_query {
            candidate == symbol_name
        } else {
            candidate == symbol_name || symbol_unqualified_name(candidate) == symbol_name
        };

        if matches && seen.insert(candidate.to_string()) {
            candidates.push(candidate.to_string());
        }
    };

    for candidate in file_data.symbol_metadata.keys() {
        consider(candidate);
    }
    for candidate in file_data.calls_by_symbol.keys() {
        consider(candidate);
    }
    for candidate in &file_data.exported_symbols {
        consider(candidate);
    }

    candidates.sort();
    candidates
}

pub(crate) fn resolve_symbol_query_in_data(
    file_data: &FileCallData,
    file: &Path,
    symbol_name: &str,
) -> Result<String, AftError> {
    let candidates = symbol_query_candidates(file_data, symbol_name);
    match candidates.as_slice() {
        [candidate] => Ok(candidate.clone()),
        [] => Err(AftError::SymbolNotFound {
            name: symbol_name.to_string(),
            file: file.display().to_string(),
        }),
        _ => Err(AftError::AmbiguousSymbol {
            name: symbol_name.to_string(),
            candidates,
        }),
    }
}

/// A single call site within a function body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallSite {
    /// The short callee name (last segment, e.g. "foo" for `utils.foo()`).
    pub callee_name: String,
    /// The full callee expression (e.g. "utils.foo" for `utils.foo()`).
    pub full_callee: String,
    /// 1-based line number of the call.
    pub line: u32,
    /// Byte range of the call expression in the source.
    pub byte_start: usize,
    pub byte_end: usize,
}

/// Per-symbol metadata for entry point detection (avoids re-parsing).
#[derive(Debug, Clone, Serialize)]
pub struct SymbolMeta {
    /// The kind of symbol (function, class, method, etc).
    pub kind: SymbolKind,
    /// Whether this symbol is exported.
    pub exported: bool,
    /// Function/method signature if available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// 1-based start line of the symbol.
    pub line: u32,
    /// 0-based source range of the symbol.
    pub range: Range,
    /// Attribute that marks the symbol as externally reachable, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_point_attribute: Option<String>,
}

/// Per-file call data: call sites grouped by containing symbol, plus
/// exported symbol names and parsed imports.
#[derive(Debug, Clone)]
pub struct FileCallData {
    /// Map from symbol name → list of call sites within that symbol's body.
    pub calls_by_symbol: HashMap<String, Vec<CallSite>>,
    /// Rust function items referenced as values, grouped by containing symbol.
    /// These do not participate in callgraph navigation.
    pub value_refs_by_symbol: HashMap<String, Vec<CallSite>>,
    /// Names of exported symbols in this file.
    pub exported_symbols: Vec<String>,
    /// Per-symbol metadata (kind, exported, signature).
    pub symbol_metadata: HashMap<String, SymbolMeta>,
    /// Real or synthetic symbol name for this file's default export.
    pub default_export_symbol: Option<String>,
    /// Parsed import block for cross-file resolution.
    pub import_block: ImportBlock,
    /// Language of the file.
    pub lang: LangId,
}

impl FileCallData {
    /// Look up metadata for an exported symbol name.
    ///
    /// `exported_symbols` stores bare names (e.g. `total_disk_bytes`), but
    /// `symbol_metadata` is keyed by scoped identity (e.g.
    /// `BackupStore::total_disk_bytes` for impl methods, via
    /// [`symbol_identity`]). A bare-name `.get()` therefore misses scoped
    /// symbols and forces callers into degraded `unknown`/line-1 fallbacks.
    /// This resolves an exact key first, then falls back to the first entry
    /// whose unqualified name matches — recovering correct kind and line for
    /// methods. (Bare-name exports are already ambiguous across scopes, so
    /// first-match is the best available signal; this only affects displayed
    /// metadata, never liveness, which keys on the symbol name.)
    pub fn symbol_metadata_for(&self, name: &str) -> Option<&SymbolMeta> {
        if let Some(meta) = self.symbol_metadata.get(name) {
            return Some(meta);
        }
        self.symbol_metadata
            .iter()
            .find(|(key, _)| symbol_unqualified_name(key) == name)
            .map(|(_, meta)| meta)
    }
}

/// Result of resolving a cross-file call edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EdgeResolution {
    /// Successfully resolved to a specific file and symbol.
    Resolved { file: PathBuf, symbol: String },
    /// Could not resolve — callee name preserved for diagnostics.
    Unresolved { callee_name: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedSymbol {
    file: PathBuf,
    symbol: String,
}

#[derive(Debug, Clone)]
struct RustCrateInfo {
    lib_name: String,
    lib_root: Option<PathBuf>,
    main_root: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct RustModuleBase {
    src_dir: PathBuf,
    root_file: PathBuf,
}

#[derive(Debug, Clone)]
struct RustUseEntry {
    module_path: String,
    local_name: String,
    kind: RustUseKind,
}

#[derive(Debug, Clone)]
enum RustUseKind {
    Item { imported_name: String },
    Module,
}

/// A node in the forward call tree.
#[derive(Debug, Clone, Serialize)]
pub struct CallTreeNode {
    /// Symbol name.
    pub name: String,
    /// File path (relative to project root when possible).
    pub file: String,
    /// 1-based line number.
    pub line: u32,
    /// Function signature if available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// Whether this edge was resolved cross-file.
    pub resolved: bool,
    /// Child calls (recursive).
    pub children: Vec<CallTreeNode>,
    /// Whether traversal below this node stopped at the requested depth.
    pub depth_limited: bool,
    /// Number of child call edges omitted because of the depth limit.
    pub truncated: usize,
}

// ---------------------------------------------------------------------------
// Entry point detection
// ---------------------------------------------------------------------------

/// Well-known main/init function names (case-insensitive exact match).
const MAIN_INIT_NAMES: &[&str] = &["main", "init", "setup", "bootstrap", "run"];

/// Determine whether a symbol is an entry point.
///
/// Entry points are:
/// - Exported standalone functions (not methods — methods are class members)
/// - Functions matching well-known main/init patterns (any language)
/// - Test functions matching language-specific patterns
pub fn is_entry_point(name: &str, kind: &SymbolKind, exported: bool, lang: LangId) -> bool {
    // Exported standalone functions
    if exported && *kind == SymbolKind::Function {
        return true;
    }

    // Main/init patterns (case-insensitive exact match, any kind)
    let lower = name.to_lowercase();
    if MAIN_INIT_NAMES.contains(&lower.as_str()) {
        return true;
    }

    // Test patterns by language
    match lang {
        LangId::TypeScript | LangId::JavaScript | LangId::Tsx => {
            // describe, it, test (exact), or starts with test/spec
            matches!(lower.as_str(), "describe" | "it" | "test")
                || lower.starts_with("test")
                || lower.starts_with("spec")
        }
        LangId::Python => {
            // starts with test_ or matches setUp/tearDown
            lower.starts_with("test_") || matches!(name, "setUp" | "tearDown")
        }
        LangId::Rust => {
            // starts with test_
            lower.starts_with("test_")
        }
        LangId::Go => {
            // starts with Test (case-sensitive)
            name.starts_with("Test")
        }
        LangId::C
        | LangId::Cpp
        | LangId::Zig
        | LangId::CSharp
        | LangId::Bash
        | LangId::Solidity
        | LangId::Scss
        | LangId::Vue
        | LangId::Json
        | LangId::Scala
        | LangId::Java
        | LangId::Ruby
        | LangId::Kotlin
        | LangId::Swift
        | LangId::Php
        | LangId::Lua
        | LangId::Perl
        | LangId::Html
        | LangId::Markdown
        | LangId::Yaml
        | LangId::Pascal
        | LangId::R
        | LangId::Groovy
        | LangId::ObjC => false,
    }
}

// ---------------------------------------------------------------------------
// Trace-to types
// ---------------------------------------------------------------------------

/// A single hop in a trace path.
#[derive(Debug, Clone, Serialize)]
pub struct TraceHop {
    /// Symbol name at this hop.
    pub symbol: String,
    /// File path (relative to project root).
    pub file: String,
    /// 1-based line number.
    pub line: u32,
    /// Function signature if available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// Whether this hop is an entry point.
    pub is_entry_point: bool,
}

/// A complete path from an entry point to the target symbol (top-down).
#[derive(Debug, Clone, Serialize)]
pub struct TracePath {
    /// Hops from entry point (first) to target (last).
    pub hops: Vec<TraceHop>,
}

/// Result of a `trace_to` query.
#[derive(Debug, Clone, Serialize)]
pub struct TraceToResult {
    /// The target symbol that was traced.
    pub target_symbol: String,
    /// The target file (relative to project root).
    pub target_file: String,
    /// Complete paths from entry points to the target.
    pub paths: Vec<TracePath>,
    /// Total number of complete paths found.
    pub total_paths: usize,
    /// Number of distinct entry points found across all paths.
    pub entry_points_found: usize,
    /// Whether any path was cut short by the depth limit.
    pub max_depth_reached: bool,
    /// Number of paths that reached a dead end (no callers, not entry point).
    pub truncated_paths: usize,
}

/// A single hop in a `trace_to_symbol` path.
#[derive(Debug, Clone, Serialize)]
pub struct TraceToSymbolHop {
    /// Symbol name at this hop.
    pub symbol: String,
    /// File path (relative to project root).
    pub file: String,
    /// 1-based definition line number.
    pub line: u32,
}

/// Candidate target location for an ambiguous `trace_to_symbol` request.
#[derive(Debug, Clone, Serialize)]
pub struct TraceToSymbolCandidate {
    /// File path (relative to project root).
    pub file: String,
    /// 1-based definition line number.
    pub line: u32,
}

/// Result of a `trace_to_symbol` query.
#[derive(Debug, Clone, Serialize)]
pub struct TraceToSymbolResult {
    /// Shortest path from the origin symbol to the target symbol, if found.
    pub path: Option<Vec<TraceToSymbolHop>>,
    /// Whether traversal was complete within the requested depth.
    pub complete: bool,
    /// Machine-readable explanation when `path` is null.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

// ---------------------------------------------------------------------------
// Data flow tracking types
// ---------------------------------------------------------------------------

/// A single hop in a data flow trace.
#[derive(Debug, Clone, Serialize)]
pub struct DataFlowHop {
    /// File path (relative to project root).
    pub file: String,
    /// Symbol (function/method) containing this hop.
    pub symbol: String,
    /// Variable or parameter name being tracked at this hop.
    pub variable: String,
    /// 1-based line number.
    pub line: u32,
    /// Type of data flow: "assignment", "parameter", or "return".
    pub flow_type: String,
    /// Whether this hop is an approximation (destructuring, spread, unresolved).
    pub approximate: bool,
}

/// Result of a `trace_data` query — tracks how an expression flows through
/// variable assignments and function parameters.
#[derive(Debug, Clone, Serialize)]
pub struct TraceDataResult {
    /// The expression being tracked.
    pub expression: String,
    /// The file where tracking started.
    pub origin_file: String,
    /// The symbol where tracking started.
    pub origin_symbol: String,
    /// Hops through assignments and parameters.
    pub hops: Vec<DataFlowHop>,
    /// Whether tracking stopped due to depth limit.
    pub depth_limited: bool,
}

/// Extract parameter names from a function signature string.
///
/// Strips language-specific receivers (`self`, `&self`, `&mut self` for Rust,
/// `self` for Python) and type annotations / default values. Returns just
/// the parameter names.
pub fn extract_parameters(signature: &str, lang: LangId) -> Vec<String> {
    // Find the parameter list between parentheses
    let start = match signature.find('(') {
        Some(i) => i + 1,
        None => return Vec::new(),
    };
    let end = match signature[start..].find(')') {
        Some(i) => start + i,
        None => return Vec::new(),
    };

    let params_str = &signature[start..end].trim();
    if params_str.is_empty() {
        return Vec::new();
    }

    // Split on commas, respecting nested generics/brackets
    let parts = split_params(params_str);

    let mut result = Vec::new();
    for part in parts {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Skip language-specific receivers
        match lang {
            LangId::Rust => {
                if trimmed == "self"
                    || trimmed == "mut self"
                    || trimmed.starts_with("&self")
                    || trimmed.starts_with("&mut self")
                {
                    continue;
                }
            }
            LangId::Python => {
                if trimmed == "self" || trimmed.starts_with("self:") {
                    continue;
                }
            }
            _ => {}
        }

        // Extract just the parameter name
        let name = extract_param_name(trimmed, lang);
        if !name.is_empty() {
            result.push(name);
        }
    }

    result
}

/// Split parameter string on commas, respecting nested brackets/generics.
fn split_params(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut depth = 0i32;

    for ch in s.chars() {
        match ch {
            '<' | '[' | '{' | '(' => {
                depth += 1;
                current.push(ch);
            }
            '>' | ']' | '}' | ')' => {
                depth -= 1;
                current.push(ch);
            }
            ',' if depth == 0 => {
                parts.push(current.clone());
                current.clear();
            }
            _ => {
                current.push(ch);
            }
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

/// Extract the parameter name from a single parameter declaration.
///
/// Handles:
/// - TS/JS: `name: Type`, `name = default`, `...name`, `name?: Type`
/// - Python: `name: Type`, `name=default`, `*args`, `**kwargs`
/// - Rust: `name: Type`, `mut name: Type`
/// - Go: `name Type`, `name, name2 Type`
fn extract_param_name(param: &str, lang: LangId) -> String {
    let trimmed = param.trim();

    // Handle rest/spread params
    let working = if trimmed.starts_with("...") {
        &trimmed[3..]
    } else if trimmed.starts_with("**") {
        &trimmed[2..]
    } else if trimmed.starts_with('*') && lang == LangId::Python {
        &trimmed[1..]
    } else {
        trimmed
    };

    // Rust: `mut name: Type` → strip `mut `
    let working = if lang == LangId::Rust && working.starts_with("mut ") {
        &working[4..]
    } else {
        working
    };

    // Strip type annotation (`: Type`) and default values (`= default`)
    // Take only the name part — everything before `:`, `=`, or `?`
    let name = working
        .split(|c: char| c == ':' || c == '=')
        .next()
        .unwrap_or("")
        .trim();

    // Strip trailing `?` (optional params in TS)
    let name = name.trim_end_matches('?');

    // For Go, the name might be just `name Type` — take the first word
    if lang == LangId::Go && !name.contains(' ') {
        return name.to_string();
    }
    if lang == LangId::Go {
        return name.split_whitespace().next().unwrap_or("").to_string();
    }

    name.to_string()
}

// ---------------------------------------------------------------------------
// CallGraph
// ---------------------------------------------------------------------------

/// Worktree-scoped call graph with lazy per-file construction.
///
/// Files are parsed and analyzed on first access, then cached. The graph
/// can resolve cross-file call edges using the import engine.
pub struct CallGraph {
    /// Cached per-file call data.
    data: HashMap<PathBuf, FileCallData>,
    /// Project root for relative path resolution.
    project_root: PathBuf,
}

impl CallGraph {
    /// Create a new call graph for a project.
    pub fn new(project_root: PathBuf) -> Self {
        clear_workspace_package_cache();
        Self {
            data: HashMap::new(),
            project_root,
        }
    }

    /// Get the project root directory.
    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    fn resolve_cross_file_edge_with_exports<F, D>(
        full_callee: &str,
        short_name: &str,
        caller_file: &Path,
        import_block: &ImportBlock,
        mut file_exports_symbol: F,
        mut file_default_export_symbol: D,
    ) -> EdgeResolution
    where
        F: FnMut(&Path, &str) -> bool,
        D: FnMut(&Path) -> Option<String>,
    {
        let caller_dir = caller_file.parent().unwrap_or(Path::new("."));
        let disk = DiskFacts::new(caller_dir.ancestors().last().unwrap_or(caller_dir));
        let facts = &FactPaths {
            root: &disk.project_root,
            facts: &disk,
        };

        // Rust uses `::` module paths rather than JS/TS specifiers. Keep this
        // branch gated to `.rs` callers so the existing JS/TS resolver below
        // remains unchanged.
        if is_rust_source_file(caller_file) {
            if let Some(target) = resolve_rust_cross_file_edge(
                full_callee,
                short_name,
                caller_file,
                import_block,
                &mut file_exports_symbol,
            ) {
                return EdgeResolution::Resolved {
                    file: target.file,
                    symbol: target.symbol,
                };
            }
        }

        // Check namespace imports: "utils.foo" where utils is a namespace import
        if full_callee.contains('.') {
            let parts: Vec<&str> = full_callee.splitn(2, '.').collect();
            if parts.len() == 2 {
                let namespace = parts[0];
                let member = parts[1];

                for imp in &import_block.imports {
                    if imp.namespace_import.as_deref() == Some(namespace) {
                        if let Some(resolved_path) =
                            resolve_module_path(caller_dir, &imp.module_path)
                        {
                            if let Some(target) = resolve_reexported_symbol(
                                &resolved_path,
                                member,
                                &mut file_exports_symbol,
                                &mut file_default_export_symbol,
                            ) {
                                return EdgeResolution::Resolved {
                                    file: target.file,
                                    symbol: target.symbol,
                                };
                            }
                        }
                    }
                }
            }
        }

        // Check named imports (direct and aliased)
        for imp in &import_block.imports {
            // Direct named import: import { foo } from './utils'
            if imp.names.iter().any(|name| name == short_name) {
                if let Some(resolved_path) = resolve_module_path(caller_dir, &imp.module_path) {
                    let target = resolve_reexported_symbol(
                        &resolved_path,
                        short_name,
                        &mut file_exports_symbol,
                        &mut file_default_export_symbol,
                    )
                    .unwrap_or(ResolvedSymbol {
                        file: resolved_path,
                        symbol: short_name.to_owned(),
                    });
                    return EdgeResolution::Resolved {
                        file: target.file,
                        symbol: target.symbol,
                    };
                }
            }

            // Default import: import foo from './utils'
            if imp.default_import.as_deref() == Some(short_name) {
                if let Some(resolved_path) = resolve_module_path(caller_dir, &imp.module_path) {
                    let target = resolve_reexported_symbol(
                        &resolved_path,
                        "default",
                        &mut file_exports_symbol,
                        &mut file_default_export_symbol,
                    )
                    .unwrap_or_else(|| ResolvedSymbol {
                        symbol: file_default_export_symbol(&resolved_path)
                            .unwrap_or_else(|| synthetic_default_symbol(&resolved_path)),
                        file: resolved_path,
                    });
                    return EdgeResolution::Resolved {
                        file: target.file,
                        symbol: target.symbol,
                    };
                }
            }
        }

        // Check aliased imports by examining the raw import text.
        // ImportStatement.names stores the original name (foo), but the local code
        // uses the alias (bar). We need to parse `import { foo as bar }` to find
        // that `bar` maps to `foo`.
        if let Some((original_name, resolved_path)) =
            resolve_aliased_import(short_name, import_block, caller_dir)
        {
            let target = resolve_reexported_symbol(
                &resolved_path,
                &original_name,
                &mut file_exports_symbol,
                &mut file_default_export_symbol,
            )
            .unwrap_or(ResolvedSymbol {
                file: resolved_path,
                symbol: original_name,
            });
            return EdgeResolution::Resolved {
                file: target.file,
                symbol: target.symbol,
            };
        }

        // Try barrel file re-exports: if any import points to an index file,
        // check if that file re-exports the symbol
        for imp in &import_block.imports {
            if let Some(resolved_path) = resolve_module_path(caller_dir, &imp.module_path) {
                // Check if the resolved path is a directory (barrel file)
                if resolved_path.is_dir() {
                    if let Some(index_path) = find_index_file(&resolved_path, facts) {
                        // Check if the index file exports this symbol
                        if file_exports_symbol(&index_path, short_name) {
                            return EdgeResolution::Resolved {
                                file: index_path,
                                symbol: short_name.to_owned(),
                            };
                        }
                    }
                } else if file_exports_symbol(&resolved_path, short_name) {
                    return EdgeResolution::Resolved {
                        file: resolved_path,
                        symbol: short_name.to_owned(),
                    };
                }
            }
        }

        EdgeResolution::Unresolved {
            callee_name: short_name.to_owned(),
        }
    }

    /// Get or build the call data for a file.
    pub fn build_file(&mut self, path: &Path) -> Result<&FileCallData, AftError> {
        let canon = self.canonicalize(path)?;

        if !self.data.contains_key(&canon) {
            let file_data = build_file_data(&canon)?;
            self.data.insert(canon.clone(), file_data);
        }

        Ok(&self.data[&canon])
    }

    /// Resolve a cross-file call edge.
    ///
    /// Given a callee expression and the calling file's import block,
    /// determines which file and symbol the call targets.
    pub fn resolve_cross_file_edge(
        &mut self,
        full_callee: &str,
        short_name: &str,
        caller_file: &Path,
        import_block: &ImportBlock,
    ) -> EdgeResolution {
        let graph = RefCell::new(self);
        Self::resolve_cross_file_edge_with_exports(
            full_callee,
            short_name,
            caller_file,
            import_block,
            |path, symbol_name| graph.borrow_mut().file_exports_symbol(path, symbol_name),
            |path| graph.borrow_mut().file_default_export_symbol(path),
        )
    }

    /// Check if a file exports a given symbol name.
    fn file_exports_symbol(&mut self, path: &Path, symbol_name: &str) -> bool {
        match self.build_file(path) {
            Ok(data) => data.exported_symbols.iter().any(|name| name == symbol_name),
            Err(_) => false,
        }
    }

    fn file_default_export_symbol(&mut self, path: &Path) -> Option<String> {
        self.build_file(path)
            .ok()
            .and_then(|data| data.default_export_symbol.clone())
    }

    /// Invalidate a file by removing its cached call data.
    pub fn invalidate_file(&mut self, path: &Path) {
        // Remove from data cache (try both as-is and canonicalized)
        self.data.remove(path);
        if let Ok(canon) = self.canonicalize(path) {
            self.data.remove(&canon);
        }
        clear_workspace_package_cache();
    }

    /// Canonicalize a path, falling back to the original if canonicalization fails.
    fn canonicalize(&self, path: &Path) -> Result<PathBuf, AftError> {
        // If the path is relative, resolve it against project_root
        let full_path = if path.is_relative() {
            self.project_root.join(path)
        } else {
            path.to_path_buf()
        };

        // Try canonicalize, fall back to the full path
        Ok(std::fs::canonicalize(&full_path).unwrap_or(full_path))
    }
}

// ---------------------------------------------------------------------------
// File-level building
// ---------------------------------------------------------------------------

/// Build call data for a single file.
pub(crate) fn build_file_data(path: &Path) -> Result<FileCallData, AftError> {
    let lang = detect_language(path).ok_or_else(|| AftError::InvalidRequest {
        message: format!("unsupported file for call graph: {}", path.display()),
    })?;

    let source = std::fs::read_to_string(path).map_err(|e| AftError::FileNotFound {
        path: format!("{}: {}", path.display(), e),
    })?;

    build_file_data_from_source_with_lang(path, &source, lang)
}

pub(crate) fn build_file_data_from_source(
    path: &Path,
    source: &str,
) -> Result<FileCallData, AftError> {
    let lang = detect_language(path).ok_or_else(|| AftError::InvalidRequest {
        message: format!("unsupported file for call graph: {}", path.display()),
    })?;
    build_file_data_from_source_with_lang(path, source, lang)
}

#[derive(Debug)]
struct SymbolCallRange {
    symbol_index: usize,
    byte_start: usize,
    byte_end: usize,
}

struct SourceLineIndex {
    bounds: Vec<(usize, usize)>,
    source_len: usize,
}

impl SourceLineIndex {
    fn new(source: &str) -> Self {
        let bytes = source.as_bytes();
        let mut bounds = Vec::new();
        let mut line_start = 0usize;
        let mut index = 0usize;

        while index < bytes.len() {
            match bytes[index] {
                b'\r' => {
                    bounds.push((line_start, index));
                    index += if bytes.get(index + 1) == Some(&b'\n') {
                        2
                    } else {
                        1
                    };
                    line_start = index;
                }
                b'\n' => {
                    bounds.push((line_start, index));
                    index += 1;
                    line_start = index;
                }
                _ => index += 1,
            }
        }
        bounds.push((line_start, bytes.len()));

        Self {
            bounds,
            source_len: bytes.len(),
        }
    }

    fn byte_offset(&self, line: u32, column: u32) -> usize {
        let Some(&(line_start, line_end)) = self.bounds.get(line as usize) else {
            return self.source_len;
        };
        line_start + (column as usize).min(line_end.saturating_sub(line_start))
    }
}

fn collect_calls_by_symbol(
    source: &str,
    root: Node<'_>,
    lang: LangId,
    symbols: &[Symbol],
) -> HashMap<String, Vec<CallSite>> {
    attribute_sites_to_symbols(
        source,
        symbols,
        extract_calls_full(source, root, 0, source.len(), lang),
    )
}

fn collect_rust_value_refs_by_symbol(
    source: &str,
    root: Node<'_>,
    symbols: &[Symbol],
) -> HashMap<String, Vec<CallSite>> {
    attribute_sites_to_symbols(source, symbols, extract_rust_value_references(source, root))
}

fn attribute_sites_to_symbols(
    source: &str,
    symbols: &[Symbol],
    raw_sites: Vec<(String, String, u32, usize, usize)>,
) -> HashMap<String, Vec<CallSite>> {
    let line_index = SourceLineIndex::new(source);
    let mut ranges = symbols
        .iter()
        .enumerate()
        .map(|(symbol_index, symbol)| SymbolCallRange {
            symbol_index,
            byte_start: line_index.byte_offset(symbol.range.start_line, symbol.range.start_col),
            byte_end: line_index.byte_offset(symbol.range.end_line, symbol.range.end_col),
        })
        .collect::<Vec<_>>();
    ranges.sort_by(|left, right| {
        left.byte_start
            .cmp(&right.byte_start)
            .then_with(|| left.symbol_index.cmp(&right.symbol_index))
    });

    let mut sites_by_symbol = vec![Vec::new(); symbols.len()];
    let mut top_level_sites = Vec::new();
    let mut active_ranges = Vec::<usize>::new();
    let mut next_range = 0usize;

    for (full, short, line, byte_start, byte_end) in raw_sites {
        // AST preorder gives nondecreasing call starts. Expire by start only:
        // an outer call can end past a range that still contains a nested call.
        active_ranges.retain(|range_index| ranges[*range_index].byte_end > byte_start);
        while next_range < ranges.len() && ranges[next_range].byte_start <= byte_start {
            if ranges[next_range].byte_end > byte_start {
                active_ranges.push(next_range);
            }
            next_range += 1;
        }

        let site = CallSite {
            callee_name: short,
            full_callee: full,
            line,
            byte_start,
            byte_end,
        };
        let mut attributed = false;
        for range_index in &active_ranges {
            let range = &ranges[*range_index];
            if byte_end <= range.byte_end {
                sites_by_symbol[range.symbol_index].push(site.clone());
                attributed = true;
            }
        }
        if !attributed {
            top_level_sites.push(site);
        }
    }

    let mut calls_by_symbol = HashMap::new();
    for (symbol, sites) in symbols.iter().zip(sites_by_symbol) {
        if !sites.is_empty() {
            calls_by_symbol.insert(symbol_identity(symbol), sites);
        }
    }
    if !top_level_sites.is_empty() {
        calls_by_symbol.insert(TOP_LEVEL_SYMBOL.to_string(), top_level_sites);
    }
    calls_by_symbol
}

pub(crate) fn build_file_data_from_source_with_lang(
    path: &Path,
    source: &str,
    lang: LangId,
) -> Result<FileCallData, AftError> {
    let grammar = grammar_for(lang);
    let mut parser = Parser::new();
    parser
        .set_language(&grammar)
        .map_err(|e| AftError::ParseError {
            message: format!("grammar init failed for {:?}: {}", lang, e),
        })?;

    let tree = parser
        .parse(&source, None)
        .ok_or_else(|| AftError::ParseError {
            message: format!("parse failed for {}", path.display()),
        })?;

    // Parse imports
    let import_block = imports::parse_imports(&source, &tree, lang);

    // Get symbols (for call site extraction and export detection)
    let symbols = crate::parser::extract_symbols_from_tree(&source, &tree, lang)?;

    let root = tree.root_node();
    let mut calls_by_symbol = collect_calls_by_symbol(&source, root, lang, &symbols);
    let value_refs_by_symbol = if lang == LangId::Rust {
        collect_rust_value_refs_by_symbol(&source, root, &symbols)
    } else {
        HashMap::new()
    };

    let default_export = find_default_export(&source, root, path, lang);

    if let Some(default_export) = &default_export {
        if default_export.synthetic {
            let byte_start = default_export.node.byte_range().start;
            let byte_end = default_export.node.byte_range().end;
            let raw_calls = extract_calls_full(&source, root, byte_start, byte_end, lang);
            let sites: Vec<CallSite> = raw_calls
                .into_iter()
                .filter(|(_, short, _, _, _)| *short != default_export.symbol)
                .map(
                    |(full, short, line, call_byte_start, call_byte_end)| CallSite {
                        callee_name: short,
                        full_callee: full,
                        line,
                        byte_start: call_byte_start,
                        byte_end: call_byte_end,
                    },
                )
                .collect();
            if !sites.is_empty() {
                calls_by_symbol.insert(default_export.symbol.clone(), sites);
            }
        }
    }

    // Collect exported symbol names
    let mut exported_symbols: Vec<String> = symbols
        .iter()
        .filter(|s| s.exported)
        .map(|s| s.name.clone())
        .collect();
    if let Some(default_export) = &default_export {
        if !exported_symbols
            .iter()
            .any(|name| name == &default_export.symbol)
        {
            exported_symbols.push(default_export.symbol.clone());
        }
    }

    let rust_attribute_entry_points = if lang == LangId::Rust {
        crate::parser::rust_attribute_entry_points(&source, root)
            .into_iter()
            .map(|entry| (entry.scoped_name, entry.attribute.to_string()))
            .collect::<HashMap<_, _>>()
    } else {
        HashMap::new()
    };

    // Build per-symbol metadata for entry point detection
    let mut symbol_metadata: HashMap<String, SymbolMeta> = symbols
        .iter()
        .map(|s| {
            let identity = symbol_identity(s);
            (
                identity.clone(),
                SymbolMeta {
                    kind: s.kind.clone(),
                    exported: s.exported,
                    signature: s.signature.clone(),
                    line: s.range.start_line + 1,
                    range: s.range.clone(),
                    entry_point_attribute: rust_attribute_entry_points.get(&identity).cloned(),
                },
            )
        })
        .collect();
    if let Some(default_export) = &default_export {
        symbol_metadata
            .entry(default_export.symbol.clone())
            .or_insert_with(|| SymbolMeta {
                kind: default_export.kind.clone(),
                exported: true,
                signature: Some(first_line_signature(&source, &default_export.node)),
                line: default_export.node.start_position().row as u32 + 1,
                range: crate::parser::node_range(&default_export.node),
                entry_point_attribute: None,
            });
    }
    if calls_by_symbol.contains_key(TOP_LEVEL_SYMBOL)
        || value_refs_by_symbol.contains_key(TOP_LEVEL_SYMBOL)
    {
        symbol_metadata
            .entry(TOP_LEVEL_SYMBOL.to_string())
            .or_insert(SymbolMeta {
                kind: SymbolKind::Function,
                exported: false,
                signature: None,
                line: 1,
                range: Range {
                    start_line: 0,
                    start_col: 0,
                    end_line: 0,
                    end_col: 0,
                },
                entry_point_attribute: None,
            });
    }

    Ok(FileCallData {
        calls_by_symbol,
        value_refs_by_symbol,
        exported_symbols,
        symbol_metadata,
        default_export_symbol: default_export.map(|export| export.symbol),
        import_block,
        lang,
    })
}

#[derive(Debug, Clone)]
struct DefaultExport<'tree> {
    symbol: String,
    synthetic: bool,
    kind: SymbolKind,
    node: Node<'tree>,
}

fn find_default_export<'tree>(
    source: &str,
    root: Node<'tree>,
    path: &Path,
    lang: LangId,
) -> Option<DefaultExport<'tree>> {
    if !matches!(lang, LangId::TypeScript | LangId::Tsx | LangId::JavaScript) {
        return None;
    }
    find_default_export_inner(source, root, path)
}

fn find_default_export_inner<'tree>(
    source: &str,
    node: Node<'tree>,
    path: &Path,
) -> Option<DefaultExport<'tree>> {
    if node.kind() == "export_statement" {
        if let Some(default_export) = default_export_from_statement(source, node, path) {
            return Some(default_export);
        }
    }

    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return None;
    }

    loop {
        let child = cursor.node();
        if let Some(default_export) = find_default_export_inner(source, child, path) {
            return Some(default_export);
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }

    None
}

fn default_export_from_statement<'tree>(
    source: &str,
    node: Node<'tree>,
    path: &Path,
) -> Option<DefaultExport<'tree>> {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return None;
    }

    let mut saw_default = false;
    loop {
        let child = cursor.node();
        match child.kind() {
            "default" => saw_default = true,
            "function_declaration" | "generator_function_declaration" | "class_declaration"
                if saw_default =>
            {
                if let Some(name_node) = child.child_by_field_name("name") {
                    return Some(DefaultExport {
                        symbol: source[name_node.byte_range()].to_string(),
                        synthetic: false,
                        kind: default_export_kind(&child),
                        node: child,
                    });
                }
                return Some(DefaultExport {
                    symbol: synthetic_default_symbol(path),
                    synthetic: true,
                    kind: default_export_kind(&child),
                    node: child,
                });
            }
            "arrow_function"
            | "function"
            | "function_expression"
            | "class"
            | "class_expression"
                if saw_default =>
            {
                return Some(DefaultExport {
                    symbol: synthetic_default_symbol(path),
                    synthetic: true,
                    kind: default_export_kind(&child),
                    node: child,
                });
            }
            "identifier" | "type_identifier" | "property_identifier" if saw_default => {
                return Some(DefaultExport {
                    symbol: source[child.byte_range()].to_string(),
                    synthetic: false,
                    kind: SymbolKind::Function,
                    node: child,
                });
            }
            _ => {}
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }

    None
}

fn default_export_kind(node: &Node) -> SymbolKind {
    if node.kind().contains("class") {
        SymbolKind::Class
    } else {
        SymbolKind::Function
    }
}

fn synthetic_default_symbol(path: &Path) -> String {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown");
    format!("<default:{file_name}>")
}

fn first_line_signature(source: &str, node: &Node) -> String {
    let text = &source[node.byte_range()];
    let first_line = text.lines().next().unwrap_or(text);
    first_line
        .trim_end()
        .trim_end_matches('{')
        .trim_end()
        .to_string()
}

fn node_text(node: tree_sitter::Node, source: &str) -> String {
    source[node.start_byte()..node.end_byte()].to_string()
}

/// Find a direct child node by kind name.
fn find_child_by_kind<'a>(
    node: tree_sitter::Node<'a>,
    kind: &str,
) -> Option<tree_sitter::Node<'a>> {
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            if cursor.node().kind() == kind {
                return Some(cursor.node());
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    None
}

#[cfg(test)]
#[derive(Debug, Clone)]
struct CallSiteWithRange {
    full: String,
    short: String,
    line: u32,
    byte_start: usize,
    byte_end: usize,
}

#[cfg(test)]
fn collect_calls_full_with_ranges(
    root: tree_sitter::Node,
    source: &str,
    byte_start: usize,
    byte_end: usize,
    lang: LangId,
) -> Vec<CallSiteWithRange> {
    let mut results = Vec::new();
    let call_kinds = call_node_kinds(lang);
    collect_calls_full_with_ranges_inner(
        root,
        source,
        byte_start,
        byte_end,
        &call_kinds,
        &mut results,
    );
    results
}

#[cfg(test)]
fn collect_calls_full_with_ranges_inner(
    node: tree_sitter::Node,
    source: &str,
    byte_start: usize,
    byte_end: usize,
    call_kinds: &[&str],
    results: &mut Vec<CallSiteWithRange>,
) {
    let node_start = node.start_byte();
    let node_end = node.end_byte();

    if node_end <= byte_start || node_start >= byte_end {
        return;
    }

    if call_kinds.contains(&node.kind()) && node_start >= byte_start && node_end <= byte_end {
        if let (Some(full), Some(short)) = (
            extract_full_callee(&node, source),
            extract_callee_name(&node, source),
        ) {
            results.push(CallSiteWithRange {
                full,
                short,
                line: node.start_position().row as u32 + 1,
                byte_start: node_start,
                byte_end: node_end,
            });
        }
    }

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            collect_calls_full_with_ranges_inner(
                cursor.node(),
                source,
                byte_start,
                byte_end,
                call_kinds,
                results,
            );
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Module path resolution
// ---------------------------------------------------------------------------

/// Resolve a module path (e.g. './utils') relative to a directory.
///
/// Tries common file extensions for TypeScript/JavaScript projects.
pub(crate) fn resolve_module_path(from_dir: &Path, module_path: &str) -> Option<PathBuf> {
    let disk = DiskFacts::new(from_dir.ancestors().last().unwrap_or(from_dir));
    let facts = &FactPaths {
        root: &disk.project_root,
        facts: &disk,
    };
    resolve_module_path_uncached(from_dir, module_path, None, facts)
}

pub(crate) fn resolve_module_path_with_memo(
    from_dir: &Path,
    module_path: &str,
    memo: &ModuleResolutionMemo,
    facts: &FactPaths<'_>,
) -> Option<PathBuf> {
    memo.resolve_module_path(from_dir, module_path, facts)
}

fn resolve_module_path_uncached(
    from_dir: &Path,
    module_path: &str,
    memo: Option<&ModuleResolutionMemo>,
    facts: &FactPaths<'_>,
) -> Option<PathBuf> {
    if module_path.starts_with('.') {
        return resolve_relative_module_path(from_dir, module_path, facts);
    }

    if module_path.starts_with('/') {
        return None;
    }

    if let Some(path) = resolve_tsconfig_path(from_dir, module_path, memo, facts) {
        return Some(path);
    }

    resolve_workspace_module_path(from_dir, module_path, memo, facts)
}

fn resolve_relative_module_path(
    from_dir: &Path,
    module_path: &str,
    facts: &FactPaths<'_>,
) -> Option<PathBuf> {
    let base = from_dir.join(module_path);
    resolve_file_like_path(&base, facts)
}

fn resolve_file_like_path(base: &Path, facts: &FactPaths<'_>) -> Option<PathBuf> {
    let base = base.to_path_buf();

    // Try exact path first
    if facts.is_file(&base) {
        return Some(facts.canonical(&base).unwrap_or(base));
    }

    // Try common extensions, including ESM/CJS TypeScript pairs used by workspaces.
    for ext in JS_TS_EXTENSIONS {
        let with_ext = base.with_extension(ext);
        if facts.is_file(&with_ext) {
            return Some(facts.canonical(&with_ext).unwrap_or(with_ext));
        }
    }

    // Try as directory with index file
    if facts.is_dir(&base) {
        if let Some(index) = find_index_file(&base, facts) {
            return Some(index);
        }
    }

    None
}

fn resolve_workspace_module_path(
    from_dir: &Path,
    module_path: &str,
    memo: Option<&ModuleResolutionMemo>,
    facts: &FactPaths<'_>,
) -> Option<PathBuf> {
    let (package_name, subpath) = split_package_import(module_path)?;
    let package_root = find_package_root_for_import(from_dir, &package_name, memo, facts)?;
    resolve_package_entry(&package_root, &subpath, memo, facts)
}

fn is_rust_source_file(path: &Path) -> bool {
    path.extension().and_then(|ext| ext.to_str()) == Some("rs")
}

fn resolve_rust_cross_file_edge<F>(
    full_callee: &str,
    short_name: &str,
    caller_file: &Path,
    import_block: &ImportBlock,
    file_exports_symbol: &mut F,
) -> Option<ResolvedSymbol>
where
    F: FnMut(&Path, &str) -> bool,
{
    if let Some(target) = resolve_rust_qualified_call(caller_file, full_callee, file_exports_symbol)
    {
        return Some(target);
    }

    resolve_rust_imported_call(
        caller_file,
        full_callee,
        short_name,
        import_block,
        file_exports_symbol,
    )
}

fn resolve_rust_qualified_call<F>(
    caller_file: &Path,
    full_callee: &str,
    file_exports_symbol: &mut F,
) -> Option<ResolvedSymbol>
where
    F: FnMut(&Path, &str) -> bool,
{
    if !full_callee.contains("::") {
        return None;
    }

    let segments = rust_path_segments(full_callee)?;
    resolve_rust_call_segments(caller_file, &segments, file_exports_symbol)
}

fn resolve_rust_imported_call<F>(
    caller_file: &Path,
    full_callee: &str,
    short_name: &str,
    import_block: &ImportBlock,
    file_exports_symbol: &mut F,
) -> Option<ResolvedSymbol>
where
    F: FnMut(&Path, &str) -> bool,
{
    let call_segments = rust_path_segments(full_callee).unwrap_or_default();
    let bare_call_name = if call_segments.len() <= 1 {
        call_segments
            .first()
            .map(String::as_str)
            .unwrap_or(short_name)
    } else {
        short_name
    };

    for imp in &import_block.imports {
        for entry in rust_use_entries(imp) {
            match &entry.kind {
                RustUseKind::Item { imported_name } if call_segments.len() <= 1 => {
                    if entry.local_name != bare_call_name {
                        continue;
                    }
                    let Some(file) = resolve_rust_module_path(caller_file, &entry.module_path)
                    else {
                        continue;
                    };
                    if file_exports_symbol(&file, imported_name) {
                        return Some(ResolvedSymbol {
                            file,
                            symbol: imported_name.clone(),
                        });
                    }
                }
                RustUseKind::Module if call_segments.len() >= 2 => {
                    if call_segments.first().map(String::as_str) != Some(entry.local_name.as_str())
                    {
                        continue;
                    }
                    let symbol = call_segments.last()?.clone();
                    let mut module_path = entry.module_path.clone();
                    for segment in &call_segments[1..call_segments.len().saturating_sub(1)] {
                        module_path.push_str("::");
                        module_path.push_str(segment);
                    }
                    let Some(file) = resolve_rust_module_path(caller_file, &module_path) else {
                        continue;
                    };
                    if file_exports_symbol(&file, &symbol) {
                        return Some(ResolvedSymbol { file, symbol });
                    }
                }
                _ => {}
            }
        }
    }

    None
}

fn resolve_rust_call_segments<F>(
    caller_file: &Path,
    segments: &[String],
    file_exports_symbol: &mut F,
) -> Option<ResolvedSymbol>
where
    F: FnMut(&Path, &str) -> bool,
{
    if segments.len() < 2 {
        return None;
    }

    let symbol = segments.last()?.clone();
    let module_path = segments[..segments.len() - 1].join("::");
    let file = resolve_rust_module_path(caller_file, &module_path)?;
    if file_exports_symbol(&file, &symbol) {
        Some(ResolvedSymbol { file, symbol })
    } else {
        None
    }
}

fn resolve_rust_module_path(caller_file: &Path, module_path: &str) -> Option<PathBuf> {
    let segments = rust_path_segments(module_path)?;
    let first = segments.first()?.as_str();

    match first {
        "std" | "core" | "alloc" => None,
        "crate" => {
            let crate_root = find_rust_crate_root(caller_file)?;
            let crate_info = rust_crate_info(&crate_root)?;
            let base = rust_module_base_for_caller(&crate_info, caller_file)?;
            resolve_rust_module_segments(&base, &segments[1..])
        }
        "self" => {
            let crate_root = find_rust_crate_root(caller_file)?;
            let crate_info = rust_crate_info(&crate_root)?;
            let base = rust_module_base_for_caller(&crate_info, caller_file)?;
            if segments.len() == 1 {
                return Some(canonicalize_path(caller_file));
            }
            let mut target_segments = rust_module_segments_for_file(&base.src_dir, caller_file)?;
            target_segments.extend(segments[1..].iter().cloned());
            resolve_rust_module_segments(&base, &target_segments)
        }
        "super" => {
            let crate_root = find_rust_crate_root(caller_file)?;
            let crate_info = rust_crate_info(&crate_root)?;
            let base = rust_module_base_for_caller(&crate_info, caller_file)?;
            let mut target_segments = rust_module_segments_for_file(&base.src_dir, caller_file)?;
            target_segments.pop();
            target_segments.extend(segments[1..].iter().cloned());
            resolve_rust_module_segments(&base, &target_segments)
        }
        crate_name => {
            let caller_dir = caller_file.parent().unwrap_or_else(|| Path::new("."));
            let workspace_crates = rust_workspace_crates(caller_dir)?;
            let crate_info = workspace_crates.get(crate_name)?;
            let base = rust_lib_module_base(crate_info)?;
            resolve_rust_module_segments(&base, &segments[1..])
        }
    }
}

fn rust_use_entries(imp: &imports::ImportStatement) -> Vec<RustUseEntry> {
    let Some(body) = rust_use_body(&imp.raw_text) else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    expand_rust_use_tree(body, &mut entries);
    entries
}

fn rust_use_body(raw: &str) -> Option<&str> {
    let use_pos = raw.find("use ")?;
    let body = raw[use_pos + 4..].trim();
    let body = body.strip_suffix(';').unwrap_or(body).trim();
    (!body.is_empty()).then_some(body)
}

fn expand_rust_use_tree(path: &str, entries: &mut Vec<RustUseEntry>) {
    let path = path.trim();
    if path.is_empty() {
        return;
    }

    if let Some((prefix, inner)) = split_rust_use_braces(path) {
        let prefix = prefix.trim().trim_end_matches("::").trim();
        for part in split_top_level_commas(inner) {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            if part == "self" {
                if let Some(local_name) = rust_last_path_segment(prefix) {
                    entries.push(RustUseEntry {
                        module_path: prefix.to_string(),
                        local_name,
                        kind: RustUseKind::Module,
                    });
                }
                continue;
            }
            let combined = if prefix.is_empty() {
                part.to_string()
            } else {
                format!("{prefix}::{part}")
            };
            expand_rust_use_tree(&combined, entries);
        }
        return;
    }

    add_rust_use_leaf(path, entries);
}

fn split_rust_use_braces(path: &str) -> Option<(&str, &str)> {
    let mut depth = 0usize;
    let mut start = None;
    for (idx, ch) in path.char_indices() {
        match ch {
            '{' => {
                if depth == 0 {
                    start = Some(idx);
                }
                depth += 1;
            }
            '}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    let start = start?;
                    if !path[idx + ch.len_utf8()..].trim().is_empty() {
                        return None;
                    }
                    return Some((&path[..start], &path[start + 1..idx]));
                }
            }
            _ => {}
        }
    }
    None
}

fn split_top_level_commas(value: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (idx, ch) in value.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&value[start..idx]);
                start = idx + ch.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&value[start..]);
    parts
}

fn add_rust_use_leaf(path: &str, entries: &mut Vec<RustUseEntry>) {
    let (path, alias) = split_rust_alias(path);
    let Some(segments) = rust_path_segments(path) else {
        return;
    };
    if segments.is_empty() || segments.last().map(String::as_str) == Some("*") {
        return;
    }

    let imported_name = segments.last().cloned().unwrap_or_default();
    let local_name = alias.unwrap_or(&imported_name).to_string();
    if segments.len() >= 2 {
        entries.push(RustUseEntry {
            module_path: segments[..segments.len() - 1].join("::"),
            local_name: local_name.clone(),
            kind: RustUseKind::Item {
                imported_name: imported_name.clone(),
            },
        });
    }

    entries.push(RustUseEntry {
        module_path: segments.join("::"),
        local_name,
        kind: RustUseKind::Module,
    });
}

fn split_rust_alias(path: &str) -> (&str, Option<&str>) {
    if let Some(idx) = path.rfind(" as ") {
        let original = path[..idx].trim();
        let alias = path[idx + 4..].trim();
        if !original.is_empty() && !alias.is_empty() {
            return (original, Some(alias));
        }
    }
    (path.trim(), None)
}

fn rust_path_segments(path: &str) -> Option<Vec<String>> {
    let path = path.trim().trim_end_matches(';').trim();
    if path.is_empty() || path.contains('{') || path.contains('}') {
        return None;
    }

    let mut segments = Vec::new();
    for raw_segment in path.split("::") {
        let segment = raw_segment.trim();
        if segment.is_empty() || segment == "*" || segment.chars().any(char::is_whitespace) {
            return None;
        }
        let segment = segment.strip_prefix("r#").unwrap_or(segment);
        if segment
            .chars()
            .any(|ch| !(ch == '_' || ch.is_ascii_alphanumeric()))
        {
            return None;
        }
        segments.push(segment.to_string());
    }

    (!segments.is_empty()).then_some(segments)
}

fn rust_last_path_segment(path: &str) -> Option<String> {
    rust_path_segments(path)?.last().cloned()
}

fn find_rust_crate_root(from: &Path) -> Option<PathBuf> {
    let mut current = if from.is_file() {
        from.parent()
    } else {
        Some(from)
    };
    while let Some(dir) = current {
        if dir.join("Cargo.toml").is_file() {
            return Some(canonicalize_path(dir));
        }
        current = dir.parent();
    }
    None
}

fn rust_crate_info(crate_root: &Path) -> Option<RustCrateInfo> {
    let root = canonicalize_path(crate_root);
    if let Some(cached) = RUST_CRATE_INFO_CACHE
        .read()
        .ok()
        .and_then(|cache| cache.get(&root).cloned())
    {
        return cached;
    }

    let resolved = read_rust_crate_info(&root);
    if let Ok(mut cache) = RUST_CRATE_INFO_CACHE.write() {
        cache.insert(root, resolved.clone());
    }
    resolved
}

fn read_rust_crate_info(crate_root: &Path) -> Option<RustCrateInfo> {
    let cargo = rust_manifest_value(&crate_root.join("Cargo.toml"))?;
    let package = cargo.get("package")?;
    let package_name = package.get("name")?.as_str()?;
    let lib_name = cargo
        .get("lib")
        .and_then(|lib| lib.get("name"))
        .and_then(|name| name.as_str())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| package_name.replace('-', "_"));

    let lib_root = cargo
        .get("lib")
        .and_then(|lib| lib.get("path"))
        .and_then(|path| path.as_str())
        .map(|path| crate_root.join(path))
        .unwrap_or_else(|| crate_root.join("src/lib.rs"));
    let lib_root = lib_root.is_file().then(|| canonicalize_path(&lib_root));

    let main_root = crate_root.join("src/main.rs");
    let main_root = main_root.is_file().then(|| canonicalize_path(&main_root));

    Some(RustCrateInfo {
        lib_name,
        lib_root,
        main_root,
    })
}

fn rust_manifest_value(path: &Path) -> Option<toml::Value> {
    let source = std::fs::read_to_string(path).ok()?;
    toml::from_str(&source).ok()
}

fn rust_module_base_for_caller(
    crate_info: &RustCrateInfo,
    caller_file: &Path,
) -> Option<RustModuleBase> {
    let caller = canonicalize_path(caller_file);
    if crate_info.main_root.as_ref() == Some(&caller) {
        return rust_main_module_base(crate_info);
    }
    rust_lib_module_base(crate_info).or_else(|| rust_main_module_base(crate_info))
}

fn rust_lib_module_base(crate_info: &RustCrateInfo) -> Option<RustModuleBase> {
    let root_file = crate_info.lib_root.clone()?;
    let src_dir = root_file.parent()?.to_path_buf();
    Some(RustModuleBase { src_dir, root_file })
}

fn rust_main_module_base(crate_info: &RustCrateInfo) -> Option<RustModuleBase> {
    let root_file = crate_info.main_root.clone()?;
    let src_dir = root_file.parent()?.to_path_buf();
    Some(RustModuleBase { src_dir, root_file })
}

fn resolve_rust_module_segments(base: &RustModuleBase, segments: &[String]) -> Option<PathBuf> {
    if segments.is_empty() {
        return Some(base.root_file.clone());
    }

    let module_base = segments
        .iter()
        .fold(base.src_dir.clone(), |path, segment| path.join(segment));
    let file_path = module_base.with_extension("rs");
    if file_path.is_file() {
        return Some(canonicalize_path(&file_path));
    }

    let mod_path = module_base.join("mod.rs");
    if mod_path.is_file() {
        return Some(canonicalize_path(&mod_path));
    }

    None
}

fn rust_module_segments_for_file(src_dir: &Path, file: &Path) -> Option<Vec<String>> {
    let src_dir = canonicalize_path(src_dir);
    let file = canonicalize_path(file);
    let rel = file.strip_prefix(&src_dir).ok()?;
    let mut parts: Vec<String> = rel
        .components()
        .filter_map(|component| component.as_os_str().to_str().map(ToOwned::to_owned))
        .collect();
    if parts.is_empty() {
        return None;
    }

    let last = parts.pop()?;
    if last == "lib.rs" || last == "main.rs" {
        return Some(Vec::new());
    }
    if last == "mod.rs" {
        return Some(parts);
    }
    let stem = Path::new(&last).file_stem()?.to_str()?.to_string();
    parts.push(stem);
    Some(parts)
}

fn rust_workspace_crates(from_dir: &Path) -> Option<HashMap<String, RustCrateInfo>> {
    let workspace_root =
        find_rust_workspace_root(from_dir).or_else(|| find_rust_crate_root(from_dir))?;
    let workspace_root = canonicalize_path(&workspace_root);

    if let Some(cached) = RUST_WORKSPACE_CRATE_CACHE
        .read()
        .ok()
        .and_then(|cache| cache.get(&workspace_root).cloned())
    {
        return Some(cached);
    }

    let mut crates = HashMap::new();
    for member in rust_workspace_member_dirs(&workspace_root) {
        if let Some(info) = rust_crate_info(&member) {
            if info.lib_root.is_some() {
                crates.insert(info.lib_name.clone(), info);
            }
        }
    }
    if let Some(info) = rust_crate_info(&workspace_root) {
        if info.lib_root.is_some() {
            crates.insert(info.lib_name.clone(), info);
        }
    }

    if let Ok(mut cache) = RUST_WORKSPACE_CRATE_CACHE.write() {
        cache.insert(workspace_root, crates.clone());
    }
    Some(crates)
}

fn find_rust_workspace_root(from_dir: &Path) -> Option<PathBuf> {
    let mut current = Some(from_dir);
    while let Some(dir) = current {
        let cargo = dir.join("Cargo.toml");
        if rust_manifest_value(&cargo)
            .and_then(|value| value.get("workspace").cloned())
            .is_some()
        {
            return Some(canonicalize_path(dir));
        }
        current = dir.parent();
    }
    None
}

fn rust_workspace_member_dirs(workspace_root: &Path) -> Vec<PathBuf> {
    let Some(cargo) = rust_manifest_value(&workspace_root.join("Cargo.toml")) else {
        return Vec::new();
    };
    let Some(members) = cargo
        .get("workspace")
        .and_then(|workspace| workspace.get("members"))
        .and_then(|members| members.as_array())
    else {
        return Vec::new();
    };

    let mut dirs = Vec::new();
    for member in members.iter().filter_map(|member| member.as_str()) {
        dirs.extend(expand_rust_workspace_member(workspace_root, member));
    }
    dirs.sort();
    dirs.dedup();
    dirs
}

fn expand_rust_workspace_member(workspace_root: &Path, member: &str) -> Vec<PathBuf> {
    let member = member.trim();
    if member.is_empty() {
        return Vec::new();
    }

    if member.contains('*') || member.contains('?') || member.contains('[') {
        let pattern = workspace_root.join(member).to_string_lossy().to_string();
        return crate::walk_boundary::expand_glob_same_file_system(&pattern)
            .unwrap_or_default()
            .into_iter()
            .filter(|path| path.join("Cargo.toml").is_file())
            .map(|path| canonicalize_path(&path))
            .collect();
    }

    let path = workspace_root.join(member);
    if path.join("Cargo.toml").is_file() {
        vec![canonicalize_path(&path)]
    } else {
        Vec::new()
    }
}

fn canonicalize_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn resolve_tsconfig_path(
    from_dir: &Path,
    module_path: &str,
    memo: Option<&ModuleResolutionMemo>,
    facts: &FactPaths<'_>,
) -> Option<PathBuf> {
    let tsconfig_dir = find_tsconfig_dir(from_dir, facts)?;
    let tsconfig = package_json_like_value(&tsconfig_dir.join("tsconfig.json"), memo, facts)?;
    let compiler_options = tsconfig.get("compilerOptions")?;
    let paths = compiler_options.get("paths")?.as_object()?;
    let base_url = compiler_options
        .get("baseUrl")
        .and_then(Value::as_str)
        .unwrap_or(".");
    let base_dir = tsconfig_dir.join(base_url);

    for (alias, targets) in paths {
        let Some(capture) = ts_path_capture(alias, module_path) else {
            continue;
        };
        let Some(targets) = targets.as_array() else {
            continue;
        };
        for target in targets.iter().filter_map(Value::as_str) {
            let target = if target.contains('*') {
                target.replace('*', capture)
            } else {
                target.to_string()
            };
            if let Some(path) = resolve_file_like_path(&base_dir.join(target), facts) {
                return Some(path);
            }
        }
    }

    None
}

fn find_tsconfig_dir(from_dir: &Path, facts: &FactPaths<'_>) -> Option<PathBuf> {
    let mut current = Some(from_dir);
    while let Some(dir) = current {
        if facts.is_file(&dir.join("tsconfig.json")) {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

fn ts_path_capture<'a>(alias: &str, module_path: &'a str) -> Option<&'a str> {
    if let Some(star_index) = alias.find('*') {
        let (prefix, suffix_with_star) = alias.split_at(star_index);
        let suffix = &suffix_with_star[1..];
        if module_path.starts_with(prefix) && module_path.ends_with(suffix) {
            return Some(&module_path[prefix.len()..module_path.len() - suffix.len()]);
        }
        return None;
    }

    (alias == module_path).then_some("")
}

fn split_package_import(module_path: &str) -> Option<(String, Option<String>)> {
    let mut parts = module_path.split('/');
    let first = parts.next()?;
    if first.is_empty() {
        return None;
    }

    if first.starts_with('@') {
        let second = parts.next()?;
        if second.is_empty() {
            return None;
        }
        let package_name = format!("{first}/{second}");
        let subpath = parts.collect::<Vec<_>>().join("/");
        let subpath = (!subpath.is_empty()).then_some(subpath);
        Some((package_name, subpath))
    } else {
        let package_name = first.to_string();
        let subpath = parts.collect::<Vec<_>>().join("/");
        let subpath = (!subpath.is_empty()).then_some(subpath);
        Some((package_name, subpath))
    }
}

fn find_package_root_for_import(
    from_dir: &Path,
    package_name: &str,
    memo: Option<&ModuleResolutionMemo>,
    facts: &FactPaths<'_>,
) -> Option<PathBuf> {
    let mut current = Some(from_dir);
    while let Some(dir) = current {
        if package_json_name(dir, memo, facts).as_deref() == Some(package_name) {
            return Some(facts.canonical(dir).unwrap_or_else(|| dir.to_path_buf()));
        }
        current = dir.parent();
    }

    find_workspace_root(from_dir, memo, facts).and_then(|workspace_root| {
        resolve_workspace_package(&workspace_root, package_name, memo, facts)
    })
}

fn find_workspace_root(
    from_dir: &Path,
    memo: Option<&ModuleResolutionMemo>,
    facts: &FactPaths<'_>,
) -> Option<PathBuf> {
    let mut current = Some(from_dir);
    while let Some(dir) = current {
        if is_workspace_root(dir, memo, facts) {
            return Some(facts.canonical(dir).unwrap_or_else(|| dir.to_path_buf()));
        }
        current = dir.parent();
    }
    None
}

fn is_workspace_root(
    dir: &Path,
    memo: Option<&ModuleResolutionMemo>,
    facts: &FactPaths<'_>,
) -> bool {
    package_json_value(dir, memo, facts)
        .map(|value| !workspace_patterns(&value).is_empty())
        .unwrap_or(false)
        || !pnpm_workspace_patterns(dir, facts).is_empty()
}

pub(crate) fn clear_workspace_package_cache() {
    if let Ok(mut cache) = WORKSPACE_PACKAGE_CACHE.write() {
        cache.clear();
    }
    if let Ok(mut cache) = RUST_CRATE_INFO_CACHE.write() {
        cache.clear();
    }
    if let Ok(mut cache) = RUST_WORKSPACE_CRATE_CACHE.write() {
        cache.clear();
    }
}

fn resolve_workspace_package(
    workspace_root: &Path,
    package_name: &str,
    memo: Option<&ModuleResolutionMemo>,
    facts: &FactPaths<'_>,
) -> Option<PathBuf> {
    let workspace_root = facts
        .canonical(workspace_root)
        .unwrap_or_else(|| workspace_root.to_path_buf());
    let cache_key = (workspace_root.clone(), package_name.to_string());

    if let Some(memo) = memo {
        if let Some(cached) = memo.workspace_package(&cache_key) {
            return cached;
        }
    } else if let Ok(cache) = WORKSPACE_PACKAGE_CACHE.read() {
        if let Some(cached) = cache.get(&cache_key) {
            return cached.clone();
        }
    }

    let resolved = workspace_member_dirs(&workspace_root, memo, facts)
        .into_iter()
        .find(|dir| package_json_name(dir, memo, facts).as_deref() == Some(package_name))
        .map(|dir| facts.canonical(&dir).unwrap_or(dir));

    if let Some(memo) = memo {
        memo.remember_workspace_package(cache_key, resolved.clone());
    } else if let Ok(mut cache) = WORKSPACE_PACKAGE_CACHE.write() {
        cache.insert(cache_key, resolved.clone());
    }

    resolved
}

fn workspace_member_dirs(
    workspace_root: &Path,
    memo: Option<&ModuleResolutionMemo>,
    facts: &FactPaths<'_>,
) -> Vec<PathBuf> {
    let mut patterns = package_json_value(workspace_root, memo, facts)
        .map(|package_json| workspace_patterns(&package_json))
        .unwrap_or_default();
    patterns.extend(pnpm_workspace_patterns(workspace_root, facts));

    expand_workspace_patterns(workspace_root, &patterns, facts)
}

fn workspace_patterns(package_json: &Value) -> Vec<String> {
    match package_json.get("workspaces") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(non_empty_workspace_pattern)
            .collect(),
        Some(Value::Object(map)) => map
            .get("packages")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(non_empty_workspace_pattern)
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn non_empty_workspace_pattern(value: &Value) -> Option<String> {
    let pattern = value.as_str()?.trim();
    (!pattern.is_empty()).then(|| pattern.to_string())
}

fn pnpm_workspace_patterns(workspace_root: &Path, facts: &FactPaths<'_>) -> Vec<String> {
    let Some(bytes) = facts.bytes(&workspace_root.join("pnpm-workspace.yaml")) else {
        return Vec::new();
    };

    let Ok(source) = std::str::from_utf8(&bytes) else {
        return Vec::new();
    };
    let mut patterns = Vec::new();
    let mut in_packages = false;
    for line in source.lines() {
        let without_comment = line.split('#').next().unwrap_or("").trim_end();
        let trimmed = without_comment.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == "packages:" {
            in_packages = true;
            continue;
        }
        if !trimmed.starts_with('-') && !line.starts_with(' ') && !line.starts_with('\t') {
            in_packages = false;
        }
        if in_packages {
            if let Some(pattern) = trimmed.strip_prefix('-') {
                let pattern = pattern.trim().trim_matches('"').trim_matches('\'');
                if !pattern.is_empty() {
                    patterns.push(pattern.to_string());
                }
            }
        }
    }
    patterns
}

fn expand_workspace_patterns(
    workspace_root: &Path,
    patterns: &[String],
    facts: &FactPaths<'_>,
) -> Vec<PathBuf> {
    let positive_patterns: Vec<&str> = patterns
        .iter()
        .map(|pattern| pattern.trim())
        .filter(|pattern| !pattern.is_empty() && !pattern.starts_with('!'))
        .collect();
    if positive_patterns.is_empty() {
        return Vec::new();
    }

    let positives = build_glob_set(&positive_patterns);
    let negative_patterns: Vec<&str> = patterns
        .iter()
        .map(|pattern| pattern.trim())
        .filter_map(|pattern| pattern.strip_prefix('!'))
        .map(str::trim)
        .filter(|pattern| !pattern.is_empty())
        .collect();
    let negatives = build_glob_set(&negative_patterns);

    let mut members = Vec::new();
    collect_workspace_member_dirs(
        workspace_root,
        workspace_root,
        &positives,
        &negatives,
        &mut members,
        facts,
    );
    members
}

fn build_glob_set(patterns: &[&str]) -> GlobSet {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        if let Ok(glob) = Glob::new(pattern) {
            builder.add(glob);
        }
    }
    builder
        .build()
        .unwrap_or_else(|_| GlobSetBuilder::new().build().unwrap())
}

fn collect_workspace_member_dirs(
    workspace_root: &Path,
    dir: &Path,
    positives: &GlobSet,
    negatives: &GlobSet,
    members: &mut Vec<PathBuf>,
    facts: &FactPaths<'_>,
) {
    for entry in facts.list_dir(dir) {
        if entry.kind != EntryKind::Directory {
            continue;
        }
        let path = dir.join(byte_path(&entry.name));
        let name = String::from_utf8_lossy(&entry.name);
        if matches!(
            name.as_ref(),
            "node_modules" | ".git" | "target" | "dist" | "build"
        ) {
            continue;
        }

        if facts.is_file(&path.join("package.json")) {
            if let Ok(rel) = path.strip_prefix(workspace_root) {
                let rel = rel.to_string_lossy().replace('\\', "/");
                if positives.is_match(&rel) && !negatives.is_match(&rel) {
                    members.push(path.clone());
                }
            }
        }

        collect_workspace_member_dirs(workspace_root, &path, positives, negatives, members, facts);
    }
}

fn package_json_value(
    dir: &Path,
    memo: Option<&ModuleResolutionMemo>,
    facts: &FactPaths<'_>,
) -> Option<Arc<Value>> {
    package_json_like_value(&dir.join("package.json"), memo, facts)
}

fn package_json_like_value(
    path: &Path,
    memo: Option<&ModuleResolutionMemo>,
    facts: &FactPaths<'_>,
) -> Option<Arc<Value>> {
    if let Some(memo) = memo {
        return memo.json_value(path, facts);
    }
    let json = facts.bytes(path)?;
    serde_json::from_slice(&json).ok().map(Arc::new)
}

fn package_json_name(
    dir: &Path,
    memo: Option<&ModuleResolutionMemo>,
    facts: &FactPaths<'_>,
) -> Option<String> {
    package_json_value(dir, memo, facts)?
        .get("name")?
        .as_str()
        .map(ToOwned::to_owned)
}

fn resolve_package_entry(
    package_root: &Path,
    subpath: &Option<String>,
    memo: Option<&ModuleResolutionMemo>,
    facts: &FactPaths<'_>,
) -> Option<PathBuf> {
    let package_json =
        package_json_value(package_root, memo, facts).unwrap_or_else(|| Arc::new(Value::Null));

    if let Some(exports) = package_json.get("exports") {
        if let Some(target) = export_target_for_subpath(exports, subpath.as_deref()) {
            if let Some(path) = resolve_package_target(package_root, &target, facts) {
                return Some(path);
            }
        }
    }

    if subpath.is_none() {
        for field in ["module", "main"] {
            if let Some(target) = package_json.get(field).and_then(Value::as_str) {
                if let Some(path) = resolve_package_target(package_root, target, facts) {
                    return Some(path);
                }
            }
        }
    }

    resolve_package_fallback(package_root, subpath.as_deref(), facts)
}

fn export_target_for_subpath(exports: &Value, subpath: Option<&str>) -> Option<String> {
    let key = subpath
        .map(|value| format!("./{value}"))
        .unwrap_or_else(|| ".".to_string());

    match exports {
        Value::String(target) if key == "." => Some(target.clone()),
        Value::Object(map) => {
            if let Some(target) = map.get(&key).and_then(export_condition_target) {
                return Some(target);
            }

            if let Some(target) = wildcard_export_target(map, &key) {
                return Some(target);
            }

            if key == "." && !map.contains_key(".") && !map.keys().any(|k| k.starts_with("./")) {
                return export_condition_target(exports);
            }

            None
        }
        _ => None,
    }
}

fn wildcard_export_target(map: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    for (pattern, target) in map {
        let Some(star_index) = pattern.find('*') else {
            continue;
        };
        let (prefix, suffix_with_star) = pattern.split_at(star_index);
        let suffix = &suffix_with_star[1..];
        if !key.starts_with(prefix) || !key.ends_with(suffix) {
            continue;
        }
        let matched = &key[prefix.len()..key.len() - suffix.len()];
        if let Some(target_pattern) = export_condition_target(target) {
            return Some(target_pattern.replace('*', matched));
        }
    }
    None
}

fn export_condition_target(value: &Value) -> Option<String> {
    match value {
        Value::String(target) => Some(target.clone()),
        Value::Object(map) => ["source", "import", "module", "default", "types"]
            .into_iter()
            .find_map(|field| map.get(field).and_then(export_condition_target)),
        _ => None,
    }
}

fn resolve_package_target(
    package_root: &Path,
    target: &str,
    facts: &FactPaths<'_>,
) -> Option<PathBuf> {
    let target = target.strip_prefix("./").unwrap_or(target);
    // Prefer source over compiled bundle when both exist: the callgraph
    // walks source files and cannot extract symbols from a built JS bundle.
    if let Some(src_relative) = target.strip_prefix("dist/") {
        if let Some(path) =
            resolve_file_like_path(&package_root.join("src").join(src_relative), facts)
        {
            return Some(path);
        }
    }

    resolve_file_like_path(&package_root.join(target), facts)
}

fn resolve_package_fallback(
    package_root: &Path,
    subpath: Option<&str>,
    facts: &FactPaths<'_>,
) -> Option<PathBuf> {
    match subpath {
        Some(subpath) => resolve_file_like_path(&package_root.join(subpath), facts)
            .or_else(|| resolve_file_like_path(&package_root.join("src").join(subpath), facts)),
        None => resolve_file_like_path(&package_root.join("src").join("index"), facts)
            .or_else(|| resolve_file_like_path(&package_root.join("index"), facts)),
    }
}

pub(crate) fn resolve_reexported_symbol_target<F, D>(
    file: &Path,
    symbol_name: &str,
    file_exports_symbol: &mut F,
    file_default_export_symbol: &mut D,
) -> Option<(PathBuf, String)>
where
    F: FnMut(&Path, &str) -> bool,
    D: FnMut(&Path) -> Option<String>,
{
    resolve_reexported_symbol(
        file,
        symbol_name,
        file_exports_symbol,
        file_default_export_symbol,
    )
    .map(|target| (target.file, target.symbol))
}

fn resolve_reexported_symbol<F, D>(
    file: &Path,
    symbol_name: &str,
    file_exports_symbol: &mut F,
    file_default_export_symbol: &mut D,
) -> Option<ResolvedSymbol>
where
    F: FnMut(&Path, &str) -> bool,
    D: FnMut(&Path) -> Option<String>,
{
    let mut visited = HashSet::new();
    resolve_reexported_symbol_inner(
        file,
        symbol_name,
        file_exports_symbol,
        file_default_export_symbol,
        &mut visited,
    )
}

fn resolve_reexported_symbol_inner<F, D>(
    file: &Path,
    symbol_name: &str,
    file_exports_symbol: &mut F,
    file_default_export_symbol: &mut D,
    visited: &mut HashSet<(PathBuf, String)>,
) -> Option<ResolvedSymbol>
where
    F: FnMut(&Path, &str) -> bool,
    D: FnMut(&Path) -> Option<String>,
{
    let canon = std::fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
    if !visited.insert((canon.clone(), symbol_name.to_string())) {
        return None;
    }

    let source = std::fs::read_to_string(&canon).ok()?;
    let lang = detect_language(&canon)?;
    if !matches!(lang, LangId::TypeScript | LangId::Tsx | LangId::JavaScript) {
        if symbol_name == "default" {
            return file_default_export_symbol(&canon).map(|symbol| ResolvedSymbol {
                file: canon,
                symbol,
            });
        }
        return file_exports_symbol(&canon, symbol_name).then(|| ResolvedSymbol {
            file: canon,
            symbol: symbol_name.to_string(),
        });
    }

    let grammar = grammar_for(lang);
    let mut parser = Parser::new();
    parser.set_language(&grammar).ok()?;
    let tree = parser.parse(&source, None)?;
    let from_dir = canon.parent().unwrap_or_else(|| Path::new("."));

    let mut cursor = tree.root_node().walk();
    if !cursor.goto_first_child() {
        return None;
    }

    loop {
        let node = cursor.node();
        if node.kind() == "export_statement" {
            if let Some(target) = resolve_reexport_statement(
                &source,
                node,
                from_dir,
                symbol_name,
                file_exports_symbol,
                file_default_export_symbol,
                visited,
            ) {
                return Some(target);
            }
        }

        if !cursor.goto_next_sibling() {
            break;
        }
    }

    if symbol_name == "default" {
        if let Some(symbol) = file_default_export_symbol(&canon) {
            return Some(ResolvedSymbol {
                file: canon,
                symbol,
            });
        }
    }

    if let Some(symbol) = resolve_local_export_alias(&source, &canon, symbol_name) {
        return Some(ResolvedSymbol {
            file: canon,
            symbol,
        });
    }

    if file_exports_symbol(&canon, symbol_name) {
        let symbol = symbol_name.to_string();
        return Some(ResolvedSymbol {
            file: canon,
            symbol,
        });
    }

    None
}

fn resolve_reexport_statement<F, D>(
    source: &str,
    node: tree_sitter::Node,
    from_dir: &Path,
    symbol_name: &str,
    file_exports_symbol: &mut F,
    file_default_export_symbol: &mut D,
    visited: &mut HashSet<(PathBuf, String)>,
) -> Option<ResolvedSymbol>
where
    F: FnMut(&Path, &str) -> bool,
    D: FnMut(&Path) -> Option<String>,
{
    let source_node = node
        .child_by_field_name("source")
        .or_else(|| find_child_by_kind(node, "string"))?;
    let module_path = string_literal_content(source, source_node)?;
    let target_file = resolve_module_path(from_dir, &module_path)?;
    let raw_export = node_text(node, source);

    if let Some(source_symbol) = reexport_clause_source_symbol(&raw_export, symbol_name) {
        return resolve_reexported_symbol_inner(
            &target_file,
            &source_symbol,
            file_exports_symbol,
            file_default_export_symbol,
            visited,
        )
        .or(Some(ResolvedSymbol {
            file: target_file,
            symbol: source_symbol,
        }));
    }

    if raw_export.contains('*') {
        return resolve_reexported_symbol_inner(
            &target_file,
            symbol_name,
            file_exports_symbol,
            file_default_export_symbol,
            visited,
        );
    }

    None
}

fn resolve_local_export_alias(source: &str, file: &Path, requested_export: &str) -> Option<String> {
    let lang = detect_language(file)?;
    let grammar = grammar_for(lang);
    let mut parser = Parser::new();
    parser.set_language(&grammar).ok()?;
    let tree = parser.parse(source, None)?;

    let mut cursor = tree.root_node().walk();
    if !cursor.goto_first_child() {
        return None;
    }

    loop {
        let node = cursor.node();
        if node.kind() == "export_statement" && node.child_by_field_name("source").is_none() {
            let raw_export = node_text(node, source);
            if let Some(source_symbol) =
                reexport_clause_source_symbol(&raw_export, requested_export)
            {
                return Some(source_symbol);
            }
        }

        if !cursor.goto_next_sibling() {
            break;
        }
    }

    None
}

fn reexport_clause_source_symbol(raw_export: &str, requested_export: &str) -> Option<String> {
    let start = raw_export.find('{')? + 1;
    let end = raw_export[start..].find('}')? + start;
    for specifier in raw_export[start..end].split(',') {
        let specifier = specifier.trim();
        if specifier.is_empty() {
            continue;
        }
        let specifier = specifier.strip_prefix("type ").unwrap_or(specifier).trim();
        if let Some((imported, exported)) = specifier.split_once(" as ") {
            if exported.trim() == requested_export {
                return Some(imported.trim().to_string());
            }
        } else if specifier == requested_export {
            return Some(requested_export.to_string());
        }
    }
    None
}

fn string_literal_content(source: &str, node: tree_sitter::Node) -> Option<String> {
    let raw = source[node.byte_range()].trim();
    let quote = raw.chars().next()?;
    if quote != '\'' && quote != '"' {
        return None;
    }
    raw.strip_prefix(quote)
        .and_then(|value| value.strip_suffix(quote))
        .map(ToOwned::to_owned)
}

/// Find an index file in a directory.
fn find_index_file(dir: &Path, facts: &FactPaths<'_>) -> Option<PathBuf> {
    for name in JS_TS_INDEX_FILES {
        let p = dir.join(name);
        if facts.is_file(&p) {
            return Some(facts.canonical(&p).unwrap_or(p));
        }
    }
    None
}

/// Resolve an aliased import: `import { foo as bar } from './utils'`
/// where `local_name` is "bar". Returns `(original_name, resolved_file_path)`.
fn resolve_aliased_import(
    local_name: &str,
    import_block: &ImportBlock,
    caller_dir: &Path,
) -> Option<(String, PathBuf)> {
    for imp in &import_block.imports {
        // Parse the raw text to find "as <alias>" patterns
        // This handles: import { foo as bar, baz as qux } from './mod'
        if let Some(original) = find_alias_original(&imp.raw_text, local_name) {
            if let Some(resolved_path) = resolve_module_path(caller_dir, &imp.module_path) {
                return Some((original, resolved_path));
            }
        }
    }
    None
}

/// Parse import raw text to find the original name for an alias.
/// Given raw text like `import { foo as bar, baz } from './utils'` and
/// local_name "bar", returns Some("foo").
fn find_alias_original(raw_import: &str, local_name: &str) -> Option<String> {
    // Look for pattern: <original> as <alias>
    // This is a simple text-based search; handles the common TS/JS pattern
    let search = format!(" as {}", local_name);
    if let Some(pos) = raw_import.find(&search) {
        // Walk backwards from `pos` to find the original name
        let before = &raw_import[..pos];
        // The original name is the last word-like token before " as "
        let original = before
            .rsplit(|c: char| c == '{' || c == ',' || c.is_whitespace())
            .find(|s| !s.is_empty())?;
        return Some(original.to_string());
    }
    None
}

// ---------------------------------------------------------------------------
// Worktree file discovery
// ---------------------------------------------------------------------------

/// Walk project files respecting .gitignore, excluding common non-source dirs.
///
/// Returns an iterator of file paths for supported source file types.
pub fn walk_project_files(root: &Path) -> impl Iterator<Item = PathBuf> {
    use ignore::WalkBuilder;

    // A disappearing child mount can make ReadDir::drop panic on ENXIO and abort
    // the daemon, so never open directories outside this walk root's filesystem.
    let walker = WalkBuilder::new(root)
        .same_file_system(true)
        .hidden(true)         // skip hidden files/dirs
        .git_ignore(true)     // respect .gitignore
        .git_global(true)     // respect global gitignore
        .git_exclude(true)    // respect .git/info/exclude
        .add_custom_ignore_filename(".aftignore") // AFT-specific ignores (e.g. submodules)
        .filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            // Always exclude these directories regardless of .gitignore
            if entry.file_type().map_or(false, |ft| ft.is_dir()) {
                return !matches!(
                    name.as_ref(),
                    "node_modules" | "target" | "venv" | ".venv" | ".git" | "__pycache__"
                        | ".tox" | "dist" | "build"
                );
            }
            true
        })
        .build();

    walker
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().map_or(false, |ft| ft.is_file()))
        .filter(|entry| detect_language(entry.path()).is_some())
        .map(|entry| entry.into_path())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn collect_calls_by_symbol_reference(
        source: &str,
        root: Node<'_>,
        lang: LangId,
        symbols: &[Symbol],
    ) -> HashMap<String, Vec<CallSite>> {
        let mut calls_by_symbol = HashMap::new();
        for symbol in symbols {
            let byte_start =
                line_col_to_byte(source, symbol.range.start_line, symbol.range.start_col);
            let byte_end = line_col_to_byte(source, symbol.range.end_line, symbol.range.end_col);
            let sites = extract_calls_full(source, root, byte_start, byte_end, lang)
                .into_iter()
                .map(
                    |(full, short, line, call_byte_start, call_byte_end)| CallSite {
                        callee_name: short,
                        full_callee: full,
                        line,
                        byte_start: call_byte_start,
                        byte_end: call_byte_end,
                    },
                )
                .collect::<Vec<_>>();
            if !sites.is_empty() {
                calls_by_symbol.insert(symbol_identity(symbol), sites);
            }
        }

        let symbol_ranges = symbols
            .iter()
            .map(|symbol| {
                (
                    line_col_to_byte(source, symbol.range.start_line, symbol.range.start_col),
                    line_col_to_byte(source, symbol.range.end_line, symbol.range.end_col),
                )
            })
            .collect::<Vec<_>>();
        let top_level_sites = collect_calls_full_with_ranges(root, source, 0, source.len(), lang)
            .into_iter()
            .filter(|site| {
                !symbol_ranges
                    .iter()
                    .any(|(start, end)| site.byte_start >= *start && site.byte_end <= *end)
            })
            .map(|site| CallSite {
                callee_name: site.short,
                full_callee: site.full,
                line: site.line,
                byte_start: site.byte_start,
                byte_end: site.byte_end,
            })
            .collect::<Vec<_>>();
        if !top_level_sites.is_empty() {
            calls_by_symbol.insert(TOP_LEVEL_SYMBOL.to_string(), top_level_sites);
        }
        calls_by_symbol
    }

    fn parse_symbols(source: &str, lang: LangId) -> (tree_sitter::Tree, Vec<Symbol>) {
        let mut parser = Parser::new();
        parser.set_language(&grammar_for(lang)).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let symbols = crate::parser::extract_symbols_from_tree(source, &tree, lang).unwrap();
        (tree, symbols)
    }

    fn test_symbol(name: &str, start_col: u32, end_col: u32) -> Symbol {
        Symbol {
            name: name.to_string(),
            kind: SymbolKind::Function,
            range: Range {
                start_line: 0,
                start_col,
                end_line: 0,
                end_col,
            },
            signature: None,
            scope_chain: Vec::new(),
            exported: false,
            parent: None,
        }
    }

    #[test]
    fn source_line_index_matches_shared_line_column_conversion() {
        let source = "a\r\nbb\rc\n";
        let index = SourceLineIndex::new(source);
        for line in 0..=5 {
            for column in 0..=5 {
                assert_eq!(
                    index.byte_offset(line, column),
                    line_col_to_byte(source, line, column),
                    "line={line}, column={column}"
                );
            }
        }
    }

    #[test]
    fn single_pass_call_attribution_matches_per_symbol_reference() {
        let corpora = [
            (
                "typescript",
                LangId::TypeScript,
                r#"bootstrap();
class Worker {
    run() {
        before();
        function nested() { nestedCall(); }
        nested();
    }
    next() { adjacentCall(); }
}
function left() { leftCall(); }
function right() { rightCall(); }
"#,
            ),
            (
                "python",
                LangId::Python,
                r#"bootstrap()
class Worker:
    def run(self):
        before()
        def nested():
            nested_call()
        nested()

    def next(self):
        adjacent_call()

def left():
    left_call()

def right():
    right_call()
"#,
            ),
        ];

        for (name, lang, source) in corpora {
            let (tree, symbols) = parse_symbols(source, lang);
            let reference =
                collect_calls_by_symbol_reference(source, tree.root_node(), lang, &symbols);
            let actual = collect_calls_by_symbol(source, tree.root_node(), lang, &symbols);
            assert_eq!(actual, reference, "call attribution changed for {name}");

            let class_sites = actual.get("Worker").expect("class receives method calls");
            let method_sites = actual
                .get("Worker::run")
                .expect("method receives its own calls");
            assert!(class_sites.iter().any(|site| site.callee_name == "before"));
            assert!(method_sites.iter().any(|site| site.callee_name == "before"));
            let nested_sites = actual
                .iter()
                .find(|(symbol, _)| symbol.rsplit("::").next() == Some("nested"))
                .map(|(_, sites)| sites)
                .expect("nested function receives its own calls");
            assert!(nested_sites.iter().any(|site| {
                site.callee_name == "nested_call" || site.callee_name == "nestedCall"
            }));
            assert_eq!(actual[TOP_LEVEL_SYMBOL][0].callee_name, "bootstrap");
        }

        let source = "first();second();third();";
        let (tree, _) = parse_symbols(source, LangId::TypeScript);
        let symbols = vec![
            test_symbol("outer", 0, 17),
            test_symbol("left", 0, 8),
            test_symbol("right", 8, 17),
            test_symbol("empty", 24, 24),
        ];
        let reference = collect_calls_by_symbol_reference(
            source,
            tree.root_node(),
            LangId::TypeScript,
            &symbols,
        );
        let actual =
            collect_calls_by_symbol(source, tree.root_node(), LangId::TypeScript, &symbols);
        assert_eq!(actual, reference, "overlapping and adjacent ranges changed");
        assert_eq!(
            actual["outer"]
                .iter()
                .map(|site| site.callee_name.as_str())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
        assert_eq!(actual["left"][0].callee_name, "first");
        assert_eq!(actual["right"][0].callee_name, "second");
        assert_eq!(actual[TOP_LEVEL_SYMBOL][0].callee_name, "third");
        assert!(!actual.contains_key("empty"));
    }

    #[test]
    fn symbol_metadata_for_recovers_scoped_method_by_bare_name() {
        // exported_symbols carries the bare name; symbol_metadata is keyed by
        // scoped identity (impl method). A plain .get(bare) misses and would
        // force the degraded unknown/line-1 fallback. symbol_metadata_for must
        // recover the scoped entry via unqualified-name match.
        let mut symbol_metadata = HashMap::new();
        symbol_metadata.insert(
            "BackupStore::total_disk_bytes".to_string(),
            SymbolMeta {
                kind: SymbolKind::Method,
                exported: true,
                signature: None,
                line: 703,
                range: Range {
                    start_line: 702,
                    start_col: 0,
                    end_line: 705,
                    end_col: 0,
                },
                entry_point_attribute: None,
            },
        );
        let file_data = FileCallData {
            calls_by_symbol: HashMap::new(),
            value_refs_by_symbol: HashMap::new(),
            exported_symbols: vec!["total_disk_bytes".to_string()],
            symbol_metadata,
            default_export_symbol: None,
            import_block: ImportBlock::empty(),
            lang: LangId::Rust,
        };

        let meta = file_data
            .symbol_metadata_for("total_disk_bytes")
            .expect("scoped method recovered by bare name");
        assert_eq!(meta.kind, SymbolKind::Method);
        assert_eq!(
            meta.line, 703,
            "real declaration line, not the line-1 fallback"
        );

        // A genuinely-absent symbol still returns None (no false recovery).
        assert!(file_data.symbol_metadata_for("does_not_exist").is_none());
    }

    /// Create a temp directory with TypeScript files for testing.
    fn setup_ts_project() -> TempDir {
        let dir = TempDir::new().unwrap();

        // main.ts: imports from utils and calls functions
        fs::write(
            dir.path().join("main.ts"),
            r#"import { helper, compute } from './utils';
import * as math from './math';

export function main() {
    const a = helper(1);
    const b = compute(a, 2);
    const c = math.add(a, b);
    return c;
}
"#,
        )
        .unwrap();

        // utils.ts: defines helper and compute, imports from helpers
        fs::write(
            dir.path().join("utils.ts"),
            r#"import { double } from './helpers';

export function helper(x: number): number {
    return double(x);
}

export function compute(a: number, b: number): number {
    return a + b;
}
"#,
        )
        .unwrap();

        // helpers.ts: defines double
        fs::write(
            dir.path().join("helpers.ts"),
            r#"export function double(x: number): number {
    return x * 2;
}

export function triple(x: number): number {
    return x * 3;
}
"#,
        )
        .unwrap();

        // math.ts: defines add (for namespace import test)
        fs::write(
            dir.path().join("math.ts"),
            r#"export function add(a: number, b: number): number {
    return a + b;
}

export function subtract(a: number, b: number): number {
    return a - b;
}
"#,
        )
        .unwrap();

        dir
    }

    /// Create a project with import aliasing.
    fn setup_alias_project() -> TempDir {
        let dir = TempDir::new().unwrap();

        fs::write(
            dir.path().join("main.ts"),
            r#"import { helper as h } from './utils';

export function main() {
    return h(42);
}
"#,
        )
        .unwrap();

        fs::write(
            dir.path().join("utils.ts"),
            r#"export function helper(x: number): number {
    return x + 1;
}
"#,
        )
        .unwrap();

        dir
    }

    // --- Single-file call extraction ---

    #[test]
    fn callgraph_single_file_call_extraction() {
        let dir = setup_ts_project();
        let mut graph = CallGraph::new(dir.path().to_path_buf());

        let file_data = graph.build_file(&dir.path().join("main.ts")).unwrap();
        let main_calls = &file_data.calls_by_symbol["main"];

        let callee_names: Vec<&str> = main_calls.iter().map(|c| c.callee_name.as_str()).collect();
        assert!(
            callee_names.contains(&"helper"),
            "main should call helper, got: {:?}",
            callee_names
        );
        assert!(
            callee_names.contains(&"compute"),
            "main should call compute, got: {:?}",
            callee_names
        );
        assert!(
            callee_names.contains(&"add"),
            "main should call math.add (short name: add), got: {:?}",
            callee_names
        );
    }

    #[test]
    fn callgraph_file_data_has_exports() {
        let dir = setup_ts_project();
        let mut graph = CallGraph::new(dir.path().to_path_buf());

        let file_data = graph.build_file(&dir.path().join("utils.ts")).unwrap();
        assert!(
            file_data.exported_symbols.contains(&"helper".to_string()),
            "utils.ts should export helper, got: {:?}",
            file_data.exported_symbols
        );
        assert!(
            file_data.exported_symbols.contains(&"compute".to_string()),
            "utils.ts should export compute, got: {:?}",
            file_data.exported_symbols
        );
    }

    // --- Cross-file resolution ---

    #[test]
    fn callgraph_resolve_direct_import() {
        let dir = setup_ts_project();
        let mut graph = CallGraph::new(dir.path().to_path_buf());

        let main_path = dir.path().join("main.ts");
        let file_data = graph.build_file(&main_path).unwrap();
        let import_block = file_data.import_block.clone();

        let edge = graph.resolve_cross_file_edge("helper", "helper", &main_path, &import_block);
        match edge {
            EdgeResolution::Resolved { file, symbol } => {
                assert!(
                    file.ends_with("utils.ts"),
                    "helper should resolve to utils.ts, got: {:?}",
                    file
                );
                assert_eq!(symbol, "helper");
            }
            EdgeResolution::Unresolved { callee_name } => {
                panic!("Expected resolved, got unresolved: {}", callee_name);
            }
        }
    }

    #[test]
    fn callgraph_resolve_namespace_import() {
        let dir = setup_ts_project();
        let mut graph = CallGraph::new(dir.path().to_path_buf());

        let main_path = dir.path().join("main.ts");
        let file_data = graph.build_file(&main_path).unwrap();
        let import_block = file_data.import_block.clone();

        let edge = graph.resolve_cross_file_edge("math.add", "add", &main_path, &import_block);
        match edge {
            EdgeResolution::Resolved { file, symbol } => {
                assert!(
                    file.ends_with("math.ts"),
                    "math.add should resolve to math.ts, got: {:?}",
                    file
                );
                assert_eq!(symbol, "add");
            }
            EdgeResolution::Unresolved { callee_name } => {
                panic!("Expected resolved, got unresolved: {}", callee_name);
            }
        }
    }

    #[test]
    fn callgraph_resolve_aliased_import() {
        let dir = setup_alias_project();
        let mut graph = CallGraph::new(dir.path().to_path_buf());

        let main_path = dir.path().join("main.ts");
        let file_data = graph.build_file(&main_path).unwrap();
        let import_block = file_data.import_block.clone();

        let edge = graph.resolve_cross_file_edge("h", "h", &main_path, &import_block);
        match edge {
            EdgeResolution::Resolved { file, symbol } => {
                assert!(
                    file.ends_with("utils.ts"),
                    "h (alias for helper) should resolve to utils.ts, got: {:?}",
                    file
                );
                assert_eq!(symbol, "helper");
            }
            EdgeResolution::Unresolved { callee_name } => {
                panic!("Expected resolved, got unresolved: {}", callee_name);
            }
        }
    }

    #[test]
    fn callgraph_unresolved_edge_marked() {
        let dir = setup_ts_project();
        let mut graph = CallGraph::new(dir.path().to_path_buf());

        let main_path = dir.path().join("main.ts");
        let file_data = graph.build_file(&main_path).unwrap();
        let import_block = file_data.import_block.clone();

        let edge =
            graph.resolve_cross_file_edge("unknownFunc", "unknownFunc", &main_path, &import_block);
        assert_eq!(
            edge,
            EdgeResolution::Unresolved {
                callee_name: "unknownFunc".to_string()
            },
            "Unknown callee should be unresolved"
        );
    }

    // --- Worktree walker ---

    #[test]
    fn callgraph_walker_excludes_gitignored() {
        let dir = TempDir::new().unwrap();

        // Create a .gitignore
        fs::write(dir.path().join(".gitignore"), "ignored_dir/\n").unwrap();

        // Create files
        fs::write(dir.path().join("main.ts"), "export function main() {}").unwrap();
        fs::create_dir(dir.path().join("ignored_dir")).unwrap();
        fs::write(
            dir.path().join("ignored_dir").join("secret.ts"),
            "export function secret() {}",
        )
        .unwrap();

        // Also create node_modules (should always be excluded)
        fs::create_dir(dir.path().join("node_modules")).unwrap();
        fs::write(
            dir.path().join("node_modules").join("dep.ts"),
            "export function dep() {}",
        )
        .unwrap();

        // Init git repo for .gitignore to work
        let mut command = std::process::Command::new("git");
        crate::test_env::apply_hermetic_git_env(command.current_dir(dir.path()))
            .args(["init"])
            .output()
            .unwrap();

        let files: Vec<PathBuf> = walk_project_files(dir.path()).collect();
        let file_names: Vec<String> = files
            .iter()
            .map(|f| f.file_name().unwrap().to_string_lossy().to_string())
            .collect();

        assert!(
            file_names.contains(&"main.ts".to_string()),
            "Should include main.ts, got: {:?}",
            file_names
        );
        assert!(
            !file_names.contains(&"secret.ts".to_string()),
            "Should exclude gitignored secret.ts, got: {:?}",
            file_names
        );
        assert!(
            !file_names.contains(&"dep.ts".to_string()),
            "Should exclude node_modules, got: {:?}",
            file_names
        );
    }

    #[test]
    fn callgraph_walker_excludes_aftignored() {
        let dir = TempDir::new().unwrap();

        // .aftignore is honored without a git repo (custom ignore file).
        fs::write(dir.path().join(".aftignore"), "vendored/\n").unwrap();
        fs::write(dir.path().join("main.ts"), "export function main() {}").unwrap();
        fs::create_dir(dir.path().join("vendored")).unwrap();
        fs::write(
            dir.path().join("vendored").join("sub.ts"),
            "export function sub() {}",
        )
        .unwrap();

        let files: Vec<PathBuf> = walk_project_files(dir.path()).collect();
        let file_names: Vec<String> = files
            .iter()
            .map(|f| f.file_name().unwrap().to_string_lossy().to_string())
            .collect();

        assert!(
            file_names.contains(&"main.ts".to_string()),
            "Should include main.ts, got: {:?}",
            file_names
        );
        assert!(
            !file_names.contains(&"sub.ts".to_string()),
            "Should exclude .aftignored sub.ts, got: {:?}",
            file_names
        );
    }

    #[test]
    fn callgraph_walker_only_source_files() {
        let dir = TempDir::new().unwrap();

        fs::write(dir.path().join("main.ts"), "export function main() {}").unwrap();
        fs::write(dir.path().join("module.mts"), "export function esm() {}").unwrap();
        fs::write(dir.path().join("common.cts"), "export function cjs() {}").unwrap();
        fs::write(
            dir.path().join("runtime.mjs"),
            "export function runtime() {}",
        )
        .unwrap();
        fs::write(
            dir.path().join("legacy.cjs"),
            "exports.legacy = function() {};",
        )
        .unwrap();
        fs::write(dir.path().join("types.pyi"), "def typed() -> None: ...").unwrap();
        fs::write(dir.path().join("readme.md"), "# Hello").unwrap();
        fs::write(dir.path().join("data.json"), "{}").unwrap();

        let files: Vec<PathBuf> = walk_project_files(dir.path()).collect();
        let file_names: Vec<String> = files
            .iter()
            .map(|f| f.file_name().unwrap().to_string_lossy().to_string())
            .collect();

        assert!(file_names.contains(&"main.ts".to_string()));
        for modern_ext_file in [
            "module.mts",
            "common.cts",
            "runtime.mjs",
            "legacy.cjs",
            "types.pyi",
        ] {
            assert!(
                file_names.contains(&modern_ext_file.to_string()),
                "walker should include {modern_ext_file}, got: {:?}",
                file_names
            );
        }
        assert!(
            file_names.contains(&"readme.md".to_string()),
            "Markdown is now a supported source language"
        );
        assert!(
            file_names.contains(&"data.json".to_string()),
            "JSON is now a supported source language"
        );
    }

    // --- find_alias_original ---

    #[test]
    fn callgraph_find_alias_original_simple() {
        let raw = "import { foo as bar } from './utils';";
        assert_eq!(find_alias_original(raw, "bar"), Some("foo".to_string()));
    }

    #[test]
    fn callgraph_find_alias_original_multiple() {
        let raw = "import { foo as bar, baz as qux } from './utils';";
        assert_eq!(find_alias_original(raw, "bar"), Some("foo".to_string()));
        assert_eq!(find_alias_original(raw, "qux"), Some("baz".to_string()));
    }

    #[test]
    fn callgraph_find_alias_no_match() {
        let raw = "import { foo } from './utils';";
        assert_eq!(find_alias_original(raw, "foo"), None);
    }

    // --- Reverse callers ---

    #[test]
    fn is_entry_point_exported_function() {
        assert!(is_entry_point(
            "handleRequest",
            &SymbolKind::Function,
            true,
            LangId::TypeScript
        ));
    }

    #[test]
    fn is_entry_point_exported_method_is_not_entry() {
        // Methods are class members, not standalone entry points
        assert!(!is_entry_point(
            "handleRequest",
            &SymbolKind::Method,
            true,
            LangId::TypeScript
        ));
    }

    #[test]
    fn is_entry_point_main_init_patterns() {
        for name in &["main", "Main", "MAIN", "init", "setup", "bootstrap", "run"] {
            assert!(
                is_entry_point(name, &SymbolKind::Function, false, LangId::TypeScript),
                "{} should be an entry point",
                name
            );
        }
    }

    #[test]
    fn is_entry_point_test_patterns_ts() {
        assert!(is_entry_point(
            "describe",
            &SymbolKind::Function,
            false,
            LangId::TypeScript
        ));
        assert!(is_entry_point(
            "it",
            &SymbolKind::Function,
            false,
            LangId::TypeScript
        ));
        assert!(is_entry_point(
            "test",
            &SymbolKind::Function,
            false,
            LangId::TypeScript
        ));
        assert!(is_entry_point(
            "testValidation",
            &SymbolKind::Function,
            false,
            LangId::TypeScript
        ));
        assert!(is_entry_point(
            "specHelper",
            &SymbolKind::Function,
            false,
            LangId::TypeScript
        ));
    }

    #[test]
    fn is_entry_point_test_patterns_python() {
        assert!(is_entry_point(
            "test_login",
            &SymbolKind::Function,
            false,
            LangId::Python
        ));
        assert!(is_entry_point(
            "setUp",
            &SymbolKind::Function,
            false,
            LangId::Python
        ));
        assert!(is_entry_point(
            "tearDown",
            &SymbolKind::Function,
            false,
            LangId::Python
        ));
        // "testSomething" should NOT match Python (needs test_ prefix)
        assert!(!is_entry_point(
            "testSomething",
            &SymbolKind::Function,
            false,
            LangId::Python
        ));
    }

    #[test]
    fn is_entry_point_test_patterns_rust() {
        assert!(is_entry_point(
            "test_parse",
            &SymbolKind::Function,
            false,
            LangId::Rust
        ));
        assert!(!is_entry_point(
            "TestSomething",
            &SymbolKind::Function,
            false,
            LangId::Rust
        ));
    }

    #[test]
    fn is_entry_point_test_patterns_go() {
        assert!(is_entry_point(
            "TestParsing",
            &SymbolKind::Function,
            false,
            LangId::Go
        ));
        // lowercase test should NOT match Go (needs uppercase Test prefix)
        assert!(!is_entry_point(
            "testParsing",
            &SymbolKind::Function,
            false,
            LangId::Go
        ));
    }

    #[test]
    fn is_entry_point_non_exported_non_main_is_not_entry() {
        assert!(!is_entry_point(
            "helperUtil",
            &SymbolKind::Function,
            false,
            LangId::TypeScript
        ));
    }

    // --- symbol_metadata ---

    #[test]
    fn callgraph_symbol_metadata_populated() {
        let dir = setup_ts_project();
        let mut graph = CallGraph::new(dir.path().to_path_buf());

        let file_data = graph.build_file(&dir.path().join("utils.ts")).unwrap();
        assert!(
            file_data.symbol_metadata.contains_key("helper"),
            "symbol_metadata should contain helper"
        );
        let meta = &file_data.symbol_metadata["helper"];
        assert_eq!(meta.kind, SymbolKind::Function);
        assert!(meta.exported, "helper should be exported");
    }

    #[test]
    fn namespace_import_follows_barrel_reexport_and_rejects_private_member() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("main.ts"),
            r#"import * as lib from './index';

export function main() {
    lib.helper();
    lib.hidden();
}
"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("index.ts"),
            "export { helper } from './utils';\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("utils.ts"),
            r#"export function helper() {}
function hidden() {}
"#,
        )
        .unwrap();

        let mut graph = CallGraph::new(dir.path().to_path_buf());
        let main_path = dir.path().join("main.ts");
        let import_block = graph.build_file(&main_path).unwrap().import_block.clone();

        let helper =
            graph.resolve_cross_file_edge("lib.helper", "helper", &main_path, &import_block);
        match helper {
            EdgeResolution::Resolved { file, symbol } => {
                assert!(
                    file.ends_with("utils.ts"),
                    "helper should resolve through barrel: {file:?}"
                );
                assert_eq!(symbol, "helper");
            }
            other => panic!("expected helper to resolve through barrel, got {other:?}"),
        }

        let hidden =
            graph.resolve_cross_file_edge("lib.hidden", "hidden", &main_path, &import_block);
        assert_eq!(
            hidden,
            EdgeResolution::Unresolved {
                callee_name: "hidden".to_string()
            }
        );
    }

    #[test]
    fn workspace_package_resolution_prefers_modern_ts_source_extensions() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("package.json"),
            r#"{"workspaces":["packages/*"]}"#,
        )
        .unwrap();
        let package_dir = dir.path().join("packages/lib");
        fs::create_dir_all(package_dir.join("src")).unwrap();
        fs::create_dir_all(package_dir.join("dist")).unwrap();
        fs::write(
            package_dir.join("package.json"),
            r#"{"name":"@scope/lib","exports":{".":"./dist/index.mjs"}}"#,
        )
        .unwrap();
        fs::write(
            package_dir.join("src/index.mts"),
            "export function helper() {}\n",
        )
        .unwrap();
        fs::write(package_dir.join("dist/index.mjs"), "export{};\n").unwrap();

        let resolved = resolve_module_path(dir.path(), "@scope/lib").unwrap();
        assert!(
            resolved.ends_with("src/index.mts"),
            "dist/index.mjs should map to src/index.mts, got {resolved:?}"
        );
    }

    #[test]
    fn same_named_methods_use_scoped_symbol_identity() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("classes.ts"),
            r#"class A {
    run() { helperA(); }
}

class B {
    run() { helperB(); }
}

function helperA() {}
function helperB() {}
"#,
        )
        .unwrap();

        let mut graph = CallGraph::new(dir.path().to_path_buf());
        let path = dir.path().join("classes.ts");
        let data = graph.build_file(&path).unwrap();

        assert!(
            data.symbol_metadata.contains_key("A::run"),
            "A::run metadata missing"
        );
        assert!(
            data.symbol_metadata.contains_key("B::run"),
            "B::run metadata missing"
        );
        assert!(
            data.calls_by_symbol["A::run"]
                .iter()
                .any(|call| call.callee_name == "helperA"),
            "A::run calls should not be overwritten"
        );
        assert!(
            data.calls_by_symbol["B::run"]
                .iter()
                .any(|call| call.callee_name == "helperB"),
            "B::run calls should not be overwritten"
        );
    }

    // --- extract_parameters ---

    #[test]
    fn extract_parameters_typescript() {
        let params = extract_parameters(
            "function processData(input: string, count: number): void",
            LangId::TypeScript,
        );
        assert_eq!(params, vec!["input", "count"]);
    }

    #[test]
    fn extract_parameters_typescript_optional() {
        let params = extract_parameters(
            "function fetch(url: string, options?: RequestInit): Promise<Response>",
            LangId::TypeScript,
        );
        assert_eq!(params, vec!["url", "options"]);
    }

    #[test]
    fn extract_parameters_typescript_defaults() {
        let params = extract_parameters(
            "function greet(name: string, greeting: string = \"hello\"): string",
            LangId::TypeScript,
        );
        assert_eq!(params, vec!["name", "greeting"]);
    }

    #[test]
    fn extract_parameters_typescript_rest() {
        let params = extract_parameters(
            "function sum(...numbers: number[]): number",
            LangId::TypeScript,
        );
        assert_eq!(params, vec!["numbers"]);
    }

    #[test]
    fn extract_parameters_python_self_skipped() {
        let params = extract_parameters(
            "def process(self, data: str, count: int) -> bool",
            LangId::Python,
        );
        assert_eq!(params, vec!["data", "count"]);
    }

    #[test]
    fn extract_parameters_python_no_self() {
        let params = extract_parameters("def validate(input: str) -> bool", LangId::Python);
        assert_eq!(params, vec!["input"]);
    }

    #[test]
    fn extract_parameters_python_star_args() {
        let params = extract_parameters("def func(*args, **kwargs)", LangId::Python);
        assert_eq!(params, vec!["args", "kwargs"]);
    }

    #[test]
    fn extract_parameters_rust_self_skipped() {
        let params = extract_parameters(
            "fn process(&self, data: &str, count: usize) -> bool",
            LangId::Rust,
        );
        assert_eq!(params, vec!["data", "count"]);
    }

    #[test]
    fn extract_parameters_rust_mut_self_skipped() {
        let params = extract_parameters("fn update(&mut self, value: i32)", LangId::Rust);
        assert_eq!(params, vec!["value"]);
    }

    #[test]
    fn extract_parameters_rust_no_self() {
        let params = extract_parameters("fn validate(input: &str) -> bool", LangId::Rust);
        assert_eq!(params, vec!["input"]);
    }

    #[test]
    fn extract_parameters_rust_mut_param() {
        let params = extract_parameters("fn process(mut buf: Vec<u8>, len: usize)", LangId::Rust);
        assert_eq!(params, vec!["buf", "len"]);
    }

    #[test]
    fn extract_parameters_go() {
        let params = extract_parameters(
            "func ProcessData(input string, count int) error",
            LangId::Go,
        );
        assert_eq!(params, vec!["input", "count"]);
    }

    #[test]
    fn extract_parameters_empty() {
        let params = extract_parameters("function noArgs(): void", LangId::TypeScript);
        assert!(
            params.is_empty(),
            "no-arg function should return empty params"
        );
    }

    #[test]
    fn extract_parameters_no_parens() {
        let params = extract_parameters("const x = 42", LangId::TypeScript);
        assert!(params.is_empty(), "no parens should return empty params");
    }

    #[test]
    fn extract_parameters_javascript() {
        let params = extract_parameters("function handleClick(event, target)", LangId::JavaScript);
        assert_eq!(params, vec!["event", "target"]);
    }
}
