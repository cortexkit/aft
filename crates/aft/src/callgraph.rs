//! Call graph engine: cross-file call resolution and forward traversal.
//!
//! Builds a lazy, worktree-scoped call graph that resolves calls across files
//! using import chains. Supports depth-limited forward traversal with cycle
//! detection.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, RwLock};

use globset::{Glob, GlobSet, GlobSetBuilder};
use rayon::prelude::*;
use serde::Serialize;
use serde_json::Value;
use tree_sitter::{Node, Parser};

use crate::calls::{call_node_kinds, extract_callee_name, extract_calls_full, extract_full_callee};
use crate::edit::line_col_to_byte;
use crate::error::AftError;
use crate::imports::{self, ImportBlock};
use crate::language::LanguageProvider;
use crate::parser::{detect_language, grammar_for, LangId};
use crate::symbols::{Range, Symbol, SymbolKind};

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

type SharedPath = Arc<PathBuf>;
type SharedStr = Arc<str>;
type ReverseIndex = HashMap<PathBuf, HashMap<String, Vec<IndexedCallerSite>>>;
type WorkspacePackageCache = HashMap<(PathBuf, String), Option<PathBuf>>;
type RustCrateInfoCache = HashMap<PathBuf, Option<RustCrateInfo>>;
type RustWorkspaceCrateCache = HashMap<PathBuf, HashMap<String, RustCrateInfo>>;

static WORKSPACE_PACKAGE_CACHE: LazyLock<RwLock<WorkspacePackageCache>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));
static RUST_CRATE_INFO_CACHE: LazyLock<RwLock<RustCrateInfoCache>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));
static RUST_WORKSPACE_CRATE_CACHE: LazyLock<RwLock<RustWorkspaceCrateCache>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

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

fn symbol_query_matches(symbol: &str, query: &str) -> bool {
    symbol == query || symbol_unqualified_name(symbol) == query
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
#[derive(Debug, Clone)]
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
}

/// Per-file call data: call sites grouped by containing symbol, plus
/// exported symbol names and parsed imports.
#[derive(Debug, Clone)]
pub struct FileCallData {
    /// Map from symbol name → list of call sites within that symbol's body.
    pub calls_by_symbol: HashMap<String, Vec<CallSite>>,
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

/// A single caller site: who calls a given symbol and from where.
#[derive(Debug, Clone, Serialize)]
pub struct CallerSite {
    /// File containing the caller.
    pub caller_file: PathBuf,
    /// Symbol that makes the call.
    pub caller_symbol: String,
    /// 1-based line number of the call.
    pub line: u32,
    /// 0-based column (byte start within file, kept for future use).
    pub col: u32,
    /// Whether the edge was resolved via import chain.
    pub resolved: bool,
}

#[derive(Debug, Clone)]
struct IndexedCallerSite {
    caller_file: SharedPath,
    caller_symbol: SharedStr,
    line: u32,
    col: u32,
    resolved: bool,
}

/// A group of callers from a single file.
#[derive(Debug, Clone, Serialize)]
pub struct CallerGroup {
    /// File path (relative to project root).
    pub file: String,
    /// Individual call sites in this file.
    pub callers: Vec<CallerEntry>,
}

/// A single caller entry within a CallerGroup.
#[derive(Debug, Clone, Serialize)]
pub struct CallerEntry {
    pub symbol: String,
    /// 1-based line number of the call.
    pub line: u32,
}

/// Result of a `callers_of` query.
#[derive(Debug, Clone, Serialize)]
pub struct CallersResult {
    /// Target symbol queried.
    pub symbol: String,
    /// Target file queried.
    pub file: String,
    /// Caller groups, one per calling file.
    pub callers: Vec<CallerGroup>,
    /// Total number of call sites found.
    pub total_callers: usize,
    /// Number of files scanned to build the reverse index.
    pub scanned_files: usize,
    /// Whether recursive caller expansion stopped at the requested depth.
    pub depth_limited: bool,
    /// Number of caller edges omitted because of the depth limit.
    pub truncated: usize,
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
        | LangId::Yaml => false,
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
// Impact analysis types
// ---------------------------------------------------------------------------

/// A single caller in an impact analysis result.
#[derive(Debug, Clone, Serialize)]
pub struct ImpactCaller {
    /// Symbol that calls the target.
    pub caller_symbol: String,
    /// File containing the caller (relative to project root).
    pub caller_file: String,
    /// 1-based line number of the call site.
    pub line: u32,
    /// Caller's function/method signature, if available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// Whether the caller is an entry point.
    pub is_entry_point: bool,
    /// Source line at the call site (trimmed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_expression: Option<String>,
    /// Parameter names extracted from the caller's signature.
    pub parameters: Vec<String>,
}

/// Result of an `impact` query — enriched callers analysis.
#[derive(Debug, Clone, Serialize)]
pub struct ImpactResult {
    /// The target symbol being analyzed.
    pub symbol: String,
    /// The target file (relative to project root).
    pub file: String,
    /// Target symbol's signature, if available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// Parameter names extracted from the target's signature.
    pub parameters: Vec<String>,
    /// Total number of affected call sites.
    pub total_affected: usize,
    /// Number of distinct files containing callers.
    pub affected_files: usize,
    /// Enriched caller details.
    pub callers: Vec<ImpactCaller>,
    /// Whether transitive impact expansion stopped at the requested depth.
    pub depth_limited: bool,
    /// Number of caller edges omitted because of the depth limit.
    pub truncated: usize,
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
    /// All files discovered in the worktree (lazily populated).
    project_files: Option<Vec<PathBuf>>,
    /// Reverse index: target_file → target_symbol → callers.
    /// Built lazily on first `callers_of` call, cleared on `invalidate_file`.
    reverse_index: Option<ReverseIndex>,
}

impl CallGraph {
    /// Create a new call graph for a project.
    pub fn new(project_root: PathBuf) -> Self {
        clear_workspace_package_cache();
        Self {
            data: HashMap::new(),
            project_root,
            project_files: None,
            reverse_index: None,
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
                    if let Some(index_path) = find_index_file(&resolved_path) {
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

    /// Resolve a user-provided symbol query to the unique scoped symbol identity
    /// used internally by the call graph.
    pub fn resolve_symbol_query(&mut self, file: &Path, symbol: &str) -> Result<String, AftError> {
        let canon = self.canonicalize(file)?;
        let file_data = self.build_file(&canon)?;
        resolve_symbol_query_in_data(file_data, &canon, symbol)
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

    fn file_exports_symbol_cached(&self, path: &Path, symbol_name: &str) -> bool {
        self.lookup_file_data(path)
            .map(|data| data.exported_symbols.iter().any(|name| name == symbol_name))
            .unwrap_or(false)
    }

    fn file_default_export_symbol_cached(&self, path: &Path) -> Option<String> {
        self.lookup_file_data(path)
            .and_then(|data| data.default_export_symbol.clone())
    }

    /// Depth-limited forward call tree traversal.
    ///
    /// Starting from a (file, symbol) pair, recursively follows calls
    /// up to `max_depth` levels. Uses a visited set for cycle detection.
    pub fn forward_tree(
        &mut self,
        file: &Path,
        symbol: &str,
        max_depth: usize,
    ) -> Result<CallTreeNode, AftError> {
        let canon = self.canonicalize(file)?;
        let resolved_symbol = {
            let file_data = self.build_file(&canon)?;
            resolve_symbol_query_in_data(file_data, &canon, symbol)?
        };
        let mut visited = HashSet::new();
        self.forward_tree_inner(&canon, &resolved_symbol, max_depth, 0, &mut visited)
    }

    fn forward_tree_inner(
        &mut self,
        file: &Path,
        symbol: &str,
        max_depth: usize,
        current_depth: usize,
        visited: &mut HashSet<(PathBuf, String)>,
    ) -> Result<CallTreeNode, AftError> {
        let canon = self.canonicalize(file)?;
        let visit_key = (canon.clone(), symbol.to_string());

        // Cycle detection
        if visited.contains(&visit_key) {
            let (line, signature) = self
                .lookup_file_data(&canon)
                .map(|data| get_symbol_meta_from_data(data, symbol))
                .unwrap_or_else(|| get_symbol_meta(&canon, symbol));
            return Ok(CallTreeNode {
                name: symbol.to_string(),
                file: self.relative_path(&canon),
                line,
                signature,
                resolved: true,
                children: vec![], // cycle — stop recursion
                depth_limited: false,
                truncated: 0,
            });
        }

        visited.insert(visit_key.clone());

        let (import_block, call_sites, sym_line, sym_signature) = {
            let file_data = self.build_file(&canon)?;
            let meta = get_symbol_meta_from_data(file_data, symbol);

            (
                file_data.import_block.clone(),
                file_data
                    .calls_by_symbol
                    .get(symbol)
                    .cloned()
                    .unwrap_or_default(),
                meta.0,
                meta.1,
            )
        };

        // Build children
        let mut children = Vec::new();
        let mut depth_limited = false;
        let mut truncated = 0;

        if current_depth < max_depth {
            for call_site in &call_sites {
                let edge = self.resolve_cross_file_edge(
                    &call_site.full_callee,
                    &call_site.callee_name,
                    &canon,
                    &import_block,
                );

                match edge {
                    EdgeResolution::Resolved {
                        file: ref target_file,
                        ref symbol,
                    } => {
                        match self.forward_tree_inner(
                            target_file,
                            symbol,
                            max_depth,
                            current_depth + 1,
                            visited,
                        ) {
                            Ok(child) => {
                                depth_limited |= child.depth_limited;
                                truncated += child.truncated;
                                children.push(child);
                            }
                            Err(_) => {
                                // Target file can't be parsed — mark as unresolved leaf
                                children.push(CallTreeNode {
                                    name: call_site.callee_name.clone(),
                                    file: self.relative_path(target_file),
                                    line: call_site.line,
                                    signature: None,
                                    resolved: false,
                                    children: vec![],
                                    depth_limited: false,
                                    truncated: 0,
                                });
                            }
                        }
                    }
                    EdgeResolution::Unresolved { callee_name } => {
                        if let Some(local_child) = self.resolve_local_call_tree_child(
                            &canon,
                            symbol,
                            call_site,
                            &callee_name,
                            max_depth,
                            current_depth,
                            visited,
                        )? {
                            depth_limited |= local_child.depth_limited;
                            truncated += local_child.truncated;
                            children.push(local_child);
                            continue;
                        }
                        children.push(CallTreeNode {
                            name: callee_name,
                            file: self.relative_path(&canon),
                            line: call_site.line,
                            signature: None,
                            resolved: false,
                            children: vec![],
                            depth_limited: false,
                            truncated: 0,
                        });
                    }
                }
            }
        } else if !call_sites.is_empty() {
            depth_limited = true;
            truncated = call_sites.len();
        }

        visited.remove(&visit_key);

        Ok(CallTreeNode {
            name: symbol.to_string(),
            file: self.relative_path(&canon),
            line: sym_line,
            signature: sym_signature,
            resolved: true,
            children,
            depth_limited,
            truncated,
        })
    }

    fn resolve_local_call_tree_child(
        &mut self,
        canon: &Path,
        current_symbol: &str,
        call_site: &CallSite,
        callee_name: &str,
        max_depth: usize,
        current_depth: usize,
        visited: &mut HashSet<(PathBuf, String)>,
    ) -> Result<Option<CallTreeNode>, AftError> {
        if !is_bare_callee(&call_site.full_callee, callee_name) {
            return Ok(None);
        }

        let target_symbol = match self
            .lookup_file_data(canon)
            .and_then(|data| resolve_symbol_query_in_data(data, canon, callee_name).ok())
        {
            Some(symbol) => symbol,
            None => return Ok(None),
        };

        if target_symbol == current_symbol {
            return Ok(None);
        }

        match self.forward_tree_inner(canon, &target_symbol, max_depth, current_depth + 1, visited)
        {
            Ok(child) => Ok(Some(child)),
            Err(_) => Ok(Some(CallTreeNode {
                name: target_symbol,
                file: self.relative_path(canon),
                line: call_site.line,
                signature: None,
                resolved: false,
                children: vec![],
                depth_limited: false,
                truncated: 0,
            })),
        }
    }

    /// Get all project files (lazily discovered).
    pub fn project_files(&mut self) -> &[PathBuf] {
        if self.project_files.is_none() {
            let project_root = self.project_root.clone();
            self.project_files = Some(walk_project_files(&project_root).collect());
        }
        self.project_files.as_deref().unwrap_or(&[])
    }

    /// Get the total number of project source files.
    ///
    /// Triggers project file discovery on first access and returns the cached
    /// count thereafter. Prefer [`project_file_count_bounded`] when the caller
    /// only needs to know whether a threshold is exceeded.
    pub fn project_file_count(&mut self) -> usize {
        self.project_files().len()
    }

    /// Count project source files, stopping after `limit + 1` so huge roots
    /// do not pay for a full walk or allocate a giant vector.
    ///
    /// Returns the real count when ≤ `limit`, or `limit + 1` when exceeded.
    /// Uses the cached `project_files` vec when it already exists (e.g. a
    /// previous call-graph op succeeded at this cap), otherwise short-circuits
    /// the underlying `ignore::Walk` iterator via `.take(limit + 1)`.
    ///
    /// CRITICAL: This method must NOT populate `self.project_files`. The whole
    /// point is to reject oversized roots before the full walk-and-collect runs.
    pub fn project_file_count_bounded(&self, limit: usize) -> usize {
        if let Some(files) = self.project_files.as_deref() {
            return files.len();
        }
        walk_project_files(&self.project_root)
            .take(limit.saturating_add(1))
            .count()
    }

    /// Build call data for all project files, failing fast when the configured
    /// source-file cap is exceeded.
    fn ensure_project_files_built(&mut self, max_files: usize) -> Result<(), AftError> {
        // Bounded count first — never populate project_files on oversized roots.
        // `walk_project_files(...).take(max_files + 1)` is lazy (Walk is an
        // iterator), so this costs at most (max_files + 1) directory entries
        // worth of work, not a full O(N) walk of the whole tree.
        let count = self.project_file_count_bounded(max_files);
        if count > max_files {
            return Err(AftError::ProjectTooLarge {
                count,
                max: max_files,
            });
        }

        // TODO(v0.16): rust-side deadline for graceful timeout recovery
        // (unbounded walks remain a soft cliff for users who raise the cap).
        // Discover all project files first.
        let all_files = self.project_files().to_vec();

        // Build file data for all project files.
        let uncached_files: Vec<PathBuf> = all_files
            .iter()
            .filter(|f| self.lookup_file_data(f).is_none())
            .cloned()
            .collect();

        let computed: Vec<(PathBuf, FileCallData)> = uncached_files
            .par_iter()
            .filter_map(|f| build_file_data(f).ok().map(|data| (f.clone(), data)))
            .collect();

        for (file, data) in computed {
            self.data.insert(file, data);
        }

        Ok(())
    }

    /// Build the reverse index by scanning all project files.
    ///
    /// For each file, builds the call data (if not cached), then for each
    /// (symbol, call_sites) pair, resolves cross-file edges and inserts
    /// into the reverse map: `(target_file, target_symbol) → Vec<CallerSite>`.
    fn build_reverse_index(&mut self, max_files: usize) -> Result<(), AftError> {
        self.ensure_project_files_built(max_files)?;
        let all_files = self.project_files().to_vec();

        // Now build the reverse map
        let mut reverse: ReverseIndex = HashMap::new();

        for caller_file in &all_files {
            // Canonicalize the caller file path for consistent lookups
            let canon_caller = Arc::new(
                std::fs::canonicalize(caller_file).unwrap_or_else(|_| caller_file.clone()),
            );
            let file_data = match self
                .data
                .get(caller_file)
                .or_else(|| self.data.get(canon_caller.as_ref()))
            {
                Some(d) => d,
                None => continue,
            };

            for (symbol_name, call_sites) in &file_data.calls_by_symbol {
                let caller_symbol: SharedStr = Arc::from(symbol_name.as_str());

                for call_site in call_sites {
                    let edge = Self::resolve_cross_file_edge_with_exports(
                        &call_site.full_callee,
                        &call_site.callee_name,
                        canon_caller.as_ref(),
                        &file_data.import_block,
                        |path, symbol_name| self.file_exports_symbol_cached(path, symbol_name),
                        |path| self.file_default_export_symbol_cached(path),
                    );

                    let (target_file, target_symbol, resolved) = match edge {
                        EdgeResolution::Resolved { file, symbol } => (file, symbol, true),
                        EdgeResolution::Unresolved { callee_name } => {
                            if !is_bare_callee(&call_site.full_callee, &callee_name) {
                                continue;
                            }

                            let Ok(target_symbol) = resolve_symbol_query_in_data(
                                file_data,
                                canon_caller.as_ref(),
                                &callee_name,
                            ) else {
                                continue;
                            };

                            (canon_caller.as_ref().clone(), target_symbol, false)
                        }
                    };

                    if target_file == *canon_caller.as_ref() && target_symbol == *symbol_name {
                        continue;
                    }

                    reverse
                        .entry(target_file)
                        .or_default()
                        .entry(target_symbol)
                        .or_default()
                        .push(IndexedCallerSite {
                            caller_file: Arc::clone(&canon_caller),
                            caller_symbol: Arc::clone(&caller_symbol),
                            line: call_site.line,
                            col: 0,
                            resolved,
                        });
                }
            }
        }

        self.reverse_index = Some(reverse);
        Ok(())
    }

    fn reverse_sites(&self, file: &Path, symbol: &str) -> Option<&[IndexedCallerSite]> {
        self.reverse_index
            .as_ref()?
            .get(file)?
            .get(symbol)
            .map(Vec::as_slice)
    }

    /// Get callers of a symbol in a file, grouped by calling file.
    ///
    /// Builds the reverse index on first call (scans all project files).
    /// Supports recursive depth expansion: depth=1 returns direct callers,
    /// depth=2 returns callers-of-callers, etc. depth=0 is treated as 1.
    pub fn callers_of(
        &mut self,
        file: &Path,
        symbol: &str,
        depth: usize,
        max_files: usize,
    ) -> Result<CallersResult, AftError> {
        let canon = self.canonicalize(file)?;

        // Ensure file is built (may already be cached) and resolve scoped identity.
        let resolved_symbol = {
            let file_data = self.build_file(&canon)?;
            resolve_symbol_query_in_data(file_data, &canon, symbol)?
        };

        // Build the reverse index if not cached
        if self.reverse_index.is_none() {
            self.build_reverse_index(max_files)?;
        }

        let scanned_files = self.project_files.as_ref().map(|f| f.len()).unwrap_or(0);
        let effective_depth = if depth == 0 { 1 } else { depth };

        let mut visited = HashSet::new();
        let mut all_sites: Vec<CallerSite> = Vec::new();
        let mut depth_limited = false;
        let mut truncated = 0;
        self.collect_callers_recursive(
            &canon,
            &resolved_symbol,
            effective_depth,
            0,
            &mut visited,
            &mut all_sites,
            &mut depth_limited,
            &mut truncated,
        );

        // Group by file

        let mut groups_map: HashMap<PathBuf, Vec<CallerEntry>> = HashMap::new();
        let total_callers = all_sites.len();
        for site in all_sites {
            let caller_file: PathBuf = site.caller_file;
            let caller_symbol: String = site.caller_symbol;
            let line = site.line;
            let entry = CallerEntry {
                symbol: caller_symbol,
                line,
            };

            if let Some(entries) = groups_map.get_mut(&caller_file) {
                entries.push(entry);
            } else {
                groups_map.insert(caller_file, vec![entry]);
            }
        }

        let mut callers: Vec<CallerGroup> = groups_map
            .into_iter()
            .map(|(file_path, entries)| CallerGroup {
                file: self.relative_path(&file_path),
                callers: entries,
            })
            .collect();

        // Sort groups by file path for deterministic output
        callers.sort_by(|a, b| a.file.cmp(&b.file));

        Ok(CallersResult {
            symbol: resolved_symbol,
            file: self.relative_path(&canon),
            callers,
            total_callers,
            scanned_files,
            depth_limited,
            truncated,
        })
    }

    /// Trace backward from a symbol to all entry points.
    ///
    /// Returns complete paths (top-down: entry point first, target last).
    /// Uses BFS backward through the reverse index, with per-path cycle
    /// detection and depth limiting.
    pub fn trace_to(
        &mut self,
        file: &Path,
        symbol: &str,
        max_depth: usize,
        max_files: usize,
    ) -> Result<TraceToResult, AftError> {
        let canon = self.canonicalize(file)?;

        // Ensure file is built and resolve scoped identity.
        let resolved_symbol = {
            let file_data = self.build_file(&canon)?;
            resolve_symbol_query_in_data(file_data, &canon, symbol)?
        };

        // Build the reverse index if not cached
        if self.reverse_index.is_none() {
            self.build_reverse_index(max_files)?;
        }

        let target_rel = self.relative_path(&canon);
        let effective_max = if max_depth == 0 { 10 } else { max_depth };
        if self.reverse_index.is_none() {
            return Err(AftError::ParseError {
                message: format!(
                    "reverse index unavailable after building callers for {}",
                    canon.display()
                ),
            });
        }

        // Get line/signature for the target symbol
        let (target_line, target_sig) = self
            .lookup_file_data(&canon)
            .map(|data| get_symbol_meta_from_data(data, &resolved_symbol))
            .unwrap_or_else(|| get_symbol_meta(&canon, &resolved_symbol));

        // Check if target itself is an entry point
        let target_is_entry = self
            .lookup_file_data(&canon)
            .and_then(|fd| {
                let meta = fd.symbol_metadata.get(&resolved_symbol)?;
                Some(is_entry_point(
                    &resolved_symbol,
                    &meta.kind,
                    meta.exported,
                    fd.lang,
                ))
            })
            .unwrap_or(false);

        // BFS state: each item is a partial path (bottom-up, will be reversed later)
        // Each path element: (canonicalized file, symbol name, line, signature)
        type PathElem = (SharedPath, SharedStr, u32, Option<String>);
        let mut complete_paths: Vec<Vec<PathElem>> = Vec::new();
        let mut max_depth_reached = false;
        let mut truncated_paths: usize = 0;

        // Initial path starts at the target
        let initial: Vec<PathElem> = vec![(
            Arc::new(canon.clone()),
            Arc::from(resolved_symbol.as_str()),
            target_line,
            target_sig,
        )];

        // If the target itself is an entry point, record it as a trivial path
        if target_is_entry {
            complete_paths.push(initial.clone());
        }

        // Queue of (current_path, depth)
        let mut queue: Vec<(Vec<PathElem>, usize)> = vec![(initial, 0)];

        while let Some((path, depth)) = queue.pop() {
            if depth >= effective_max {
                max_depth_reached = true;
                continue;
            }

            let Some((current_file, current_symbol, _, _)) = path.last() else {
                continue;
            };

            // Look up callers in reverse index
            let callers = match self.reverse_sites(current_file.as_ref(), current_symbol.as_ref()) {
                Some(sites) => sites,
                None => {
                    // Dead end: no callers and not an entry point
                    // (if it were an entry point, we'd have recorded it already)
                    if path.len() > 1 {
                        // Only count as truncated if this isn't the target itself
                        // (the target with no callers is just "no paths found")
                        truncated_paths += 1;
                    }
                    continue;
                }
            };

            let mut has_new_path = false;
            for site in callers {
                // Cycle detection: skip if this caller is already in the current path
                if path.iter().any(|(file_path, sym, _, _)| {
                    file_path.as_ref() == site.caller_file.as_ref()
                        && sym.as_ref() == site.caller_symbol.as_ref()
                }) {
                    continue;
                }

                has_new_path = true;

                // Get caller's metadata
                let (caller_line, caller_sig) = self
                    .lookup_file_data(site.caller_file.as_ref())
                    .map(|data| get_symbol_meta_from_data(data, site.caller_symbol.as_ref()))
                    .unwrap_or_else(|| {
                        get_symbol_meta(site.caller_file.as_ref(), site.caller_symbol.as_ref())
                    });

                let mut new_path = path.clone();
                new_path.push((
                    Arc::clone(&site.caller_file),
                    Arc::clone(&site.caller_symbol),
                    caller_line,
                    caller_sig,
                ));

                // Check if this caller is an entry point
                // Try both canonical and non-canonical keys (build_reverse_index
                // may have stored data under the raw walker path)
                let caller_is_entry = self
                    .lookup_file_data(site.caller_file.as_ref())
                    .and_then(|fd| {
                        let meta = fd.symbol_metadata.get(site.caller_symbol.as_ref())?;
                        Some(is_entry_point(
                            site.caller_symbol.as_ref(),
                            &meta.kind,
                            meta.exported,
                            fd.lang,
                        ))
                    })
                    .unwrap_or(false);

                if caller_is_entry {
                    complete_paths.push(new_path.clone());
                }
                // Always continue searching backward — there may be longer
                // paths through other entry points beyond this one
                queue.push((new_path, depth + 1));
            }

            // If we had callers but none were new (all cycles), count as truncated
            if !has_new_path && path.len() > 1 {
                truncated_paths += 1;
            }
        }

        // Reverse each path so it reads top-down (entry point → ... → target)
        // and convert to TraceHop/TracePath
        let mut paths: Vec<TracePath> = complete_paths
            .into_iter()
            .map(|mut elems| {
                elems.reverse();
                let hops: Vec<TraceHop> = elems
                    .iter()
                    .enumerate()
                    .map(|(i, (file_path, sym, line, sig))| {
                        let is_ep = if i == 0 {
                            // First hop (after reverse) is the entry point
                            self.lookup_file_data(file_path.as_ref())
                                .and_then(|fd| {
                                    let meta = fd.symbol_metadata.get(sym.as_ref())?;
                                    Some(is_entry_point(
                                        sym.as_ref(),
                                        &meta.kind,
                                        meta.exported,
                                        fd.lang,
                                    ))
                                })
                                .unwrap_or(false)
                        } else {
                            false
                        };
                        TraceHop {
                            symbol: sym.to_string(),
                            file: self.relative_path(file_path.as_ref()),
                            line: *line,
                            signature: sig.clone(),
                            is_entry_point: is_ep,
                        }
                    })
                    .collect();
                TracePath { hops }
            })
            .collect();

        // Sort paths for deterministic output (by entry point name, then path length)
        paths.sort_by(|a, b| {
            let a_entry = a.hops.first().map(|h| h.symbol.as_str()).unwrap_or("");
            let b_entry = b.hops.first().map(|h| h.symbol.as_str()).unwrap_or("");
            a_entry.cmp(b_entry).then(a.hops.len().cmp(&b.hops.len()))
        });

        // Count distinct entry points by identity, not just display name.
        let mut entry_points: HashSet<(String, String)> = HashSet::new();
        for p in &paths {
            if let Some(first) = p.hops.first() {
                if first.is_entry_point {
                    entry_points.insert((first.file.clone(), first.symbol.clone()));
                }
            }
        }

        let total_paths = paths.len();
        let entry_points_found = entry_points.len();

        Ok(TraceToResult {
            target_symbol: resolved_symbol,
            target_file: target_rel,
            paths,
            total_paths,
            entry_points_found,
            max_depth_reached,
            truncated_paths,
        })
    }

    /// Find all files that define a symbol matching a `trace_to_symbol` target query.
    ///
    /// The result is de-duplicated by file because `toFile` is only required
    /// when a target symbol name exists in multiple files.
    pub fn trace_to_symbol_candidates(
        &mut self,
        to_symbol: &str,
        max_files: usize,
    ) -> Result<Vec<TraceToSymbolCandidate>, AftError> {
        self.ensure_project_files_built(max_files)?;

        let mut candidates_by_file: HashMap<PathBuf, u32> = HashMap::new();
        let all_files = self.project_files().to_vec();

        for file in all_files {
            let canon = self.canonicalize(&file)?;
            let Some(file_data) = self
                .lookup_file_data(&canon)
                .or_else(|| self.lookup_file_data(&file))
            else {
                continue;
            };

            let symbol_candidates = symbol_query_candidates(file_data, to_symbol);
            if symbol_candidates.is_empty() {
                continue;
            }

            let line = symbol_candidates
                .iter()
                .filter_map(|symbol| file_data.symbol_metadata.get(symbol).map(|meta| meta.line))
                .min()
                .unwrap_or(1);

            candidates_by_file
                .entry(canon)
                .and_modify(|existing| *existing = (*existing).min(line))
                .or_insert(line);
        }

        let mut candidates: Vec<TraceToSymbolCandidate> = candidates_by_file
            .into_iter()
            .map(|(file, line)| TraceToSymbolCandidate {
                file: self.relative_path(&file),
                line,
            })
            .collect();
        candidates.sort_by(|a, b| a.file.cmp(&b.file).then(a.line.cmp(&b.line)));
        Ok(candidates)
    }

    /// Find the shortest forward call path from one symbol to another symbol.
    ///
    /// Performs breadth-first traversal over resolved call edges. A global
    /// `(file, symbol)` visited set keeps cycles finite while preserving BFS's
    /// shortest-path guarantee.
    pub fn trace_to_symbol(
        &mut self,
        file: &Path,
        symbol: &str,
        to_symbol: &str,
        to_file: Option<&Path>,
        max_depth: usize,
        max_files: usize,
    ) -> Result<TraceToSymbolResult, AftError> {
        let canon = self.canonicalize(file)?;

        // Ensure the origin file is built and resolve scoped identity.
        let resolved_symbol = {
            let file_data = self.build_file(&canon)?;
            resolve_symbol_query_in_data(file_data, &canon, symbol)?
        };

        self.ensure_project_files_built(max_files)?;

        let target_file = to_file.map(|path| self.canonicalize(path)).transpose()?;
        let effective_max = if max_depth == 0 {
            10
        } else {
            max_depth.min(16)
        };

        let start_hop = self.trace_to_symbol_hop(&canon, &resolved_symbol);
        if Self::trace_to_symbol_matches_target(&canon, &resolved_symbol, to_symbol, &target_file) {
            return Ok(TraceToSymbolResult {
                path: Some(vec![start_hop]),
                complete: true,
                reason: None,
            });
        }

        let mut queue: VecDeque<(PathBuf, String, Vec<TraceToSymbolHop>, usize)> = VecDeque::new();
        queue.push_back((canon.clone(), resolved_symbol.clone(), vec![start_hop], 0));

        let mut visited: HashSet<(PathBuf, String)> = HashSet::new();
        visited.insert((canon, resolved_symbol));
        let mut max_depth_exhausted = false;

        while let Some((current_file, current_symbol, path, depth)) = queue.pop_front() {
            let callees = self.forward_resolved_callees(&current_file, &current_symbol)?;

            if depth >= effective_max {
                if callees
                    .iter()
                    .any(|(file, symbol)| !visited.contains(&(file.clone(), symbol.clone())))
                {
                    max_depth_exhausted = true;
                }
                continue;
            }

            for (callee_file, callee_symbol) in callees {
                let visit_key = (callee_file.clone(), callee_symbol.clone());
                if !visited.insert(visit_key) {
                    continue;
                }

                let mut next_path = path.clone();
                next_path.push(self.trace_to_symbol_hop(&callee_file, &callee_symbol));

                if Self::trace_to_symbol_matches_target(
                    &callee_file,
                    &callee_symbol,
                    to_symbol,
                    &target_file,
                ) {
                    return Ok(TraceToSymbolResult {
                        path: Some(next_path),
                        complete: true,
                        reason: None,
                    });
                }

                queue.push_back((callee_file, callee_symbol, next_path, depth + 1));
            }
        }

        if max_depth_exhausted {
            Ok(TraceToSymbolResult {
                path: None,
                complete: false,
                reason: Some("max_depth_exhausted".to_string()),
            })
        } else {
            Ok(TraceToSymbolResult {
                path: None,
                complete: true,
                reason: Some("no_path_found".to_string()),
            })
        }
    }

    fn trace_to_symbol_matches_target(
        file: &Path,
        symbol: &str,
        to_symbol: &str,
        to_file: &Option<PathBuf>,
    ) -> bool {
        if !symbol_query_matches(symbol, to_symbol) {
            return false;
        }

        if let Some(target_file) = to_file {
            file == target_file
        } else {
            true
        }
    }

    fn trace_to_symbol_hop(&self, file: &Path, symbol: &str) -> TraceToSymbolHop {
        let (line, _) = self
            .lookup_file_data(file)
            .map(|data| get_symbol_meta_from_data(data, symbol))
            .unwrap_or_else(|| get_symbol_meta(file, symbol));

        TraceToSymbolHop {
            symbol: symbol.to_string(),
            file: self.relative_path(file),
            line,
        }
    }

    fn forward_resolved_callees(
        &mut self,
        file: &Path,
        symbol: &str,
    ) -> Result<Vec<(PathBuf, String)>, AftError> {
        let canon = self.canonicalize(file)?;
        let (import_block, call_sites) = {
            let file_data = self.build_file(&canon)?;
            (
                file_data.import_block.clone(),
                file_data
                    .calls_by_symbol
                    .get(symbol)
                    .cloned()
                    .unwrap_or_default(),
            )
        };

        let mut callees = Vec::new();
        for call_site in call_sites {
            let edge = self.resolve_cross_file_edge(
                &call_site.full_callee,
                &call_site.callee_name,
                &canon,
                &import_block,
            );

            match edge {
                EdgeResolution::Resolved {
                    file: target_file,
                    symbol: target_symbol,
                } => {
                    let target_canon = self.canonicalize(&target_file)?;
                    if self.build_file(&target_canon).is_err() {
                        continue;
                    }

                    let resolved_target_symbol = self
                        .lookup_file_data(&target_canon)
                        .and_then(|data| {
                            resolve_symbol_query_in_data(data, &target_canon, &target_symbol).ok()
                        })
                        .unwrap_or(target_symbol);

                    callees.push((target_canon, resolved_target_symbol));
                }
                EdgeResolution::Unresolved { callee_name } => {
                    if !is_bare_callee(&call_site.full_callee, &callee_name) {
                        continue;
                    }

                    let local_symbol = self.lookup_file_data(&canon).and_then(|data| {
                        resolve_symbol_query_in_data(data, &canon, &callee_name).ok()
                    });

                    if let Some(local_symbol) = local_symbol {
                        callees.push((canon.clone(), local_symbol));
                    }
                }
            }
        }

        Ok(callees)
    }

    /// Impact analysis: enriched callers query.
    ///
    /// Returns all call sites affected by a change to the given symbol,
    /// annotated with each caller's signature, entry point status, the
    /// source line at the call site, and extracted parameter names.
    pub fn impact(
        &mut self,
        file: &Path,
        symbol: &str,
        depth: usize,
        max_files: usize,
    ) -> Result<ImpactResult, AftError> {
        let canon = self.canonicalize(file)?;

        // Ensure file is built and resolve scoped identity.
        let resolved_symbol = {
            let file_data = self.build_file(&canon)?;
            resolve_symbol_query_in_data(file_data, &canon, symbol)?
        };

        // Build the reverse index if not cached
        if self.reverse_index.is_none() {
            self.build_reverse_index(max_files)?;
        }

        let effective_depth = if depth == 0 { 1 } else { depth };

        // Get the target symbol's own metadata
        let (target_signature, target_parameters, target_lang) = {
            let file_data = match self.data.get(&canon) {
                Some(d) => d,
                None => {
                    return Err(AftError::InvalidRequest {
                        message: "file data missing after build".to_string(),
                    })
                }
            };
            let meta = file_data.symbol_metadata.get(&resolved_symbol);
            let sig = meta.and_then(|m| m.signature.clone());
            let lang = file_data.lang;
            let params = sig
                .as_deref()
                .map(|s| extract_parameters(s, lang))
                .unwrap_or_default();
            (sig, params, lang)
        };

        // Collect all caller sites (transitive)
        let mut visited = HashSet::new();
        let mut all_sites: Vec<CallerSite> = Vec::new();
        let mut depth_limited = false;
        let mut truncated = 0;
        self.collect_callers_recursive(
            &canon,
            &resolved_symbol,
            effective_depth,
            0,
            &mut visited,
            &mut all_sites,
            &mut depth_limited,
            &mut truncated,
        );

        // Deduplicate sites by (file, symbol, line)
        let mut seen: HashSet<(PathBuf, String, u32)> = HashSet::new();
        all_sites.retain(|site| {
            seen.insert((
                site.caller_file.clone(),
                site.caller_symbol.clone(),
                site.line,
            ))
        });

        // Enrich each caller site
        let mut callers = Vec::new();
        let mut affected_file_set = HashSet::new();

        for site in &all_sites {
            // Build the caller's file to get metadata
            if let Err(e) = self.build_file(site.caller_file.as_path()) {
                log::debug!(
                    "callgraph: skipping caller file {}: {}",
                    site.caller_file.display(),
                    e
                );
            }

            let (sig, is_ep, params, _lang) = {
                if let Some(fd) = self.lookup_file_data(site.caller_file.as_path()) {
                    let meta = fd.symbol_metadata.get(&site.caller_symbol);
                    let sig = meta.and_then(|m| m.signature.clone());
                    let kind = meta.map(|m| m.kind.clone()).unwrap_or(SymbolKind::Function);
                    let exported = meta.map(|m| m.exported).unwrap_or(false);
                    let is_ep = is_entry_point(&site.caller_symbol, &kind, exported, fd.lang);
                    let lang = fd.lang;
                    let params = sig
                        .as_deref()
                        .map(|s| extract_parameters(s, lang))
                        .unwrap_or_default();
                    (sig, is_ep, params, lang)
                } else {
                    (None, false, Vec::new(), target_lang)
                }
            };

            // Read the source line at the call site
            let call_expression = self.read_source_line(site.caller_file.as_path(), site.line);

            let rel_file = self.relative_path(site.caller_file.as_path());
            affected_file_set.insert(rel_file.clone());

            callers.push(ImpactCaller {
                caller_symbol: site.caller_symbol.clone(),
                caller_file: rel_file,
                line: site.line,
                signature: sig,
                is_entry_point: is_ep,
                call_expression,
                parameters: params,
            });
        }

        // Sort callers by file then line for deterministic output
        callers.sort_by(|a, b| a.caller_file.cmp(&b.caller_file).then(a.line.cmp(&b.line)));

        let total_affected = callers.len();
        let affected_files = affected_file_set.len();

        Ok(ImpactResult {
            symbol: resolved_symbol,
            file: self.relative_path(&canon),
            signature: target_signature,
            parameters: target_parameters,
            total_affected,
            affected_files,
            callers,
            depth_limited,
            truncated,
        })
    }

    /// Trace how an expression flows through variable assignments within a
    /// function body and across function boundaries via argument-to-parameter
    /// matching.
    ///
    /// Algorithm:
    /// 1. Parse the function body, find the expression text.
    /// 2. Walk AST for assignments that reference the tracked name.
    /// 3. When the tracked name appears as a call argument, resolve the callee,
    ///    match argument position to parameter name, recurse.
    /// 4. Destructuring, spread, and unresolved calls produce approximate hops.
    pub fn trace_data(
        &mut self,
        file: &Path,
        symbol: &str,
        expression: &str,
        max_depth: usize,
        max_files: usize,
    ) -> Result<TraceDataResult, AftError> {
        let canon = self.canonicalize(file)?;
        let rel_file = self.relative_path(&canon);

        // Ensure file data is built and resolve scoped identity.
        let resolved_symbol = {
            let file_data = self.build_file(&canon)?;
            resolve_symbol_query_in_data(file_data, &canon, symbol)?
        };

        // Bounded count: short-circuits at `max_files + 1` so oversized roots
        // reject in microseconds instead of paying the full walk/collect cost.
        // Matches the guard used by build_reverse_index / callers_of / trace_to / impact.
        let count = self.project_file_count_bounded(max_files);
        if count > max_files {
            return Err(AftError::ProjectTooLarge {
                count,
                max: max_files,
            });
        }

        let mut hops = Vec::new();
        let mut depth_limited = false;

        self.trace_data_inner(
            &canon,
            &resolved_symbol,
            expression,
            max_depth,
            0,
            &mut hops,
            &mut depth_limited,
            &mut HashSet::new(),
        );

        Ok(TraceDataResult {
            expression: expression.to_string(),
            origin_file: rel_file,
            origin_symbol: resolved_symbol,
            hops,
            depth_limited,
        })
    }

    /// Inner recursive data flow tracking.
    fn trace_data_inner(
        &mut self,
        file: &Path,
        symbol: &str,
        tracking_name: &str,
        max_depth: usize,
        current_depth: usize,
        hops: &mut Vec<DataFlowHop>,
        depth_limited: &mut bool,
        visited: &mut HashSet<(PathBuf, String, String)>,
    ) {
        let visit_key = (
            file.to_path_buf(),
            symbol.to_string(),
            tracking_name.to_string(),
        );
        if visited.contains(&visit_key) {
            return; // cycle
        }
        visited.insert(visit_key);

        // Read and parse the file
        let source = match std::fs::read_to_string(file) {
            Ok(s) => s,
            Err(_) => return,
        };

        let lang = match detect_language(file) {
            Some(l) => l,
            None => return,
        };

        let grammar = grammar_for(lang);
        let mut parser = Parser::new();
        if parser.set_language(&grammar).is_err() {
            return;
        }
        let tree = match parser.parse(&source, None) {
            Some(t) => t,
            None => return,
        };

        // Find the symbol's AST node range
        let symbols = match crate::parser::extract_symbols_from_tree(&source, &tree, lang) {
            Ok(symbols) => symbols,
            Err(_) => return,
        };
        let sym_info = match symbols
            .iter()
            .find(|s| symbol_identity(s) == symbol || s.name == symbol)
        {
            Some(s) => s,
            None => return,
        };

        let body_start =
            line_col_to_byte(&source, sym_info.range.start_line, sym_info.range.start_col);
        let body_end = line_col_to_byte(&source, sym_info.range.end_line, sym_info.range.end_col);

        let root = tree.root_node();

        // Find the symbol's body node (the function/method definition node)
        let body_node = match find_node_covering_range(root, body_start, body_end) {
            Some(n) => n,
            None => return,
        };

        // Track names through the body
        let mut tracked_names: Vec<String> = vec![tracking_name.to_string()];
        let rel_file = self.relative_path(file);

        // Walk the body looking for assignments and calls
        self.walk_for_data_flow(
            body_node,
            &source,
            &mut tracked_names,
            file,
            symbol,
            &rel_file,
            lang,
            max_depth,
            current_depth,
            hops,
            depth_limited,
            visited,
        );
    }

    /// Walk an AST subtree looking for assignments and call expressions that
    /// reference tracked names.
    #[allow(clippy::too_many_arguments)]
    fn walk_for_data_flow(
        &mut self,
        node: tree_sitter::Node,
        source: &str,
        tracked_names: &mut Vec<String>,
        file: &Path,
        symbol: &str,
        rel_file: &str,
        lang: LangId,
        max_depth: usize,
        current_depth: usize,
        hops: &mut Vec<DataFlowHop>,
        depth_limited: &mut bool,
        visited: &mut HashSet<(PathBuf, String, String)>,
    ) {
        let kind = node.kind();

        // Check for variable declarations / assignments
        let is_var_decl = matches!(
            kind,
            "variable_declarator"
                | "assignment_expression"
                | "augmented_assignment_expression"
                | "assignment"
                | "let_declaration"
                | "short_var_declaration"
        );

        if is_var_decl {
            if let Some((new_name, init_text, line, is_approx)) =
                self.extract_assignment_info(node, source, lang, tracked_names)
            {
                // The RHS references a tracked name — add assignment hop
                if !is_approx {
                    hops.push(DataFlowHop {
                        file: rel_file.to_string(),
                        symbol: symbol.to_string(),
                        variable: new_name.clone(),
                        line,
                        flow_type: "assignment".to_string(),
                        approximate: false,
                    });
                    tracked_names.push(new_name);
                } else {
                    // Destructuring or pattern — approximate
                    hops.push(DataFlowHop {
                        file: rel_file.to_string(),
                        symbol: symbol.to_string(),
                        variable: init_text,
                        line,
                        flow_type: "assignment".to_string(),
                        approximate: true,
                    });
                    // Don't track further through this branch
                    return;
                }
            }
        }

        // Check for call expressions where tracked name is an argument
        if kind == "call_expression" || kind == "call" || kind == "macro_invocation" {
            self.check_call_for_data_flow(
                node,
                source,
                tracked_names,
                file,
                symbol,
                rel_file,
                lang,
                max_depth,
                current_depth,
                hops,
                depth_limited,
                visited,
            );
        }

        // Recurse into children
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                // Don't re-process the current node type in recursion
                self.walk_for_data_flow(
                    child,
                    source,
                    tracked_names,
                    file,
                    symbol,
                    rel_file,
                    lang,
                    max_depth,
                    current_depth,
                    hops,
                    depth_limited,
                    visited,
                );
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
    }

    /// Check if an assignment/declaration node assigns from a tracked name.
    /// Returns (new_name, init_text, line, is_approximate).
    fn extract_assignment_info(
        &self,
        node: tree_sitter::Node,
        source: &str,
        _lang: LangId,
        tracked_names: &[String],
    ) -> Option<(String, String, u32, bool)> {
        let kind = node.kind();
        let line = node.start_position().row as u32 + 1;

        match kind {
            "variable_declarator" => {
                // TS/JS: const x = <expr>
                let name_node = node.child_by_field_name("name")?;
                let value_node = node.child_by_field_name("value")?;
                let name_text = node_text(name_node, source);
                let value_text = node_text(value_node, source);

                // Check if name is a destructuring pattern
                if name_node.kind() == "object_pattern" || name_node.kind() == "array_pattern" {
                    // Check if value references a tracked name
                    if tracked_names.iter().any(|t| value_text.contains(t)) {
                        return Some((name_text.clone(), name_text, line, true));
                    }
                    return None;
                }

                // Check if value references any tracked name
                if tracked_names.iter().any(|t| {
                    value_text == *t
                        || value_text.starts_with(&format!("{}.", t))
                        || value_text.starts_with(&format!("{}[", t))
                }) {
                    return Some((name_text, value_text, line, false));
                }
                None
            }
            "assignment_expression" | "augmented_assignment_expression" => {
                // TS/JS: x = <expr>
                let left = node.child_by_field_name("left")?;
                let right = node.child_by_field_name("right")?;
                let left_text = node_text(left, source);
                let right_text = node_text(right, source);

                if tracked_names.iter().any(|t| right_text == *t) {
                    return Some((left_text, right_text, line, false));
                }
                None
            }
            "assignment" => {
                // Python: x = <expr>
                let left = node.child_by_field_name("left")?;
                let right = node.child_by_field_name("right")?;
                let left_text = node_text(left, source);
                let right_text = node_text(right, source);

                if tracked_names.iter().any(|t| right_text == *t) {
                    return Some((left_text, right_text, line, false));
                }
                None
            }
            "let_declaration" | "short_var_declaration" => {
                // Rust / Go
                let left = node
                    .child_by_field_name("pattern")
                    .or_else(|| node.child_by_field_name("left"))?;
                let right = node
                    .child_by_field_name("value")
                    .or_else(|| node.child_by_field_name("right"))?;
                let left_text = node_text(left, source);
                let right_text = node_text(right, source);

                if tracked_names.iter().any(|t| right_text == *t) {
                    return Some((left_text, right_text, line, false));
                }
                None
            }
            _ => None,
        }
    }

    /// Check if a call expression uses a tracked name as an argument, and if so,
    /// resolve the callee and recurse into its body tracking the parameter name.
    #[allow(clippy::too_many_arguments)]
    fn check_call_for_data_flow(
        &mut self,
        node: tree_sitter::Node,
        source: &str,
        tracked_names: &[String],
        file: &Path,
        _symbol: &str,
        rel_file: &str,
        _lang: LangId,
        max_depth: usize,
        current_depth: usize,
        hops: &mut Vec<DataFlowHop>,
        depth_limited: &mut bool,
        visited: &mut HashSet<(PathBuf, String, String)>,
    ) {
        // Find the arguments node
        let args_node = find_child_by_kind(node, "arguments")
            .or_else(|| find_child_by_kind(node, "argument_list"));

        let args_node = match args_node {
            Some(n) => n,
            None => return,
        };

        // Collect argument texts and find which position a tracked name appears at
        let mut arg_positions: Vec<(usize, String)> = Vec::new(); // (position, tracked_name)
        let mut arg_idx = 0;

        let mut cursor = args_node.walk();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                let child_kind = child.kind();

                // Skip punctuation (parentheses, commas)
                if child_kind == "(" || child_kind == ")" || child_kind == "," {
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                    continue;
                }

                let arg_text = node_text(child, source);

                // Check for spread element — approximate
                if child_kind == "spread_element" || child_kind == "dictionary_splat" {
                    if tracked_names.iter().any(|t| arg_text.contains(t)) {
                        hops.push(DataFlowHop {
                            file: rel_file.to_string(),
                            symbol: _symbol.to_string(),
                            variable: arg_text,
                            line: child.start_position().row as u32 + 1,
                            flow_type: "parameter".to_string(),
                            approximate: true,
                        });
                    }
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                    arg_idx += 1;
                    continue;
                }

                if tracked_names.iter().any(|t| arg_text == *t) {
                    arg_positions.push((arg_idx, arg_text));
                }

                arg_idx += 1;
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }

        if arg_positions.is_empty() {
            return;
        }

        // Resolve the callee
        let (full_callee, short_callee) = extract_callee_names(node, source);
        let full_callee = match full_callee {
            Some(f) => f,
            None => return,
        };
        let short_callee = match short_callee {
            Some(s) => s,
            None => return,
        };

        // Try to resolve cross-file edge
        let import_block = {
            match self.data.get(file) {
                Some(fd) => fd.import_block.clone(),
                None => return,
            }
        };

        let edge = self.resolve_cross_file_edge(&full_callee, &short_callee, file, &import_block);

        match edge {
            EdgeResolution::Resolved {
                file: target_file,
                symbol: target_symbol,
            } => {
                if current_depth + 1 > max_depth {
                    *depth_limited = true;
                    return;
                }

                // Build target file to get parameter info
                if let Err(e) = self.build_file(&target_file) {
                    log::debug!(
                        "callgraph: skipping target file {}: {}",
                        target_file.display(),
                        e
                    );
                }
                let (params, target_line) = {
                    match self.lookup_file_data(&target_file) {
                        Some(fd) => {
                            let meta = fd.symbol_metadata.get(&target_symbol);
                            let sig = meta.and_then(|m| m.signature.clone());
                            let params = sig
                                .as_deref()
                                .map(|s| extract_parameters(s, fd.lang))
                                .unwrap_or_default();
                            let line = meta.map(|m| m.line).unwrap_or(1);
                            (params, line)
                        }
                        None => return,
                    }
                };

                let target_rel = self.relative_path(&target_file);

                for (pos, _tracked) in &arg_positions {
                    if let Some(param_name) = params.get(*pos) {
                        // Add parameter hop
                        hops.push(DataFlowHop {
                            file: target_rel.clone(),
                            symbol: target_symbol.clone(),
                            variable: param_name.clone(),
                            line: target_line,
                            flow_type: "parameter".to_string(),
                            approximate: false,
                        });

                        // Recurse into callee's body tracking the parameter name
                        self.trace_data_inner(
                            &target_file.clone(),
                            &target_symbol.clone(),
                            param_name,
                            max_depth,
                            current_depth + 1,
                            hops,
                            depth_limited,
                            visited,
                        );
                    }
                }
            }
            EdgeResolution::Unresolved { callee_name } => {
                let local_symbol = if is_bare_callee(&full_callee, &callee_name) {
                    self.data
                        .get(file)
                        .and_then(|fd| resolve_symbol_query_in_data(fd, file, &callee_name).ok())
                } else {
                    None
                };

                if let Some(local_symbol) = local_symbol {
                    // Same-file bare call — get param info
                    let (params, target_line) = {
                        let Some(fd) = self.data.get(file) else {
                            return;
                        };
                        let meta = fd.symbol_metadata.get(&local_symbol);
                        let sig = meta.and_then(|m| m.signature.clone());
                        let params = sig
                            .as_deref()
                            .map(|s| extract_parameters(s, fd.lang))
                            .unwrap_or_default();
                        let line = meta.map(|m| m.line).unwrap_or(1);
                        (params, line)
                    };

                    let file_rel = self.relative_path(file);

                    for (pos, _tracked) in &arg_positions {
                        if let Some(param_name) = params.get(*pos) {
                            hops.push(DataFlowHop {
                                file: file_rel.clone(),
                                symbol: local_symbol.clone(),
                                variable: param_name.clone(),
                                line: target_line,
                                flow_type: "parameter".to_string(),
                                approximate: false,
                            });

                            // Recurse into same-file function
                            self.trace_data_inner(
                                file,
                                &local_symbol,
                                param_name,
                                max_depth,
                                current_depth + 1,
                                hops,
                                depth_limited,
                                visited,
                            );
                        }
                    }
                } else {
                    // Truly unresolved — approximate hop
                    for (_pos, tracked) in &arg_positions {
                        hops.push(DataFlowHop {
                            file: self.relative_path(file),
                            symbol: callee_name.clone(),
                            variable: tracked.clone(),
                            line: node.start_position().row as u32 + 1,
                            flow_type: "parameter".to_string(),
                            approximate: true,
                        });
                    }
                }
            }
        }
    }

    /// Read a single source line (1-based) from a file, trimmed.
    fn read_source_line(&self, path: &Path, line: u32) -> Option<String> {
        let content = std::fs::read_to_string(path).ok()?;
        content
            .lines()
            .nth(line.saturating_sub(1) as usize)
            .map(|l| l.trim().to_string())
    }

    /// Recursively collect callers up to the given depth.
    fn collect_callers_recursive(
        &self,
        file: &Path,
        symbol: &str,
        max_depth: usize,
        current_depth: usize,
        visited: &mut HashSet<(PathBuf, SharedStr)>,
        result: &mut Vec<CallerSite>,
        depth_limited: &mut bool,
        truncated: &mut usize,
    ) {
        // Canonicalize for consistent reverse index lookup
        let canon = std::fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
        let key_symbol: SharedStr = Arc::from(symbol);

        if current_depth >= max_depth {
            let omitted = self
                .reverse_sites(&canon, key_symbol.as_ref())
                .map(|sites| sites.len())
                .unwrap_or(0);
            if omitted > 0 {
                *depth_limited = true;
                *truncated += omitted;
            }
            return;
        }

        if !visited.insert((canon.clone(), Arc::clone(&key_symbol))) {
            return; // cycle detection
        }

        if let Some(sites) = self.reverse_sites(&canon, key_symbol.as_ref()) {
            for site in sites {
                result.push(CallerSite {
                    caller_file: site.caller_file.as_ref().clone(),
                    caller_symbol: site.caller_symbol.to_string(),
                    line: site.line,
                    col: site.col,
                    resolved: site.resolved,
                });
                // Recurse: find callers of the caller
                if current_depth + 1 < max_depth {
                    self.collect_callers_recursive(
                        site.caller_file.as_ref(),
                        site.caller_symbol.as_ref(),
                        max_depth,
                        current_depth + 1,
                        visited,
                        result,
                        depth_limited,
                        truncated,
                    );
                } else {
                    let omitted = self
                        .reverse_sites(site.caller_file.as_ref(), site.caller_symbol.as_ref())
                        .map(|sites| sites.len())
                        .unwrap_or(0);
                    if omitted > 0 {
                        *depth_limited = true;
                        *truncated += omitted;
                    }
                }
            }
        }
    }

    /// Invalidate a file: remove its cached data and clear the reverse index.
    ///
    /// Called by the file watcher when a file changes on disk. The reverse
    /// index is rebuilt lazily on the next `callers_of` call.
    pub fn invalidate_file(&mut self, path: &Path) {
        // Remove from data cache (try both as-is and canonicalized)
        self.data.remove(path);
        if let Ok(canon) = self.canonicalize(path) {
            self.data.remove(&canon);
        }
        // Clear the reverse index — it's stale
        self.reverse_index = None;
        // Clear project_files cache for create/remove events
        self.project_files = None;
        clear_workspace_package_cache();
    }

    /// Return a path relative to the project root, or the absolute path if
    /// it's outside the project.
    fn relative_path(&self, path: &Path) -> String {
        path.strip_prefix(&self.project_root)
            .unwrap_or(path)
            .display()
            .to_string()
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

    /// Look up cached file data, trying both the given path and its
    /// canonicalized form. Needed because `build_reverse_index` may store
    /// data under raw walker paths while CallerSite uses canonical paths.
    fn lookup_file_data(&self, path: &Path) -> Option<&FileCallData> {
        if let Some(fd) = self.data.get(path) {
            return Some(fd);
        }
        // Try canonical
        let canon = std::fs::canonicalize(path).ok()?;
        self.data.get(&canon).or_else(|| {
            // Try non-canonical forms stored by the walker
            self.data.iter().find_map(|(k, v)| {
                if std::fs::canonicalize(k).ok().as_ref() == Some(&canon) {
                    Some(v)
                } else {
                    None
                }
            })
        })
    }
}

// ---------------------------------------------------------------------------
// File-level building
// ---------------------------------------------------------------------------

/// Build call data for a single file.
fn build_file_data(path: &Path) -> Result<FileCallData, AftError> {
    let lang = detect_language(path).ok_or_else(|| AftError::InvalidRequest {
        message: format!("unsupported file for call graph: {}", path.display()),
    })?;

    let source = std::fs::read_to_string(path).map_err(|e| AftError::FileNotFound {
        path: format!("{}: {}", path.display(), e),
    })?;

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

    // Build calls_by_symbol
    let mut calls_by_symbol: HashMap<String, Vec<CallSite>> = HashMap::new();
    let root = tree.root_node();

    for sym in &symbols {
        let byte_start = line_col_to_byte(&source, sym.range.start_line, sym.range.start_col);
        let byte_end = line_col_to_byte(&source, sym.range.end_line, sym.range.end_col);

        let raw_calls = extract_calls_full(&source, root, byte_start, byte_end, lang);

        let sites: Vec<CallSite> = raw_calls
            .into_iter()
            .map(|(full, short, line)| CallSite {
                callee_name: short,
                full_callee: full,
                line,
                byte_start,
                byte_end,
            })
            .collect();

        if !sites.is_empty() {
            calls_by_symbol.insert(symbol_identity(sym), sites);
        }
    }

    let symbol_ranges: Vec<(usize, usize)> = symbols
        .iter()
        .map(|sym| {
            (
                line_col_to_byte(&source, sym.range.start_line, sym.range.start_col),
                line_col_to_byte(&source, sym.range.end_line, sym.range.end_col),
            )
        })
        .collect();

    let top_level_sites: Vec<CallSite> =
        collect_calls_full_with_ranges(root, &source, 0, source.len(), lang)
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
            .collect();

    if !top_level_sites.is_empty() {
        calls_by_symbol.insert(TOP_LEVEL_SYMBOL.to_string(), top_level_sites);
    }

    let default_export = find_default_export(&source, root, path, lang);

    if let Some(default_export) = &default_export {
        if default_export.synthetic {
            let byte_start = default_export.node.byte_range().start;
            let byte_end = default_export.node.byte_range().end;
            let raw_calls = extract_calls_full(&source, root, byte_start, byte_end, lang);
            let sites: Vec<CallSite> = raw_calls
                .into_iter()
                .filter(|(_, short, _)| *short != default_export.symbol)
                .map(|(full, short, line)| CallSite {
                    callee_name: short,
                    full_callee: full,
                    line,
                    byte_start,
                    byte_end,
                })
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

    // Build per-symbol metadata for entry point detection
    let mut symbol_metadata: HashMap<String, SymbolMeta> = symbols
        .iter()
        .map(|s| {
            (
                symbol_identity(s),
                SymbolMeta {
                    kind: s.kind.clone(),
                    exported: s.exported,
                    signature: s.signature.clone(),
                    line: s.range.start_line + 1,
                    range: s.range.clone(),
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
            });
    }
    if calls_by_symbol.contains_key(TOP_LEVEL_SYMBOL) {
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
            });
    }

    Ok(FileCallData {
        calls_by_symbol,
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

fn get_symbol_meta_from_data(file_data: &FileCallData, symbol_name: &str) -> (u32, Option<String>) {
    file_data
        .symbol_metadata
        .get(symbol_name)
        .map(|meta| (meta.line, meta.signature.clone()))
        .unwrap_or((1, None))
}

/// Get symbol metadata (line, signature) from a file.
fn get_symbol_meta(path: &Path, symbol_name: &str) -> (u32, Option<String>) {
    let provider = crate::parser::TreeSitterProvider::new();
    match provider.list_symbols(path) {
        Ok(symbols) => {
            for s in &symbols {
                if symbol_identity(s) == symbol_name || s.name == symbol_name {
                    return (s.range.start_line + 1, s.signature.clone());
                }
            }
            (1, None)
        }
        Err(_) => (1, None),
    }
}

// ---------------------------------------------------------------------------
// Data flow tracking helpers
// ---------------------------------------------------------------------------

/// Get the text of a tree-sitter node from the source.
fn node_text(node: tree_sitter::Node, source: &str) -> String {
    source[node.start_byte()..node.end_byte()].to_string()
}

/// Find the smallest node that fully covers a byte range.
fn find_node_covering_range(
    root: tree_sitter::Node,
    start: usize,
    end: usize,
) -> Option<tree_sitter::Node> {
    let mut best = None;
    let mut cursor = root.walk();

    fn walk_covering<'a>(
        cursor: &mut tree_sitter::TreeCursor<'a>,
        start: usize,
        end: usize,
        best: &mut Option<tree_sitter::Node<'a>>,
    ) {
        let node = cursor.node();
        if node.start_byte() <= start && node.end_byte() >= end {
            *best = Some(node);
            if cursor.goto_first_child() {
                loop {
                    walk_covering(cursor, start, end, best);
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                }
                cursor.goto_parent();
            }
        }
    }

    walk_covering(&mut cursor, start, end, &mut best);
    best
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

#[derive(Debug, Clone)]
struct CallSiteWithRange {
    full: String,
    short: String,
    line: u32,
    byte_start: usize,
    byte_end: usize,
}

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

/// Extract full and short callee names from a call_expression node.
fn extract_callee_names(node: tree_sitter::Node, source: &str) -> (Option<String>, Option<String>) {
    // The "function" field holds the callee
    let callee = match node.child_by_field_name("function") {
        Some(c) => c,
        None => return (None, None),
    };

    let full = node_text(callee, source);
    let short = if full.contains('.') {
        full.rsplit('.').next().unwrap_or(&full).to_string()
    } else {
        full.clone()
    };

    (Some(full), Some(short))
}

// ---------------------------------------------------------------------------
// Module path resolution
// ---------------------------------------------------------------------------

/// Resolve a module path (e.g. './utils') relative to a directory.
///
/// Tries common file extensions for TypeScript/JavaScript projects.
pub(crate) fn resolve_module_path(from_dir: &Path, module_path: &str) -> Option<PathBuf> {
    if module_path.starts_with('.') {
        return resolve_relative_module_path(from_dir, module_path);
    }

    if module_path.starts_with('/') {
        return None;
    }

    if let Some(path) = resolve_tsconfig_path(from_dir, module_path) {
        return Some(path);
    }

    resolve_workspace_module_path(from_dir, module_path)
}

fn resolve_relative_module_path(from_dir: &Path, module_path: &str) -> Option<PathBuf> {
    let base = from_dir.join(module_path);
    resolve_file_like_path(&base)
}

fn resolve_file_like_path(base: &Path) -> Option<PathBuf> {
    let base = base.to_path_buf();

    // Try exact path first
    if base.is_file() {
        return Some(std::fs::canonicalize(&base).unwrap_or(base));
    }

    // Try common extensions, including ESM/CJS TypeScript pairs used by workspaces.
    for ext in JS_TS_EXTENSIONS {
        let with_ext = base.with_extension(ext);
        if with_ext.is_file() {
            return Some(std::fs::canonicalize(&with_ext).unwrap_or(with_ext));
        }
    }

    // Try as directory with index file
    if base.is_dir() {
        if let Some(index) = find_index_file(&base) {
            return Some(index);
        }
    }

    None
}

fn resolve_workspace_module_path(from_dir: &Path, module_path: &str) -> Option<PathBuf> {
    let (package_name, subpath) = split_package_import(module_path)?;
    let package_root = find_package_root_for_import(from_dir, &package_name)?;
    resolve_package_entry(&package_root, &subpath)
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
        return glob::glob(&pattern)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
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

fn resolve_tsconfig_path(from_dir: &Path, module_path: &str) -> Option<PathBuf> {
    let tsconfig_dir = find_tsconfig_dir(from_dir)?;
    let tsconfig = package_json_like_value(&tsconfig_dir.join("tsconfig.json"))?;
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
            if let Some(path) = resolve_file_like_path(&base_dir.join(target)) {
                return Some(path);
            }
        }
    }

    None
}

fn find_tsconfig_dir(from_dir: &Path) -> Option<PathBuf> {
    let mut current = Some(from_dir);
    while let Some(dir) = current {
        if dir.join("tsconfig.json").is_file() {
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

fn find_package_root_for_import(from_dir: &Path, package_name: &str) -> Option<PathBuf> {
    let mut current = Some(from_dir);
    while let Some(dir) = current {
        if package_json_name(dir).as_deref() == Some(package_name) {
            return Some(std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf()));
        }
        current = dir.parent();
    }

    find_workspace_root(from_dir)
        .and_then(|workspace_root| resolve_workspace_package(&workspace_root, package_name))
}

fn find_workspace_root(from_dir: &Path) -> Option<PathBuf> {
    let mut current = Some(from_dir);
    while let Some(dir) = current {
        if is_workspace_root(dir) {
            return Some(std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf()));
        }
        current = dir.parent();
    }
    None
}

fn is_workspace_root(dir: &Path) -> bool {
    package_json_value(dir)
        .map(|value| !workspace_patterns(&value).is_empty())
        .unwrap_or(false)
        || !pnpm_workspace_patterns(dir).is_empty()
}

fn clear_workspace_package_cache() {
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

fn resolve_workspace_package(workspace_root: &Path, package_name: &str) -> Option<PathBuf> {
    let workspace_root =
        std::fs::canonicalize(workspace_root).unwrap_or_else(|_| workspace_root.to_path_buf());
    let cache_key = (workspace_root.clone(), package_name.to_string());

    if let Ok(cache) = WORKSPACE_PACKAGE_CACHE.read() {
        if let Some(cached) = cache.get(&cache_key) {
            return cached.clone();
        }
    }

    let resolved = workspace_member_dirs(&workspace_root)
        .into_iter()
        .find(|dir| package_json_name(dir).as_deref() == Some(package_name))
        .map(|dir| std::fs::canonicalize(&dir).unwrap_or(dir));

    if let Ok(mut cache) = WORKSPACE_PACKAGE_CACHE.write() {
        cache.insert(cache_key, resolved.clone());
    }

    resolved
}

fn workspace_member_dirs(workspace_root: &Path) -> Vec<PathBuf> {
    let mut patterns = package_json_value(workspace_root)
        .map(|package_json| workspace_patterns(&package_json))
        .unwrap_or_default();
    patterns.extend(pnpm_workspace_patterns(workspace_root));

    expand_workspace_patterns(workspace_root, &patterns)
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

fn pnpm_workspace_patterns(workspace_root: &Path) -> Vec<String> {
    let Ok(source) = std::fs::read_to_string(workspace_root.join("pnpm-workspace.yaml")) else {
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

fn expand_workspace_patterns(workspace_root: &Path, patterns: &[String]) -> Vec<PathBuf> {
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
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if matches!(
            name.as_ref(),
            "node_modules" | ".git" | "target" | "dist" | "build"
        ) {
            continue;
        }

        if path.join("package.json").is_file() {
            if let Ok(rel) = path.strip_prefix(workspace_root) {
                let rel = rel.to_string_lossy().replace('\\', "/");
                if positives.is_match(&rel) && !negatives.is_match(&rel) {
                    members.push(path.clone());
                }
            }
        }

        collect_workspace_member_dirs(workspace_root, &path, positives, negatives, members);
    }
}

fn package_json_value(dir: &Path) -> Option<Value> {
    package_json_like_value(&dir.join("package.json"))
}

fn package_json_like_value(path: &Path) -> Option<Value> {
    let json = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&json).ok()
}

fn package_json_name(dir: &Path) -> Option<String> {
    package_json_value(dir)?
        .get("name")?
        .as_str()
        .map(ToOwned::to_owned)
}

fn resolve_package_entry(package_root: &Path, subpath: &Option<String>) -> Option<PathBuf> {
    let package_json = package_json_value(package_root).unwrap_or(Value::Null);

    if let Some(exports) = package_json.get("exports") {
        if let Some(target) = export_target_for_subpath(exports, subpath.as_deref()) {
            if let Some(path) = resolve_package_target(package_root, &target) {
                return Some(path);
            }
        }
    }

    if subpath.is_none() {
        for field in ["module", "main"] {
            if let Some(target) = package_json.get(field).and_then(Value::as_str) {
                if let Some(path) = resolve_package_target(package_root, target) {
                    return Some(path);
                }
            }
        }
    }

    resolve_package_fallback(package_root, subpath.as_deref())
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

fn resolve_package_target(package_root: &Path, target: &str) -> Option<PathBuf> {
    let target = target.strip_prefix("./").unwrap_or(target);
    // Prefer source over compiled bundle when both exist: the callgraph
    // walks source files and cannot extract symbols from a built JS bundle.
    if let Some(src_relative) = target.strip_prefix("dist/") {
        if let Some(path) = resolve_file_like_path(&package_root.join("src").join(src_relative)) {
            return Some(path);
        }
    }

    resolve_file_like_path(&package_root.join(target))
}

fn resolve_package_fallback(package_root: &Path, subpath: Option<&str>) -> Option<PathBuf> {
    match subpath {
        Some(subpath) => resolve_file_like_path(&package_root.join(subpath))
            .or_else(|| resolve_file_like_path(&package_root.join("src").join(subpath))),
        None => resolve_file_like_path(&package_root.join("src").join("index"))
            .or_else(|| resolve_file_like_path(&package_root.join("index"))),
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
fn find_index_file(dir: &Path) -> Option<PathBuf> {
    for name in JS_TS_INDEX_FILES {
        let p = dir.join(name);
        if p.is_file() {
            return Some(std::fs::canonicalize(&p).unwrap_or(p));
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

    let walker = WalkBuilder::new(root)
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
            },
        );
        let file_data = FileCallData {
            calls_by_symbol: HashMap::new(),
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

    /// Create a project with a cycle: A → B → A.
    fn setup_cycle_project() -> TempDir {
        let dir = TempDir::new().unwrap();

        fs::write(
            dir.path().join("a.ts"),
            r#"import { funcB } from './b';

export function funcA() {
    return funcB();
}
"#,
        )
        .unwrap();

        fs::write(
            dir.path().join("b.ts"),
            r#"import { funcA } from './a';

export function funcB() {
    return funcA();
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

    // --- Cycle detection ---

    #[test]
    fn callgraph_cycle_detection_stops() {
        let dir = setup_cycle_project();
        let mut graph = CallGraph::new(dir.path().to_path_buf());

        // This should NOT infinite loop
        let tree = graph
            .forward_tree(&dir.path().join("a.ts"), "funcA", 10)
            .unwrap();

        assert_eq!(tree.name, "funcA");
        assert!(tree.resolved);

        // funcA calls funcB, funcB calls funcA (cycle), so the depth should be bounded
        // The tree should have children but not infinitely deep
        fn count_depth(node: &CallTreeNode) -> usize {
            if node.children.is_empty() {
                1
            } else {
                1 + node.children.iter().map(count_depth).max().unwrap_or(0)
            }
        }

        let depth = count_depth(&tree);
        assert!(
            depth <= 4,
            "Cycle should be detected and bounded, depth was: {}",
            depth
        );
    }

    // --- Depth limiting ---

    #[test]
    fn callgraph_depth_limit_truncates() {
        let dir = setup_ts_project();
        let mut graph = CallGraph::new(dir.path().to_path_buf());

        // main → helper → double, main → compute
        // With depth 1, we should see direct callees but not their children
        let tree = graph
            .forward_tree(&dir.path().join("main.ts"), "main", 1)
            .unwrap();

        assert_eq!(tree.name, "main");
        assert!(tree.depth_limited, "depth limit should be reported");
        assert!(
            tree.truncated > 0,
            "truncated edge count should be reported"
        );

        // At depth 1, children should exist (direct calls) but their children should be empty
        for child in &tree.children {
            assert!(
                child.children.is_empty(),
                "At depth 1, child '{}' should have no children, got {:?}",
                child.name,
                child.children.len()
            );
        }
    }

    #[test]
    fn callgraph_depth_zero_no_children() {
        let dir = setup_ts_project();
        let mut graph = CallGraph::new(dir.path().to_path_buf());

        let tree = graph
            .forward_tree(&dir.path().join("main.ts"), "main", 0)
            .unwrap();

        assert_eq!(tree.name, "main");
        assert!(
            tree.children.is_empty(),
            "At depth 0, should have no children"
        );
    }

    // --- Forward tree cross-file ---

    #[test]
    fn callgraph_forward_tree_cross_file() {
        let dir = setup_ts_project();
        let mut graph = CallGraph::new(dir.path().to_path_buf());

        // main → helper (in utils.ts) → double (in helpers.ts)
        let tree = graph
            .forward_tree(&dir.path().join("main.ts"), "main", 5)
            .unwrap();

        assert_eq!(tree.name, "main");
        assert!(tree.resolved);

        // Find the helper child
        let helper_child = tree.children.iter().find(|c| c.name == "helper");
        assert!(
            helper_child.is_some(),
            "main should have helper as child, children: {:?}",
            tree.children.iter().map(|c| &c.name).collect::<Vec<_>>()
        );

        let helper = helper_child.unwrap();
        assert!(
            helper.file.ends_with("utils.ts") || helper.file == "utils.ts",
            "helper should be in utils.ts, got: {}",
            helper.file
        );

        // helper should call double (in helpers.ts)
        let double_child = helper.children.iter().find(|c| c.name == "double");
        assert!(
            double_child.is_some(),
            "helper should call double, children: {:?}",
            helper.children.iter().map(|c| &c.name).collect::<Vec<_>>()
        );

        let double = double_child.unwrap();
        assert!(
            double.file.ends_with("helpers.ts") || double.file == "helpers.ts",
            "double should be in helpers.ts, got: {}",
            double.file
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
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
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
    fn callgraph_callers_of_direct() {
        let dir = setup_ts_project();
        let mut graph = CallGraph::new(dir.path().to_path_buf());

        // helpers.ts:double is called by utils.ts:helper
        let result = graph
            .callers_of(&dir.path().join("helpers.ts"), "double", 1, usize::MAX)
            .unwrap();

        assert_eq!(result.symbol, "double");
        assert!(result.total_callers > 0, "double should have callers");
        assert!(result.scanned_files > 0, "should have scanned files");

        // Find the caller from utils.ts
        let utils_group = result.callers.iter().find(|g| g.file.contains("utils.ts"));
        assert!(
            utils_group.is_some(),
            "double should be called from utils.ts, groups: {:?}",
            result.callers.iter().map(|g| &g.file).collect::<Vec<_>>()
        );

        let group = utils_group.unwrap();
        let helper_caller = group.callers.iter().find(|c| c.symbol == "helper");
        assert!(
            helper_caller.is_some(),
            "double should be called by helper, callers: {:?}",
            group.callers.iter().map(|c| &c.symbol).collect::<Vec<_>>()
        );
    }

    #[test]
    fn callgraph_callers_of_no_callers() {
        let dir = setup_ts_project();
        let mut graph = CallGraph::new(dir.path().to_path_buf());

        // main.ts:main is the entry point — nothing calls it
        let result = graph
            .callers_of(&dir.path().join("main.ts"), "main", 1, usize::MAX)
            .unwrap();

        assert_eq!(result.symbol, "main");
        assert_eq!(result.total_callers, 0, "main should have no callers");
        assert!(result.callers.is_empty());
    }

    #[test]
    fn callgraph_callers_recursive_depth() {
        let dir = setup_ts_project();
        let mut graph = CallGraph::new(dir.path().to_path_buf());

        // helpers.ts:double is called by utils.ts:helper
        // utils.ts:helper is called by main.ts:main
        // With depth=2, we should see both direct and transitive callers
        let result = graph
            .callers_of(&dir.path().join("helpers.ts"), "double", 2, usize::MAX)
            .unwrap();

        assert!(
            result.total_callers >= 2,
            "with depth 2, double should have >= 2 callers (direct + transitive), got {}",
            result.total_callers
        );

        // Should include caller from main.ts (transitive: main → helper → double)
        let main_group = result.callers.iter().find(|g| g.file.contains("main.ts"));
        assert!(
            main_group.is_some(),
            "recursive callers should include main.ts, groups: {:?}",
            result.callers.iter().map(|g| &g.file).collect::<Vec<_>>()
        );
    }

    #[test]
    fn callgraph_invalidate_file_clears_reverse_index() {
        let dir = setup_ts_project();
        let mut graph = CallGraph::new(dir.path().to_path_buf());

        // Build callers to populate the reverse index
        let _ = graph
            .callers_of(&dir.path().join("helpers.ts"), "double", 1, usize::MAX)
            .unwrap();
        assert!(
            graph.reverse_index.is_some(),
            "reverse index should be built"
        );

        // Invalidate a file
        graph.invalidate_file(&dir.path().join("utils.ts"));

        // Reverse index should be cleared
        assert!(
            graph.reverse_index.is_none(),
            "invalidate_file should clear reverse index"
        );
        // Data cache for the file should be cleared
        let canon = std::fs::canonicalize(dir.path().join("utils.ts")).unwrap();
        assert!(
            !graph.data.contains_key(&canon),
            "invalidate_file should remove file from data cache"
        );
        // Project files should be cleared
        assert!(
            graph.project_files.is_none(),
            "invalidate_file should clear project_files"
        );
    }

    // --- is_entry_point ---

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

    // --- trace_to ---

    /// Setup a multi-path project for trace_to tests.
    ///
    /// Structure:
    ///   main.ts: exported main() → processData (from utils)
    ///   service.ts: exported handleRequest() → processData (from utils)
    ///   utils.ts: exported processData() → validate (from helpers)
    ///   helpers.ts: exported validate() → checkFormat (local, not exported)
    ///   test_helpers.ts: testValidation() → validate (from helpers)
    ///
    /// checkFormat should have 3 paths:
    ///   main → processData → validate → checkFormat
    ///   handleRequest → processData → validate → checkFormat
    ///   testValidation → validate → checkFormat
    fn setup_trace_project() -> TempDir {
        let dir = TempDir::new().unwrap();

        fs::write(
            dir.path().join("main.ts"),
            r#"import { processData } from './utils';

export function main() {
    const result = processData("hello");
    return result;
}
"#,
        )
        .unwrap();

        fs::write(
            dir.path().join("service.ts"),
            r#"import { processData } from './utils';

export function handleRequest(input: string): string {
    return processData(input);
}
"#,
        )
        .unwrap();

        fs::write(
            dir.path().join("utils.ts"),
            r#"import { validate } from './helpers';

export function processData(input: string): string {
    const valid = validate(input);
    if (!valid) {
        throw new Error("invalid input");
    }
    return input.toUpperCase();
}
"#,
        )
        .unwrap();

        fs::write(
            dir.path().join("helpers.ts"),
            r#"export function validate(input: string): boolean {
    return checkFormat(input);
}

function checkFormat(input: string): boolean {
    return input.length > 0 && /^[a-zA-Z]+$/.test(input);
}
"#,
        )
        .unwrap();

        fs::write(
            dir.path().join("test_helpers.ts"),
            r#"import { validate } from './helpers';

function testValidation() {
    const result = validate("hello");
    console.log(result);
}
"#,
        )
        .unwrap();

        // git init so the walker works
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .unwrap();

        dir
    }

    #[test]
    fn trace_to_multi_path() {
        let dir = setup_trace_project();
        let mut graph = CallGraph::new(dir.path().to_path_buf());

        let result = graph
            .trace_to(
                &dir.path().join("helpers.ts"),
                "checkFormat",
                10,
                usize::MAX,
            )
            .unwrap();

        assert_eq!(result.target_symbol, "checkFormat");
        assert!(
            result.total_paths >= 2,
            "checkFormat should have at least 2 paths, got {} (paths: {:?})",
            result.total_paths,
            result
                .paths
                .iter()
                .map(|p| p.hops.iter().map(|h| h.symbol.as_str()).collect::<Vec<_>>())
                .collect::<Vec<_>>()
        );

        // Check that paths are top-down: entry point first, target last
        for path in &result.paths {
            assert!(
                path.hops.first().unwrap().is_entry_point,
                "First hop should be an entry point, got: {}",
                path.hops.first().unwrap().symbol
            );
            assert_eq!(
                path.hops.last().unwrap().symbol,
                "checkFormat",
                "Last hop should be checkFormat"
            );
        }

        // Verify entry_points_found > 0
        assert!(
            result.entry_points_found >= 2,
            "should find at least 2 entry points, got {}",
            result.entry_points_found
        );
    }

    #[test]
    fn trace_to_single_path() {
        let dir = setup_trace_project();
        let mut graph = CallGraph::new(dir.path().to_path_buf());

        // validate is called from processData, testValidation
        // processData is called from main, handleRequest
        // So validate has paths: main→processData→validate, handleRequest→processData→validate, testValidation→validate
        let result = graph
            .trace_to(&dir.path().join("helpers.ts"), "validate", 10, usize::MAX)
            .unwrap();

        assert_eq!(result.target_symbol, "validate");
        assert!(
            result.total_paths >= 2,
            "validate should have at least 2 paths, got {}",
            result.total_paths
        );
    }

    #[test]
    fn trace_to_cycle_detection() {
        let dir = setup_cycle_project();
        let mut graph = CallGraph::new(dir.path().to_path_buf());

        // funcA ↔ funcB cycle — should terminate
        let result = graph
            .trace_to(&dir.path().join("a.ts"), "funcA", 10, usize::MAX)
            .unwrap();

        // Should not hang — the fact we got here means cycle detection works
        assert_eq!(result.target_symbol, "funcA");
    }

    #[test]
    fn trace_to_depth_limit() {
        let dir = setup_trace_project();
        let mut graph = CallGraph::new(dir.path().to_path_buf());

        // With max_depth=1, should not be able to reach entry points that are 3+ hops away
        let result = graph
            .trace_to(&dir.path().join("helpers.ts"), "checkFormat", 1, usize::MAX)
            .unwrap();

        // testValidation→validate→checkFormat is 2 hops, which requires depth >= 2
        // main→processData→validate→checkFormat is 3 hops, which requires depth >= 3
        // With depth=1, most paths should be truncated
        assert_eq!(result.target_symbol, "checkFormat");

        // The shallow result should have fewer paths than the deep one
        let deep_result = graph
            .trace_to(
                &dir.path().join("helpers.ts"),
                "checkFormat",
                10,
                usize::MAX,
            )
            .unwrap();

        assert!(
            result.total_paths <= deep_result.total_paths,
            "shallow trace should find <= paths compared to deep: {} vs {}",
            result.total_paths,
            deep_result.total_paths
        );
    }

    #[test]
    fn trace_to_entry_point_target() {
        let dir = setup_trace_project();
        let mut graph = CallGraph::new(dir.path().to_path_buf());

        // main is itself an entry point — should return a single trivial path
        let result = graph
            .trace_to(&dir.path().join("main.ts"), "main", 10, usize::MAX)
            .unwrap();

        assert_eq!(result.target_symbol, "main");
        assert!(
            result.total_paths >= 1,
            "main should have at least 1 path (itself), got {}",
            result.total_paths
        );
        // Check the trivial path has just one hop
        let trivial = result.paths.iter().find(|p| p.hops.len() == 1);
        assert!(
            trivial.is_some(),
            "should have a trivial path with just the entry point itself"
        );
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
    fn unresolved_member_calls_do_not_become_same_file_callers() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("main.ts"),
            r#"function caller() {
    db.connect();
}

function connect() {}
"#,
        )
        .unwrap();

        let mut graph = CallGraph::new(dir.path().to_path_buf());
        let result = graph
            .callers_of(&dir.path().join("main.ts"), "connect", 1, usize::MAX)
            .unwrap();

        assert_eq!(
            result.total_callers, 0,
            "db.connect() must not call local connect"
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

        assert!(matches!(
            graph.resolve_symbol_query(&path, "run"),
            Err(AftError::AmbiguousSymbol { .. })
        ));
        assert_eq!(
            graph.resolve_symbol_query(&path, "A::run").unwrap(),
            "A::run"
        );
    }

    #[test]
    fn trace_to_counts_same_named_entry_points_by_file_and_symbol() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("web")).unwrap();
        fs::create_dir_all(dir.path().join("cli")).unwrap();
        fs::write(
            dir.path().join("target.ts"),
            r#"export function target() {
    leaf();
}

function leaf() {}
"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("web/main.ts"),
            r#"import { target } from '../target';

export function main() {
    target();
}
"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("cli/main.ts"),
            r#"import { target } from '../target';

export function main() {
    target();
}
"#,
        )
        .unwrap();

        let mut graph = CallGraph::new(dir.path().to_path_buf());
        let result = graph
            .trace_to(&dir.path().join("target.ts"), "leaf", 10, usize::MAX)
            .unwrap();

        assert_eq!(
            result.total_paths, 3,
            "target plus two main entry paths expected"
        );
        assert_eq!(
            result.entry_points_found, 3,
            "same-named main entry points in different files must both count"
        );
    }

    #[test]
    fn callers_and_impact_report_depth_truncation() {
        let dir = setup_ts_project();
        let mut graph = CallGraph::new(dir.path().to_path_buf());

        let callers = graph
            .callers_of(&dir.path().join("helpers.ts"), "double", 1, usize::MAX)
            .unwrap();
        assert!(
            callers.depth_limited,
            "callers should report omitted transitive callers"
        );
        assert!(
            callers.truncated > 0,
            "callers should report truncated edge count"
        );

        let impact = graph
            .impact(&dir.path().join("helpers.ts"), "double", 1, usize::MAX)
            .unwrap();
        assert!(
            impact.depth_limited,
            "impact should report omitted transitive callers"
        );
        assert!(
            impact.truncated > 0,
            "impact should report truncated edge count"
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
