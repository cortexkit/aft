//! Call graph engine: cross-file call resolution and forward traversal.
//!
//! Builds a lazy, worktree-scoped call graph that resolves calls across files
//! using import chains. Supports depth-limited forward traversal with cycle
//! detection.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, OnceLock, RwLock};

use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Serialize;
use serde_json::Value;
use tree_sitter::Node;

use crate::callgraph_store::disk_facts::DiskFacts;
use crate::callgraph_store::facts::{byte_path, EntryKind, FactPaths};
use crate::callgraph_store::join::syntax_index::{Direct, Nav, SyntaxIndex};
#[cfg(any(test, feature = "test-support"))]
use crate::calls::{call_node_kinds, extract_callee_name, extract_full_callee};
use crate::calls::{extract_calls_full_with, extract_rust_value_references_with};
use crate::error::AftError;
use crate::imports::{self, ImportBlock};
use crate::parser::{detect_language, detect_language_from_source, LangId};
use crate::symbols::{Range, Symbol, SymbolKind};

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

type WorkspacePackageCache = HashMap<(PathBuf, String), Option<PathBuf>>;
// A cell per canonical root coalesces simultaneous parse-pool misses without
// holding the process-wide map lock while traversing unrelated workspaces.
type WorkspaceMemberDirsCache = HashMap<PathBuf, Arc<OnceLock<Arc<Vec<PathBuf>>>>>;
type RustCrateInfoCache = HashMap<PathBuf, Option<RustCrateInfo>>;
type RustWorkspaceCrateCache = HashMap<PathBuf, HashMap<String, RustCrateInfo>>;

static WORKSPACE_PACKAGE_CACHE: LazyLock<RwLock<WorkspacePackageCache>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));
static WORKSPACE_MEMBER_DIRS_CACHE: LazyLock<RwLock<WorkspaceMemberDirsCache>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));
static RUST_CRATE_INFO_CACHE: LazyLock<RwLock<RustCrateInfoCache>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));
static RUST_WORKSPACE_CRATE_CACHE: LazyLock<RwLock<RustWorkspaceCrateCache>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

// Cold builds retain one resolver memo across every staged reference window. The
// entry and retained-weight ceilings keep that speedup inside the bounded-build
// working-set contract: at most 9 MiB of estimated keys/values are retained here,
// plus 4 MiB for canonical probe identities owned by the same snapshot.
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

trait ModuleKeyParts {
    fn parts(&self) -> (&Path, &str);
}

impl ModuleKeyParts for ModuleResolutionKey {
    fn parts(&self) -> (&Path, &str) {
        (&self.0, &self.1)
    }
}

impl ModuleKeyParts for (&Path, &str) {
    fn parts(&self) -> (&Path, &str) {
        *self
    }
}

// Owned and borrowed compound keys must hash the same path/string pair. The
// borrowed trait object lets HashMap probe without constructing either owner.
impl std::hash::Hash for dyn ModuleKeyParts + '_ {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::hash::Hash::hash(&self.parts(), state);
    }
}

impl PartialEq for dyn ModuleKeyParts + '_ {
    fn eq(&self, other: &Self) -> bool {
        self.parts() == other.parts()
    }
}

impl Eq for dyn ModuleKeyParts + '_ {}

impl<'a> std::borrow::Borrow<dyn ModuleKeyParts + 'a> for ModuleResolutionKey {
    fn borrow(&self) -> &(dyn ModuleKeyParts + 'a) {
        self
    }
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    pub static OWNED_MODULE_KEY_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn owned_module_key(from_dir: &Path, module_path: &str) -> ModuleResolutionKey {
    #[cfg(any(test, feature = "test-support"))]
    OWNED_MODULE_KEY_COUNT.with(|count| count.set(count.get() + 1));
    (from_dir.to_path_buf(), module_path.to_string())
}
type WorkspacePackageKey = (PathBuf, String);
type RustDeclaredModuleMap = HashMap<String, Option<String>>;

#[derive(Clone, Debug)]
struct RustCrateTargets {
    lib_root: Option<PathBuf>,
    target_roots: Vec<PathBuf>,
}

/// Interior-mutable memo storage. A lock rather than a `RefCell` lets one memo
/// be shared by the cold build's parse pool; each access holds it only for a
/// single lookup or insert, never across the work that computes a value.
#[derive(Default)]
struct MemoCell<T>(std::sync::Mutex<T>);

impl<T> MemoCell<T> {
    fn new(value: T) -> Self {
        Self(std::sync::Mutex::new(value))
    }

    fn borrow(&self) -> std::sync::MutexGuard<'_, T> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn borrow_mut(&self) -> std::sync::MutexGuard<'_, T> {
        self.borrow()
    }
}

#[derive(Clone, Default)]
pub struct RustCrateRootMemo {
    caller_roots: Arc<MemoCell<HashMap<PathBuf, Option<PathBuf>>>>,
    crate_targets: Arc<MemoCell<HashMap<PathBuf, Option<RustCrateTargets>>>>,
}

test_support_visibility! {
struct BoundedMemo<K, V> {
    #[cfg(any(test, feature = "test-support"))]
    pub entries: HashMap<K, (V, usize)>,
    #[cfg(not(any(test, feature = "test-support")))]
    entries: HashMap<K, (V, usize)>,
    order: std::collections::VecDeque<K>,
    #[cfg(any(test, feature = "test-support"))]
    pub retained_weight: usize,
    #[cfg(not(any(test, feature = "test-support")))]
    retained_weight: usize,
    max_entries: usize,
    max_retained_weight: usize,
}
}

test_support_visibility! {
/// Heap bytes of a memo key, charged once more for the copy the eviction
/// order keeps alongside the map entry.
trait MemoKeyWeight {
    fn key_weight(&self) -> usize;
}
}

impl MemoKeyWeight for (PathBuf, String) {
    fn key_weight(&self) -> usize {
        path_retained_weight(&self.0) + self.1.len()
    }
}

impl MemoKeyWeight for PathBuf {
    fn key_weight(&self) -> usize {
        path_retained_weight(self)
    }
}

impl MemoKeyWeight for String {
    fn key_weight(&self) -> usize {
        self.len()
    }
}

impl<K: Eq + std::hash::Hash + Clone + MemoKeyWeight, V> BoundedMemo<K, V> {
    test_support_visibility! {
    fn new(max_entries: usize, max_retained_weight: usize) -> Self {
            Self {
                entries: HashMap::new(),
                order: std::collections::VecDeque::new(),
                retained_weight: 0,
                max_entries,
                max_retained_weight,
            }
        }
    }

    test_support_visibility! {
    fn get<Q>(&self, key: &Q) -> Option<&V>
        where
            K: std::borrow::Borrow<Q>,
            Q: Eq + std::hash::Hash + ?Sized,
        {
            self.entries.get(key).map(|(value, _)| value)
        }
    }

    test_support_visibility! {
    fn insert(&mut self, key: K, value: V, retained_weight: usize) {
            // The eviction order owns a second copy of the key, so charge its bytes
            // too; the retained-byte ceiling then still covers everything held.
            let retained_weight = retained_weight.saturating_add(key.key_weight());
            if self.entries.contains_key(&key)
                || self.max_entries == 0
                || retained_weight > self.max_retained_weight
            {
                return;
            }
            // FIFO eviction keeps later resolution windows cacheable without
            // relaxing either bound or flushing unrelated entries together.
            while self.entries.len() >= self.max_entries
                || self.retained_weight.saturating_add(retained_weight) > self.max_retained_weight
            {
                let oldest = self.order.pop_front().expect("full memo has an entry");
                let (_, weight) = self.entries.remove(&oldest).expect("ordered memo entry");
                self.retained_weight -= weight;
            }
            self.retained_weight += retained_weight;
            self.order.push_back(key.clone());
            self.entries.insert(key, (value, retained_weight));
        }

    }
}

/// Snapshot-scoped filesystem memo used by the staged cold-build resolver.
///
/// The cold build reads one fixed corpus, so no watcher invalidation is needed:
/// the memo is dropped when publication completes or the build aborts.
pub struct ModuleResolutionMemo {
    enabled: bool,
    module_paths: MemoCell<BoundedMemo<ModuleResolutionKey, Option<PathBuf>>>,
    json_values: MemoCell<BoundedMemo<PathBuf, Option<Arc<Value>>>>,
    workspace_packages: MemoCell<BoundedMemo<WorkspacePackageKey, Option<PathBuf>>>,
    rust_declared_modules: MemoCell<BoundedMemo<String, Arc<RustDeclaredModuleMap>>>,
    rust_crate_roots: RustCrateRootMemo,
    #[cfg(any(test, feature = "test-support"))]
    collect_metrics: bool,
    #[cfg(any(test, feature = "test-support"))]
    module_computations: MemoCell<HashMap<ModuleResolutionKey, usize>>,
    #[cfg(any(test, feature = "test-support"))]
    json_probes: MemoCell<HashMap<PathBuf, usize>>,
    #[cfg(any(test, feature = "test-support"))]
    rust_declaration_parses: MemoCell<HashMap<String, usize>>,
}

impl Default for ModuleResolutionMemo {
    fn default() -> Self {
        Self {
            enabled: true,
            module_paths: MemoCell::new(BoundedMemo::new(
                MODULE_RESOLUTION_MEMO_MAX_ENTRIES,
                MODULE_RESOLUTION_MEMO_MAX_RETAINED_BYTES,
            )),
            json_values: MemoCell::new(BoundedMemo::new(
                JSON_VALUE_MEMO_MAX_ENTRIES,
                JSON_VALUE_MEMO_MAX_RETAINED_BYTES,
            )),
            workspace_packages: MemoCell::new(BoundedMemo::new(
                WORKSPACE_PACKAGE_MEMO_MAX_ENTRIES,
                WORKSPACE_PACKAGE_MEMO_MAX_RETAINED_BYTES,
            )),
            rust_declared_modules: MemoCell::new(BoundedMemo::new(
                RUST_DECLARED_MODULE_MEMO_MAX_ENTRIES,
                RUST_DECLARED_MODULE_MEMO_MAX_RETAINED_BYTES,
            )),
            rust_crate_roots: RustCrateRootMemo::default(),
            #[cfg(any(test, feature = "test-support"))]
            collect_metrics: false,
            #[cfg(any(test, feature = "test-support"))]
            module_computations: MemoCell::new(HashMap::new()),
            #[cfg(any(test, feature = "test-support"))]
            json_probes: MemoCell::new(HashMap::new()),
            #[cfg(any(test, feature = "test-support"))]
            rust_declaration_parses: MemoCell::new(HashMap::new()),
        }
    }
}

impl ModuleResolutionMemo {
    test_support_visibility! {
    fn resolve_module_path(
            &self,
            from_dir: &Path,
            module_path: &str,
            facts: &FactPaths<'_>,
        ) -> Option<PathBuf> {
            if self.enabled {
                let borrowed = (from_dir, module_path);
                if let Some(cached) = self
                    .module_paths
                    .borrow()
                    .get(&borrowed as &dyn ModuleKeyParts)
                {
                    facts.facts.memo_replay(from_dir, "module", module_path);
                    return cached.clone();
                }
            }

            let key = owned_module_key(from_dir, module_path);
            self.note_module_computation(&key);
            facts.facts.memo_start(from_dir, "module", module_path);
            let resolved = resolve_module_path_uncached(from_dir, module_path, Some(self), facts);
            facts.facts.memo_finish(from_dir, "module", module_path);
            if self.enabled {
                let retained_weight = module_resolution_entry_weight(&key, resolved.as_deref());
                self.module_paths
                    .borrow_mut()
                    .insert(key, resolved.clone(), retained_weight);
            }
            resolved
        }
    }

    fn json_value(&self, path: &Path, facts: &FactPaths<'_>) -> Option<Arc<Value>> {
        if self.enabled {
            if let Some(cached) = self.json_values.borrow().get(path) {
                return cached.clone();
            }
        }

        self.note_json_probe(path);
        let parsed = facts
            .attributed_bytes(path)
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

    pub fn rust_crate_root_file(
        &self,
        project_root: &Path,
        caller_file: &Path,
        facts: &FactPaths<'_>,
    ) -> Option<PathBuf> {
        self.rust_crate_roots
            .root_file(project_root, caller_file, facts)
    }

    pub fn rust_declared_module_target(
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

    #[cfg(any(test, feature = "test-support"))]
    fn note_module_computation(&self, key: &ModuleResolutionKey) {
        if self.collect_metrics {
            *self
                .module_computations
                .borrow_mut()
                .entry(key.clone())
                .or_default() += 1;
        }
    }

    #[cfg(not(any(test, feature = "test-support")))]
    fn note_module_computation(&self, _key: &ModuleResolutionKey) {}

    #[cfg(any(test, feature = "test-support"))]
    fn note_json_probe(&self, path: &Path) {
        if self.collect_metrics {
            *self
                .json_probes
                .borrow_mut()
                .entry(path.to_path_buf())
                .or_default() += 1;
        }
    }

    #[cfg(not(any(test, feature = "test-support")))]
    fn note_json_probe(&self, _path: &Path) {}

    #[cfg(any(test, feature = "test-support"))]
    fn note_rust_declaration_parse(&self, caller_file: &str) {
        if self.collect_metrics {
            *self
                .rust_declaration_parses
                .borrow_mut()
                .entry(caller_file.to_string())
                .or_default() += 1;
        }
    }

    #[cfg(not(any(test, feature = "test-support")))]
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

test_support_visibility! {
const TOP_LEVEL_SYMBOL: &str = "<top-level>";
}
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

test_support_visibility! {
fn symbol_identity(symbol: &Symbol) -> String {
    if symbol.scope_chain.is_empty() {
        symbol.name.clone()
    } else {
        format!("{}::{}", symbol.scope_chain.join("::"), symbol.name)
    }
}
}

fn symbol_unqualified_name(symbol: &str) -> &str {
    symbol.rsplit("::").next().unwrap_or(symbol)
}

pub fn is_bare_callee(full_callee: &str, short_name: &str) -> bool {
    full_callee == short_name || (!full_callee.contains('.') && !full_callee.contains("::"))
}

/// A call or function-value dependency site owned by a symbol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallSite {
    pub kind: CallSiteKind,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CallSiteKind {
    #[default]
    Call,
    Construction,
    CallbackReference,
}

/// Per-symbol metadata for entry point detection (avoids re-parsing).
#[derive(Debug, Clone, Serialize)]
pub struct SymbolMeta {
    /// None means extraction could not establish whether the item is test code.
    pub is_test: Option<bool>,
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
    /// Whether the program starts here, as the symbol's language defines
    /// it (see [`starts_program`]). For Rust this is only half the answer:
    /// the file must also be the root of a binary target, which depends on
    /// its path and Cargo manifest, so readers that know those check it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub program_start: bool,
}

/// Whether the symbol `identity` of `kind` is where a program in `lang`
/// starts, as the language defines it: `fn main` at the top of a Rust file,
/// `func main` in a Go `package main` (`go_main_package`), and `main` in C,
/// C++ and Objective-C. Go also starts every package's top-level
/// `func init()` (`go_init`: the file declares one with no parameters)
/// before `main`; the runtime calls it and code never can. A Python program
/// starts in the module code run as `__main__`, which
/// [`python_runs_as_main`] finds. JavaScript and TypeScript define no
/// starting function.
fn starts_program(
    lang: LangId,
    identity: &str,
    kind: &SymbolKind,
    go_main_package: bool,
    go_init: bool,
) -> bool {
    if *kind != SymbolKind::Function {
        return false;
    }
    match lang {
        LangId::Rust | LangId::C | LangId::Cpp | LangId::ObjC => identity == "main",
        LangId::Go => (identity == "main" && go_main_package) || (identity == "init" && go_init),
        _ => false,
    }
}

/// Whether a Go file declares a top-level `func init()`: a function, so no
/// receiver, with no parameters. Go runs each one when the program starts.
fn go_declares_init(source: &str, root: tree_sitter::Node<'_>) -> bool {
    root.named_children(&mut root.walk()).any(|node| {
        node.kind() == "function_declaration"
            && node
                .child_by_field_name("name")
                .is_some_and(|name| &source[name.byte_range()] == "init")
            && node
                .child_by_field_name("parameters")
                .is_some_and(|parameters| parameters.named_child_count() == 0)
    })
}

/// Whether a Python module has a top-level `if __name__ == "__main__":`
/// block: the code Python runs only when the module is the program itself.
/// The module's top-level code then starts the program.
fn python_runs_as_main(source: &str, root: tree_sitter::Node<'_>) -> bool {
    root.named_children(&mut root.walk()).any(|node| {
        node.kind() == "if_statement"
            && node
                .child_by_field_name("condition")
                .is_some_and(|condition| {
                    let text = source[condition.byte_range()]
                        .chars()
                        .filter(|ch| !ch.is_whitespace())
                        .map(|ch| if ch == '\'' { '"' } else { ch })
                        .collect::<String>();
                    text == "__name__==\"__main__\"" || text == "\"__main__\"==__name__"
                })
    })
}

/// Per-file call data: call sites grouped by containing symbol, plus
/// exported symbol names and parsed imports.
#[derive(Debug, Clone)]
pub struct FileCallData {
    /// Map from symbol name → list of call sites within that symbol's body.
    pub calls_by_symbol: HashMap<String, Vec<CallSite>>,
    /// Rust function items referenced as values, grouped by containing symbol.
    /// Callable targets participate in reverse references, not execution trees.
    pub value_refs_by_symbol: HashMap<String, Vec<CallSite>>,
    /// Calls written in a `macro_rules!` template outside any item defined in
    /// that template, keyed by macro name with its `!` (e.g. `check!`). They
    /// run wherever the macro is invoked, so `callers` reports the macro's
    /// invocation sites for them instead of a caller in this file.
    pub macro_body_calls_by_macro: HashMap<String, Vec<CallSite>>,
    /// Identifiers inside Rust macro token trees that could not be parsed as
    /// Rust, grouped by containing symbol. `callers` counts these as mentions
    /// it could not analyze, so a target named only there is not reported as
    /// having no callers without comment.
    pub macro_mentions_by_symbol: HashMap<String, Vec<CallSite>>,
    /// Names of exported symbols in this file.
    pub exported_symbols: Vec<String>,
    /// Per-symbol metadata (kind, exported, signature).
    pub symbol_metadata: HashMap<String, SymbolMeta>,
    /// Real or synthetic symbol name for this file's default export.
    pub default_export_symbol: Option<String>,
    /// Parsed import block for cross-file resolution.
    pub import_block: ImportBlock,
    /// Lexically selected Python import bindings, sorted by call-expression span.
    pub python_bindings: Vec<python::CallBinding>,
    /// Language of the file.
    pub lang: LangId,
}

pub mod python;

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

test_support_visibility! {
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedSymbol {
    file: PathBuf,
    #[cfg(any(test, feature = "test-support"))]
    pub symbol: String,
    #[cfg(not(any(test, feature = "test-support")))]
    symbol: String,
}
}

#[derive(Debug, Clone)]
struct RustCrateInfo {
    package_root: PathBuf,
    lib_name: String,
    lib_root: Option<PathBuf>,
    target_roots: Vec<PathBuf>,
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
    /// Number of omitted edges, a lower bound when the work budget is exhausted.
    pub truncated: usize,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub work_gap: Option<CallTreeWorkGap>,
}

/// The call-tree node budget as a literal, so the work-gap text below is built
/// from the same number as the budget instead of repeating it.
macro_rules! call_tree_node_budget {
    () => {
        1000
    };
}

pub const CALL_TREE_NODE_BUDGET: usize = call_tree_node_budget!();
pub const CALL_TREE_WORK_GAP: &str = concat!(
    "Call tree cut short: examined ",
    call_tree_node_budget!(),
    " call nodes (work budget); additional nodes not examined · narrow: symbol, depth"
);

#[derive(Debug, Clone, Serialize)]
pub struct CallTreeWorkGap {
    pub complete: bool,
    pub nodes_examined: usize,
    pub gap: String,
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
        | LangId::Cuda
        | LangId::Metal
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
        | LangId::ObjC
        | LangId::Toml
        | LangId::Luau
        | LangId::Nix => false,
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
pub fn build_file_data(path: &Path) -> Result<FileCallData, AftError> {
    let source =
        crate::project_content::read_to_string(path).map_err(|e| AftError::FileNotFound {
            path: format!("{}: {}", path.display(), e),
        })?;

    build_file_data_from_source(path, &source)
}

pub fn build_file_data_from_source(path: &Path, source: &str) -> Result<FileCallData, AftError> {
    let lang =
        detect_language_from_source(path, source).ok_or_else(|| AftError::InvalidRequest {
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

test_support_visibility! {
type SourceLineIndex = crate::callgraph_store::LineIndex;
}

test_support_visibility! {
fn collect_calls_by_symbol<'tree>(
    source: &str,
    root: Node<'tree>,
    lang: LangId,
    symbols: &[Symbol],
    line_index: &SourceLineIndex,
    nav: impl Nav<'tree>,
) -> HashMap<String, Vec<CallSite>> {
    attribute_sites_to_symbols(
        line_index,
        symbols,
        extract_calls_full_with(source, root, 0, source.len(), lang, nav),
        lang,
    )
}
}

/// Separate calls written directly in a `macro_rules!` template from ordinary
/// calls. A call inside an item that the template itself defines (a method in
/// a macro that stamps out a whole `impl` body) stays an ordinary call of that
/// item. Any other call in a template belongs to the macro, keyed `name!`,
/// because attributing it to the code around the definition would name a
/// caller that never runs it.
fn split_macro_body_calls(
    line_index: &SourceLineIndex,
    symbols: &[Symbol],
    raw_calls: Vec<crate::calls::CallTuple>,
    bodies: &[crate::calls::RustMacroBody],
) -> (Vec<crate::calls::CallTuple>, HashMap<String, Vec<CallSite>>) {
    let mut body_calls: HashMap<String, Vec<CallSite>> = HashMap::new();
    if bodies.is_empty() {
        return (raw_calls, body_calls);
    }
    let symbol_ranges = symbols
        .iter()
        .map(|symbol| {
            (
                line_index.byte_offset(symbol.range.start_line, symbol.range.start_col),
                line_index.byte_offset(symbol.range.end_line, symbol.range.end_col),
            )
        })
        .collect::<Vec<_>>();
    // For each template, the items it defines itself.
    let body_symbols = bodies
        .iter()
        .map(|body| {
            symbol_ranges
                .iter()
                .copied()
                .filter(|(start, end)| body.byte_start <= *start && *end <= body.byte_end)
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();

    let mut ordinary = Vec::with_capacity(raw_calls.len());
    for call in raw_calls {
        let (_, _, _, call_start, call_end) = call;
        // Templates can nest; the innermost one holding the call owns it.
        let innermost = bodies
            .iter()
            .enumerate()
            .filter(|(_, body)| body.byte_start <= call_start && call_end <= body.byte_end)
            .min_by_key(|(_, body)| body.byte_end - body.byte_start);
        let Some((body_index, body)) = innermost else {
            ordinary.push(call);
            continue;
        };
        let inside_defined_item = body_symbols[body_index]
            .iter()
            .any(|(start, end)| *start <= call_start && call_end <= *end);
        if inside_defined_item {
            ordinary.push(call);
            continue;
        }
        let (full, short, line, byte_start, byte_end) = call;
        body_calls
            .entry(format!("{}!", body.name))
            .or_default()
            .push(CallSite {
                kind: CallSiteKind::Call,
                callee_name: short,
                full_callee: full,
                line,
                byte_start,
                byte_end,
            });
    }
    (ordinary, body_calls)
}

fn collect_rust_value_refs_by_symbol<'tree>(
    source: &str,
    root: Node<'tree>,
    symbols: &[Symbol],
    line_index: &SourceLineIndex,
    nav: impl Nav<'tree>,
) -> HashMap<String, Vec<CallSite>> {
    let mut references = attribute_sites_to_symbols(
        line_index,
        symbols,
        extract_rust_value_references_with(source, root, nav),
        LangId::Rust,
    );
    // A function item remains a reference whether it is passed to an API or
    // stored for later use. The resolver rejects non-callable value candidates.
    for site in references.values_mut().flatten() {
        site.kind = CallSiteKind::CallbackReference;
    }
    references
}

fn rust_item_is_test<'tree>(
    source: &str,
    root: tree_sitter::Node<'tree>,
    byte: usize,
    nav: impl Nav<'tree>,
) -> Option<bool> {
    if root.has_error() {
        return None;
    }
    let mut node = root.descendant_for_byte_range(byte, byte)?;
    let mut item = node;
    while let Some(parent) = nav.parent(item) {
        if nav.kind(item) == "attribute_item" {
            while let Some(next) = nav.next_named_sibling(item) {
                item = next;
                if nav.kind(item) != "attribute_item" && !item.is_extra() {
                    break;
                }
            }
            node = item;
            break;
        }
        item = parent;
    }
    while let Some(parent) = nav.parent(node) {
        let mut sibling = nav.prev_named_sibling(node);
        while let Some(attribute) =
            sibling.filter(|n| nav.kind(*n) == "attribute_item" || n.is_extra())
        {
            let text = source[attribute.byte_range()]
                .split_whitespace()
                .collect::<String>();
            if text == "#[test]" || text == "#[cfg(test)]" {
                return Some(true);
            }
            sibling = nav.prev_named_sibling(attribute);
        }
        node = parent;
    }
    Some(false)
}

fn attribute_sites_to_symbols(
    line_index: &SourceLineIndex,
    symbols: &[Symbol],
    raw_sites: Vec<(String, String, u32, usize, usize)>,
    lang: LangId,
) -> HashMap<String, Vec<CallSite>> {
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
            kind: CallSiteKind::Call,
            callee_name: short,
            full_callee: full,
            line,
            byte_start,
            byte_end,
        };
        let mut attributed = false;
        for range_index in &active_ranges {
            let range = &ranges[*range_index];
            // Method bodies execute when the method is called, not when its class is constructed.
            if symbols[range.symbol_index].kind == SymbolKind::Class
                && active_ranges.iter().any(|index| {
                    let callable = &ranges[*index];
                    range.byte_start <= callable.byte_start
                        && callable.byte_end <= range.byte_end
                        && byte_end <= callable.byte_end
                        && matches!(
                            symbols[callable.symbol_index].kind,
                            SymbolKind::Function | SymbolKind::Method
                        )
                })
            {
                continue;
            }
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
            if lang == LangId::Go && symbol.kind == SymbolKind::Function && symbol.name == "init" {
                // Go runs every init declaration in a file. They share a
                // file-local owner, so retain all bodies rather than the last.
                calls_by_symbol
                    .entry(symbol_identity(symbol))
                    .or_insert_with(Vec::new)
                    .extend(sites);
            } else {
                calls_by_symbol.insert(symbol_identity(symbol), sites);
            }
        }
    }
    if !top_level_sites.is_empty() {
        calls_by_symbol.insert(TOP_LEVEL_SYMBOL.to_string(), top_level_sites);
    }
    calls_by_symbol
}

pub fn build_file_data_from_source_with_lang(
    path: &Path,
    source: &str,
    lang: LangId,
) -> Result<FileCallData, AftError> {
    let (tree, lang) = crate::parser::parse_resolved_source(path, source, lang)?;

    let line_index = SourceLineIndex::new(source);
    build_file_data_from_tree(path, source, lang, &tree, &line_index)
}

#[derive(Default)]
struct CallSiteSyntax {
    construction: bool,
    argument: bool,
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    pub static CALL_SITE_KIND_NODE_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn classify_call_site_kinds<'tree>(
    root: tree_sitter::Node<'tree>,
    calls: &mut HashMap<String, Vec<CallSite>>,
    value_refs: &mut HashMap<String, Vec<CallSite>>,
    nav: impl Nav<'tree>,
) {
    let mut syntax_by_span = calls
        .values()
        .chain(value_refs.values())
        .flatten()
        .map(|site| ((site.byte_start, site.byte_end), CallSiteSyntax::default()))
        .collect::<rustc_hash::FxHashMap<_, _>>();
    if syntax_by_span.is_empty() {
        return;
    }

    // Looking every range up from the root repeats long descents on nested
    // calls. One walk keeps each node's parent kind, the argument context.
    nav.walk_with_parent_kind(root, |node, kind, parent_kind| {
        #[cfg(any(test, feature = "test-support"))]
        CALL_SITE_KIND_NODE_VISITS.with(|visits| visits.set(visits.get() + 1));
        if let Some(syntax) = syntax_by_span.get_mut(&(node.start_byte(), node.end_byte())) {
            // Equal-span descendants overwrite ancestors, matching the deepest range lookup.
            syntax.construction = kind == "new_expression";
            syntax.argument = matches!(parent_kind, Some("arguments" | "argument_list"));
        }
    });
    for site in calls.values_mut().flatten() {
        if syntax_by_span[&(site.byte_start, site.byte_end)].construction {
            site.kind = CallSiteKind::Construction;
        }
    }
    for site in value_refs.values_mut().flatten() {
        if syntax_by_span[&(site.byte_start, site.byte_end)].argument {
            site.kind = CallSiteKind::CallbackReference;
        }
    }
}

/// Reuse a caller-owned syntax tree and position index during blob extraction.
pub fn build_file_data_from_tree(
    path: &Path,
    source: &str,
    lang: LangId,
    tree: &tree_sitter::Tree,
    line_index: &SourceLineIndex,
) -> Result<FileCallData, AftError> {
    build_file_data_from_tree_observed(path, source, lang, tree, line_index, false, Direct, None)
        .map(|(data, _)| data)
}

/// Like [`build_file_data_from_tree`], with `index`, the tree's syntax
/// index, answering every walk and parent or sibling lookup. With
/// `outline`, also fills it with the file's symbols as the parser extracts
/// them, before the call graph qualifies, adds or merges any.
pub fn build_file_data_from_index<'tree>(
    path: &Path,
    source: &str,
    lang: LangId,
    tree: &'tree tree_sitter::Tree,
    line_index: &SourceLineIndex,
    index: &SyntaxIndex<'tree>,
    outline: Option<&mut Vec<Symbol>>,
) -> Result<FileCallData, AftError> {
    build_file_data_from_tree_observed(path, source, lang, tree, line_index, false, index, outline)
        .map(|(data, _)| data)
}

#[allow(clippy::too_many_arguments)]
fn build_file_data_from_tree_observed<'tree>(
    path: &Path,
    source: &str,
    lang: LangId,
    tree: &'tree tree_sitter::Tree,
    line_index: &SourceLineIndex,
    collect_objects: bool,
    nav: impl Nav<'tree>,
    outline: Option<&mut Vec<Symbol>>,
) -> Result<
    (
        FileCallData,
        Option<crate::callgraph_store::join::ParseBlob>,
    ),
    AftError,
> {
    // Parse imports
    let python = (lang == LangId::Python).then(|| python::extract(source, tree.root_node()));
    let (import_block, python_bindings) = if let Some(python) = &python {
        (python.imports.clone(), python.bindings.clone())
    } else {
        (imports::parse_imports(&source, &tree, lang), Vec::new())
    };

    // Get symbols (for call site extraction and export detection)
    let mut symbols = crate::parser::extract_symbols_from_tree_with(&source, &tree, lang, nav)?;
    if let Some(outline) = outline {
        outline.clone_from(&symbols);
    }
    if lang == LangId::Luau {
        symbols.extend(
            crate::luau::exports(source, tree.root_node())
                .into_values()
                .filter_map(|export| export.inline)
                .map(|declaration| crate::parser::luau_symbol(source, declaration)),
        );
    }
    let object_nodes = crate::callgraph_store::join::dispatch::complete_typescript_symbols(
        source,
        lang,
        tree.root_node(),
        &mut symbols,
        nav,
    );
    let mut rust_qualified = HashMap::new();
    if lang == LangId::Rust {
        rust_qualified = qualify_rust_impl_methods(source, tree.root_node(), &mut symbols, nav);
        symbols.extend(rust_trait_members(source, tree.root_node(), nav));
    }

    let root = tree.root_node();
    let mut macro_body_calls_by_macro = HashMap::new();
    let mut macro_mentions_by_symbol = HashMap::new();
    let mut calls_by_symbol = if lang == LangId::Rust {
        let (raw_calls, macro_facts) = crate::calls::extract_rust_calls_with_macro_facts_with(
            &source,
            root,
            0,
            source.len(),
            nav,
        );
        let (raw_calls, body_calls) =
            split_macro_body_calls(line_index, &symbols, raw_calls, &macro_facts.bodies);
        macro_body_calls_by_macro = body_calls;
        macro_mentions_by_symbol =
            attribute_sites_to_symbols(line_index, &symbols, macro_facts.unparsed_mentions, lang);
        attribute_sites_to_symbols(line_index, &symbols, raw_calls, lang)
    } else {
        collect_calls_by_symbol(&source, root, lang, &symbols, line_index, nav)
    };
    if let Some(python) = &python {
        let mut owned = HashMap::<String, Vec<CallSite>>::new();
        let mut seen = HashSet::new();
        for site in calls_by_symbol.into_values().flatten() {
            if seen.insert((site.byte_start, site.byte_end)) {
                if let Some(owners) = python.owners.get(&site.byte_start) {
                    for owner in owners {
                        owned.entry(owner.clone()).or_default().push(site.clone());
                    }
                }
            }
        }
        for sites in owned.values_mut() {
            sites.sort_by_key(|site| (site.byte_start, site.byte_end));
        }
        calls_by_symbol = owned;
    }
    let mut value_refs_by_symbol = if lang == LangId::Rust {
        collect_rust_value_refs_by_symbol(&source, root, &symbols, line_index, nav)
    } else if lang == LangId::Go {
        attribute_sites_to_symbols(
            line_index,
            &symbols,
            crate::calls::extract_go_value_references(source, root),
            lang,
        )
    } else if matches!(lang, LangId::TypeScript | LangId::Tsx | LangId::JavaScript) {
        attribute_sites_to_symbols(
            line_index,
            &symbols,
            crate::calls::extract_js_callback_references_with(source, root, nav),
            lang,
        )
    } else {
        HashMap::new()
    };

    let default_export = find_default_export(&source, root, path, lang, nav);

    if let Some(default_export) = &default_export {
        if default_export.synthetic {
            let byte_start = default_export.node.byte_range().start;
            let byte_end = default_export.node.byte_range().end;
            let raw_calls = extract_calls_full_with(&source, root, byte_start, byte_end, lang, nav);
            let sites: Vec<CallSite> = raw_calls
                .into_iter()
                .filter(|(_, short, _, _, _)| *short != default_export.symbol)
                .map(
                    |(full, short, line, call_byte_start, call_byte_end)| CallSite {
                        kind: CallSiteKind::Call,
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

    classify_call_site_kinds(root, &mut calls_by_symbol, &mut value_refs_by_symbol, nav);

    // Collect exported symbol names
    let mut exported_symbols: Vec<String> = symbols
        .iter()
        .filter(|s| s.exported)
        .map(|s| s.name.clone())
        .collect();
    if lang == LangId::Luau {
        // A symbol shown in the outline need not belong to the returned table.
        exported_symbols = crate::luau::exports(source, root).into_keys().collect();
    }
    if let Some(default_export) = &default_export {
        if !exported_symbols
            .iter()
            .any(|name| name == &default_export.symbol)
        {
            exported_symbols.push(default_export.symbol.clone());
        }
    }

    let rust_attribute_entry_points = if lang == LangId::Rust {
        crate::parser::rust_attribute_entry_points_with(&source, root, nav)
            .into_iter()
            .map(|entry| {
                let name = rust_qualified
                    .get(&entry.scoped_name)
                    .and_then(|name| name.clone())
                    .unwrap_or(entry.scoped_name);
                (name, entry.attribute.to_string())
            })
            .collect::<HashMap<_, _>>()
    } else {
        HashMap::new()
    };

    // Every Go variable of a main package initializes with the program.
    let go_main_package = lang == LangId::Go
        && root.named_children(&mut root.walk()).any(|n| {
            n.kind() == "package_clause" && source[n.byte_range()].trim() == "package main"
        });
    let python_main = lang == LangId::Python && python_runs_as_main(source, root);
    let go_init = lang == LangId::Go && go_declares_init(source, root);
    // Build per-symbol metadata for entry point detection
    let mut symbol_metadata: HashMap<String, SymbolMeta> = symbols
        .iter()
        .map(|s| {
            let identity = symbol_identity(s);
            (
                identity.clone(),
                SymbolMeta {
                    is_test: if lang == LangId::Rust {
                        rust_item_is_test(
                            source,
                            root,
                            line_index.byte_offset(s.range.start_line, s.range.start_col),
                            nav,
                        )
                    } else {
                        Some(false)
                    },
                    kind: s.kind.clone(),
                    exported: s.exported,
                    signature: s.signature.clone(),
                    line: s.range.start_line + 1,
                    range: s.range.clone(),
                    entry_point_attribute: rust_attribute_entry_points
                        .get(&identity)
                        .cloned()
                        .or_else(|| {
                            (go_main_package && s.kind == SymbolKind::Variable)
                                .then(|| "go_package_initializer".to_string())
                        }),
                    program_start: starts_program(
                        lang,
                        &identity,
                        &s.kind,
                        go_main_package,
                        go_init,
                    ),
                },
            )
        })
        .collect();
    if let Some(default_export) = &default_export {
        symbol_metadata
            .entry(default_export.symbol.clone())
            .or_insert_with(|| SymbolMeta {
                is_test: Some(false),
                kind: default_export.kind.clone(),
                exported: true,
                signature: Some(first_line_signature(&source, &default_export.node)),
                line: default_export.node.start_position().row as u32 + 1,
                range: crate::parser::node_range(&default_export.node),
                entry_point_attribute: None,
                program_start: false,
            });
    }
    // Template calls have a real macro owner. The owner is not a root: only
    // an invocation from reachable code can make its outgoing edges live.
    if lang == LangId::Rust {
        // A later definition of the same macro name replaces an earlier one,
        // in the order of a stack walk over named children.
        nav.walk_named(root, |node, kind| {
            if kind == "macro_definition" {
                if let Some(name) = node.child_by_field_name("name") {
                    symbol_metadata.insert(
                        format!("{}!", &source[name.byte_range()]),
                        SymbolMeta {
                            is_test: rust_item_is_test(source, root, node.start_byte(), nav),
                            kind: SymbolKind::Function,
                            exported: false,
                            signature: None,
                            line: node.start_position().row as u32 + 1,
                            range: crate::parser::node_range(&node),
                            entry_point_attribute: None,
                            program_start: false,
                        },
                    );
                }
            }
            true
        });
    }
    if calls_by_symbol.contains_key(TOP_LEVEL_SYMBOL)
        || value_refs_by_symbol.contains_key(TOP_LEVEL_SYMBOL)
    {
        symbol_metadata
            .entry(TOP_LEVEL_SYMBOL.to_string())
            .or_insert(SymbolMeta {
                is_test: Some(false),
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
                program_start: python_main,
            });
    }
    if lang == LangId::Go {
        symbol_metadata.insert(
            "<go-package-init>".to_string(),
            SymbolMeta {
                is_test: Some(false),
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
                program_start: false,
            },
        );
    }

    let data = FileCallData {
        calls_by_symbol,
        value_refs_by_symbol,
        macro_body_calls_by_macro,
        macro_mentions_by_symbol,
        exported_symbols,
        symbol_metadata,
        default_export_symbol: default_export.map(|export| export.symbol),
        import_block,
        python_bindings,
        lang,
    };
    let objects = if collect_objects {
        crate::callgraph_store::join::dispatch::typescript_objects_from_data(
            source,
            lang,
            &object_nodes,
            &data,
        )
    } else {
        None
    };
    Ok((data, objects))
}

/// `name` qualified by the inline modules around `node`.
pub fn rust_module_qualified_name<'tree>(
    node: Node<'tree>,
    source: &str,
    name: &str,
    nav: impl Nav<'tree>,
) -> String {
    let mut modules = Vec::new();
    let mut parent = nav.parent(node);
    while let Some(ancestor) = parent {
        if nav.kind(ancestor) == "mod_item" {
            if let Some(name) = ancestor.child_by_field_name("name") {
                modules.push(node_text(name, source));
            }
        }
        parent = nav.parent(ancestor);
    }
    modules.reverse();
    modules.push(name.to_string());
    modules.join("::")
}

// Same-named types in separate inline modules must not share method identities.
fn qualify_rust_impl_methods<'tree>(
    source: &str,
    root: Node<'tree>,
    symbols: &mut [Symbol],
    nav: impl Nav<'tree>,
) -> HashMap<String, Option<String>> {
    let mut types = HashSet::new();
    nav.walk_named(root, |node, kind| {
        if matches!(
            kind,
            "struct_item" | "enum_item" | "trait_item" | "type_item"
        ) {
            if let Some(name) = node.child_by_field_name("name") {
                types.insert(rust_module_qualified_name(
                    node,
                    source,
                    &node_text(name, source),
                    nav,
                ));
            }
        }
        true
    });
    let mut scopes = HashMap::new();
    nav.walk_named(root, |node, kind| {
        if kind == "impl_item" {
            let declared_here = ["type", "trait"]
                .iter()
                .filter_map(|field| node.child_by_field_name(field))
                .any(|ty| {
                    let name = node_text(ty, source);
                    let base = name.split('<').next().unwrap_or(&name);
                    types.contains(&rust_module_qualified_name(node, source, base, nav))
                });
            if !declared_here {
                // Nothing under an impl of a type declared elsewhere is
                // qualified, nested impls included.
                return false;
            }
            if let Some(body) = node.child_by_field_name("body") {
                for method in body
                    .named_children(&mut body.walk())
                    .filter(|n| n.kind() == "function_item")
                {
                    if let Some(name) = method.child_by_field_name("name") {
                        let prefix = rust_module_qualified_name(node, source, "", nav);
                        scopes.insert(
                            (
                                method.end_position().row as u32,
                                method.end_position().column as u32,
                                node_text(name, source),
                            ),
                            prefix,
                        );
                    }
                }
            }
        }
        true
    });
    let mut translated = HashMap::new();
    for symbol in symbols.iter_mut().filter(|s| s.kind == SymbolKind::Method) {
        if let Some(prefix) = scopes.get(&(
            symbol.range.end_line,
            symbol.range.end_col,
            symbol.name.clone(),
        )) {
            if !prefix.is_empty() {
                let old = symbol_identity(symbol);
                let mut scope = prefix
                    .trim_end_matches("::")
                    .split("::")
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                scope.extend(symbol.scope_chain.clone());
                symbol.scope_chain = scope;
                let new = symbol_identity(symbol);
                translated
                    .entry(old)
                    .and_modify(|name| *name = None)
                    .or_insert(Some(new));
            }
        }
    }
    translated
}

pub fn rust_trait_members<'tree>(
    source: &str,
    root: Node<'tree>,
    nav: impl Nav<'tree>,
) -> Vec<Symbol> {
    let mut symbols = Vec::new();
    nav.walk_named(root, |node, kind| {
        if kind == "trait_item" {
            if let (Some(name), Some(body)) = (
                node.child_by_field_name("name"),
                node.child_by_field_name("body"),
            ) {
                let name = node_text(name, source);
                let mut scope = Vec::new();
                let mut parent = nav.parent(node);
                while let Some(ancestor) = parent {
                    if nav.kind(ancestor) == "mod_item" {
                        if let Some(module) = ancestor.child_by_field_name("name") {
                            scope.push(node_text(module, source));
                        }
                    }
                    parent = nav.parent(ancestor);
                }
                scope.reverse();
                scope.push(name.clone());
                for method in body.named_children(&mut body.walk()) {
                    if !matches!(method.kind(), "function_item" | "function_signature_item") {
                        continue;
                    }
                    let Some(method_name) = method.child_by_field_name("name") else {
                        continue;
                    };
                    let end = method
                        .child_by_field_name("body")
                        .map_or(method.end_byte(), |body| body.start_byte());
                    symbols.push(Symbol {
                        name: node_text(method_name, source),
                        kind: SymbolKind::Method,
                        range: crate::symbols::Range {
                            start_line: method.start_position().row as u32,
                            start_col: method.start_position().column as u32,
                            end_line: method.end_position().row as u32,
                            end_col: method.end_position().column as u32,
                        },
                        signature: Some(
                            source[method.start_byte()..end]
                                .trim()
                                .trim_end_matches(';')
                                .to_string(),
                        ),
                        scope_chain: scope.clone(),
                        // Members are reached through the trait, not imported from the module.
                        exported: false,
                        parent: Some(name.clone()),
                    });
                }
            }
        }
        true
    });
    symbols
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
    nav: impl Nav<'tree>,
) -> Option<DefaultExport<'tree>> {
    if !matches!(lang, LangId::TypeScript | LangId::Tsx | LangId::JavaScript) {
        return None;
    }
    // The first export statement in preorder that names a default export.
    let mut found = None;
    nav.walk(root, |node, kind| {
        if found.is_some() {
            return false;
        }
        if kind == "export_statement" {
            found = default_export_from_statement(source, node, path);
        }
        found.is_none()
    });
    found
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

#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, Clone)]
pub struct CallSiteWithRange {
    pub full: String,
    pub short: String,
    pub line: u32,
    pub byte_start: usize,
    pub byte_end: usize,
}

#[cfg(any(test, feature = "test-support"))]
pub fn collect_calls_full_with_ranges(
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

#[cfg(any(test, feature = "test-support"))]
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
pub fn resolve_module_path(from_dir: &Path, module_path: &str) -> Option<PathBuf> {
    let disk = DiskFacts::new(from_dir.ancestors().last().unwrap_or(from_dir));
    let facts = &FactPaths {
        root: &disk.project_root,
        facts: &disk,
    };
    resolve_module_path_uncached(from_dir, module_path, None, facts)
}

pub fn resolve_module_path_with_memo(
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
            let mut target_segments = rust_module_segments_for_file(&base, caller_file)?;
            target_segments.extend(segments[1..].iter().cloned());
            resolve_rust_module_segments(&base, &target_segments)
        }
        "super" => {
            let crate_root = find_rust_crate_root(caller_file)?;
            let crate_info = rust_crate_info(&crate_root)?;
            let base = rust_module_base_for_caller(&crate_info, caller_file)?;
            let mut target_segments = rust_module_segments_for_file(&base, caller_file)?;
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

/// Reuse the resolver's use-tree expansion when a caller only needs the names
/// introduced into scope, without resolving or reading imported modules.
pub fn rust_import_local_names(imp: &imports::ImportStatement) -> Vec<String> {
    rust_use_entries(imp)
        .into_iter()
        .map(|entry| entry.local_name)
        .collect()
}

/// The local name and full path of each explicit Rust use-tree leaf.
pub fn rust_import_paths(raw: &str) -> Vec<(String, String)> {
    let mut entries = Vec::new();
    if let Some(body) = rust_use_body(raw) {
        expand_rust_use_tree(body.trim_start_matches("::"), &mut entries);
    }
    entries
        .into_iter()
        .filter_map(|entry| match entry.kind {
            RustUseKind::Module => Some((entry.local_name, entry.module_path)),
            RustUseKind::Item { .. } => None,
        })
        .collect()
}

fn rust_use_body(raw: &str) -> Option<&str> {
    let use_pos = raw.find("use ")?;
    let body = raw[use_pos + 4..].trim();
    let body = body.strip_suffix(';').unwrap_or(body).trim();
    (!body.is_empty()).then_some(body)
}

fn expand_rust_use_tree(path: &str, entries: &mut Vec<RustUseEntry>) {
    enum Work {
        Expand(String),
        ModuleSelf(String),
    }
    // Use trees can nest beyond the file walker's stack. Keep both subtrees
    // and `self` entries on the heap, in reverse order, so popping them emits
    // exactly the depth-first order stored by the recursive expansion.
    let mut pending = vec![Work::Expand(path.to_string())];
    while let Some(work) = pending.pop() {
        let path = match work {
            Work::ModuleSelf(prefix) => {
                if let Some(local_name) = rust_last_path_segment(&prefix) {
                    entries.push(RustUseEntry {
                        module_path: prefix,
                        local_name,
                        kind: RustUseKind::Module,
                    });
                }
                continue;
            }
            Work::Expand(path) => path,
        };
        let path = path.trim();
        if path.is_empty() {
            continue;
        }
        if let Some((prefix, inner)) = split_rust_use_braces(path) {
            let prefix = prefix.trim().trim_end_matches("::").trim();
            for part in split_top_level_commas(inner).into_iter().rev() {
                let part = part.trim();
                if part.is_empty() {
                    continue;
                }
                if part == "self" {
                    pending.push(Work::ModuleSelf(prefix.to_string()));
                } else {
                    pending.push(Work::Expand(if prefix.is_empty() {
                        part.to_string()
                    } else {
                        format!("{prefix}::{part}")
                    }));
                }
            }
        } else {
            add_rust_use_leaf(path, entries);
        }
    }
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

    Some(RustCrateInfo {
        package_root: canonicalize_path(crate_root),
        lib_name,
        lib_root,
        target_roots: rust_target_roots(
            crate_root,
            &cargo,
            &FactPaths {
                root: crate_root,
                facts: &DiskFacts::new(crate_root),
            },
        ),
    })
}

fn rust_manifest_value(path: &Path) -> Option<toml::Value> {
    let source = crate::project_content::read_to_string(path).ok()?;
    toml::from_str(&source).ok()
}

fn rust_target_roots(
    crate_root: &Path,
    cargo: &toml::Value,
    facts: &FactPaths<'_>,
) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for (table, directory, auto_key) in [
        ("bin", "src/bin", "autobins"),
        ("example", "examples", "autoexamples"),
        ("test", "tests", "autotests"),
        ("bench", "benches", "autobenches"),
    ] {
        if let Some(targets) = cargo.get(table).and_then(toml::Value::as_array) {
            for path in targets.iter().filter_map(|target| {
                target
                    .get("path")
                    .and_then(toml::Value::as_str)
                    .map(|path| crate_root.join(path))
            }) {
                if facts.is_file(&path) {
                    roots.push(facts.canonical(&path).unwrap_or(path));
                }
            }
        }
        let enabled = cargo
            .get("package")
            .and_then(|package| package.get(auto_key))
            .and_then(toml::Value::as_bool)
            .unwrap_or(true);
        if !enabled {
            continue;
        }
        let directory = crate_root.join(directory);
        for entry in facts.list_dir(&directory) {
            let path = directory.join(byte_path(&entry.name));
            let root = match entry.kind {
                EntryKind::Regular
                    if path.extension().and_then(|ext| ext.to_str()) == Some("rs") =>
                {
                    Some(path)
                }
                EntryKind::Directory => Some(path.join("main.rs")),
                _ => None,
            };
            if let Some(root) = root.filter(|root| facts.is_file(root)) {
                roots.push(facts.canonical(&root).unwrap_or(root));
            }
        }
    }
    if cargo
        .get("package")
        .and_then(|package| package.get("autobins"))
        .and_then(toml::Value::as_bool)
        .unwrap_or(true)
    {
        let main = crate_root.join("src/main.rs");
        if facts.is_file(&main) {
            roots.push(facts.canonical(&main).unwrap_or(main));
        }
    }
    roots.sort();
    roots.dedup();
    roots
}

impl RustCrateRootMemo {
    pub fn root_file(
        &self,
        project_root: &Path,
        caller_file: &Path,
        facts: &FactPaths<'_>,
    ) -> Option<PathBuf> {
        let caller = facts
            .canonical(caller_file)
            .unwrap_or_else(|| caller_file.to_path_buf());
        if let Some(cached) = self.caller_roots.borrow().get(&caller).cloned() {
            facts.facts.memo_replay(&caller, "rust-crate-root", "");
            return cached;
        }

        facts.facts.memo_start(&caller, "rust-crate-root", "");
        let resolved = self.resolve_root_file(project_root, &caller, facts);
        facts.facts.memo_finish(&caller, "rust-crate-root", "");
        self.caller_roots
            .borrow_mut()
            .insert(caller, resolved.clone());
        resolved
    }

    fn resolve_root_file(
        &self,
        project_root: &Path,
        caller: &Path,
        facts: &FactPaths<'_>,
    ) -> Option<PathBuf> {
        let mut current = caller.parent();
        let crate_root = loop {
            let dir = current?;
            if facts.is_file(&dir.join("Cargo.toml")) {
                break dir.to_path_buf();
            }
            if dir == project_root {
                return None;
            }
            current = dir.parent();
        };
        let cached_targets = { self.crate_targets.borrow().get(&crate_root).cloned() };
        let targets = if let Some(cached) = cached_targets {
            facts
                .facts
                .memo_replay(&crate_root, "rust-target-roots", "");
            cached
        } else {
            facts.facts.memo_start(&crate_root, "rust-target-roots", "");
            let resolved = read_rust_crate_targets(&crate_root, facts);
            facts
                .facts
                .memo_finish(&crate_root, "rust-target-roots", "");
            self.crate_targets
                .borrow_mut()
                .insert(crate_root.clone(), resolved.clone());
            resolved
        }?;
        if targets.lib_root.as_ref() == Some(&caller.to_path_buf()) {
            return targets.lib_root;
        }
        targets
            .target_roots
            .into_iter()
            .filter(|root| {
                let default_main = *root == crate_root.join("src/main.rs");
                root.parent()
                    .is_some_and(|dir| caller == root || (!default_main && caller.starts_with(dir)))
            })
            .max_by_key(|root| {
                (
                    caller == root,
                    root.parent()
                        .map(|dir| dir.components().count())
                        .unwrap_or(0),
                )
            })
            .or(targets.lib_root)
    }
}

fn read_rust_crate_targets(crate_root: &Path, facts: &FactPaths<'_>) -> Option<RustCrateTargets> {
    let manifest = crate_root.join("Cargo.toml");
    // A crate's targets come from `[lib] path`, the target tables and their
    // auto-discovery switches, so a change to any of them changes this
    // answer. Without these fields the read named no field, and only another
    // reader's unrelated field on the same file kept it from counting as
    // reading the whole file.
    facts.config_fact(&manifest, "disk.lib.path");
    facts.config_fact(&manifest, "disk.targets");
    let bytes = facts.attributed_bytes(&manifest)?;
    let cargo: toml::Value = toml::from_str(std::str::from_utf8(&bytes).ok()?).ok()?;
    let lib = cargo
        .get("lib")
        .and_then(|lib| lib.get("path"))
        .and_then(toml::Value::as_str)
        .map(|path| crate_root.join(path))
        .unwrap_or_else(|| crate_root.join("src/lib.rs"));
    let lib_root = facts
        .is_file(&lib)
        .then(|| facts.canonical(&lib).unwrap_or(lib));
    Some(RustCrateTargets {
        lib_root,
        target_roots: rust_target_roots(crate_root, &cargo, facts),
    })
}

pub fn rust_crate_root_file_for_caller(
    project_root: &Path,
    caller_file: &Path,
    facts: &FactPaths<'_>,
    memo: &RustCrateRootMemo,
) -> Option<PathBuf> {
    memo.root_file(project_root, caller_file, facts)
}

fn rust_module_base_for_caller(
    crate_info: &RustCrateInfo,
    caller_file: &Path,
) -> Option<RustModuleBase> {
    let caller = canonicalize_path(caller_file);
    if crate_info.lib_root.as_ref() == Some(&caller) {
        return rust_lib_module_base(crate_info);
    }
    crate_info
        .target_roots
        .iter()
        .filter_map(|root_file| {
            let src_dir = root_file.parent()?;
            let default_main = *root_file == crate_info.package_root.join("src/main.rs");
            (caller == *root_file || (!default_main && caller.starts_with(src_dir))).then(|| {
                RustModuleBase {
                    src_dir: src_dir.to_path_buf(),
                    root_file: root_file.clone(),
                }
            })
        })
        .max_by_key(|base| (caller == base.root_file, base.src_dir.components().count()))
        .or_else(|| rust_lib_module_base(crate_info))
}

fn rust_lib_module_base(crate_info: &RustCrateInfo) -> Option<RustModuleBase> {
    let root_file = crate_info.lib_root.clone()?;
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

fn rust_module_segments_for_file(base: &RustModuleBase, file: &Path) -> Option<Vec<String>> {
    let src_dir = canonicalize_path(&base.src_dir);
    let file = canonicalize_path(file);
    if file == canonicalize_path(&base.root_file) {
        return Some(Vec::new());
    }
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

test_support_visibility! {
fn canonicalize_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}
}

fn resolve_tsconfig_path(
    from_dir: &Path,
    module_path: &str,
    memo: Option<&ModuleResolutionMemo>,
    facts: &FactPaths<'_>,
) -> Option<PathBuf> {
    let tsconfig_dir = find_tsconfig_dir(from_dir, facts)?;
    let tsconfig = package_json_like_value(&tsconfig_dir.join("tsconfig.json"), memo, facts);
    facts.config_field(&tsconfig_dir, "tsconfig.json", "compilerOptions.paths");
    let tsconfig = tsconfig?;
    let compiler_options = tsconfig.get("compilerOptions")?;
    let paths = compiler_options.get("paths")?.as_object()?;
    facts.config_field(&tsconfig_dir, "tsconfig.json", "baseUrl");
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
    facts.config_field(dir, "package.json", "workspaces");
    package_json_value(dir, memo, facts)
        .map(|value| !workspace_patterns(&value).is_empty())
        .unwrap_or(false)
        || !pnpm_workspace_patterns(dir, facts).is_empty()
}

/// Drop the workspace and crate caches for one project root only. The caches
/// are process-wide and the daemon serves dozens of roots that bind about ten
/// times a minute; clearing everything on each configure gave the caches a
/// lifetime of seconds, so every Tier-2 rescan re-walked every workspace under
/// every root. A configure is a signal about its own root's manifests, and
/// the keys are canonical paths, so containment is the right scope.
pub fn clear_workspace_package_cache_under(root: &Path) {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    if let Ok(mut cache) = WORKSPACE_PACKAGE_CACHE.write() {
        cache.retain(|(workspace, _), _| !workspace.starts_with(&root));
    }
    if let Ok(mut cache) = WORKSPACE_MEMBER_DIRS_CACHE.write() {
        cache.retain(|workspace, _| !workspace.starts_with(&root));
    }
    if let Ok(mut cache) = RUST_CRATE_INFO_CACHE.write() {
        cache.retain(|dir, _| !dir.starts_with(&root));
    }
    if let Ok(mut cache) = RUST_WORKSPACE_CRATE_CACHE.write() {
        cache.retain(|workspace, _| !workspace.starts_with(&root));
    }
}

pub fn clear_workspace_package_cache() {
    if let Ok(mut cache) = WORKSPACE_PACKAGE_CACHE.write() {
        cache.clear();
    }
    if let Ok(mut cache) = WORKSPACE_MEMBER_DIRS_CACHE.write() {
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

    // The memo is a bounded per-pass layer; the process-wide cache is the
    // durable one. Both are consulted whenever present: several call sites
    // resolve with a memo that lives for a single import, and a memo alone
    // turned every bare package import in a workspace monorepo into a full
    // member-directory walk (realpath per directory) because the miss never
    // reached the cache that already held the answer. Two daemon threads sat
    // at 70% each in that walk during one-file Tier-2 rescans across roots.
    if let Some(memo) = memo {
        if let Some(cached) = memo.workspace_package(&cache_key) {
            facts
                .facts
                .memo_replay(&workspace_root, "package", package_name);
            return cached;
        }
    }
    if facts.facts.records_config_facts() {
        if let Some(cached) = facts.facts.workspace_package(&workspace_root, package_name) {
            facts
                .facts
                .memo_replay(&workspace_root, "package", package_name);
            return cached;
        }
    } else if let Ok(cache) = WORKSPACE_PACKAGE_CACHE.read() {
        if let Some(cached) = cache.get(&cache_key) {
            if let Some(memo) = memo {
                memo.remember_workspace_package(cache_key, cached.clone());
            }
            facts
                .facts
                .memo_replay(&workspace_root, "package", package_name);
            return cached.clone();
        }
    }

    facts
        .facts
        .memo_start(&workspace_root, "package", package_name);
    // A recorder that keeps every read with its answer records the name's key
    // instead of the member walk and the names it read: the walk is the same
    // for every reader, and kept in each it was most of a monorepo's stored
    // call graph. `js_workspace_package_map` sets the key when the name's
    // member changes.
    let keyed = facts.facts.records_answers();
    if keyed {
        facts
            .facts
            .record_domain(&crate::callgraph_store::join::js_package_key(
                &checkout_relative(facts, &workspace_root),
                package_name,
            ));
        facts.facts.pause_recording();
    }
    let resolved = workspace_package_member(&workspace_root, package_name, memo, facts);
    if keyed {
        facts.facts.resume_recording();
    }

    facts
        .facts
        .memo_finish(&workspace_root, "package", package_name);
    if let Some(memo) = memo {
        memo.remember_workspace_package(cache_key.clone(), resolved.clone());
    }
    if facts.facts.records_config_facts() {
        facts
            .facts
            .remember_workspace_package(&workspace_root, package_name, resolved.clone());
    } else if let Ok(mut cache) = WORKSPACE_PACKAGE_CACHE.write() {
        cache.insert(cache_key, resolved.clone());
    }

    resolved
}

/// The member of the workspace at `workspace_root` whose `package.json` names
/// `package_name`, resolved: the first in member order when several do.
fn workspace_package_member(
    workspace_root: &Path,
    package_name: &str,
    memo: Option<&ModuleResolutionMemo>,
    facts: &FactPaths<'_>,
) -> Option<PathBuf> {
    cached_workspace_member_dirs(workspace_root, memo, facts)
        .iter()
        .find(|dir| package_json_name(dir, memo, facts).as_deref() == Some(package_name))
        .map(|dir| facts.canonical(dir).unwrap_or_else(|| dir.clone()))
}

/// `path` relative to the checkout root of `facts`, with `/` separators.
fn checkout_relative(facts: &FactPaths<'_>, path: &Path) -> String {
    path.strip_prefix(facts.root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Each JavaScript workspace package's resolved member folder, by the key a
/// reader records for looking its name up ([`js_package_key`]), for each of
/// `candidates` that is a workspace root. A name several members share maps
/// to the member [`resolve_workspace_package`] finds. Read through `facts`
/// without a memo, so a map built before and after a change compares what
/// the resolver would find in each.
///
/// [`js_package_key`]: crate::callgraph_store::join::js_package_key
pub fn js_workspace_package_map(
    facts: &FactPaths<'_>,
    candidates: impl IntoIterator<Item = PathBuf>,
) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for candidate in candidates {
        if !is_workspace_root(&candidate, None, facts) {
            continue;
        }
        let root = facts
            .canonical(&candidate)
            .unwrap_or_else(|| candidate.clone());
        let relative = checkout_relative(facts, &root);
        for dir in workspace_member_dirs(&root, None, facts) {
            let Some(name) = package_json_name(&dir, None, facts) else {
                continue;
            };
            let key = crate::callgraph_store::join::js_package_key(&relative, &name);
            map.entry(key).or_insert_with(|| {
                let resolved = facts.canonical(&dir).unwrap_or(dir);
                resolved.to_string_lossy().into_owned()
            });
        }
    }
    map
}

/// The member list for a workspace root, walked at most once per cache
/// lifetime. `workspace_root` is already canonical here (the caller keys on
/// it), so alias spellings cannot split the entry.
fn cached_workspace_member_dirs(
    workspace_root: &Path,
    memo: Option<&ModuleResolutionMemo>,
    facts: &FactPaths<'_>,
) -> Arc<Vec<PathBuf>> {
    if facts.facts.records_config_facts() {
        if let Some(members) = facts.facts.workspace_members(workspace_root) {
            facts.facts.memo_replay(workspace_root, "members", "");
            return members;
        }
    } else {
        let cell = WORKSPACE_MEMBER_DIRS_CACHE
            .write()
            .expect("workspace member cache poisoned")
            .entry(workspace_root.to_path_buf())
            .or_default()
            .clone();
        let mut populated = false;
        let members = cell.get_or_init(|| {
            populated = true;
            facts.facts.memo_start(workspace_root, "members", "");
            let members = Arc::new(workspace_member_dirs(workspace_root, memo, facts));
            facts.facts.memo_finish(workspace_root, "members", "");
            members
        });
        if !populated {
            facts.facts.memo_replay(workspace_root, "members", "");
        }
        return Arc::clone(members);
    }
    facts.facts.memo_start(workspace_root, "members", "");
    let members = Arc::new(workspace_member_dirs(workspace_root, memo, facts));
    facts.facts.memo_finish(workspace_root, "members", "");
    if facts.facts.records_config_facts() {
        facts
            .facts
            .remember_workspace_members(workspace_root, Arc::clone(&members));
    }
    members
}

fn workspace_member_dirs(
    workspace_root: &Path,
    memo: Option<&ModuleResolutionMemo>,
    facts: &FactPaths<'_>,
) -> Vec<PathBuf> {
    facts.config_field(workspace_root, "package.json", "workspaces");
    let mut patterns = package_json_value(workspace_root, memo, facts)
        .map(|package_json| workspace_patterns(&package_json))
        .unwrap_or_default();
    patterns.extend(pnpm_workspace_patterns(workspace_root, facts));

    expand_workspace_patterns(workspace_root, &patterns, facts)
}

pub fn workspace_patterns(package_json: &Value) -> Vec<String> {
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
    facts.config_field(workspace_root, "pnpm-workspace.yaml", "packages");
    let Some(bytes) = facts.attributed_bytes(&workspace_root.join("pnpm-workspace.yaml")) else {
        return Vec::new();
    };

    parse_pnpm_workspace_patterns(&bytes)
}

pub fn parse_pnpm_workspace_patterns(bytes: &[u8]) -> Vec<String> {
    let Ok(source) = std::str::from_utf8(bytes) else {
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

test_support_visibility! {
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

    // Workspace membership is manifest-defined, not gitignore-defined: a
    // manifest can intentionally name an ignored vendor directory. In addition
    // to the existing excluded names and filesystem boundary, prune branches
    // outside every positive pattern's literal directory prefix.
    // Keep wildcard matching unchanged (including '*' matching separators).
    let prefixes: Vec<_> = positive_patterns
        .iter()
        .map(|pattern| {
            if pattern.contains('\\') {
                return (PathBuf::new(), true);
            }
            match pattern.find(['*', '?', '[', '{']) {
                Some(first_glob) => {
                    let end = pattern[..first_glob].rfind('/').unwrap_or(0);
                    (PathBuf::from(&pattern[..end]), true)
                }
                None => (PathBuf::from(pattern), false),
            }
        })
        .collect();

    let mut members = Vec::new();
    collect_workspace_member_dirs(
        workspace_root,
        workspace_root,
        &positives,
        &negatives,
        &prefixes,
        &mut members,
        facts,
    );
    members
}
}

test_support_visibility! {
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
}

fn collect_workspace_member_dirs(
    workspace_root: &Path,
    dir: &Path,
    positives: &GlobSet,
    negatives: &GlobSet,
    prefixes: &[(PathBuf, bool)],
    members: &mut Vec<PathBuf>,
    facts: &FactPaths<'_>,
) {
    #[cfg(any(test, feature = "test-support"))]
    crate::callgraph_store::work_counts::note_workspace_directory(dir);
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

        let Ok(rel) = path.strip_prefix(workspace_root) else {
            continue;
        };
        if !prefixes.iter().any(|(prefix, recursive)| {
            prefix.starts_with(rel) || (*recursive && rel.starts_with(prefix))
        }) {
            continue;
        }
        let candidate = rel.to_string_lossy().replace('\\', "/");
        if positives.is_match(&candidate)
            && !negatives.is_match(&candidate)
            && facts.is_file(&path.join("package.json"))
        {
            members.push(path.clone());
        }

        if prefixes.iter().any(|(prefix, recursive)| {
            (prefix != rel && prefix.starts_with(rel)) || (*recursive && rel.starts_with(prefix))
        }) {
            collect_workspace_member_dirs(
                workspace_root,
                &path,
                positives,
                negatives,
                prefixes,
                members,
                facts,
            );
        }
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
    let json = facts.attributed_bytes(path)?;
    serde_json::from_slice(&json).ok().map(Arc::new)
}

fn package_json_name(
    dir: &Path,
    memo: Option<&ModuleResolutionMemo>,
    facts: &FactPaths<'_>,
) -> Option<String> {
    facts.config_field(dir, "package.json", "name");
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

    facts.config_field(package_root, "package.json", "exports");
    if let Some(exports) = package_json.get("exports") {
        if let Some(target) = export_target_for_subpath(exports, subpath.as_deref()) {
            if let Some(path) = resolve_package_target(package_root, &target, facts) {
                return Some(path);
            }
        }
    }

    if subpath.is_none() {
        for field in ["module", "main"] {
            facts.config_field(package_root, "package.json", field);
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

pub fn resolve_reexported_symbol_target<F, D>(
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

test_support_visibility! {
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
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    pub static REEXPORT_PARSE_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn parse_reexport_source(
    source: &str,
    lang: LangId,
) -> Result<tree_sitter::Tree, crate::error::AftError> {
    #[cfg(any(test, feature = "test-support"))]
    REEXPORT_PARSE_COUNT.with(|count| count.set(count.get() + 1));
    crate::parser::FileParser::parse_source(Path::new("<reexport-source>"), source, lang)
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

    let source = crate::project_content::read_to_string(&canon).ok()?;
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

    let tree = parse_reexport_source(&source, lang).ok()?;
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

    if let Some(symbol) = resolve_local_export_alias(&source, tree.root_node(), symbol_name) {
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

fn resolve_local_export_alias(
    source: &str,
    root: tree_sitter::Node,
    requested_export: &str,
) -> Option<String> {
    let mut cursor = root.walk();
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

test_support_visibility! {
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
}

// ---------------------------------------------------------------------------
// Worktree file discovery
// ---------------------------------------------------------------------------

/// Directories the callgraph walk always skips, whatever .gitignore says.
const WALK_EXCLUDED_DIR_NAMES: [&str; 9] = [
    "node_modules",
    "target",
    "venv",
    ".venv",
    ".git",
    "__pycache__",
    ".tox",
    "dist",
    "build",
];

/// Walk project files respecting .gitignore, excluding common non-source dirs.
///
/// Returns an iterator of file paths for supported source file types.
pub fn walk_project_files(
    root: &Path,
    access: crate::project_walk::IgnoreFileAccess,
) -> impl Iterator<Item = PathBuf> {
    walk_project_files_filtered(root, access, |_| true)
}

/// Walk the requested project subtrees, stopping before the next raw walker
/// advance when the caller's budget expires.
/// Starting at the project root preserves ignored-parent and nested ignore
/// rules; pruning before descent avoids traversing unrelated subtrees.
pub fn walk_project_files_in_scope_while(
    root: &Path,
    access: crate::project_walk::IgnoreFileAccess,
    scope_roots: &[PathBuf],
    keep_walking: impl Fn() -> bool + Send + Sync + 'static,
) -> impl Iterator<Item = PathBuf> {
    let roots = scope_roots.to_vec();
    walk_project_files_filtered_while(
        root,
        access,
        move |path| {
            roots
                .iter()
                .any(|root| path.starts_with(root) || root.starts_with(path))
        },
        keep_walking,
    )
}

test_support_visibility! {
fn walk_project_files_filtered(
    root: &Path,
    access: crate::project_walk::IgnoreFileAccess,
    include: impl Fn(&Path) -> bool + Send + Sync + 'static,
) -> impl Iterator<Item = PathBuf> {
    walk_project_files_filtered_while(root, access, include, || true)
}
}

/// Stop before advancing the directory iterator, not just between source files.
/// Unsupported files and directories still consume a caller's walk budget.
pub fn walk_project_files_while(
    root: &Path,
    access: crate::project_walk::IgnoreFileAccess,
    keep_walking: impl Fn() -> bool + Send + Sync + 'static,
) -> impl Iterator<Item = PathBuf> {
    walk_project_files_filtered_while(root, access, |_| true, keep_walking)
}

#[cfg(any(test, feature = "test-support"))]
#[test]
fn audit01_walk_cancellation_observes_unsupported_entries() {
    let root = tempfile::tempdir().unwrap();
    for name in ["one.unsupported", "two.unsupported", "three.unsupported"] {
        std::fs::write(root.path().join(name), "").unwrap();
    }
    let checks = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = checks.clone();
    let files = walk_project_files_while(
        root.path(),
        crate::project_walk::IgnoreFileAccess::Allowed,
        move || {
            observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            false
        },
    )
    .collect::<Vec<_>>();
    assert!(files.is_empty());
    assert_eq!(
        checks.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "cancellation must run before source-file filtering"
    );
}

/// Walk all non-excluded files under a root, using the same bounded iterator
/// and ignore rules as the source-file census. Caller-excluded paths are pruned
/// before descent.
pub fn walk_project_paths_while(
    root: &Path,
    access: crate::project_walk::IgnoreFileAccess,
    include: impl Fn(&Path) -> bool + Send + Sync + 'static,
    keep_walking: impl Fn() -> bool + Send + Sync + 'static,
) -> impl Iterator<Item = PathBuf> {
    walk_project_paths_filtered_while_with_builder(root, access, include, keep_walking, |_| {})
}

fn walk_project_files_filtered_while(
    root: &Path,
    access: crate::project_walk::IgnoreFileAccess,
    include: impl Fn(&Path) -> bool + Send + Sync + 'static,
    keep_walking: impl Fn() -> bool + Send + Sync + 'static,
) -> impl Iterator<Item = PathBuf> {
    walk_project_files_filtered_while_with_builder(root, access, include, keep_walking, |_| {})
}

#[cfg(any(test, feature = "test-support"))]
pub fn walk_project_files_while_sorted_for_test(
    root: &Path,
    access: crate::project_walk::IgnoreFileAccess,
    keep_walking: impl Fn() -> bool + Send + Sync + 'static,
) -> impl Iterator<Item = PathBuf> {
    walk_project_files_filtered_while_with_builder(
        root,
        access,
        |_| true,
        keep_walking,
        |builder| {
            builder.sort_by_file_name(|left, right| left.cmp(right));
        },
    )
}

fn walk_project_files_filtered_while_with_builder(
    root: &Path,
    access: crate::project_walk::IgnoreFileAccess,
    include: impl Fn(&Path) -> bool + Send + Sync + 'static,
    keep_walking: impl Fn() -> bool + Send + Sync + 'static,
    configure_builder: impl FnOnce(&mut ignore::WalkBuilder),
) -> impl Iterator<Item = PathBuf> {
    walk_project_paths_filtered_while_with_builder(
        root,
        access,
        include,
        keep_walking,
        configure_builder,
    )
    .filter(|path| {
        crate::parser::detect_language_any(path, cfg!(feature = "wasm-grammars")).is_some()
    })
}

fn walk_project_paths_filtered_while_with_builder(
    root: &Path,
    access: crate::project_walk::IgnoreFileAccess,
    include: impl Fn(&Path) -> bool + Send + Sync + 'static,
    keep_walking: impl Fn() -> bool + Send + Sync + 'static,
    configure_builder: impl FnOnce(&mut ignore::WalkBuilder),
) -> impl Iterator<Item = PathBuf> {
    use ignore::WalkBuilder;

    // A disappearing child mount can make ReadDir::drop panic on ENXIO and abort
    // the daemon, so never open directories outside this walk root's filesystem.
    let mut builder = WalkBuilder::new(root);
    builder.same_file_system(true).hidden(true); // skip hidden files/dirs
                                                 // .gitignore (also outside a git repository), global excludes,
                                                 // .git/info/exclude and .aftignore (e.g. submodules).
    configure_builder(&mut builder);
    let filter_keep_walking = std::sync::Arc::new(keep_walking);
    let filter_budget = std::sync::Arc::clone(&filter_keep_walking);
    // Skip nested linked worktrees of this repository: their files are indexed
    // under their own roots. The walk root itself is always admitted (depth 0).
    let worktrees = crate::project_content::NestedWorktreeBoundary::for_root(root);
    let mut walker = crate::project_walk::apply_project_ignore_rules(&mut builder, root, access)
        .filter_entry(move |entry| {
            #[cfg(any(test, feature = "test-support"))]
            crate::walk_profile::entry();
            #[cfg(any(test, feature = "test-support"))]
            PROJECT_WALK_FILTER_ENTRIES
                .with(|entries| entries.borrow_mut().push(entry.path().to_path_buf()));
            // The ignore crate filters hidden and ignored entries before this hook.
            // Once the budget expires, it prevents descent and yielding, but
            // the current Walk::next() can still finish this directory. The
            // remaining worst case is one directory listing.
            if !filter_budget() {
                return false;
            }
            #[cfg(any(test, feature = "test-support"))]
            {
                PROJECT_WALK_ENTRIES.with(|count| count.set(count.get() + 1));
                PROJECT_WALK_ACCEPTED_FILTER_ENTRIES
                    .with(|entries| entries.borrow_mut().push(entry.path().to_path_buf()));
            }
            include(entry.path()) && project_walk_entry_is_included(entry, &worktrees)
        })
        .build();
    // Filtering to source paths first can hide arbitrarily many directory/file
    // visits inside a single next() call. Check before each raw walker advance.
    let walk_budget = std::sync::Arc::clone(&filter_keep_walking);
    std::iter::from_fn(move || walk_budget().then(|| walker.next()).flatten())
        .fuse()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().map_or(false, |ft| ft.is_file()))
        .map(|entry| entry.into_path())
}

pub fn project_walk_entry_is_included(
    entry: &ignore::DirEntry,
    worktrees: &crate::project_content::NestedWorktreeBoundary,
) -> bool {
    let file_type = entry.file_type();
    if crate::project_content::is_excluded_with_file_type(entry.path(), file_type.as_ref())
        || crate::project_walk::is_nested_worktree_entry(entry, worktrees)
    {
        return false;
    }
    if file_type.as_ref().is_some_and(std::fs::FileType::is_dir) {
        return !WALK_EXCLUDED_DIR_NAMES.contains(&entry.file_name().to_string_lossy().as_ref());
    }
    !crate::os_metadata::is_os_metadata_file_name(entry.file_name())
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    pub static PROJECT_WALK_ENTRIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub static PROJECT_WALK_FILTER_ENTRIES: std::cell::RefCell<Vec<PathBuf>> = const { std::cell::RefCell::new(Vec::new()) };
    pub static PROJECT_WALK_ACCEPTED_FILTER_ENTRIES: std::cell::RefCell<Vec<PathBuf>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Whether [`walk_project_files`] can yield the file at this root-relative
/// path (`/` or `\` separators), judged from the path alone: a supported
/// source type, no hidden component, no always-excluded directory and no OS
/// metadata file name. Ignore files are not consulted; callers pass paths
/// that an ignore-respecting walk has already produced.
pub fn walk_could_include(relative: &str) -> bool {
    let components: Vec<&str> = relative
        .split(['/', '\\'])
        .filter(|component| !component.is_empty())
        .collect();
    let Some((file_name, directories)) = components.split_last() else {
        return false;
    };
    if components
        .iter()
        .any(|component| component.starts_with('.'))
    {
        return false;
    }
    if directories
        .iter()
        .any(|directory| WALK_EXCLUDED_DIR_NAMES.contains(directory))
    {
        return false;
    }
    if crate::os_metadata::is_os_metadata_file_name(std::ffi::OsStr::new(file_name)) {
        return false;
    }
    crate::parser::detect_language_any(Path::new(file_name), cfg!(feature = "wasm-grammars"))
        .is_some()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(any(test, feature = "test-support"))]
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

#[cfg(any(test, feature = "test-support"))]
pub fn resolve_symbol_query_in_data(
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

#[cfg(test)]
mod rust_use_regressions {
    use super::*;
    #[test]
    fn rust_use_tree_expansion_preserves_depth_first_entry_order() {
        let mut entries = Vec::new();
        expand_rust_use_tree(
            "root::{first, branch::{self, nested::{Leaf as Renamed, *, {}}, after}, self, *, {}, last as Tail}",
            &mut entries,
        );
        let actual: Vec<_> = entries
            .iter()
            .map(|entry| {
                let imported = match &entry.kind {
                    RustUseKind::Item { imported_name } => Some(imported_name.as_str()),
                    RustUseKind::Module => None,
                };
                (
                    entry.module_path.as_str(),
                    entry.local_name.as_str(),
                    imported,
                )
            })
            .collect();
        assert_eq!(
            actual,
            [
                ("root", "first", Some("first")),
                ("root::first", "first", None),
                ("root::branch", "branch", None),
                ("root::branch::nested", "Renamed", Some("Leaf")),
                ("root::branch::nested::Leaf", "Renamed", None),
                ("root::branch", "after", Some("after")),
                ("root::branch::after", "after", None),
                ("root", "root", None),
                ("root", "Tail", Some("last")),
                ("root::last", "Tail", None),
            ]
        );
    }
}
