//! Shared call-graph resolution and query contracts.
//!
//! The index store binds extracted facts through this resolver and implements
//! the reader contract used by navigation commands. The retired SQLite store
//! no longer owns builds, generations, readers, or refresh workers here.

pub mod access;
pub(crate) mod disk_facts;
pub(crate) mod fact_answers;
pub(crate) mod facts;
mod go_dispatch;
pub mod join;
pub(crate) mod method_call_facts;
#[cfg(target_os = "macos")]
mod readiness_watch_macos;
pub(crate) mod resolution_facts;
#[cfg(test)]
use disk_facts::DiskFacts;
use facts::{byte_path, EntryKind, FactPaths, ProjectFacts};
#[cfg(test)]
use std::sync::Mutex;
#[cfg(test)]
use std::time::{Duration, Instant};

use crate::callgraph::{self, FileCallData, TraceToSymbolCandidate};
use crate::error::AftError;
use crate::imports::{ImportForm, ImportGroup, ImportKind, ImportStatement};
use crate::parser::{parse_source_with_cached_parser, LangId};
use crate::symbols::SymbolKind;
use join::syntax_index::{Direct, Nav};
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tree_sitter::Node;

pub(crate) const PROVENANCE_TREESITTER: &str = "treesitter+resolver";
pub(crate) const PROVENANCE_NAME_MATCH: &str = "name_match";
pub(crate) const PROVENANCE_TYPE_MATCH: &str = "type_match";
pub(crate) const PROVENANCE_VALUE_REF: &str = "value_ref";
/// Ref kind for a call written directly in a `macro_rules!` template; the
/// macro name is stored in `local_name`. Never resolved into an edge.
pub(crate) const MACRO_BODY_CALL_REF_KIND: &str = "macro_body_call";
/// Ref kind for an identifier inside a Rust macro token tree that could not be
/// parsed as Rust. Never resolved into an edge.
pub(crate) const MACRO_MENTION_REF_KIND: &str = "macro_mention";
pub(crate) const TOP_LEVEL_SYMBOL: &str = "<top-level>";
const JS_TS_EXTENSIONS: &[&str] = &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"];

/// Work counters for measurement tests: source parses by the store and
/// resolution-config reads (`package.json`, `tsconfig.json`, `Cargo.toml`).
/// They are keyed by absolute path because the work runs on build-pool
/// threads, and tests running in parallel must each count only the files
/// under their own fixture root.
#[cfg(test)]
pub(crate) mod work_counts {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, OnceLock};

    static PARSES: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();
    static FINGERPRINT_READS: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();
    static CONFIG_READS: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();
    static STAGING_READS: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();
    static DIRECTORY_READS: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();
    static GO_PACKAGE_BUILDS: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();
    static CANONICAL_PATHS: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();
    static REALPATHS: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();
    static WORKSPACE_DIRECTORIES: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();
    static CORPUS_LOOKUPS: OnceLock<Mutex<HashMap<PathBuf, (usize, usize, u128)>>> =
        OnceLock::new();

    fn note(map: &OnceLock<Mutex<HashMap<PathBuf, usize>>>, path: &Path) {
        *map.get_or_init(Default::default)
            .lock()
            .expect("work counter mutex poisoned")
            .entry(path.to_path_buf())
            .or_default() += 1;
    }

    fn under(
        map: &OnceLock<Mutex<HashMap<PathBuf, usize>>>,
        root: &Path,
        file_name: Option<&str>,
    ) -> usize {
        map.get_or_init(Default::default)
            .lock()
            .expect("work counter mutex poisoned")
            .iter()
            .filter(|(path, _)| path.starts_with(root))
            .filter(|(path, _)| {
                file_name.is_none_or(|name| path.file_name().is_some_and(|file| file == name))
            })
            .map(|(_, count)| *count)
            .sum()
    }

    pub(crate) fn note_config_read(path: &Path) {
        note(&CONFIG_READS, path);
    }

    pub(crate) fn note_directory_read(path: &Path) {
        note(&DIRECTORY_READS, path);
    }

    pub(crate) fn directory_reads_under(root: &Path) -> usize {
        under(&DIRECTORY_READS, root, None)
    }

    pub(crate) fn note_canonical_path(path: &Path) {
        note(&CANONICAL_PATHS, path);
    }

    pub(crate) fn note_realpath(path: &Path) {
        note(&REALPATHS, path);
    }

    pub(crate) fn note_workspace_directory(path: &Path) {
        note(&WORKSPACE_DIRECTORIES, path);
    }

    pub(crate) fn filesystem_counts(root: &Path) -> (usize, usize, usize, usize) {
        let paths = CANONICAL_PATHS
            .get_or_init(Default::default)
            .lock()
            .unwrap();
        let unique = paths.keys().filter(|path| path.starts_with(root)).count();
        let calls = paths
            .iter()
            .filter(|(path, _)| path.starts_with(root))
            .map(|(_, count)| count)
            .sum();
        (
            calls,
            unique,
            under(&WORKSPACE_DIRECTORIES, root, None),
            directory_reads_under(root),
        )
    }

    pub(crate) fn note_go_package_build(path: &Path) {
        note(&GO_PACKAGE_BUILDS, path);
    }

    pub(crate) fn reset_under(root: &Path) {
        CORPUS_LOOKUPS
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .retain(|path, _| !path.starts_with(root));
        for map in [
            &PARSES,
            &FINGERPRINT_READS,
            &CONFIG_READS,
            &STAGING_READS,
            &DIRECTORY_READS,
            &GO_PACKAGE_BUILDS,
            &CANONICAL_PATHS,
            &REALPATHS,
            &WORKSPACE_DIRECTORIES,
        ] {
            map.get_or_init(Default::default)
                .lock()
                .expect("work counter mutex poisoned")
                .retain(|path, _| !path.starts_with(root));
        }
    }
}

pub(crate) mod dead_code_projection;

#[cfg(test)]
#[path = "../../tests/store_diet/harness.rs"]
mod store_diet;

mod rust_bindings;
pub(crate) use dead_code_projection::{
    ProjectionCostEstimates, ProjectionKind, ProjectionVerdict, MAX_DELTA_BYTES,
};

#[derive(Debug)]
pub enum CallGraphStoreError {
    Io(std::io::Error),
    Sqlite(rusqlite::Error),
    Json(serde_json::Error),
    Aft(AftError),
    Lock(crate::fs_lock::AcquireError),
    MissingCallerData {
        file: String,
    },
    Unavailable(String),
    PathIdentityMismatch {
        path: PathBuf,
        project_root: PathBuf,
    },
    Suspended(crate::build_breaker::BuildSuspension),
    Superseded,
    CorpusSnapshotMissing(PathBuf),
    CorpusInventoryChanged(Option<PathBuf>),
    StaleFiles(Vec<String>),
    /// The data layer's call graph is still building or catching up with
    /// edits; a reader retries.
    Building,
}

impl CallGraphStoreError {
    /// SQLite reported the database busy or locked. A view reader opens
    /// without a busy wait, so this is a moment of contention (a publication
    /// checkpointing the database it reads), not a failure: commands report it
    /// as a retryable `callgraph_building`.
    pub fn is_transient_lock_contention(&self) -> bool {
        matches!(
            self,
            Self::Sqlite(rusqlite::Error::SqliteFailure(error, _))
                if matches!(
                    error.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                )
        )
    }
}

impl fmt::Display for CallGraphStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
            Self::Sqlite(error) => write!(formatter, "sqlite error: {error}"),
            Self::Json(error) => write!(formatter, "json error: {error}"),
            Self::Aft(error) => write!(formatter, "callgraph extraction error: {error}"),
            Self::Lock(error) => write!(formatter, "callgraph writer lease error: {error}"),
            Self::MissingCallerData { file } => {
                write!(formatter, "missing extracted caller data for {file}")
            }
            Self::Unavailable(message) => {
                write!(formatter, "callgraph store unavailable: {message}")
            }
            Self::PathIdentityMismatch { path, project_root } => write!(
                formatter,
                "callgraph path identity mismatch: {} is not under project root {}",
                path.display(),
                project_root.display()
            ),
            Self::Suspended(suspension) => write!(
                formatter,
                "callgraph build suspended for {} after {} deaths ({})",
                suspension.domain.as_str(),
                suspension.death_count,
                suspension.reason
            ),
            Self::Superseded => {
                write!(formatter, "callgraph store build superseded before publish")
            }
            Self::Building => write!(formatter, "the call graph is still building"),
            Self::CorpusSnapshotMissing(path) => write!(
                formatter,
                "callgraph corpus snapshot lookup invariant failed: {} was not admitted",
                path.display()
            ),
            Self::CorpusInventoryChanged(path) => {
                write!(
                    formatter,
                    "callgraph corpus inventory changed after admission"
                )?;
                if let Some(path) = path {
                    write!(formatter, ": {} was added", path.display())?;
                }
                Ok(())
            }
            Self::StaleFiles(files) => {
                write!(
                    formatter,
                    "callgraph store has stale files: {}",
                    files.join(", ")
                )
            }
        }
    }
}

impl std::error::Error for CallGraphStoreError {}

impl From<std::io::Error> for CallGraphStoreError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<rusqlite::Error> for CallGraphStoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

impl From<serde_json::Error> for CallGraphStoreError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

impl From<AftError> for CallGraphStoreError {
    fn from(error: AftError) -> Self {
        Self::Aft(error)
    }
}

impl From<crate::fs_lock::AcquireError> for CallGraphStoreError {
    fn from(error: crate::fs_lock::AcquireError) -> Self {
        Self::Lock(error)
    }
}

pub type Result<T> = std::result::Result<T, CallGraphStoreError>;

type WorkspaceCratePrefixes = HashMap<String, String>;

#[derive(Clone, Debug, Default)]
pub(crate) struct WorkspaceCratePrefixCache(
    Arc<OnceLock<WorkspaceCratePrefixes>>,
    Arc<OnceLock<HashMap<String, String>>>,
);

pub(crate) fn refresh_lease_release_event(event: &notify::Event, lease: &Path) -> bool {
    use notify::{event::ModifyKind, EventKind};
    let Some(directory) = lease.parent() else {
        return false;
    };
    let same_directory = |path: &Path| {
        path == directory
            || match (fs_canonicalize(path), fs_canonicalize(directory)) {
                (Ok(path), Ok(directory)) => path == directory,
                _ => false,
            }
    };
    let named_release = matches!(
        event.kind,
        EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_))
    ) && event.paths.iter().any(|path| {
        path.file_name() == lease.file_name() && path.parent().is_some_and(&same_directory)
    });
    // Native watchers can coalesce a deletion into directory/sidecar activity,
    // and spell the watched directory through an alias (/var vs /private/var).
    // Activity is a hint only when the lease is gone; the worker still checks
    // writer liveness before reopening the store.
    let sibling_change = matches!(
        event.kind,
        EventKind::Any | EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    ) && !lease.exists()
        && event
            .paths
            .iter()
            .any(|path| same_directory(path) || path.parent().is_some_and(&same_directory));
    named_release || sibling_change
}

/// Readiness notifications for one cache directory. macOS uses a vnode watch
/// so unlinking a lease wakes the waiter even if FSEvents coalesces it away.
/// Windows uses AFT's directory watcher to avoid re-arming notify reads on
/// closed handles. Other platforms keep notify's watcher.
#[cfg(not(any(windows, target_os = "macos")))]
pub(crate) type ReadinessWatcher = notify::RecommendedWatcher;
#[cfg(windows)]
pub(crate) type ReadinessWatcher = ForwardingDirectoryWatcher;
#[cfg(target_os = "macos")]
pub(crate) type ReadinessWatcher = readiness_watch_macos::DirectoryWatch;

/// Watches `dir`, without its subdirectories, and hands every event to
/// `handler` until the returned watcher is dropped.
pub(crate) fn watch_readiness_directory<F>(
    dir: &Path,
    handler: F,
) -> notify::Result<ReadinessWatcher>
where
    F: FnMut(notify::Result<notify::Event>) + Send + 'static,
{
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        use notify::{RecursiveMode, Watcher};
        let mut watcher = notify::recommended_watcher(handler)?;
        watcher.watch(dir, RecursiveMode::NonRecursive)?;
        Ok(watcher)
    }
    #[cfg(windows)]
    {
        ForwardingDirectoryWatcher::start(dir, handler)
    }
    #[cfg(target_os = "macos")]
    {
        readiness_watch_macos::DirectoryWatch::start(dir, handler)
    }
}

/// A native `DirectoryWatcher` whose events go to a callback rather than a
/// channel. A forwarding thread drains its channel without running callback
/// work on the native watcher thread.
#[cfg(windows)]
pub(crate) struct ForwardingDirectoryWatcher {
    watcher: Option<crate::watcher_backend::DirectoryWatcher>,
    forwarder: Option<std::thread::JoinHandle<()>>,
}

#[cfg(windows)]
impl ForwardingDirectoryWatcher {
    fn start<F>(dir: &Path, mut handler: F) -> notify::Result<Self>
    where
        F: FnMut(notify::Result<notify::Event>) + Send + 'static,
    {
        let (tx, rx) = std::sync::mpsc::channel();
        // The counters are private to this watch: it must not overwrite the
        // project watcher's backend state for the same root.
        let mut watcher = crate::watcher_backend::DirectoryWatcher::new(
            tx,
            Arc::new(crate::context::WatcherCounters::default()),
        );
        watcher.watch(dir, notify::RecursiveMode::NonRecursive)?;
        let forwarder = std::thread::Builder::new()
            .name("aft-readiness-watch".to_string())
            .spawn(move || {
                for event in rx {
                    handler(event);
                }
            })
            .map_err(notify::Error::io)?;
        Ok(Self {
            watcher: Some(watcher),
            forwarder: Some(forwarder),
        })
    }
}

#[cfg(windows)]
impl Drop for ForwardingDirectoryWatcher {
    fn drop(&mut self) {
        // Stopping the watch drops every sender of the forwarder's channel,
        // so the forwarder finishes the events already sent and returns. It
        // may lock the refresh queue on the way, so this must never run with
        // that lock held.
        drop(self.watcher.take());
        if let Some(forwarder) = self.forwarder.take() {
            let _ = forwarder.join();
        }
    }
}

/// Bound the work spent counting unresolved possible callers.
pub const UNRESOLVED_CALL_SITE_COUNT_LIMIT: usize = 1_000;

pub trait CallGraphRead {
    fn project_root(&self) -> &Path;
    fn project_key(&self) -> &str;
    fn sqlite_path(&self) -> &Path;
    fn is_current(&self) -> bool;
    fn edge_snapshot(&self) -> Result<BTreeSet<StoredEdge>>;
    fn indexed_file_count(&self) -> Result<usize>;
    fn node_for(&self, file_rel: &Path, symbol: &str) -> Result<StoreNode>;
    fn nodes_for(&self, file_rel: &Path, symbol: &str) -> Result<Vec<StoreNode>>;
    /// `nodes_for` for a file path this store returned (store-relative and
    /// already resolved), without resolving it against the filesystem again.
    /// Graph walks call this once per visited node.
    fn nodes_for_stored(&self, file: &str, symbol: &str) -> Result<Vec<StoreNode>> {
        self.nodes_for(Path::new(file), symbol)
    }
    /// The indexed content hash for a path returned by this store, or `None`
    /// when unknown. Compare before pairing stored positions with live text.
    fn stored_content_hash(&self, _file: &str) -> Result<Option<String>> {
        Ok(None)
    }
    fn nodes_matching(&self, symbol: &str) -> Result<Vec<StoreNode>>;
    fn direct_callers_of(&self, file_rel: &Path, symbol: &str) -> Result<Vec<StoreCallSite>>;
    fn direct_callers_for_symbols(
        &self,
        targets: &[(String, String)],
    ) -> Result<HashMap<(String, String), Vec<StoreCallSite>>> {
        targets
            .iter()
            .cloned()
            .map(|target| {
                let callers = self.direct_callers_of(Path::new(&target.0), &target.1)?;
                Ok((target, callers))
            })
            .collect()
    }
    fn direct_caller_counts_of(
        &self,
        targets: &[(String, String)],
    ) -> Result<HashMap<(String, String), usize>>;
    fn outgoing_calls_for_symbols(
        &self,
        sources: &[(String, String)],
    ) -> Result<HashMap<(String, String), Vec<StoreCallSite>>>;
    fn callers_of(&self, file_rel: &Path, symbol: &str, depth: usize)
        -> Result<StoreCallersResult>;
    fn impact_of(&self, file_rel: &Path, symbol: &str, depth: usize) -> Result<StoreImpactResult>;
    fn outgoing_calls_of(&self, node: &StoreNode) -> Result<Vec<StoreCallSite>>;
    fn resolved_self_calls_of(&self, node: &StoreNode) -> Result<Vec<StoreCallSite>>;
    fn unresolved_calls_of(&self, node: &StoreNode) -> Result<Vec<StoreUnresolvedCall>>;
    /// Raw ref rows of `kind` whose short name is `short_name`, resolved or
    /// not, at most `limit` of them in file and line order. `callers` uses it
    /// to find macro invocations and macro facts by name. Stores that keep no
    /// such rows answer with none.
    fn ref_sites_named(
        &self,
        kind: &str,
        short_name: &str,
        limit: usize,
    ) -> Result<Vec<StoreRefSite>> {
        let _ = (kind, short_name, limit);
        Ok(Vec::new())
    }
    /// The calls written inside the `macro_rules!` template that `node`, a
    /// macro owner (`name!`), defines, resolved or not, in source order.
    /// `call_tree` and `trace_to_symbol` follow them by name, as `callers`
    /// lists them. Stores that keep no such rows answer with none.
    fn macro_template_calls_of(&self, node: &StoreNode) -> Result<Vec<StoreUnresolvedCall>> {
        let _ = node;
        Ok(Vec::new())
    }
    /// Unbound receiver calls of the written member name, excluding sites that
    /// already have a supplemental edge. An empty reverse graph cannot prove
    /// these calls do not reach the queried method.
    fn unresolved_method_sites_named(
        &self,
        name: &str,
        language: LangId,
    ) -> Result<Vec<StoreRefSite>> {
        let _ = (name, language);
        Ok(Vec::new())
    }
    /// Count unresolved invocation sites, stopping at `UNRESOLVED_CALL_SITE_COUNT_LIMIT`.
    /// A count at the limit is a lower bound, not an exact total.
    fn unresolved_call_site_count(&self, language: LangId, include_tests: bool) -> Result<usize> {
        let _ = (language, include_tests);
        Ok(0)
    }
    /// Count distinct (caller file, byte start, byte end) spans with this short
    /// name, including bare imported calls. The test filter and SQL count limit
    /// match the unnamed count.
    fn unresolved_call_site_count_named(
        &self,
        name: &str,
        language: LangId,
        include_tests: bool,
    ) -> Result<usize> {
        let _ = (name, language, include_tests);
        Ok(0)
    }
    fn call_tree(
        &self,
        file_rel: &Path,
        symbol: &str,
        depth: usize,
    ) -> Result<callgraph::CallTreeNode>;
    fn trace_to(
        &self,
        file_rel: &Path,
        symbol: &str,
        max_depth: usize,
    ) -> Result<callgraph::TraceToResult>;
    fn trace_to_symbol_candidates(&self, to_symbol: &str) -> Result<Vec<TraceToSymbolCandidate>>;
    fn trace_to_symbol(
        &self,
        file_rel: &Path,
        symbol: &str,
        to_symbol: &str,
        to_file: Option<&Path>,
        max_depth: usize,
    ) -> Result<callgraph::TraceToSymbolResult>;
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct StoredEdge {
    pub source_file: String,
    pub source_symbol: String,
    pub target_file: String,
    pub target_symbol: String,
    pub kind: String,
    pub line: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreNode {
    node_id: String,
    pub file: String,
    pub symbol: String,
    pub name: String,
    pub kind: String,
    pub line: u32,
    pub end_line: u32,
    pub signature: Option<String>,
    pub exported: bool,
    /// The store's own entry-point guess. `trace_to` and `impact` label the
    /// roots they reach from [`StoreNode::entry`] instead.
    pub is_entry_point: bool,
    pub lang: LangId,
    /// What extraction recorded about execution starting at this symbol.
    pub entry: EntryFacts,
}

/// What extraction recorded about where execution can start at a symbol.
/// `trace_to` and `impact` label the roots they reach with these facts.
/// The data layer's call graph records them; today's database does not,
/// so its nodes carry the defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EntryFacts {
    /// Whether the symbol is test code (a `#[test]` or `#[cfg(test)]`
    /// item); `None` when extraction could not tell.
    pub is_test: Option<bool>,
    /// Whether the program starts here, as the language defines it. For
    /// Rust this is `fn main` at the top of its file, and the file must also
    /// be a binary target's root.
    pub program_start: bool,
    /// The entry attribute or decorator recorded on the symbol, such as
    /// `no_mangle` or `tauri::command`.
    pub entry_attribute: Option<String>,
    /// Whether the symbol is its module's default export.
    pub default_export: bool,
}

impl StoreNode {
    /// The positional node identity, for request-local memo keys.
    pub(crate) fn node_id(&self) -> &str {
        &self.node_id
    }

    /// This node with `entry` as its entry facts.
    #[must_use]
    pub(crate) fn with_entry(mut self, entry: EntryFacts) -> Self {
        self.entry = entry;
        self
    }

    /// A node from its fields, for stores that do not keep database rows.
    #[expect(clippy::too_many_arguments, reason = "one argument per node field")]
    pub(crate) fn from_parts(
        node_id: String,
        file: String,
        symbol: String,
        name: String,
        kind: String,
        line: u32,
        end_line: u32,
        signature: Option<String>,
        exported: bool,
        is_entry_point: bool,
        lang: LangId,
    ) -> Self {
        Self {
            node_id,
            file,
            symbol,
            name,
            kind,
            line,
            end_line,
            signature,
            exported,
            is_entry_point,
            lang,
            entry: EntryFacts::default(),
        }
    }
}

#[cfg(test)]
impl StoreNode {
    pub(crate) fn for_test(file: &str, symbol: &str, is_entry_point: bool) -> Self {
        Self {
            node_id: format!("{file}:{symbol}"),
            file: file.to_string(),
            symbol: symbol.to_string(),
            name: symbol.to_string(),
            kind: "function".to_string(),
            line: 1,
            end_line: 1,
            signature: None,
            exported: is_entry_point,
            is_entry_point,
            lang: LangId::TypeScript,
            entry: EntryFacts::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreCallSite {
    pub kind: String,
    pub caller_is_test: Option<bool>,
    pub caller: StoreNode,
    pub target_file: String,
    pub target_symbol: String,
    pub target: Option<StoreNode>,
    pub line: u32,
    pub byte_start: usize,
    pub byte_end: usize,
    pub resolved: bool,
    pub provenance: String,
}

impl StoreCallSite {
    pub fn approximate(&self) -> bool {
        self.provenance == PROVENANCE_NAME_MATCH
    }

    pub fn resolved_by(&self) -> &str {
        &self.provenance
    }

    pub fn supplemental_resolution(&self) -> Option<&str> {
        match self.provenance.as_str() {
            PROVENANCE_NAME_MATCH | PROVENANCE_TYPE_MATCH => Some(self.provenance.as_str()),
            "dispatch" => Some("possible_target (dispatch)"),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreUnresolvedCall {
    pub caller: StoreNode,
    pub symbol: String,
    pub full_ref: Option<String>,
    pub line: u32,
    pub byte_start: usize,
    pub byte_end: usize,
}

/// One raw ref row located by name; see `CallGraphRead::ref_sites_named`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreRefSite {
    pub is_test: Option<bool>,
    pub file: String,
    /// Scoped name of the symbol containing the ref, when it has a node.
    pub caller_symbol: Option<String>,
    pub line: u32,
    /// For macro-template call refs, the macro (`name!`) making the call.
    pub local_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreCallersResult {
    pub target: StoreNode,
    pub callers: Vec<StoreCallSite>,
    pub scanned_files: usize,
    pub depth_limited: bool,
    pub truncated: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreImpactCaller {
    pub site: StoreCallSite,
    pub signature: Option<String>,
    pub is_entry_point: bool,
    pub call_expression: Option<String>,
    pub parameters: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreImpactResult {
    pub target: StoreNode,
    pub parameters: Vec<String>,
    pub callers: Vec<StoreImpactCaller>,
    pub depth_limited: bool,
    pub truncated: usize,
}

#[derive(Debug, Clone)]
struct FileExtract {
    rel_path: String,
    lang: LangId,
    data: FileCallData,
    nodes: Vec<NodeRecord>,
    raw_refs: Vec<RawRef>,
}

#[derive(Debug, Clone)]
struct NodeRecord {
    id: String,
    name: String,
    scoped_name: String,
    kind: String,
}

#[derive(Debug, Clone)]
struct RawRef {
    ref_id: String,
    caller_symbol: Option<String>,
    caller_file: String,
    kind: String,
    short_name: Option<String>,
    full_ref: Option<String>,
    module_path: Option<String>,
    import_kind: Option<String>,
    local_name: Option<String>,
    requested_name: Option<String>,
    namespace_alias: Option<String>,
    wildcard: bool,
    line: u32,
    byte_start: usize,
    byte_end: usize,
    dependencies: BTreeSet<String>,
}

#[derive(Debug, Clone)]
struct ResolvedRef {
    target_file: Option<String>,
    target_symbol: Option<String>,
    dependencies: BTreeSet<String>,
}

impl DbFileIndex {
    /// The modules this file declares, each with the file it names.
    pub(crate) fn declared_module_targets(&self) -> &HashMap<String, Option<String>> {
        &self.declared_module_targets
    }
}

#[derive(Debug, Clone)]
pub(crate) struct DbFileIndex {
    lang: Option<LangId>,
    exports: HashSet<String>,
    default_export: Option<String>,
    export_aliases: HashMap<String, String>,
    node_by_scoped: HashMap<String, String>,
    node_by_bare: HashMap<String, String>,
    node_kind_by_id: HashMap<String, String>,
    module_targets: HashMap<String, Option<String>>,
    declared_module_targets: HashMap<String, Option<String>>,
    macro_use_module_targets: BTreeSet<String>,
    reexports: Vec<ReexportIndex>,
}

#[derive(Debug, Clone)]
pub(crate) struct ReexportIndex {
    target_file: Option<String>,
    named: HashMap<String, String>,
    wildcard: bool,
}

aft_store::heap_bytes_none!(LangId);
aft_store::heap_bytes_of_fields!(DbFileIndex {
    lang,
    exports,
    default_export,
    export_aliases,
    node_by_scoped,
    node_by_bare,
    node_kind_by_id,
    module_targets,
    declared_module_targets,
    macro_use_module_targets,
    reexports,
});
aft_store::heap_bytes_of_fields!(ReexportIndex {
    target_file,
    named,
    wildcard,
});

#[derive(Clone)]
pub(crate) struct ProjectIndex<'a> {
    facts: Rc<dyn ProjectFacts + 'a>,
    project_root: PathBuf,
    /// Shared, so several resolver threads can each wrap the same bound
    /// surfaces in their own index with their own memos.
    files: Arc<HashMap<String, DbFileIndex>>,
    caller_data: HashMap<String, &'a FileCallData>,
    /// File indexes loaded on demand: the legacy store's rows, for an
    /// incremental refresh, or the data layer's files bound on first use.
    /// Files in `files` take precedence over it.
    lazy: Option<Rc<dyn LoadedFileIndexes + 'a>>,
    /// Root-scoped maps shared by successive refresh-worker batches. Cargo.toml
    /// and go.mod watcher events replace them before another batch resolves refs.
    /// Cold/direct refreshes use a private cache so each refresh builds and uses
    /// its own workspace mapping.
    workspace_crate_prefixes: WorkspaceCratePrefixCache,
    rust_crate_roots: callgraph::RustCrateRootMemo,
    /// Crate-wide lookups derived from `files`, built on first use and never
    /// rebuilt. Only an index with every file in memory (`lazy` is `None`)
    /// uses it. Such an index's file set is fixed when it is constructed: a
    /// changed file set means a new `ProjectIndex`, which starts empty.
    crate_scans: std::cell::OnceCell<CrateScanMemo>,
    /// The lazy index overlays fresh caller extracts on stored rows. Its parent
    /// map and caller paths belong to that immutable snapshot, not the worker.
    lazy_module_parents: std::cell::OnceCell<HashMap<String, (String, String)>>,
    lazy_inline_candidates: std::cell::OnceCell<RustInlineCandidates>,
    rust_caller_paths: RefCell<HashMap<String, Vec<String>>>,
    go_packages: RefCell<GoPackageMemo>,
    local_symbols: RefCell<CallerSymbolMemo>,
}

/// Symbol-query answers for the active caller. Resolution visits references in
/// caller order, so retaining only one file avoids a corpus-sized name cache.
/// The caller's symbol set is immutable for the lifetime of a resolver index.
#[derive(Clone, Default)]
struct CallerSymbolMemo {
    file: Option<String>,
    symbols: HashMap<String, Option<String>>,
    #[cfg(test)]
    builds: usize,
}

impl CallerSymbolMemo {
    fn resolve(&mut self, file: &str, data: &FileCallData, query: &str) -> Option<String> {
        if self.file.as_deref() != Some(file) {
            self.symbols.clear();
            for symbol in data
                .symbol_metadata
                .keys()
                .chain(data.calls_by_symbol.keys())
                .chain(data.exported_symbols.iter())
            {
                // Qualified queries require an exact match; bare queries must
                // remain ambiguous even when one candidate is file-level.
                if symbol.contains("::") {
                    self.symbols.insert(symbol.clone(), Some(symbol.clone()));
                }
                if symbol.starts_with("@object:") {
                    continue;
                }
                let bare = symbol.rsplit("::").next().unwrap_or(symbol);
                self.symbols
                    .entry(bare.to_string())
                    .and_modify(|existing| {
                        if existing.as_ref() != Some(symbol) {
                            *existing = None;
                        }
                    })
                    .or_insert_with(|| Some(symbol.clone()));
            }
            self.file = Some(file.to_string());
            #[cfg(test)]
            {
                self.builds += 1;
            }
        }
        self.symbols.get(query).cloned().flatten()
    }
}

#[cfg(test)]
mod local_symbol_memo_tests {
    use super::*;

    #[test]
    fn local_symbol_memo_preserves_scan_results_and_builds_once_per_caller() {
        let file = "fixture.rs";
        let mut source = String::from(
            "fn local() {} fn ambiguous() {}\n\
             mod nested { fn ambiguous() {} fn scoped_only() {} }\n\
             struct Item; impl Item { fn method(&self) {} }\n",
        );
        for i in 0..512 {
            source.push_str(&format!("fn function_{i}() {{ local(); }}\n"));
        }
        let mut data = callgraph::build_file_data_from_source(Path::new(file), &source).unwrap();
        data.calls_by_symbol
            .insert("caller_only".into(), Vec::new());
        data.exported_symbols.push("nested::export_only".into());
        let index = ProjectIndex::from_parts(
            Path::new("/fixture"),
            HashMap::new(),
            HashMap::from([(file.to_string(), &data)]),
            WorkspaceCratePrefixCache::default(),
            Rc::new(DiskFacts::new(Path::new("/fixture"))),
        );
        assert_eq!(
            rust_unique_file_level_function(&index, file, "local", "local", &data),
            Some(("resolved_local".into(), file.into(), "local".into()))
        );
        assert!(
            rust_unique_file_level_function(&index, file, "ambiguous", "ambiguous", &data)
                .is_none()
        );
        assert_eq!(index.local_symbol(file, &data, "ambiguous"), None);
        assert_eq!(
            index.local_symbol(file, &data, "nested::ambiguous"),
            Some("nested::ambiguous".into())
        );
        let queries = data
            .symbol_metadata
            .keys()
            .chain(data.calls_by_symbol.keys())
            .chain(data.exported_symbols.iter())
            .flat_map(|name| [name.clone(), name.rsplit("::").next().unwrap().to_string()])
            .chain(["missing".into(), "caller_only".into(), "export_only".into()])
            .collect::<Vec<_>>();
        let started = Instant::now();
        let expected = queries
            .iter()
            .map(|query| {
                callgraph::resolve_symbol_query_in_data(&data, Path::new(file), query).ok()
            })
            .collect::<Vec<_>>();
        let scan_elapsed = started.elapsed();
        let started = Instant::now();
        for _ in 0..8 {
            for (query, expected) in queries.iter().zip(&expected) {
                assert_eq!(&index.local_symbol(file, &data, query), expected, "{query}");
            }
        }
        let memo_elapsed = started.elapsed();
        assert_eq!(index.local_symbols.borrow().builds, 1);
        let other =
            callgraph::build_file_data_from_source(Path::new("other.rs"), "fn other() {}").unwrap();
        assert_eq!(index.local_symbol("other.rs", &other, "local"), None);
        assert_eq!(
            index.local_symbol("other.rs", &other, "other"),
            Some("other".into())
        );
        assert_eq!(index.local_symbols.borrow().builds, 2);
        // A later cold-build window can reparse the same caller path.
        index.local_symbols.borrow_mut().file = None;
        assert_eq!(index.local_symbol("other.rs", &data, "other"), None);
        assert_eq!(
            index.local_symbol("other.rs", &data, "local"),
            Some("local".into())
        );
        assert_eq!(index.local_symbols.borrow().builds, 3);
        eprintln!(
            "local symbol queries={} scan_ns_per_query={} memo_ns_per_query={} builds=1",
            queries.len(),
            scan_elapsed.as_nanos() / queries.len() as u128,
            memo_elapsed.as_nanos() / (queries.len() * 8) as u128,
        );
    }
}

#[cfg(test)]
thread_local! {
    /// Lets a measurement compare the memoized lookups with the scans on one
    /// real corpus, in the same process.
    pub(crate) static SCAN_WITHOUT_CRATE_MEMO: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

#[cfg(test)]
mod rust_module_work {
    use super::*;

    #[derive(Debug, Default)]
    pub(super) struct Work {
        pub resolved_refs: usize,
        pub resolved_files: BTreeSet<String>,
        pub parent_lookups: usize,
        pub parent_scans: usize,
        pub parent_file_visits: usize,
        pub inline_file_visits: usize,
        pub caller_requests: usize,
        pub caller_walks: usize,
        pub parent_time: Duration,
        pub caller_time: Duration,
    }

    thread_local! {
        static ACTIVE: RefCell<Option<Work>> = const { RefCell::new(None) };
        static DISABLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    pub(super) fn disabled() -> bool {
        DISABLED.with(std::cell::Cell::get)
    }

    pub(super) fn note(update: impl FnOnce(&mut Work)) {
        ACTIVE.with(|active| {
            if let Some(work) = active.borrow_mut().as_mut() {
                update(work);
            }
        });
    }

    pub(super) struct Timing {
        started: Option<Instant>,
        parent: bool,
    }

    impl Timing {
        pub fn parent() -> Self {
            note(|work| work.parent_lookups += 1);
            Self::start(true)
        }

        pub fn caller() -> Self {
            note(|work| work.caller_requests += 1);
            Self::start(false)
        }

        fn start(parent: bool) -> Self {
            Self {
                started: ACTIVE.with(|active| active.borrow().is_some().then(Instant::now)),
                parent,
            }
        }
    }

    impl Drop for Timing {
        fn drop(&mut self) {
            if let Some(started) = self.started {
                let elapsed = started.elapsed();
                note(|work| {
                    if self.parent {
                        work.parent_time += elapsed;
                    } else {
                        work.caller_time += elapsed;
                    }
                });
            }
        }
    }
}

/// Answers to the two Rust lookups that otherwise scan every indexed file on
/// each call (`module_parent`, `inline_scoped_target`), precomputed from an
/// in-memory file set. Lookups return exactly what the scans return.
#[derive(Clone, Debug, Default)]
struct CrateScanMemo {
    /// Module prefixes derived from Rust source-file paths, in scan order.
    inline_candidates: RustInlineCandidates,
    /// For each declared module target, the smallest `(declaring file,
    /// module name)` pair, which is the scan's first match after sorting.
    module_parents: HashMap<String, (String, String)>,
}

impl CrateScanMemo {
    fn build(files: &HashMap<String, DbFileIndex>) -> Self {
        let mut rust_files = files
            .iter()
            .filter(|(_, index)| index.lang == Some(LangId::Rust))
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        rust_files.sort();
        let mut module_parents = HashMap::<String, (String, String)>::new();
        for (file, index) in files {
            Self::note_module_parents(&mut module_parents, file, index);
        }
        Self {
            inline_candidates: RustInlineCandidates::build(&rust_files),
            module_parents,
        }
    }

    fn note_module_parents(
        parents: &mut HashMap<String, (String, String)>,
        file: &str,
        index: &DbFileIndex,
    ) {
        for (module, target) in &index.declared_module_targets {
            let Some(target) = target else {
                continue;
            };
            let candidate = (file.to_string(), module.clone());
            let best = parents
                .entry(target.clone())
                .or_insert_with(|| candidate.clone());
            if candidate < *best {
                *best = candidate;
            }
        }
    }
}

/// Resolution reads symbols and exports through one interface. Incremental
/// refreshes use the in-memory index, while cold builds query only the rows
/// needed by the active caller from SQLite.
pub(crate) trait ResolverIndex {
    fn macro_use_modules(&self, file: &str) -> Vec<String>;
    fn go_package_files(&self, caller_file: &str, module: Option<&str>) -> Vec<String>;
    // Enumerate package files without building or reading a cached package
    // index. Cached adapters override this to avoid rebuilding oversized
    // indexes during symbol scans and to allow independent membership checks.
    fn uncached_go_package_files(&self, caller_file: &str, module: Option<&str>) -> Vec<String> {
        self.go_package_files(caller_file, module)
    }
    fn go_symbol_target(
        &self,
        caller_file: &str,
        module: Option<&str>,
        symbol: &str,
    ) -> Option<String> {
        go_symbol_target_by_scan(self, caller_file, module, symbol)
    }
    fn caller_data(&self, file: &str) -> Option<&FileCallData>;
    fn local_symbol(&self, file: &str, data: &FileCallData, query: &str) -> Option<String>;
    fn lang_for(&self, file: &str) -> Option<LangId>;
    fn module_target(&self, caller_file: &str, module_path: &str) -> Option<String>;
    fn module_parent(&self, target_file: &str) -> Option<(String, String)>;
    fn rust_caller_module_segments(&self, caller_file: &str) -> Vec<String> {
        rust_caller_module_segments_uncached(self, caller_file)
    }
    /// The module names each `use` in `data` binds, one entry per import in
    /// order. Resolution asks for them once per qualified reference, so an
    /// index that resolves one file's references can keep them.
    fn rust_module_aliases(&self, data: &FileCallData) -> std::rc::Rc<[RustModuleAliases]> {
        rust_module_aliases_of(data)
    }
    fn reexports_for(&self, file: &str) -> Vec<ReexportIndex>;
    fn node_for_symbol(&self, file: &str, symbol: &str) -> Option<String>;
    fn node_is_callable(&self, file: &str, node_id: &str) -> bool;
    fn export_alias(&self, file: &str, symbol: &str) -> Option<String>;
    fn has_export(&self, file: &str, symbol: &str) -> bool;
    fn default_export(&self, file: &str) -> Option<String>;
    fn contains_file(&self, file: &str) -> bool;
    fn crate_src_prefix(&self, crate_name: &str) -> Option<String>;
    fn rust_crate_root_file(&self, caller_file: &str) -> Option<String>;
    fn inline_scoped_target(
        &self,
        caller_file: &str,
        module_segments: &[String],
        short_name: &str,
    ) -> Option<(String, String)>;
}

impl ResolverIndex for ProjectIndex<'_> {
    fn macro_use_modules(&self, file: &str) -> Vec<String> {
        self.file_index(file)
            .map(|index| index.macro_use_module_targets.iter().cloned().collect())
            .unwrap_or_default()
    }
    fn go_package_files(&self, caller_file: &str, module: Option<&str>) -> Vec<String> {
        let paths = FactPaths {
            root: &self.project_root,
            facts: self.facts.as_ref(),
        };
        if let Some(package) = self.go_package(&paths, caller_file, module) {
            return package.files_for(caller_file);
        }
        self.uncached_go_package_files(caller_file, module)
    }
    fn uncached_go_package_files(&self, caller_file: &str, module: Option<&str>) -> Vec<String> {
        let paths = FactPaths {
            root: &self.project_root,
            facts: self.facts.as_ref(),
        };
        go_package_files(&paths, &self.workspace_crate_prefixes, caller_file, module)
            .into_iter()
            .filter(|file| self.contains_file(file))
            .collect()
    }
    fn go_symbol_target(
        &self,
        caller_file: &str,
        module: Option<&str>,
        symbol: &str,
    ) -> Option<String> {
        let paths = FactPaths {
            root: &self.project_root,
            facts: self.facts.as_ref(),
        };
        if let Some(package) = self.go_package(&paths, caller_file, module) {
            return package.target(caller_file, module.is_some(), symbol);
        }
        go_symbol_target_by_scan(self, caller_file, module, symbol)
    }
    fn caller_data(&self, file: &str) -> Option<&FileCallData> {
        self.caller_data.get(file).copied()
    }

    fn local_symbol(&self, file: &str, data: &FileCallData, query: &str) -> Option<String> {
        self.local_symbols.borrow_mut().resolve(file, data, query)
    }

    fn lang_for(&self, file: &str) -> Option<LangId> {
        self.lang_for(file)
    }

    fn module_target(&self, caller_file: &str, module_path: &str) -> Option<String> {
        self.module_target(caller_file, module_path)
    }

    fn module_parent(&self, target_file: &str) -> Option<(String, String)> {
        #[cfg(test)]
        let _timing = rust_module_work::Timing::parent();
        if self.lazy.is_some() {
            #[cfg(test)]
            if rust_module_work::disabled() {
                return self.module_parent_by_scan(target_file);
            }
            return self.lazy_module_parents().get(target_file).cloned();
        }
        match self.crate_scans() {
            Some(memo) => memo.module_parents.get(target_file).cloned(),
            None => self.module_parent_by_scan(target_file),
        }
    }

    fn rust_caller_module_segments(&self, caller_file: &str) -> Vec<String> {
        #[cfg(test)]
        if rust_module_work::disabled() {
            return rust_caller_module_segments_uncached(self, caller_file);
        }
        if let Some(path) = self.rust_caller_paths.borrow().get(caller_file) {
            return path.clone();
        }
        let path = rust_caller_module_segments_uncached(self, caller_file);
        self.rust_caller_paths
            .borrow_mut()
            .insert(caller_file.to_string(), path.clone());
        path
    }

    fn reexports_for(&self, file: &str) -> Vec<ReexportIndex> {
        self.reexports_for(file)
    }

    fn node_for_symbol(&self, file: &str, symbol: &str) -> Option<String> {
        self.node_for_symbol(file, symbol)
    }

    fn node_is_callable(&self, file: &str, node_id: &str) -> bool {
        self.node_is_callable(file, node_id)
    }

    fn export_alias(&self, file: &str, symbol: &str) -> Option<String> {
        self.file_index(file)
            .and_then(|item| item.export_aliases.get(symbol).cloned())
    }

    fn has_export(&self, file: &str, symbol: &str) -> bool {
        self.file_index(file)
            .is_some_and(|item| item.exports.contains(symbol))
    }

    fn default_export(&self, file: &str) -> Option<String> {
        self.file_index(file)
            .and_then(|item| item.default_export.clone())
    }

    fn contains_file(&self, file: &str) -> bool {
        self.files.contains_key(file)
            || self
                .lazy
                .as_ref()
                .is_some_and(|lazy| lazy.contains_file(file))
    }

    fn crate_src_prefix(&self, crate_name: &str) -> Option<String> {
        // The map reads every workspace Cargo.toml. Readers record its domain
        // key and the requested crate-name key, so their dependencies do not
        // depend on which reader built the map.
        self.facts.record_domain(join::VIEW_RUST_CRATES_DOMAIN);
        self.facts.record_domain(&join::rust_crate_key(crate_name));
        self.workspace_crate_prefixes
            .0
            .get_or_init(|| {
                self.facts.pause_recording();
                let prefixes = build_workspace_crate_prefixes(
                    &self.project_root,
                    &FactPaths {
                        root: &self.project_root,
                        facts: self.facts.as_ref(),
                    },
                );
                self.facts.resume_recording();
                prefixes
            })
            .get(crate_name)
            .cloned()
    }

    fn rust_crate_root_file(&self, caller_file: &str) -> Option<String> {
        let paths = FactPaths {
            root: &self.project_root,
            facts: self.facts.as_ref(),
        };
        callgraph::rust_crate_root_file_for_caller(
            &self.project_root,
            &self.project_root.join(caller_file),
            &paths,
            &self.rust_crate_roots,
        )
        .map(|path| relative_path(&self.project_root, &path))
    }

    fn inline_scoped_target(
        &self,
        caller_file: &str,
        module_segments: &[String],
        short_name: &str,
    ) -> Option<(String, String)> {
        let candidates = match self.crate_scans() {
            Some(memo) => &memo.inline_candidates,
            None => {
                #[cfg(test)]
                if SCAN_WITHOUT_CRATE_MEMO.with(std::cell::Cell::get) {
                    return self.inline_scoped_target_by_scan(
                        caller_file,
                        module_segments,
                        short_name,
                    );
                }
                self.lazy_inline_candidates.get_or_init(|| {
                    // Fresh caller extracts replace stored paths in this batch,
                    // including files whose language or module layout changed.
                    let mut paths = self.scan_paths(|lazy| lazy.rust_files());
                    paths.sort();
                    RustInlineCandidates::build(&paths)
                })
            }
        };
        self.inline_scoped_target_in(candidates, caller_file, module_segments, short_name)
    }
}

impl ProjectIndex<'_> {
    fn lazy_module_parents(&self) -> &HashMap<String, (String, String)> {
        self.lazy_module_parents.get_or_init(|| {
            #[cfg(test)]
            rust_module_work::note(|work| work.parent_scans += 1);
            let mut parents = HashMap::new();
            // scan_paths excludes stored rows replaced by this refresh's
            // extracts, including declarations that have just been removed.
            for file in self.scan_paths(|lazy| lazy.module_declaring_files()) {
                #[cfg(test)]
                rust_module_work::note(|work| work.parent_file_visits += 1);
                if let Some(index) = self.file_index(&file) {
                    CrateScanMemo::note_module_parents(&mut parents, &file, &index);
                }
            }
            parents
        })
    }

    fn go_package(
        &self,
        paths: &FactPaths<'_>,
        caller: &str,
        module: Option<&str>,
    ) -> Option<Rc<GoPackage>> {
        // Builds that record configuration facts also track filesystem reads
        // and per-file symbol/export answers to invalidate reused results later.
        // Keep those reads observable instead of hiding them behind this memo.
        if self.facts.records_config_facts() {
            return None;
        }
        cached_go_package(
            paths,
            &self.workspace_crate_prefixes,
            &self.go_packages,
            caller,
            module,
            |file| self.contains_file(file),
            |file| self.file_index(file),
        )
    }
    /// The eager crate-wide memo. Lazy indexes build their parent map and
    /// inline candidate index separately from the current caller overlay.
    fn crate_scans(&self) -> Option<&CrateScanMemo> {
        #[cfg(test)]
        if SCAN_WITHOUT_CRATE_MEMO.with(std::cell::Cell::get) {
            return None;
        }
        if self.lazy.is_some() {
            return None;
        }
        Some(
            self.crate_scans
                .get_or_init(|| CrateScanMemo::build(&self.files)),
        )
    }

    /// `module_parent` by scanning every indexed file: the smallest
    /// `(declaring file, module name)` pair whose declared target is
    /// `target_file`.
    fn module_parent_by_scan(&self, target_file: &str) -> Option<(String, String)> {
        #[cfg(test)]
        rust_module_work::note(|work| work.parent_scans += 1);
        let mut parents = self
            .scan_paths(|lazy| lazy.module_declaring_files())
            .into_iter()
            .filter_map(|file| {
                #[cfg(test)]
                rust_module_work::note(|work| work.parent_file_visits += 1);
                let index = self.file_index(&file)?;
                let modules = index
                    .declared_module_targets
                    .iter()
                    .filter(|(_, target)| target.as_deref() == Some(target_file))
                    .map(|(module, _)| (file.clone(), module.clone()))
                    .collect::<Vec<_>>();
                Some(modules)
            })
            .flatten()
            .collect::<Vec<_>>();
        parents.sort();
        parents.into_iter().next()
    }

    /// Tries the caller first, then files whose path-derived module segments
    /// prefix the requested module, in global path order. The candidate index
    /// belongs to this eager or lazy resolver snapshot.
    fn inline_scoped_target_in(
        &self,
        memo: &RustInlineCandidates,
        caller_file: &str,
        module_segments: &[String],
        short_name: &str,
    ) -> Option<(String, String)> {
        let caller = memo
            .by_path
            .get(caller_file)
            .map(|ordinal| &memo.files[*ordinal]);
        let fallback_prefix;
        let src_prefix = if let Some(file) = caller {
            file.src_prefix.as_str()
        } else {
            fallback_prefix = rust_src_prefix(caller_file);
            &fallback_prefix
        };
        let check = |file: &RustInlineFile| {
            #[cfg(test)]
            rust_module_work::note(|work| work.inline_file_visits += 1);
            if file.src_prefix != src_prefix
                || !module_segments.starts_with(&file.module_segments)
                || self.lang_for(&file.path) != Some(LangId::Rust)
            {
                return None;
            }
            let scoped_segments = &module_segments[file.module_segments.len()..];
            if scoped_segments.is_empty() {
                return None;
            }
            let scoped_symbol = format!("{}::{short_name}", scoped_segments.join("::"));
            self.node_for_symbol(&file.path, &scoped_symbol)
                .map(|_| (file.path.clone(), scoped_symbol))
        };
        caller.and_then(check).or_else(|| {
            memo.eligible_files(src_prefix, module_segments)
                .into_iter()
                .map(|ordinal| &memo.files[ordinal])
                .filter(|file| file.path != caller_file)
                .find_map(check)
        })
    }

    /// `inline_scoped_target` by sorting and scanning every indexed path.
    #[cfg(test)]
    fn inline_scoped_target_by_scan(
        &self,
        caller_file: &str,
        module_segments: &[String],
        short_name: &str,
    ) -> Option<(String, String)> {
        let src_prefix = rust_src_prefix(caller_file);
        // Stored files outside `files` are limited to Rust ones up front; the
        // language check below still applies to every path.
        let mut file_paths = self.scan_paths(|lazy| lazy.rust_files());
        file_paths.sort();
        if let Some(position) = file_paths.iter().position(|file| file == caller_file) {
            let caller = file_paths.remove(position);
            file_paths.insert(0, caller);
        }
        for file_path in file_paths {
            rust_module_work::note(|work| work.inline_file_visits += 1);
            // Path checks come before the language check so that a stored
            // file's index is loaded only when its module path can match.
            if rust_src_prefix(&file_path) != src_prefix {
                continue;
            }
            let file_module_segments = rust_module_segments_for_rel(&file_path);
            if !module_segments.starts_with(&file_module_segments)
                || self.lang_for(&file_path) != Some(LangId::Rust)
            {
                continue;
            }
            let scoped_segments = &module_segments[file_module_segments.len()..];
            if scoped_segments.is_empty() {
                continue;
            }
            let scoped_symbol = format!("{}::{short_name}", scoped_segments.join("::"));
            if self.node_for_symbol(&file_path, &scoped_symbol).is_some() {
                return Some((file_path, scoped_symbol));
            }
        }
        None
    }
}

#[derive(Clone, Debug)]
struct RustInlineFile {
    path: String,
    src_prefix: String,
    module_segments: Vec<String>,
}

impl RustInlineFile {
    fn new(path: &str) -> Self {
        Self {
            path: path.to_string(),
            src_prefix: rust_src_prefix(path),
            module_segments: rust_module_segments_for_rel(path),
        }
    }
}

#[derive(Clone, Debug, Default)]
struct RustInlineCandidates {
    files: Vec<RustInlineFile>,
    by_path: HashMap<String, usize>,
    by_prefix: HashMap<String, HashMap<Vec<String>, Vec<usize>>>,
}

impl RustInlineCandidates {
    fn build(paths: &[String]) -> Self {
        let mut index = Self::default();
        for path in paths {
            let file = RustInlineFile::new(path);
            let ordinal = index.files.len();
            index.by_path.insert(path.clone(), ordinal);
            index
                .by_prefix
                .entry(file.src_prefix.clone())
                .or_default()
                .entry(file.module_segments.clone())
                .or_default()
                .push(ordinal);
            index.files.push(file);
        }
        index
    }

    fn eligible_files(&self, src_prefix: &str, module_segments: &[String]) -> Vec<usize> {
        let mut files = Vec::new();
        if let Some(prefixes) = self.by_prefix.get(src_prefix) {
            // An equal physical path has no inline suffix and is left to the
            // module-file fallback. Only proper prefixes can contain the target.
            for len in 0..module_segments.len() {
                if let Some(ordinals) = prefixes.get(&module_segments[..len]) {
                    files.extend_from_slice(ordinals);
                }
            }
        }
        // Prefix depth must not change the old global path-order preference.
        files.sort_unstable();
        files
    }
}

impl<T: CallGraphRead + ?Sized> CallGraphRead for Arc<T> {
    fn stored_content_hash(&self, file: &str) -> Result<Option<String>> {
        (**self).stored_content_hash(file)
    }
    fn project_root(&self) -> &Path {
        (**self).project_root()
    }
    fn project_key(&self) -> &str {
        (**self).project_key()
    }
    fn sqlite_path(&self) -> &Path {
        (**self).sqlite_path()
    }
    fn is_current(&self) -> bool {
        (**self).is_current()
    }
    fn edge_snapshot(&self) -> Result<BTreeSet<StoredEdge>> {
        (**self).edge_snapshot()
    }
    fn indexed_file_count(&self) -> Result<usize> {
        (**self).indexed_file_count()
    }
    fn node_for(&self, file_rel: &Path, symbol: &str) -> Result<StoreNode> {
        (**self).node_for(file_rel, symbol)
    }
    fn nodes_for(&self, file_rel: &Path, symbol: &str) -> Result<Vec<StoreNode>> {
        (**self).nodes_for(file_rel, symbol)
    }
    fn nodes_for_stored(&self, file: &str, symbol: &str) -> Result<Vec<StoreNode>> {
        (**self).nodes_for_stored(file, symbol)
    }
    fn nodes_matching(&self, symbol: &str) -> Result<Vec<StoreNode>> {
        (**self).nodes_matching(symbol)
    }
    fn direct_callers_of(&self, file_rel: &Path, symbol: &str) -> Result<Vec<StoreCallSite>> {
        (**self).direct_callers_of(file_rel, symbol)
    }
    fn direct_callers_for_symbols(
        &self,
        targets: &[(String, String)],
    ) -> Result<HashMap<(String, String), Vec<StoreCallSite>>> {
        (**self).direct_callers_for_symbols(targets)
    }
    fn direct_caller_counts_of(
        &self,
        targets: &[(String, String)],
    ) -> Result<HashMap<(String, String), usize>> {
        (**self).direct_caller_counts_of(targets)
    }
    fn callers_of(
        &self,
        file_rel: &Path,
        symbol: &str,
        depth: usize,
    ) -> Result<StoreCallersResult> {
        (**self).callers_of(file_rel, symbol, depth)
    }
    fn impact_of(&self, file_rel: &Path, symbol: &str, depth: usize) -> Result<StoreImpactResult> {
        (**self).impact_of(file_rel, symbol, depth)
    }
    fn outgoing_calls_of(&self, node: &StoreNode) -> Result<Vec<StoreCallSite>> {
        (**self).outgoing_calls_of(node)
    }
    fn outgoing_calls_for_symbols(
        &self,
        sources: &[(String, String)],
    ) -> Result<HashMap<(String, String), Vec<StoreCallSite>>> {
        (**self).outgoing_calls_for_symbols(sources)
    }
    fn resolved_self_calls_of(&self, node: &StoreNode) -> Result<Vec<StoreCallSite>> {
        (**self).resolved_self_calls_of(node)
    }
    fn unresolved_calls_of(&self, node: &StoreNode) -> Result<Vec<StoreUnresolvedCall>> {
        (**self).unresolved_calls_of(node)
    }
    fn ref_sites_named(
        &self,
        kind: &str,
        short_name: &str,
        limit: usize,
    ) -> Result<Vec<StoreRefSite>> {
        (**self).ref_sites_named(kind, short_name, limit)
    }
    fn macro_template_calls_of(&self, node: &StoreNode) -> Result<Vec<StoreUnresolvedCall>> {
        (**self).macro_template_calls_of(node)
    }
    fn unresolved_method_sites_named(
        &self,
        name: &str,
        language: LangId,
    ) -> Result<Vec<StoreRefSite>> {
        (**self).unresolved_method_sites_named(name, language)
    }
    fn unresolved_call_site_count(&self, language: LangId, include_tests: bool) -> Result<usize> {
        (**self).unresolved_call_site_count(language, include_tests)
    }
    fn unresolved_call_site_count_named(
        &self,
        name: &str,
        language: LangId,
        include_tests: bool,
    ) -> Result<usize> {
        (**self).unresolved_call_site_count_named(name, language, include_tests)
    }
    fn call_tree(
        &self,
        file_rel: &Path,
        symbol: &str,
        depth: usize,
    ) -> Result<callgraph::CallTreeNode> {
        (**self).call_tree(file_rel, symbol, depth)
    }
    fn trace_to(
        &self,
        file_rel: &Path,
        symbol: &str,
        max_depth: usize,
    ) -> Result<callgraph::TraceToResult> {
        (**self).trace_to(file_rel, symbol, max_depth)
    }
    fn trace_to_symbol_candidates(&self, to_symbol: &str) -> Result<Vec<TraceToSymbolCandidate>> {
        (**self).trace_to_symbol_candidates(to_symbol)
    }
    fn trace_to_symbol(
        &self,
        file_rel: &Path,
        symbol: &str,
        to_symbol: &str,
        to_file: Option<&Path>,
        max_depth: usize,
    ) -> Result<callgraph::TraceToSymbolResult> {
        (**self).trace_to_symbol(file_rel, symbol, to_symbol, to_file, max_depth)
    }
}

/// A node's id: its class prefix and the first 64 bits of a BLAKE3 digest of
/// its file, kind and scoped name, as 16 hex digits.
///
/// Node ids are repeated in every ref, edge and index that names a caller or a
/// target, so their length is a large share of the store. 64 bits keep them
/// unique in practice; the store's insert and update triggers refuse two
/// different symbols with the same id (`initialize_schema`) rather than merging
/// them. Hex digits of a prefix sort as the full digest does, so ordering by id
/// is unchanged.
fn node_id(rel_path: &str, kind: &str, scoped_name: &str) -> String {
    if scoped_name == TOP_LEVEL_SYMBOL {
        return format!("top:{}", short_hash_hex(blake3::hash(rel_path.as_bytes())));
    }
    // Length-delimited fields avoid aliasing paths or names containing colons.
    let mut hash = blake3::Hasher::new();
    for field in [rel_path, kind, scoped_name] {
        hash.update(&(field.len() as u64).to_le_bytes());
        hash.update(field.as_bytes());
    }
    format!("pos:symbol:{}", short_hash_hex(hash.finalize()))
}

/// A views store's node id: `vp` plus 16 hex digits for a symbol, `vt` plus
/// 16 hex digits for a file's top level.
///
/// The digests are `node_id`'s. Views stores never hold `node_id`'s longer
/// spellings, so the short class letters only have to keep the two classes
/// apart and in the same order: symbol ids still sort before top-level ids, and
/// ids of one class still sort by digest.
pub(crate) fn view_node_id(rel_path: &str, kind: &str, scoped_name: &str) -> String {
    let id = node_id(rel_path, kind, scoped_name);
    match (id.strip_prefix("pos:symbol:"), id.strip_prefix("top:")) {
        (Some(digest), _) => format!("vp{digest}"),
        (_, Some(digest)) => format!("vt{digest}"),
        _ => unreachable!("node_id returns a symbol or top-level id"),
    }
}

fn build_import_refs(
    project_root: &Path,
    abs_path: &Path,
    rel_path: &str,
    imports: &[ImportStatement],
    line_index: &LineIndex,
    memo: &callgraph::ModuleResolutionMemo,
    facts: &FactPaths<'_>,
) -> Vec<RawRef> {
    let mut refs = Vec::new();
    for (index, import) in imports.iter().enumerate() {
        let import_kind = import_kind_label(import.kind).to_string();
        let local_name = import_local_names(import).join(",");
        let requested_name = import_requested_names(import).join(",");
        let ref_id = ref_id(&[
            rel_path,
            "import",
            &import.byte_range.start.to_string(),
            &import.byte_range.end.to_string(),
            &import.module_path,
            &index.to_string(),
        ]);
        refs.push(RawRef {
            ref_id,
            caller_symbol: None,
            caller_file: rel_path.to_string(),
            kind: "import".to_string(),
            short_name: None,
            full_ref: Some(import.raw_text.clone()),
            module_path: Some(import.module_path.clone()),
            import_kind: Some(import_kind),
            local_name: empty_to_none(local_name),
            requested_name: empty_to_none(requested_name),
            namespace_alias: import.namespace_import.clone(),
            wildcard: import_is_wildcard(import),
            line: line_index.byte_to_line(import.byte_range.start),
            byte_start: import.byte_range.start,
            byte_end: import.byte_range.end,
            dependencies: module_dependencies_with_memo(
                project_root,
                abs_path,
                &import.module_path,
                memo,
                facts,
            ),
        });
    }
    refs
}

fn rust_declared_module_target(
    project_root: &Path,
    caller_file: &str,
    module_name: &str,
    memo: &callgraph::ModuleResolutionMemo,
    facts: &FactPaths<'_>,
) -> Option<String> {
    memo.rust_declared_module_target(caller_file, module_name, || {
        rust_declared_module_targets(project_root, caller_file, facts)
    })
}

fn rust_declared_module_targets(
    project_root: &Path,
    caller_file: &str,
    facts: &FactPaths<'_>,
) -> HashMap<String, Option<String>> {
    let declaring_file = project_root.join(caller_file);
    let Ok(source) = crate::project_content::read_to_string(&declaring_file) else {
        return HashMap::new();
    };
    let Ok(tree) = parse_source_with_cached_parser(&declaring_file, &source, LangId::Rust) else {
        return HashMap::new();
    };
    let mut targets = HashMap::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.kind() == "mod_item"
            && node
                .named_children(&mut node.walk())
                .all(|child| child.kind() != "declaration_list")
        {
            if let Some(name) = node.child_by_field_name("name") {
                let module_name = node_text(name, &source);
                let target = rust_external_module_target(
                    &declaring_file,
                    rust_module_path_override(&source, node),
                    module_name,
                    facts,
                )
                .map(|target| {
                    relative_path(
                        project_root,
                        &facts.canonical(&target).unwrap_or(target.clone()),
                    )
                });
                targets.entry(module_name.to_string()).or_insert(target);
            }
        }
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                stack.push(cursor.node());
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
    }
    targets
}

fn rust_external_module_target(
    declaring_file: &Path,
    path_override: Option<&str>,
    module_name: &str,
    facts: &FactPaths<'_>,
) -> Option<PathBuf> {
    let parent = declaring_file.parent()?;
    if let Some(path) = path_override {
        let candidate = parent.join(path);
        return facts.is_file(&candidate).then_some(candidate);
    }

    let stem = declaring_file.file_stem().and_then(|stem| stem.to_str())?;
    let declaring_file = facts
        .canonical(declaring_file)
        .unwrap_or_else(|| declaring_file.to_path_buf());
    let is_crate_root = callgraph::rust_crate_root_file_for_caller(
        facts.root,
        &declaring_file,
        facts,
        &callgraph::RustCrateRootMemo::default(),
    )
    .as_ref()
        == Some(&declaring_file);
    let module_dir = if matches!(stem, "lib" | "main" | "mod") || is_crate_root {
        parent.to_path_buf()
    } else {
        parent.join(stem)
    };
    [
        module_dir.join(format!("{module_name}.rs")),
        module_dir.join(module_name).join("mod.rs"),
    ]
    .into_iter()
    .find(|candidate| facts.is_file(candidate))
}

fn rust_module_path_override<'a>(source: &'a str, module: Node<'_>) -> Option<&'a str> {
    rust_module_path_override_with(source, module, Direct)
}

/// The `#[path = "..."]` attribute before a `mod` declaration.
fn rust_module_path_override_with<'a, 'tree>(
    source: &'a str,
    module: Node<'tree>,
    nav: impl Nav<'tree>,
) -> Option<&'a str> {
    let mut previous = nav.prev_sibling(module);
    while let Some(attribute) = previous {
        if nav.kind(attribute) != "attribute_item" {
            break;
        }
        if let Some(path) = rust_path_attribute(source.get(attribute.byte_range())?) {
            return Some(path);
        }
        previous = nav.prev_sibling(attribute);
    }
    None
}

pub(super) fn rust_module_has_macro_use(source: &str, module: Node<'_>) -> bool {
    rust_module_has_macro_use_with(source, module, Direct)
}

/// Whether a `#[macro_use]` attribute precedes a `mod` declaration.
fn rust_module_has_macro_use_with<'tree>(
    source: &str,
    module: Node<'tree>,
    nav: impl Nav<'tree>,
) -> bool {
    let mut previous = nav.prev_sibling(module);
    while let Some(attribute) = previous {
        if matches!(nav.kind(attribute), "line_comment" | "block_comment") {
            previous = nav.prev_sibling(attribute);
            continue;
        }
        if nav.kind(attribute) != "attribute_item" {
            break;
        }
        if node_text(attribute, source)
            .trim()
            .strip_prefix("#[")
            .and_then(|body| body.strip_suffix(']'))
            .is_some_and(|body| body.trim() == "macro_use")
        {
            return true;
        }
        previous = nav.prev_sibling(attribute);
    }
    false
}

fn rust_path_attribute(attribute: &str) -> Option<&str> {
    let body = attribute.trim().strip_prefix("#[")?.strip_suffix(']')?;
    let (name, value) = body.split_once('=')?;
    (name.trim() == "path")
        .then(|| value.trim().trim_matches('"'))
        .filter(|path| !path.is_empty())
}

fn extend_rust_imports_from_tree(source: &str, root: Node<'_>, data: &mut FileCallData) {
    let mut seen = HashSet::new();
    let mut nested_imports = Vec::new();
    collect_rust_use_imports(source, root, &mut seen, &mut nested_imports);
    if nested_imports.is_empty() {
        return;
    }

    // Replace use declarations with their leaf bindings once, at extraction.
    // Other import forms retain their original representation.
    data.import_block
        .imports
        .retain(|import| !seen.contains(&(import.byte_range.start, import.byte_range.end)));
    data.import_block.imports.extend(nested_imports);
    data.import_block
        .imports
        .sort_by_key(|import| import.byte_range.start);
    data.import_block.byte_range = import_byte_range_from_imports(&data.import_block.imports);
}

/// Like [`extend_rust_imports_from_tree`], given the tree's
/// `use_declaration` nodes in preorder instead of walking it for them.
fn extend_rust_imports_from_uses<'tree>(
    source: &str,
    uses: impl Iterator<Item = Node<'tree>>,
    data: &mut FileCallData,
) {
    let mut seen = HashSet::new();
    let mut nested_imports = Vec::new();
    for node in uses {
        collect_rust_use_import(source, node, &mut seen, &mut nested_imports);
    }
    if nested_imports.is_empty() {
        return;
    }
    data.import_block
        .imports
        .retain(|import| !seen.contains(&(import.byte_range.start, import.byte_range.end)));
    data.import_block.imports.extend(nested_imports);
    data.import_block
        .imports
        .sort_by_key(|import| import.byte_range.start);
    data.import_block.byte_range = import_byte_range_from_imports(&data.import_block.imports);
}

fn collect_rust_use_imports(
    source: &str,
    node: Node<'_>,
    seen: &mut HashSet<(usize, usize)>,
    imports: &mut Vec<ImportStatement>,
) {
    if node.kind() == "use_declaration" {
        collect_rust_use_import(source, node, seen, imports);
    }

    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return;
    }
    loop {
        collect_rust_use_imports(source, cursor.node(), seen, imports);
        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

/// The imports one `use` declaration binds: one per leaf of its use tree.
fn collect_rust_use_import(
    source: &str,
    node: Node<'_>,
    seen: &mut HashSet<(usize, usize)>,
    imports: &mut Vec<ImportStatement>,
) {
    let range = node.byte_range();
    if !seen.insert((range.start, range.end)) {
        return;
    }
    let Some(import) = rust_import_from_use_node(source, node) else {
        return;
    };
    let paths = node
        .child_by_field_name("argument")
        .map(|argument| rust_use_leaf_paths(source, argument))
        .unwrap_or_default();
    if paths.is_empty() {
        imports.push(import);
        return;
    }
    for (path, range) in paths {
        let mut leaf = import.clone();
        leaf.byte_range = range;
        leaf.module_path = path.clone();
        leaf.raw_text = format!(
            "{}use {path};",
            leaf.default_import
                .as_ref()
                .map(|v| format!("{v} "))
                .unwrap_or_default()
        );
        leaf.names = Vec::new();
        if let ImportForm::RustUse { named, .. } = &mut leaf.form {
            named.clear();
        }
        imports.push(leaf);
    }
}

fn rust_import_from_use_node(source: &str, node: Node<'_>) -> Option<ImportStatement> {
    let raw_text = source[node.byte_range()].to_string();
    let body = rust_use_body(&raw_text)?.to_string();
    let visibility = rust_use_visibility(&raw_text);
    let names = rust_use_list_names(&body);
    let group = classify_rust_import_group(&body);
    let byte_range = node.byte_range();

    Some(ImportStatement {
        module_path: body,
        names: names.clone(),
        default_import: visibility.clone(),
        namespace_import: None,
        kind: ImportKind::Value,
        group,
        byte_range,
        raw_text,
        form: ImportForm::RustUse {
            visibility,
            named: names,
        },
    })
}

fn rust_use_leaf_paths(source: &str, root: Node<'_>) -> Vec<(String, std::ops::Range<usize>)> {
    fn path_text(source: &str, node: Node<'_>) -> String {
        if node.kind() == "scoped_identifier" {
            let prefix = node
                .child_by_field_name("path")
                .map(|path| path_text(source, path))
                .unwrap_or_default();
            let name = node
                .child_by_field_name("name")
                .map(|name| node_text(name, source))
                .unwrap_or_default();
            format!("{prefix}::{name}")
        } else {
            node_text(node, source).to_string()
        }
    }
    fn walk(
        source: &str,
        node: Node<'_>,
        prefix: &str,
        paths: &mut Vec<(String, std::ops::Range<usize>)>,
    ) {
        if node.kind() == "scoped_use_list" {
            let path = node
                .child_by_field_name("path")
                .map(|path| path_text(source, path))
                .unwrap_or_default();
            let joined = if prefix.is_empty() {
                path
            } else {
                format!("{prefix}::{path}")
            };
            if let Some(list) = node.child_by_field_name("list") {
                walk(source, list, &joined, paths);
            }
        } else if node.kind() == "use_list" {
            for child in node.named_children(&mut node.walk()) {
                if !matches!(child.kind(), "line_comment" | "block_comment") {
                    walk(source, child, prefix, paths);
                }
            }
        } else {
            let (path, alias) = if node.kind() == "use_as_clause" {
                (
                    node.child_by_field_name("path")
                        .map(|p| path_text(source, p))
                        .unwrap_or_default(),
                    node.child_by_field_name("alias")
                        .map(|a| node_text(a, source)),
                )
            } else {
                (path_text(source, node), None)
            };
            let mut joined = if path == "self" && !prefix.is_empty() {
                prefix.to_string()
            } else if prefix.is_empty() {
                path
            } else {
                format!("{prefix}::{path}")
            };
            if let Some(alias) = alias {
                joined.push_str(" as ");
                joined.push_str(alias);
            }
            if !joined.is_empty() {
                paths.push((joined, node.byte_range()));
            }
        }
    }
    let mut paths = Vec::new();
    walk(source, root, "", &mut paths);
    paths
}

fn import_byte_range_from_imports(imports: &[ImportStatement]) -> Option<std::ops::Range<usize>> {
    let start = imports.iter().map(|import| import.byte_range.start).min()?;
    let end = imports.iter().map(|import| import.byte_range.end).max()?;
    Some(start..end)
}

fn rust_use_visibility(raw_text: &str) -> Option<String> {
    let use_pos = raw_text.find("use ")?;
    let prefix = raw_text[..use_pos].trim();
    if prefix.is_empty() {
        None
    } else {
        Some(prefix.to_string())
    }
}

fn rust_use_body(raw_text: &str) -> Option<&str> {
    let use_pos = raw_text.find("use ")?;
    Some(raw_text[use_pos + 4..].trim().trim_end_matches(';').trim())
}

fn rust_use_list_names(body: &str) -> Vec<String> {
    let Some(open) = body.find("::{") else {
        return Vec::new();
    };
    let Some(close) = body[open + 3..].find('}').map(|offset| open + 3 + offset) else {
        return Vec::new();
    };
    body[open + 3..close]
        .split(',')
        .filter_map(|spec| {
            let spec = spec.trim();
            if spec.is_empty() {
                None
            } else {
                Some(spec.to_string())
            }
        })
        .collect()
}

fn classify_rust_import_group(body: &str) -> ImportGroup {
    let first = body
        .split("::")
        .next()
        .unwrap_or(body)
        .split_whitespace()
        .next()
        .unwrap_or(body);
    match first.trim() {
        "std" | "core" | "alloc" => ImportGroup::Stdlib,
        "crate" | "self" | "super" => ImportGroup::Internal,
        _ => ImportGroup::External,
    }
}

#[derive(Debug, Clone)]
struct ReexportRefs {
    raw_refs: Vec<RawRef>,
}

fn collect_reexport_refs_with_line_index(
    project_root: &Path,
    abs_path: &Path,
    rel_path: &str,
    source: &str,
    lang: LangId,
    memo: &callgraph::ModuleResolutionMemo,
    facts: &FactPaths<'_>,
    line_index: &LineIndex,
    python_root: Option<Node<'_>>,
) -> ReexportRefs {
    if lang == LangId::Python {
        // Reuse the caller's syntax tree when the source has already been parsed.
        let exports = if let Some(root) = python_root {
            callgraph::python::reexports(source, root)
        } else {
            let Ok(tree) = parse_source_with_cached_parser(abs_path, source, LangId::Python) else {
                return ReexportRefs {
                    raw_refs: Vec::new(),
                };
            };
            callgraph::python::reexports(source, tree.root_node())
        };
        let mut raw_refs = build_import_refs(
            project_root,
            abs_path,
            rel_path,
            &exports,
            line_index,
            memo,
            facts,
        );
        for raw in &mut raw_refs {
            let names = raw
                .local_name
                .as_deref()
                .unwrap_or_default()
                .split(',')
                .zip(raw.requested_name.as_deref().unwrap_or_default().split(','))
                .map(|(local, source)| format!("{source} as {local}"))
                .collect::<Vec<_>>()
                .join(", ");
            let statement = format!("export {{ {names} }}");
            raw.ref_id = ref_id(&["python_reexport", &raw.ref_id]);
            raw.kind = "reexport".to_string();
            raw.import_kind = Some("reexport".to_string());
            raw.full_ref = Some(statement.clone());
        }
        return ReexportRefs { raw_refs };
    }
    let mut raw_refs = Vec::new();
    let mut search_start = 0usize;
    let mut ordinal = 0usize;
    while let Some(export_offset) = source[search_start..].find("export") {
        let start = search_start + export_offset;
        let Some(statement_end_offset) = source[start..].find(';') else {
            break;
        };
        let end = start + statement_end_offset + 1;
        let statement = &source[start..end];
        search_start = end;
        if !statement.contains(" from ") || !statement.contains(['\'', '"']) {
            continue;
        }
        let Some(module_path) = quoted_module_path(statement) else {
            continue;
        };
        ordinal += 1;
        let wildcard = statement.contains('*');
        let line = line_index.byte_to_line(start);
        let ref_id = ref_id(&[
            rel_path,
            "reexport",
            &start.to_string(),
            &end.to_string(),
            &module_path,
            &ordinal.to_string(),
        ]);
        raw_refs.push(RawRef {
            ref_id,
            caller_symbol: None,
            caller_file: rel_path.to_string(),
            kind: "reexport".to_string(),
            short_name: None,
            full_ref: Some(statement.to_string()),
            module_path: Some(module_path.clone()),
            import_kind: Some("reexport".to_string()),
            local_name: None,
            requested_name: None,
            namespace_alias: None,
            wildcard,
            line,
            byte_start: start,
            byte_end: end,
            dependencies: module_dependencies_with_memo(
                project_root,
                abs_path,
                &module_path,
                memo,
                facts,
            ),
        });
    }
    ReexportRefs { raw_refs }
}

fn collect_rust_pub_use_reexport_refs(
    project_root: &Path,
    abs_path: &Path,
    rel_path: &str,
    imports: &[ImportStatement],
    line_index: &LineIndex,
    facts: &FactPaths<'_>,
) -> ReexportRefs {
    let mut raw_refs = Vec::new();
    let mut ordinal = 0usize;
    let module_memo = callgraph::ModuleResolutionMemo::default();

    for import in imports {
        let Some(visibility) = &import.default_import else {
            continue;
        };
        if !visibility.starts_with("pub") {
            continue;
        }
        let Some((module_path, named, wildcard)) = rust_pub_use_reexport_parts(import) else {
            continue;
        };
        ordinal += 1;
        let ref_id = ref_id(&[
            rel_path,
            "rust_reexport",
            &import.byte_range.start.to_string(),
            &import.byte_range.end.to_string(),
            &module_path,
            &ordinal.to_string(),
        ]);
        raw_refs.push(RawRef {
            ref_id,
            caller_symbol: None,
            caller_file: rel_path.to_string(),
            kind: "reexport".to_string(),
            short_name: None,
            full_ref: Some(rust_reexport_statement_for_index(&named, &import.raw_text)),
            module_path: Some(module_path.clone()),
            import_kind: Some("reexport".to_string()),
            local_name: None,
            requested_name: None,
            namespace_alias: None,
            wildcard,
            line: line_index.byte_to_line(import.byte_range.start),
            byte_start: import.byte_range.start,
            byte_end: import.byte_range.end,
            dependencies: rust_module_dependencies(
                project_root,
                abs_path,
                &module_path,
                &module_memo,
                facts,
            ),
        });
    }

    ReexportRefs { raw_refs }
}

fn rust_pub_use_reexport_parts(
    import: &ImportStatement,
) -> Option<(String, HashMap<String, String>, bool)> {
    let body = rust_use_body(&import.raw_text).unwrap_or(import.module_path.as_str());
    let body = body.trim();
    if let Some(module_path) = body.strip_suffix("::*") {
        return Some((module_path.trim().to_string(), HashMap::new(), true));
    }

    if let Some(brace_start) = body.find("::{") {
        rust_use_group_close(body)?;
        let module_path = body.get(..brace_start)?.trim().to_string();
        // Validate a real group, but retain the existing range, including
        // trailing comments, for output compatibility.
        let specs = body.get(brace_start + 3..body.rfind('}')?)?;
        let names = rust_reexport_names_from_specs(specs);
        if names.is_empty() {
            return None;
        }
        return Some((module_path, names, false));
    }

    let (module_path, spec) = body.rsplit_once("::")?;
    let names = rust_reexport_names_from_specs(spec);
    if names.is_empty() {
        return None;
    }
    Some((module_path.trim().to_string(), names, false))
}

fn rust_use_group_close(body: &str) -> Option<usize> {
    let bytes = body.as_bytes();
    let mut pos = 0;
    let mut depth: usize = 0;
    while let Some(&byte) = bytes.get(pos) {
        let rest = bytes.get(pos..)?;
        if rest.starts_with(b"//") {
            pos += rest.iter().position(|&b| b == b'\n').unwrap_or(rest.len());
            continue;
        }
        if rest.starts_with(b"/*") {
            pos += 2;
            let mut comments = 1;
            while comments > 0 {
                let rest = bytes.get(pos..)?;
                if rest.starts_with(b"/*") {
                    comments += 1;
                    pos += 2;
                } else if rest.starts_with(b"*/") {
                    comments -= 1;
                    pos += 2;
                } else {
                    bytes.get(pos)?;
                    pos += 1;
                }
            }
            continue;
        }
        // Raw strings may also have byte or C prefixes; scanning reaches their
        // `r` after the prefix without interpreting their contents as code.
        if byte == b'r' {
            let hashes = rest[1..].iter().take_while(|&&b| b == b'#').count();
            if rest.get(1 + hashes) == Some(&b'"') {
                pos += 2 + hashes;
                loop {
                    let &next = bytes.get(pos)?;
                    pos += 1;
                    if next == b'"' && bytes.get(pos..pos + hashes)?.iter().all(|&b| b == b'#') {
                        pos += hashes;
                        break;
                    }
                }
                continue;
            }
        }
        if byte == b'"' || (byte == b'\'' && rust_use_char_literal(body.get(pos..)?)) {
            pos += 1;
            loop {
                let &next = bytes.get(pos)?;
                pos += 1;
                if next == b'\\' {
                    bytes.get(pos)?;
                    pos += 1;
                } else if next == byte {
                    break;
                }
            }
            continue;
        }
        if depth == 0 && rest.starts_with(b"::{") {
            depth = 1;
            pos += 3;
            continue;
        }
        if depth > 0 {
            if byte == b'{' {
                depth += 1;
            } else if byte == b'}' {
                depth -= 1;
                if depth == 0 {
                    return Some(pos);
                }
            }
        }
        pos += 1;
    }
    None
}

fn rust_use_char_literal(rest: &str) -> bool {
    // Apostrophes also introduce lifetimes. A character has either an escape or
    // one UTF-8 character followed by its closing quote.
    if rest.as_bytes().get(1) == Some(&b'\\') {
        return true;
    }
    rest.strip_prefix('\'')
        .and_then(|text| text.chars().next())
        .is_some_and(|ch| rest.as_bytes().get(1 + ch.len_utf8()) == Some(&b'\''))
}

fn rust_reexport_names_from_specs(specs: &str) -> HashMap<String, String> {
    let mut names = HashMap::new();
    for spec in specs.split(',') {
        let spec = spec.trim();
        if spec.is_empty() || spec == "self" {
            continue;
        }
        if let Some((source, local)) = spec.split_once(" as ") {
            let source = source.trim();
            let local = local.trim();
            if !source.is_empty() && !local.is_empty() && source != "self" {
                names.insert(local.to_string(), source.to_string());
            }
        } else {
            names.insert(spec.to_string(), spec.to_string());
        }
    }
    names
}

fn rust_reexport_statement_for_index(named: &HashMap<String, String>, fallback: &str) -> String {
    if named.is_empty() {
        return fallback.to_string();
    }
    let mut specs = named
        .iter()
        .map(|(local, source)| {
            if local == source {
                source.clone()
            } else {
                format!("{source} as {local}")
            }
        })
        .collect::<Vec<_>>();
    specs.sort();
    format!("pub use {{{}}};", specs.join(", "))
}

fn quoted_module_path(statement: &str) -> Option<String> {
    let quote = match (statement.find('\''), statement.find('"')) {
        (Some(single), Some(double)) if single < double => '\'',
        (Some(_), Some(_)) => '"',
        (Some(_), None) => '\'',
        (None, Some(_)) => '"',
        (None, None) => return None,
    };
    let start = statement.find(quote)? + 1;
    let end = statement[start..].find(quote)? + start;
    Some(statement[start..end].to_string())
}

#[derive(Debug, Clone)]
struct SourceLessExportRefs {
    raw_refs: Vec<RawRef>,
}

fn collect_source_less_export_alias_refs_with_line_index(
    rel_path: &str,
    source: &str,
    line_index: &LineIndex,
) -> SourceLessExportRefs {
    let mut raw_refs = Vec::new();
    let mut search_start = 0usize;
    let mut ordinal = 0usize;
    while let Some(export_offset) = source[search_start..].find("export") {
        let start = search_start + export_offset;
        let Some(statement_end_offset) = source[start..].find(';') else {
            break;
        };
        let end = start + statement_end_offset + 1;
        let statement = &source[start..end];
        search_start = end;
        if statement.contains(" from ") || !statement.contains('{') || !statement.contains('}') {
            continue;
        }
        let aliases = parse_reexport_names(statement);
        if aliases.is_empty() {
            continue;
        }
        // Number the aliases in a fixed order: the ordinal is part of each ref
        // id and the parts feed the surface fingerprint, and hash-map order
        // differs between two extractions of the same file.
        let mut aliases = aliases.into_iter().collect::<Vec<_>>();
        aliases.sort();
        let line = line_index.byte_to_line(start);
        for (exported, source_symbol) in aliases {
            ordinal += 1;
            let ref_id = ref_id(&[
                rel_path,
                "export_alias",
                &start.to_string(),
                &end.to_string(),
                &exported,
                &source_symbol,
                &ordinal.to_string(),
            ]);
            raw_refs.push(RawRef {
                ref_id,
                caller_symbol: None,
                caller_file: rel_path.to_string(),
                kind: "export_alias".to_string(),
                short_name: None,
                full_ref: Some(statement.to_string()),
                module_path: None,
                import_kind: Some("export_alias".to_string()),
                local_name: Some(exported),
                requested_name: Some(source_symbol),
                namespace_alias: None,
                wildcard: false,
                line,
                byte_start: start,
                byte_end: end,
                dependencies: BTreeSet::new(),
            });
        }
    }
    SourceLessExportRefs { raw_refs }
}

fn resolve_ref<I: ResolverIndex>(raw: RawRef, index: &I) -> Result<ResolvedRef> {
    #[cfg(test)]
    rust_module_work::note(|work| {
        work.resolved_refs += 1;
        work.resolved_files.insert(raw.caller_file.clone());
    });
    if raw.kind == "import" && index.lang_for(&raw.caller_file) == Some(LangId::Go) {
        let files = raw
            .module_path
            .as_deref()
            .map(|module| index.go_package_files(&raw.caller_file, Some(module)))
            .unwrap_or_default();
        let target_file = files.first().cloned();
        let mut dependencies = raw.dependencies.clone();
        dependencies.extend(files);
        return Ok(ResolvedRef {
            target_file,
            target_symbol: None,
            dependencies,
        });
    }
    if !matches!(
        raw.kind.as_str(),
        "call" | "construction" | "callback_ref" | "value_ref" | MACRO_BODY_CALL_REF_KIND
    ) {
        return Ok(ResolvedRef {
            dependencies: raw.dependencies.clone(),
            target_file: None,
            target_symbol: None,
        });
    }

    let caller_file = raw.caller_file.clone();
    let caller_data =
        index
            .caller_data(&caller_file)
            .ok_or_else(|| CallGraphStoreError::MissingCallerData {
                file: caller_file.clone(),
            })?;
    let full_ref = raw.full_ref.as_deref().unwrap_or_default();
    let short_name = raw.short_name.as_deref().unwrap_or_default();
    let mut dependencies = raw.dependencies.clone();

    let resolved = match index.lang_for(&caller_file) {
        Some(LangId::Rust) => {
            resolve_rust_target(index, &caller_file, full_ref, short_name, caller_data, &raw)
        }
        Some(LangId::Go) => {
            resolve_go_target(index, &caller_file, full_ref, short_name, caller_data)
        }
        Some(LangId::Python) => resolve_python_target(
            index,
            &caller_file,
            full_ref,
            short_name,
            caller_data,
            &raw,
            &mut dependencies,
        ),
        Some(LangId::TypeScript | LangId::Tsx | LangId::JavaScript) => {
            resolve_js_ts_target(index, &caller_file, full_ref, short_name, caller_data)
        }
        _ => resolve_local_target(index, &caller_file, full_ref, short_name, caller_data),
    };

    let Some((_, target_file, mut target_symbol)) = resolved else {
        return Ok(ResolvedRef {
            target_file: None,
            target_symbol: None,
            dependencies,
        });
    };

    dependencies.insert(target_file.clone());
    if raw.kind == "construction" {
        let Some(node) = index.node_for_symbol(&target_file, &target_symbol) else {
            return Ok(ResolvedRef {
                target_file: None,
                target_symbol: None,
                dependencies,
            });
        };
        let constructor = format!("{target_symbol}::constructor");
        if index.node_is_callable(&target_file, &node) {
            // JavaScript functions can also be constructed; they remain callable targets.
        } else if index.node_for_symbol(&target_file, &constructor).is_some() {
            target_symbol = constructor;
        } else {
        }
    }
    let target_node = index.node_for_symbol(&target_file, &target_symbol);
    if matches!(raw.kind.as_str(), "value_ref" | "callback_ref")
        && index.lang_for(&caller_file) != Some(LangId::Go)
        && !target_node
            .as_deref()
            .is_some_and(|node_id| index.node_is_callable(&target_file, node_id))
    {
        return Ok(ResolvedRef {
            target_file: None,
            target_symbol: None,
            dependencies,
        });
    }

    Ok(ResolvedRef {
        target_file: Some(target_file),
        target_symbol: Some(target_symbol),
        dependencies,
    })
}

// Retain at most 8 MiB of conservatively charged package data per resolution
// pass, independently of the file-index memo. Evict whole packages, and use the
// original scan for oversized ones: never trim the resolver's candidates.
const GO_PACKAGE_MEMO_BYTES: usize = 8 * 1024 * 1024;
// Remember oversized package directories separately from cached indexes, using
// at most 64 KiB of conservatively charged path/set storage. If that fills up,
// stop building new indexes for this pass: otherwise an unremembered oversized
// directory could trigger the same failed construction on every reference.
const GO_PACKAGE_OVERSIZED_MEMO_BYTES: usize = 64 * 1024;

#[derive(Clone, Default)]
struct GoPackageMemo {
    packages: HashMap<PathBuf, Rc<GoPackage>>,
    order: VecDeque<(PathBuf, usize)>,
    bytes: usize,
    oversized_dirs: HashSet<PathBuf>,
    oversized_bytes: usize,
}

impl GoPackageMemo {
    fn mark_oversized(&mut self, dir: PathBuf) {
        if self.oversized_dirs.contains(&dir) {
            return;
        }
        let bytes = dir.as_os_str().len() + 4 * std::mem::size_of::<PathBuf>() + 32;
        if self.oversized_bytes + bytes > GO_PACKAGE_OVERSIZED_MEMO_BYTES {
            self.oversized_bytes = GO_PACKAGE_OVERSIZED_MEMO_BYTES;
            return;
        }
        self.oversized_bytes += bytes;
        self.oversized_dirs.insert(dir);
    }

    fn insert(&mut self, dir: PathBuf, package: Rc<GoPackage>) {
        while self.bytes + package.bytes > GO_PACKAGE_MEMO_BYTES {
            let (oldest, bytes) = self.order.pop_front().expect("nonempty package memo");
            self.packages.remove(&oldest);
            self.bytes -= bytes;
        }
        self.bytes += package.bytes;
        self.order.push_back((dir.clone(), package.bytes));
        self.packages.insert(dir, package);
    }
}

#[derive(Default)]
struct GoPackage {
    files: Vec<String>,
    symbols: HashMap<String, GoSymbolTargets>,
    bytes: usize,
}

#[derive(Default)]
struct GoSymbolTargets {
    // Slot 0 excludes _test.go files; slot 1 includes them. `exported` also
    // requires the symbol to be exported, as an imported package lookup does.
    // None means no matching file; usize::MAX means multiple matching files.
    local: [Option<usize>; 2],
    exported: [Option<usize>; 2],
}

impl GoPackage {
    fn files_for(&self, caller: &str) -> Vec<String> {
        self.files
            .iter()
            .filter(|file| caller.ends_with("_test.go") || !file.ends_with("_test.go"))
            .cloned()
            .collect()
    }

    fn target(&self, caller: &str, imported: bool, symbol: &str) -> Option<String> {
        let targets = self.symbols.get(symbol)?;
        let candidates = if imported {
            &targets.exported
        } else {
            &targets.local
        };
        self.files
            .get(candidates[usize::from(caller.ends_with("_test.go"))]?)
            .cloned()
    }

    fn add_symbols(&mut self, file: usize, index: &DbFileIndex) -> bool {
        let test_only = self.files[file].ends_with("_test.go");
        for symbol in index.node_by_scoped.keys().chain(
            index
                .node_by_bare
                .keys()
                .filter(|symbol| !index.node_by_scoped.contains_key(*symbol)),
        ) {
            if !self.symbols.contains_key(symbol) {
                // Charge spare hash buckets and string allocation overhead as
                // well as payload, so tiny symbols/packages are bounded too.
                self.bytes += symbol.len() + 4 * std::mem::size_of::<(String, GoSymbolTargets)>();
                if self.bytes > GO_PACKAGE_MEMO_BYTES {
                    return false;
                }
            }
            let targets = self.symbols.entry(symbol.clone()).or_default();
            for variant in usize::from(test_only)..2 {
                let add = |slot: &mut Option<usize>| {
                    *slot = Some(if slot.is_none() { file } else { usize::MAX });
                };
                add(&mut targets.local[variant]);
                if index.exports.contains(symbol) {
                    add(&mut targets.exported[variant]);
                }
            }
        }
        true
    }
}

fn cached_go_package<T: std::ops::Deref<Target = DbFileIndex>>(
    facts: &FactPaths<'_>,
    modules: &WorkspaceCratePrefixCache,
    memo: &RefCell<GoPackageMemo>,
    caller: &str,
    module: Option<&str>,
    contains: impl Fn(&str) -> bool,
    load: impl Fn(&str) -> Option<T>,
) -> Option<Rc<GoPackage>> {
    let Some(dir) = go_package_dir(facts, modules, caller, module) else {
        return Some(Rc::new(GoPackage::default()));
    };
    if let Some(package) = memo.borrow().packages.get(&dir) {
        return Some(Rc::clone(package));
    }
    if memo.borrow().oversized_bytes >= GO_PACKAGE_OVERSIZED_MEMO_BYTES
        || memo.borrow().oversized_dirs.contains(&dir)
    {
        return None;
    }
    #[cfg(test)]
    work_counts::note_go_package_build(&dir);
    let files = go_files_in_dir(facts, &dir, true)
        .into_iter()
        .filter(|file| contains(file))
        .collect::<Vec<_>>();
    let bytes = 512
        + 2 * dir.as_os_str().len()
        + files.capacity() * std::mem::size_of::<String>()
        + files.iter().map(|file| file.capacity() + 32).sum::<usize>();
    if bytes > GO_PACKAGE_MEMO_BYTES {
        memo.borrow_mut().mark_oversized(dir);
        return None;
    }
    let mut package = GoPackage {
        files,
        bytes,
        ..Default::default()
    };
    for file in 0..package.files.len() {
        if let Some(index) = load(&package.files[file]) {
            if !package.add_symbols(file, &index) {
                memo.borrow_mut().mark_oversized(dir);
                return None;
            }
        }
    }
    let package = Rc::new(package);
    memo.borrow_mut().insert(dir, Rc::clone(&package));
    Some(package)
}

fn go_symbol_target_by_scan<I: ResolverIndex + ?Sized>(
    index: &I,
    caller_file: &str,
    module: Option<&str>,
    symbol: &str,
) -> Option<String> {
    let mut targets = index
        .uncached_go_package_files(caller_file, module)
        .into_iter()
        .filter(|file| {
            index.node_for_symbol(file, symbol).is_some()
                && (module.is_none() || index.has_export(file, symbol))
        });
    let target = targets.next()?;
    targets.next().is_none().then_some(target)
}

fn go_package_files(
    facts: &FactPaths<'_>,
    cache: &WorkspaceCratePrefixCache,
    caller_file: &str,
    module: Option<&str>,
) -> Vec<String> {
    let Some(dir) = go_package_dir(facts, cache, caller_file, module) else {
        return Vec::new();
    };
    go_files_in_dir(facts, &dir, caller_file.ends_with("_test.go"))
}

/// The Go module map: each `go.mod`'s module path, by the folder it names,
/// from a walk of every folder of the checkout. Every reader of a Go import
/// path depends on it as a whole, through its domain key.
pub(crate) fn go_module_map(facts: &FactPaths<'_>) -> HashMap<String, String> {
    let mut modules = HashMap::new();
    let mut stack = vec![facts.root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if matches!(
            dir.file_name().and_then(|s| s.to_str()),
            Some("target" | "node_modules" | ".git")
        ) {
            continue;
        }
        let manifest = dir.join("go.mod");
        facts.config_fact(&manifest, "module");
        if let Some(bytes) = facts.attributed_bytes(&manifest) {
            if let Ok(text) = std::str::from_utf8(&bytes) {
                for line in text.lines() {
                    let mut words = line.split_whitespace();
                    if words.next() == Some("module") {
                        if let Some(name) = words.next() {
                            modules.insert(
                                name.trim_matches('"').to_string(),
                                relative_path(facts.root, &dir),
                            );
                        }
                    }
                }
            }
        }
        for entry in facts.list_dir(&dir) {
            if entry.kind == EntryKind::Directory {
                stack.push(dir.join(byte_path(&entry.name)));
            }
        }
    }
    modules
}

/// The Rust crate map: each workspace crate's name, by the prefix of its
/// source folder, from every `Cargo.toml` of the checkout. Every reader of a
/// crate path depends on it as a whole, through its domain key.
pub(crate) fn rust_crate_map(facts: &FactPaths<'_>) -> HashMap<String, String> {
    build_workspace_crate_prefixes(facts.root, facts)
}

fn go_package_dir(
    facts: &FactPaths<'_>,
    cache: &WorkspaceCratePrefixCache,
    caller_file: &str,
    module: Option<&str>,
) -> Option<PathBuf> {
    Some(if let Some(module) = module {
        // The module map is built once per index by walking every directory
        // for its go.mod. A reader records the map's domain key, not the walk:
        // the walk names every directory, and which reader happened to build
        // the map must not change what any reader records.
        facts.facts.record_domain(join::VIEW_GO_MODULES_DOMAIN);
        let modules = cache.1.get_or_init(|| {
            facts.facts.pause_recording();
            let modules = go_module_map(facts);
            facts.facts.resume_recording();
            modules
        });
        let Some((prefix, dir)) = modules
            .iter()
            .filter(|(prefix, _)| {
                module == prefix.as_str()
                    || module
                        .strip_prefix(prefix.as_str())
                        .is_some_and(|rest| rest.starts_with('/'))
            })
            .max_by_key(|(prefix, _)| prefix.len())
        else {
            return None;
        };
        facts
            .root
            .join(dir)
            .join(module[prefix.len()..].trim_start_matches('/'))
    } else {
        facts
            .root
            .join(caller_file)
            .parent()
            .unwrap_or(facts.root)
            .to_path_buf()
    })
}

fn go_files_in_dir(facts: &FactPaths<'_>, dir: &Path, include_tests: bool) -> Vec<String> {
    let mut files = facts
        .list_dir(&dir)
        .into_iter()
        .filter_map(|entry| {
            let path = dir.join(byte_path(&entry.name));
            (entry.kind == EntryKind::Regular
                && path.extension().and_then(|s| s.to_str()) == Some("go")
                && (include_tests || !path.to_string_lossy().ends_with("_test.go")))
            .then(|| relative_path(facts.root, &path))
        })
        .collect::<Vec<_>>();
    files.sort();
    files
}

fn resolve_go_target<I: ResolverIndex>(
    index: &I,
    caller_file: &str,
    full_ref: &str,
    short_name: &str,
    caller_data: &FileCallData,
) -> Option<(String, String, String)> {
    let module = if let Some((alias, member)) = full_ref.split_once('.') {
        if member != short_name || member.contains('.') {
            return None;
        }
        // An object selector is not a package-level symbol lookup. Leave
        // it for receiver-aware dispatch rather than binding its bare name.
        let import = caller_data.import_block.imports.iter().find(|import| {
            let raw_alias = import.raw_text.split('"').next().unwrap_or("").trim();
            import.default_import.as_deref().unwrap_or_else(|| {
                if !raw_alias.is_empty() {
                    raw_alias
                } else {
                    import.module_path.rsplit('/').next().unwrap_or("")
                }
            }) == alias
        })?;
        Some(import.module_path.as_str())
    } else {
        None
    };
    let target = index.go_symbol_target(caller_file, module, short_name)?;
    Some(("resolved".to_string(), target, short_name.to_string()))
}

fn resolve_python_target<I: ResolverIndex>(
    index: &I,
    caller_file: &str,
    full_ref: &str,
    short_name: &str,
    caller_data: &FileCallData,
    raw: &RawRef,
    dependencies: &mut BTreeSet<String>,
) -> Option<(String, String, String)> {
    if let Ok(binding_index) = caller_data
        .python_bindings
        .binary_search_by_key(&(raw.byte_start, raw.byte_end), |binding| {
            (binding.byte_start, binding.byte_end)
        })
    {
        let binding = &caller_data.python_bindings[binding_index];
        let (module, symbol) = binding.target.as_ref()?;
        let target_file = if module.is_empty() {
            caller_file.to_string()
        } else {
            index.module_target(caller_file, module)?
        };
        let (file, symbol) = resolve_python_symbol(index, &target_file, symbol, dependencies)?;
        return Some(("resolved".to_string(), file, symbol));
    }
    resolve_local_target(index, caller_file, full_ref, short_name, caller_data)
}

fn resolve_python_symbol<I: ResolverIndex>(
    index: &I,
    file: &str,
    symbol: &str,
    dependencies: &mut BTreeSet<String>,
) -> Option<(String, String)> {
    let mut file = file.to_string();
    let mut symbol = symbol.to_string();
    let mut visited = HashSet::new();
    loop {
        dependencies.insert(file.clone());
        if !visited.insert((file.clone(), symbol.clone())) {
            return None;
        }
        if let Some(node) = index.node_for_symbol(&file, &symbol) {
            // Python index lookups require an exact scoped name, so a bare
            // import cannot select a class method with the same name.
            if index.node_is_callable(&file, &node) {
                return Some((file, symbol));
            }
            let initializer = format!("{symbol}::__init__");
            let init_node = index.node_for_symbol(&file, &initializer)?;
            return index
                .node_is_callable(&file, &init_node)
                .then_some((file, initializer));
        }
        let mut targets = BTreeSet::new();
        for export in index.reexports_for(&file) {
            if let (Some(source), Some(target)) = (export.named.get(&symbol), export.target_file) {
                targets.insert((target, source.clone()));
            }
        }
        if targets.len() != 1 {
            return None;
        }
        (file, symbol) = targets.into_iter().next()?;
    }
}

fn resolve_js_ts_target<I: ResolverIndex>(
    index: &I,
    caller_file: &str,
    full_ref: &str,
    short_name: &str,
    caller_data: &FileCallData,
) -> Option<(String, String, String)> {
    if let Some((namespace, member)) = full_ref.split_once('.') {
        for import in &caller_data.import_block.imports {
            if import.namespace_import.as_deref() == Some(namespace) {
                if let Some(target_file) = index.module_target(caller_file, &import.module_path) {
                    if let Some((file, symbol)) =
                        resolve_exported_symbol(index, &target_file, member, 0)
                    {
                        return Some(("resolved".to_string(), file, symbol));
                    }
                }
            }
        }
    }

    for import in &caller_data.import_block.imports {
        for spec in &import.names {
            if crate::imports::specifier_local_name(spec) == short_name {
                if let Some(target_file) = index.module_target(caller_file, &import.module_path) {
                    let requested = crate::imports::specifier_imported_name(spec);
                    let (file, symbol) = resolve_exported_symbol(index, &target_file, requested, 0)
                        .unwrap_or_else(|| (target_file, requested.to_string()));
                    return Some(("resolved".to_string(), file, symbol));
                }
            }
        }

        if import.default_import.as_deref() == Some(short_name) {
            if let Some(target_file) = index.module_target(caller_file, &import.module_path) {
                let (file, symbol) = resolve_exported_symbol(index, &target_file, "default", 0)
                    .or_else(|| {
                        index
                            .default_export(&target_file)
                            .map(|symbol| (target_file.clone(), symbol))
                    })
                    .unwrap_or_else(|| {
                        let file_name = Path::new(&target_file)
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("unknown")
                            .to_string();
                        (target_file, format!("<default:{file_name}>"))
                    });
                return Some(("resolved".to_string(), file, symbol));
            }
        }
    }

    for import in &caller_data.import_block.imports {
        if let Some(target_file) = index.module_target(caller_file, &import.module_path) {
            if index.has_export(&target_file, short_name) {
                return Some(("resolved".to_string(), target_file, short_name.to_string()));
            }
        }
    }

    resolve_local_target(index, caller_file, full_ref, short_name, caller_data)
}

fn resolve_exported_symbol<I: ResolverIndex>(
    index: &I,
    file: &str,
    requested: &str,
    depth: usize,
) -> Option<(String, String)> {
    let mut visited = std::collections::HashMap::new();
    resolve_exported_symbol_inner(index, file, requested, depth, &mut visited)
}

/// Re-export graphs are frequently cyclic (barrel files re-exporting each
/// other, `pub use` cycles). The depth cap alone bounds path LENGTH, not path
/// COUNT: with wildcard fan-out the walk explores branching^depth paths and a
/// single resolution can burn CPU-minutes. The memo prunes re-visits of a
/// (file, symbol) pair — but only when the earlier visit had at least as much
/// remaining depth budget (a shallower re-visit can reach leaves the deeper
/// first visit had to cut off at the cap, so plain visited-set pruning would
/// lose resolutions the capped walk finds).
fn resolve_exported_symbol_inner<I: ResolverIndex>(
    index: &I,
    file: &str,
    requested: &str,
    depth: usize,
    visited: &mut std::collections::HashMap<(String, String), usize>,
) -> Option<(String, String)> {
    if depth > 16 {
        return None;
    }
    if requested != "default" {
        if let Some(source_symbol) = index.export_alias(file, requested) {
            if index.lang_for(file) != Some(LangId::Rust)
                || index.node_for_symbol(file, &source_symbol).is_some()
            {
                return Some((file.to_string(), source_symbol));
            }
        }
        if index.has_export(file, requested) {
            // A Rust use can export a name without declaring it in this module.
            // Follow the re-export rather than claiming this file defines the name.
            if index.lang_for(file) != Some(LangId::Rust)
                || index.node_for_symbol(file, requested).is_some()
            {
                return Some((file.to_string(), requested.to_string()));
            }
        }
    } else if let Some(default) = index.default_export(file) {
        return Some((file.to_string(), default));
    }

    // Memo check sits after the local-export fast paths: the common direct
    // hit never allocates the key, and a hit through the memo would have
    // returned above anyway.
    match visited.entry((file.to_string(), requested.to_string())) {
        std::collections::hash_map::Entry::Occupied(mut seen) => {
            if *seen.get() <= depth {
                return None;
            }
            seen.insert(depth);
        }
        std::collections::hash_map::Entry::Vacant(slot) => {
            slot.insert(depth);
        }
    }

    for reexport in index.reexports_for(file) {
        let mut next_requested = requested.to_string();
        let matches = if reexport.wildcard {
            true
        } else if let Some(source_name) = reexport.named.get(requested) {
            next_requested = source_name.clone();
            true
        } else {
            false
        };
        if !matches {
            continue;
        }
        if let Some(target_file) = &reexport.target_file {
            if let Some(target) = resolve_exported_symbol_inner(
                index,
                target_file,
                &next_requested,
                depth + 1,
                visited,
            ) {
                return Some(target);
            }
        }
    }
    None
}

fn resolve_rust_target<I: ResolverIndex>(
    index: &I,
    caller_file: &str,
    full_ref: &str,
    short_name: &str,
    caller_data: &FileCallData,
    raw: &RawRef,
) -> Option<(String, String, String)> {
    // A self call is bound by its enclosing declaration, including trait
    // defaults and trait impls whose scope contains both trait and type names.
    if let Some(member) = full_ref.strip_prefix("self.") {
        if let Some(owner) = raw
            .caller_symbol
            .as_deref()
            .and_then(|s| s.rsplit_once("::"))
        {
            for scope in std::iter::once(owner.0) {
                let symbol = format!("{scope}::{member}");
                if caller_data
                    .symbol_metadata
                    .get(&symbol)
                    .is_some_and(|meta| {
                        meta.kind == SymbolKind::Method
                            && meta
                                .signature
                                .as_deref()
                                .and_then(signature_parameter_text)
                                .and_then(|parameters| {
                                    split_top_level_commas(parameters).first().copied()
                                })
                                .and_then(|parameter| {
                                    rust_parameter_name(
                                        parameter.split(':').next().unwrap_or(parameter),
                                    )
                                })
                                == Some("self")
                    })
                    && index.node_for_symbol(caller_file, &symbol).is_some()
                {
                    return Some((
                        "resolved_local".to_string(),
                        caller_file.to_string(),
                        symbol,
                    ));
                }
            }
        }
    }
    if full_ref.contains('.') {
        return None;
    }
    if short_name.ends_with('!') && !full_ref.contains("::") {
        if index.node_for_symbol(caller_file, short_name).is_some() {
            return Some((
                "resolved_local".to_string(),
                caller_file.to_string(),
                short_name.to_string(),
            ));
        }
        if let Some(file) = rust_macro_use_target(index, caller_file, short_name) {
            return Some(("resolved".to_string(), file, short_name.to_string()));
        }
    }
    if full_ref.contains("::") {
        if let Some((file, symbol)) =
            rust_associated_target(index, caller_file, full_ref, caller_data, raw)
        {
            return Some(("resolved".to_string(), file, symbol));
        }
        if let Some((owner, _)) = full_ref.rsplit_once("::") {
            if !matches!(owner, "Self" | "self" | "super" | "crate") {
                if let Some((file, symbol)) =
                    rust_type_binding(index, caller_file, owner, caller_data, raw)
                {
                    if index
                        .node_for_symbol(&file, &symbol)
                        .is_some_and(|node| !index.node_is_callable(&file, &node))
                    {
                        return None;
                    }
                }
            }
        }
        // Self names the enclosing impl or trait, not a module on disk. Keep
        // private associated helpers in the same graph as their public callers.
        if let Some(member) = full_ref.strip_prefix("Self::") {
            if let Some(caller) = raw.caller_symbol.as_deref().filter(|caller| {
                caller_data
                    .symbol_metadata
                    .get(*caller)
                    .is_some_and(|meta| meta.kind == SymbolKind::Method)
            }) {
                if let Some((owner, _)) = caller.rsplit_once("::") {
                    let symbol = format!("{owner}::{member}");
                    if index.node_for_symbol(caller_file, &symbol).is_some() {
                        return Some((
                            "resolved_local".to_string(),
                            caller_file.to_string(),
                            symbol,
                        ));
                    }
                }
            }
        }
        if let Some((target_file, target_symbol)) =
            rust_target_for_qualified(index, caller_file, full_ref, short_name, caller_data, raw)
        {
            return Some(("resolved".to_string(), target_file, target_symbol));
        }
        // An unresolved qualifier must not bind an unrelated unqualified import
        // that happens to share the final function name.
        return None;
    }

    // A file-level function of the caller's own file wins over imports. Rust
    // rejects a file-level `use` that binds the same name as a file-level item,
    // so any import of this name sits in a nested scope (typically
    // `mod tests { use super::{..}; }`); applying it to the whole file
    // resolved ordinary same-file calls through the wrong module.
    if let Some(local) =
        rust_unique_file_level_function(index, caller_file, full_ref, short_name, caller_data)
    {
        return Some(local);
    }

    for import in &caller_data.import_block.imports {
        if let Some((target_file, target_symbol)) =
            rust_target_for_use(index, caller_file, import, short_name, caller_data, raw)
        {
            return Some(("resolved".to_string(), target_file, target_symbol));
        }
    }

    resolve_local_target(index, caller_file, full_ref, short_name, caller_data)
}

fn rust_associated_target<I: ResolverIndex>(
    index: &I,
    caller_file: &str,
    full_ref: &str,
    caller_data: &FileCallData,
    raw: &RawRef,
) -> Option<(String, String)> {
    let (owner, member) = full_ref.rsplit_once("::")?;
    if matches!(owner, "Self" | "self" | "super" | "crate") {
        return None;
    }
    let binding = rust_type_binding(index, caller_file, owner, caller_data, raw)?;
    // Follow the imported type before appending its member: the module
    // re-exports `Backend`, not a free function named `new`.
    index.node_for_symbol(&binding.0, &binding.1)?;
    let symbol = format!("{}::{member}", binding.1);
    index.node_for_symbol(&binding.0, &symbol)?;
    Some((binding.0, symbol))
}

fn rust_type_binding<I: ResolverIndex>(
    index: &I,
    caller_file: &str,
    owner: &str,
    caller_data: &FileCallData,
    raw: &RawRef,
) -> Option<(String, String)> {
    if owner.contains("::") {
        rust_target_for_qualified(
            index,
            caller_file,
            owner,
            owner.rsplit("::").next()?,
            caller_data,
            raw,
        )
    } else {
        let modules = rust_reference_module_segments(index, caller_file, caller_data, raw);
        if let Some(target) = rust_inline_scoped_target(index, caller_file, &modules, owner) {
            if index
                .node_for_symbol(&target.0, &target.1)
                .is_some_and(|node| !index.node_is_callable(&target.0, &node))
            {
                return Some(target);
            }
        }
        if caller_data.symbol_metadata.contains_key(owner) {
            return Some((caller_file.to_string(), owner.to_string()));
        }
        caller_data
            .import_block
            .imports
            .iter()
            .filter(|import| rust_import_is_visible_to_call(import, raw))
            .find_map(|import| {
                rust_target_for_use(index, caller_file, import, owner, caller_data, raw)
            })
    }
}

fn rust_macro_use_target<I: ResolverIndex>(
    index: &I,
    caller_file: &str,
    name: &str,
) -> Option<String> {
    let mut scope = Some(caller_file.to_string());
    let mut visited_scopes = BTreeSet::new();
    while let Some(file) = scope {
        if !visited_scopes.insert(file.clone()) {
            break;
        }
        let mut queue = index.macro_use_modules(&file);
        let mut visited_modules = BTreeSet::new();
        let mut targets = BTreeSet::new();
        while let Some(module) = queue.pop() {
            if !visited_modules.insert(module.clone()) {
                continue;
            }
            if index.node_for_symbol(&module, name).is_some() {
                targets.insert(module.clone());
            }
            queue.extend(index.macro_use_modules(&module));
        }
        if targets.len() == 1 {
            return targets.into_iter().next();
        }
        if !targets.is_empty() {
            return None;
        }
        scope = index.module_parent(&file).map(|(parent, _)| parent);
    }
    None
}

fn rust_unique_file_level_function<I: ResolverIndex>(
    _index: &I,
    caller_file: &str,
    full_ref: &str,
    short_name: &str,
    caller_data: &FileCallData,
) -> Option<(String, String, String)> {
    if full_ref.contains("::") || !callgraph::is_bare_callee(full_ref, short_name) {
        return None;
    }
    // Another free function can shadow a bare call; associated functions
    // cannot. Keep trait members behind their declaring trait or type.
    let symbol = short_name.to_string();
    // Declaration metadata proves the local binding without probing indexed
    // nodes for primitive return types or other non-function receiver names.
    let is_file_level_function = caller_data
        .symbol_metadata
        .get(short_name)
        .is_some_and(|meta| meta.kind == SymbolKind::Function)
        && !caller_data.symbol_metadata.iter().any(|(name, meta)| {
            name != short_name
                && last_name_segment(name) == short_name
                && meta.kind == SymbolKind::Function
        });
    is_file_level_function.then(|| {
        (
            "resolved_local".to_string(),
            caller_file.to_string(),
            symbol,
        )
    })
}

fn rust_target_for_qualified<I: ResolverIndex>(
    index: &I,
    caller_file: &str,
    full_ref: &str,
    short_name: &str,
    caller_data: &FileCallData,
    raw: &RawRef,
) -> Option<(String, String)> {
    let mut segments: Vec<&str> = full_ref.split("::").collect();
    if segments.len() < 2 {
        return None;
    }
    segments.pop();
    let requested_symbol = rust_target_symbol(full_ref, short_name);

    for path in rust_module_path_candidates(index, &segments, caller_data, raw) {
        let path_refs = path.iter().map(String::as_str).collect::<Vec<_>>();
        if !matches!(path_refs.first().copied(), Some("crate" | "self" | "super")) {
            if let Some(target_file) = rust_workspace_file_for_segments(index, &path_refs) {
                return Some(rust_resolve_reexport_if_symbol_missing(
                    index,
                    target_file,
                    requested_symbol.clone(),
                ));
            }
        }

        // A path that does not resolve under one reading may under another, and
        // a later alias candidate may still match, so try them all in order.
        for module_segments in rust_resolve_segment_candidates_from(
            rust_reference_module_segments(index, caller_file, caller_data, raw),
            &path_refs,
        ) {
            if let Some(target) =
                rust_inline_scoped_target(index, caller_file, &module_segments, &requested_symbol)
            {
                return Some(target);
            }
            if let Some(target_file) = rust_file_for_segments(index, caller_file, &module_segments)
            {
                return Some(rust_resolve_reexport_if_symbol_missing(
                    index,
                    target_file,
                    requested_symbol.clone(),
                ));
            }
        }
    }
    None
}

fn rust_target_symbol(full_ref: &str, short_name: &str) -> String {
    full_ref
        .rsplit("::")
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(short_name)
        .to_string()
}

fn rust_resolve_reexport_if_symbol_missing<I: ResolverIndex>(
    index: &I,
    target_file: String,
    target_symbol: String,
) -> (String, String) {
    if index
        .node_for_symbol(&target_file, &target_symbol)
        .is_some()
    {
        return (target_file, target_symbol);
    }
    if let Some(resolved) = resolve_exported_symbol(index, &target_file, &target_symbol, 0) {
        resolved
    } else {
        (target_file, target_symbol)
    }
}

fn rust_module_path_candidates<I: ResolverIndex + ?Sized>(
    index: &I,
    segments: &[&str],
    caller_data: &FileCallData,
    raw: &RawRef,
) -> Vec<Vec<String>> {
    let mut candidates = Vec::new();
    if let Some(first) = segments.first().copied() {
        let aliases = index.rust_module_aliases(caller_data);
        for (import, aliases) in caller_data.import_block.imports.iter().zip(aliases.iter()) {
            if !rust_import_is_visible_to_call(import, raw) {
                continue;
            }
            for (local_name, path_segments) in aliases {
                if local_name == first {
                    let mut path_segments = path_segments.clone();
                    path_segments
                        .extend(segments[1..].iter().map(|segment| (*segment).to_string()));
                    rust_push_unique_path_candidate(&mut candidates, path_segments);
                }
            }
        }
    }
    rust_push_unique_path_candidate(
        &mut candidates,
        segments
            .iter()
            .map(|segment| (*segment).to_string())
            .collect(),
    );
    candidates
}

fn rust_push_unique_path_candidate(candidates: &mut Vec<Vec<String>>, candidate: Vec<String>) {
    if !candidates.iter().any(|existing| existing == &candidate) {
        candidates.push(candidate);
    }
}

fn rust_import_is_visible_to_call(import: &ImportStatement, raw: &RawRef) -> bool {
    import.byte_range.start <= raw.byte_start
}

/// What [`rust_module_alias_segments`] returns for one import: each local
/// module name it binds, with the path it names.
pub(crate) type RustModuleAliases = Vec<(String, Vec<String>)>;

/// [`rust_module_alias_segments`] for each import of `data`, in order.
pub(crate) fn rust_module_aliases_of(data: &FileCallData) -> std::rc::Rc<[RustModuleAliases]> {
    data.import_block
        .imports
        .iter()
        .map(rust_module_alias_segments)
        .collect()
}

/// Module names a `use` declaration binds: `use a::b;` / `use a::b as c;`
/// bind one, and a use list binds `self` (the prefix module itself, as in
/// `use crate::cache_freshness::{self, FileFreshness};`). Conservative module
/// candidates use the imported item's spelling, not the alias's capitalization.
fn rust_module_alias_segments(import: &ImportStatement) -> Vec<(String, Vec<String>)> {
    let path = rust_use_body(&import.raw_text)
        .unwrap_or(&import.module_path)
        .trim()
        .trim_end_matches(';')
        .trim();
    if path.contains('*') {
        return Vec::new();
    }
    let split_path = |path: &str| {
        path.split("::")
            .map(str::trim)
            .filter(|segment| !segment.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    let is_module_name = |name: &str| {
        !name.is_empty() && name != "_" && !name.chars().next().is_some_and(char::is_uppercase)
    };
    if let Some((prefix, rest)) = path.split_once("::{") {
        if rest.contains('{') {
            return Vec::new();
        }
        let prefix_segments = split_path(prefix);
        return rest
            .trim_end_matches('}')
            .split(',')
            .filter_map(|specifier| {
                let specifier = specifier.trim();
                let (path_without_alias, alias) = specifier
                    .split_once(" as ")
                    .map(|(left, right)| (left.trim(), Some(right.trim())))
                    .unwrap_or((specifier, None));
                let segments = if path_without_alias == "self" {
                    prefix_segments.clone()
                } else {
                    let mut segments = prefix_segments.clone();
                    segments.extend(split_path(path_without_alias));
                    segments
                };
                let local_name = alias
                    .map(str::to_string)
                    .or_else(|| segments.last().cloned())?;
                segments
                    .last()
                    .is_some_and(|name| is_module_name(name))
                    .then_some((local_name, segments))
            })
            .collect();
    }
    if path.contains('{') {
        return Vec::new();
    }
    let (path_without_alias, alias) = path
        .split_once(" as ")
        .map(|(left, right)| (left.trim(), Some(right.trim())))
        .unwrap_or((path, None));
    let segments = split_path(path_without_alias);
    let Some(local_name) = alias
        .map(str::to_string)
        .or_else(|| segments.last().cloned())
    else {
        return Vec::new();
    };
    if !segments.last().is_some_and(|name| is_module_name(name)) {
        return Vec::new();
    }
    vec![(local_name, segments)]
}

fn rust_inline_scoped_target<I: ResolverIndex>(
    index: &I,
    caller_file: &str,
    module_segments: &[String],
    short_name: &str,
) -> Option<(String, String)> {
    index.inline_scoped_target(caller_file, module_segments, short_name)
}

fn rust_target_for_use<I: ResolverIndex>(
    index: &I,
    caller_file: &str,
    import: &ImportStatement,
    short_name: &str,
    caller_data: &FileCallData,
    raw: &RawRef,
) -> Option<(String, String)> {
    let imports = &caller_data.import_block.imports;
    let path = rust_use_body(&import.raw_text)
        .unwrap_or(&import.module_path)
        .trim()
        .trim_end_matches(';');
    if let Some(module) = path.strip_suffix("::*") {
        if let Some((file, symbol)) = rust_target_for_qualified(
            index,
            caller_file,
            &format!("{module}::{short_name}"),
            short_name,
            caller_data,
            raw,
        ) {
            return index.has_export(&file, &symbol).then_some((file, symbol));
        }
        let segments = module.split("::").collect::<Vec<_>>();
        let file = rust_use_module_file(index, caller_file, &segments, imports)?;
        let (file, symbol) =
            rust_resolve_reexport_if_symbol_missing(index, file, short_name.to_string());
        return index.has_export(&file, &symbol).then_some((file, symbol));
    }
    if let Some(brace_start) = path.find("::{") {
        let prefix = &path[..brace_start];
        // `use m::{f as g};` binds `g`; resolve the imported `f`.
        let imported = import.names.iter().find_map(|name| {
            (crate::imports::specifier_local_name(name) == short_name)
                .then(|| crate::imports::specifier_imported_name(name).to_string())
        });
        if let Some(imported) = imported {
            if let Some(target) = rust_target_for_qualified(
                index,
                caller_file,
                &format!("{prefix}::{imported}"),
                &imported,
                caller_data,
                raw,
            ) {
                return Some(target);
            }
            let prefix_segments: Vec<&str> = prefix.split("::").collect();
            let inline_segments = prefix_segments
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>();
            if let Some(target) =
                index.inline_scoped_target(caller_file, &inline_segments, &imported)
            {
                return Some(target);
            }
            let file = rust_use_module_file(index, caller_file, &prefix_segments, imports)?;
            // The module may only re-export the item (`pub use inner::{f};`);
            // follow that to the definition, as qualified paths already do.
            return Some(rust_resolve_reexport_if_symbol_missing(
                index, file, imported,
            ));
        }
        return None;
    }

    let (path_without_alias, alias) = path
        .split_once(" as ")
        .map(|(left, right)| (left.trim(), Some(right.trim())))
        .unwrap_or((path, None));
    let segments: Vec<&str> = path_without_alias.split("::").collect();
    let imported = alias.or_else(|| segments.last().copied())?;
    if imported != short_name {
        return None;
    }
    if segments.len() < 2 {
        return None;
    }
    if let Some(target) = rust_target_for_qualified(
        index,
        caller_file,
        path_without_alias,
        segments.last().copied().unwrap_or(short_name),
        caller_data,
        raw,
    ) {
        return Some(target);
    }
    let module_segments = segments[..segments.len() - 1]
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>();
    if let Some(target) = index.inline_scoped_target(
        caller_file,
        &module_segments,
        segments.last().copied().unwrap_or(short_name),
    ) {
        return Some(target);
    }
    let file = rust_use_module_file(index, caller_file, &segments[..segments.len() - 1], imports)?;
    Some(rust_resolve_reexport_if_symbol_missing(
        index,
        file,
        segments.last().unwrap_or(&short_name).to_string(),
    ))
}

/// The file of the module a `use` path names. A path may start with another
/// workspace crate (`use aft::callgraph_store::{..}` in an integration test or
/// a binary), which is checked first, as qualified call paths do. It may also
/// start with a module another `use` in the same file brought into scope
/// (`use aft::search::{paging};` then `use paging::{build};`).
fn rust_use_module_file<I: ResolverIndex>(
    index: &I,
    caller_file: &str,
    module_segments: &[&str],
    imports: &[ImportStatement],
) -> Option<String> {
    if let Some(file) = rust_use_module_file_direct(index, caller_file, module_segments) {
        return Some(file);
    }
    let (first, rest) = module_segments.split_first()?;
    imports
        .iter()
        .flat_map(rust_module_alias_segments)
        .filter(|(local_name, _)| local_name == first)
        .find_map(|(_, mut alias_segments)| {
            alias_segments.extend(rest.iter().map(|segment| (*segment).to_string()));
            let alias_refs = alias_segments
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>();
            rust_use_module_file_direct(index, caller_file, &alias_refs)
        })
}

fn rust_use_module_file_direct<I: ResolverIndex>(
    index: &I,
    caller_file: &str,
    module_segments: &[&str],
) -> Option<String> {
    // Local declarations, including #[path] modules in integration-test
    // harnesses, take precedence over reconstructing a library-relative path.
    if let Some((first, rest)) = module_segments.split_first() {
        if !matches!(*first, "crate" | "self" | "super") {
            if let Some(mut file) = index
                .module_target(caller_file, first)
                .filter(|file| file != caller_file)
            {
                for segment in rest {
                    file = index.module_target(&file, segment)?;
                }
                return Some(file);
            }
        }
    }
    if !matches!(
        module_segments.first().copied(),
        Some("crate" | "self" | "super") | None
    ) {
        if let Some(file) = rust_workspace_file_for_segments(index, module_segments) {
            return Some(file);
        }
    }
    let resolved = rust_resolve_segments_with_index(index, caller_file, module_segments)?;
    rust_file_for_segments(index, caller_file, &resolved)
}

fn rust_workspace_file_for_segments<I: ResolverIndex>(
    index: &I,
    segments: &[&str],
) -> Option<String> {
    let crate_name = segments.first().copied()?;
    let src_prefix = index.crate_src_prefix(crate_name)?;
    let module_segments = segments[1..]
        .iter()
        .map(|segment| segment.to_string())
        .collect::<Vec<_>>();
    rust_file_for_src_prefix(index, &src_prefix, &module_segments)
}

#[cfg(test)]
static WORKSPACE_CRATE_PREFIX_BUILD_COUNTS: OnceLock<Mutex<HashMap<PathBuf, usize>>> =
    OnceLock::new();

#[cfg(test)]
fn note_workspace_crate_prefix_build(project_root: &Path) {
    let mut counts = WORKSPACE_CRATE_PREFIX_BUILD_COUNTS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("workspace crate prefix build counts mutex poisoned");
    *counts.entry(project_root.to_path_buf()).or_default() += 1;
}

#[cfg(not(test))]
fn note_workspace_crate_prefix_build(_project_root: &Path) {}

/// Walk the project tree once and map every Rust crate name (package name with
/// `-` normalized to `_`, plus any explicit `[lib] name`) to its `src` prefix.
/// Replaces the previous per-ref tree walk: resolving 600k+ qualified refs no
/// longer re-walks the filesystem once per ref.
fn build_workspace_crate_prefixes(
    project_root: &Path,
    facts: &FactPaths<'_>,
) -> HashMap<String, String> {
    build_workspace_crate_prefixes_with_manifests(project_root, facts, |_| {})
}

fn build_workspace_crate_prefixes_with_manifests(
    project_root: &Path,
    facts: &FactPaths<'_>,
    mut visit_manifest: impl FnMut(&Path),
) -> HashMap<String, String> {
    note_workspace_crate_prefix_build(project_root);
    let mut prefixes = HashMap::new();
    let mut stack = vec![project_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let name = dir.file_name().and_then(|name| name.to_str()).unwrap_or("");
        if matches!(name, "target" | "node_modules" | ".git") {
            continue;
        }
        let manifest = dir.join("Cargo.toml");
        if facts.is_file(&manifest) {
            visit_manifest(&manifest);
            let crate_names = rust_manifest_crate_names(&manifest, facts);
            if !crate_names.is_empty() {
                let src_prefix = relative_path(
                    project_root,
                    &facts
                        .canonical(&dir.join("src"))
                        .unwrap_or_else(|| dir.join("src")),
                );
                for crate_name in crate_names {
                    prefixes
                        .entry(crate_name)
                        .or_insert_with(|| src_prefix.clone());
                }
            }
        }
        for entry in facts.list_dir(&dir) {
            if entry.kind == EntryKind::Directory {
                stack.push(dir.join(byte_path(&entry.name)));
            }
        }
    }
    prefixes
}

/// Extract the crate names a manifest defines: the normalized package name
/// (`-` -> `_`) and any explicit `[lib] name`. Returns both so a crate is
/// reachable by either spelling, matching the previous match semantics.
fn rust_manifest_crate_names(manifest: &Path, facts: &FactPaths<'_>) -> Vec<String> {
    facts.config_fact(manifest, "manifest.name");
    facts.config_fact(manifest, "manifest.lib.name");
    let Some(bytes) = facts.attributed_bytes(manifest) else {
        return Vec::new();
    };
    let (package_name, lib_name) = rust_manifest_name_fields(&bytes);
    let mut names = Vec::new();
    if let Some(lib) = lib_name {
        names.push(lib);
    }
    if let Some(package) = package_name {
        let normalized = package.replace('-', "_");
        if !names.contains(&normalized) {
            names.push(normalized);
        }
    }
    names
}

/// Preserve the manifest resolver's line-oriented extraction, including its
/// first non-lib name and last lib name semantics; this is not a TOML parser.
pub(crate) fn rust_manifest_name_fields(bytes: &[u8]) -> (Option<String>, Option<String>) {
    let Ok(source) = std::str::from_utf8(bytes) else {
        return (None, None);
    };
    let mut in_lib = false;
    let mut package_name = None;
    let mut lib_name = None;
    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_lib = trimmed == "[lib]";
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim().trim_matches('"');
        if in_lib && key == "name" {
            lib_name = Some(value.to_string());
        } else if !in_lib && key == "name" && package_name.is_none() {
            package_name = Some(value.to_string());
        }
    }
    (package_name, lib_name)
}

fn rust_resolve_segments_with_index<I: ResolverIndex>(
    index: &I,
    caller_file: &str,
    segments: &[&str],
) -> Option<Vec<String>> {
    rust_resolve_segments_from(rust_caller_module_segments(index, caller_file), segments)
}

fn rust_caller_module_segments<I: ResolverIndex>(index: &I, caller_file: &str) -> Vec<String> {
    #[cfg(test)]
    let _timing = rust_module_work::Timing::caller();
    index.rust_caller_module_segments(caller_file)
}

fn rust_caller_module_segments_uncached<I: ResolverIndex + ?Sized>(
    index: &I,
    caller_file: &str,
) -> Vec<String> {
    #[cfg(test)]
    rust_module_work::note(|work| work.caller_walks += 1);
    if index.rust_crate_root_file(caller_file).as_deref() == Some(caller_file) {
        Vec::new()
    } else {
        rust_registered_module_segments(index, caller_file)
            .unwrap_or_else(|| rust_module_segments_for_rel(caller_file))
    }
}

fn rust_reference_module_segments<I: ResolverIndex>(
    index: &I,
    caller_file: &str,
    caller_data: &FileCallData,
    raw: &RawRef,
) -> Vec<String> {
    let mut modules = rust_caller_module_segments(index, caller_file);
    let Some(caller) = raw.caller_symbol.as_deref() else {
        return modules;
    };
    let mut scope = caller.split("::").collect::<Vec<_>>();
    scope.pop();
    if caller_data
        .symbol_metadata
        .get(caller)
        .is_some_and(|meta| meta.kind == SymbolKind::Method)
    {
        scope.pop();
    }
    // Scoped names include inline modules as well as enclosing functions and
    // impl owners. Only the module prefix changes the meaning of self/super.
    for (position, segment) in scope.iter().enumerate() {
        if caller_data
            .symbol_metadata
            .contains_key(&scope[..=position].join("::"))
        {
            break;
        }
        modules.push((*segment).to_string());
    }
    modules
}

/// A bare path is relative to the current module; try its child modules before
/// the sibling-module interpretation retained for imported paths.
fn rust_resolve_segment_candidates_from(
    caller_segments: Vec<String>,
    segments: &[&str],
) -> Vec<Vec<String>> {
    let mut candidates = Vec::new();
    if segments
        .first()
        .is_some_and(|first| !matches!(*first, "crate" | "self" | "super"))
    {
        let mut child = caller_segments.clone();
        child.extend(segments.iter().map(|item| item.to_string()));
        candidates.push(child);
    }
    if let Some(resolved) = rust_resolve_segments_from(caller_segments, segments) {
        rust_push_unique_path_candidate(&mut candidates, resolved);
    }
    candidates
}

fn rust_resolve_segments(caller_file: &str, segments: &[&str]) -> Option<Vec<String>> {
    rust_resolve_segments_from(rust_module_segments_for_rel(caller_file), segments)
}

fn rust_resolve_segments_from(
    caller_segments: Vec<String>,
    segments: &[&str],
) -> Option<Vec<String>> {
    if segments.is_empty() {
        return Some(Vec::new());
    }
    match segments[0] {
        "crate" => Some(segments[1..].iter().map(|item| item.to_string()).collect()),
        "self" => {
            let mut resolved = caller_segments;
            resolved.extend(segments[1..].iter().map(|item| item.to_string()));
            Some(resolved)
        }
        "super" => {
            let mut resolved = caller_segments;
            let parents = segments
                .iter()
                .take_while(|segment| **segment == "super")
                .count();
            for _ in 0..parents {
                resolved.pop()?;
            }
            resolved.extend(segments[parents..].iter().map(|item| item.to_string()));
            Some(resolved)
        }
        _ => {
            let mut resolved = caller_segments;
            resolved.pop();
            resolved.extend(segments.iter().map(|item| item.to_string()));
            Some(resolved)
        }
    }
}

fn rust_registered_module_segments<I: ResolverIndex + ?Sized>(
    index: &I,
    caller_file: &str,
) -> Option<Vec<String>> {
    let mut current = caller_file.to_string();
    let mut segments = Vec::new();
    let mut seen = HashSet::new();
    while seen.insert(current.clone()) {
        let Some((parent, module)) = index.module_parent(&current) else {
            break;
        };
        segments.push(module);
        current = parent;
    }
    if segments.is_empty() {
        None
    } else {
        segments.reverse();
        Some(segments)
    }
}

fn rust_file_for_segments<I: ResolverIndex>(
    index: &I,
    caller_file: &str,
    segments: &[String],
) -> Option<String> {
    let src_prefix = rust_src_prefix(caller_file);
    if let Some(target) =
        rust_file_from_module_declarations(index, caller_file, &src_prefix, segments)
    {
        return Some(target);
    }
    rust_file_for_src_prefix(index, &src_prefix, segments)
}

fn rust_file_from_module_declarations<I: ResolverIndex>(
    index: &I,
    caller_file: &str,
    src_prefix: &str,
    segments: &[String],
) -> Option<String> {
    let mut current = index.rust_crate_root_file(caller_file).or_else(|| {
        [
            format!("{src_prefix}/lib.rs"),
            format!("{src_prefix}/main.rs"),
        ]
        .into_iter()
        .find(|candidate| index.contains_file(candidate))
    })?;
    for segment in segments {
        current = index.module_target(&current, segment)?;
    }
    Some(current)
}

fn rust_file_for_src_prefix<I: ResolverIndex>(
    index: &I,
    src_prefix: &str,
    segments: &[String],
) -> Option<String> {
    let candidate = if segments.is_empty() {
        [src_prefix, "lib.rs"].join("/")
    } else {
        format!("{}/{}.rs", src_prefix, segments.join("/"))
    };
    if index.contains_file(&candidate) {
        return Some(candidate);
    }
    if !segments.is_empty() {
        let mod_candidate = format!("{}/{}/mod.rs", src_prefix, segments.join("/"));
        if index.contains_file(&mod_candidate) {
            return Some(mod_candidate);
        }
    }
    None
}

fn rust_src_prefix(rel_path: &str) -> String {
    rel_path
        .split_once("/src/")
        .map(|(prefix, _)| format!("{prefix}/src"))
        .unwrap_or_else(|| "src".to_string())
}

fn rust_module_segments_for_rel(rel_path: &str) -> Vec<String> {
    let after_src = rel_path
        .split_once("/src/")
        .map(|(_, rest)| rest)
        .or_else(|| rel_path.strip_prefix("src/"))
        .unwrap_or(rel_path);
    if matches!(after_src, "lib.rs" | "main.rs") {
        return Vec::new();
    }
    if let Some(prefix) = after_src.strip_suffix("/mod.rs") {
        return prefix.split('/').map(|item| item.to_string()).collect();
    }
    after_src
        .strip_suffix(".rs")
        .unwrap_or(after_src)
        .split('/')
        .map(|item| item.to_string())
        .collect()
}

fn resolve_local_target<I: ResolverIndex>(
    index: &I,
    caller_file: &str,
    full_ref: &str,
    short_name: &str,
    caller_data: &FileCallData,
) -> Option<(String, String, String)> {
    if !callgraph::is_bare_callee(full_ref, short_name) {
        return None;
    }
    index
        .local_symbol(caller_file, caller_data, short_name)
        .map(|symbol| {
            (
                "resolved_local".to_string(),
                caller_file.to_string(),
                symbol,
            )
        })
}

impl<'a> ProjectIndex<'a> {
    fn from_parts(
        project_root: &Path,
        files: HashMap<String, DbFileIndex>,
        caller_data: HashMap<String, &'a FileCallData>,
        workspace_crate_prefixes: WorkspaceCratePrefixCache,
        facts: Rc<dyn ProjectFacts + 'a>,
    ) -> Self {
        Self::from_shared(
            project_root,
            Arc::new(files),
            caller_data,
            workspace_crate_prefixes,
            facts,
        )
    }

    /// An index whose every file index comes from `loaded`, on first use.
    pub(crate) fn from_loaded(
        project_root: &Path,
        loaded: Rc<dyn LoadedFileIndexes + 'a>,
        workspace_crate_prefixes: WorkspaceCratePrefixCache,
        facts: Rc<dyn ProjectFacts + 'a>,
    ) -> Self {
        let mut index = Self::from_parts(
            project_root,
            HashMap::new(),
            HashMap::new(),
            workspace_crate_prefixes,
            facts,
        );
        index.lazy = Some(loaded);
        index
    }

    /// An index over bound surfaces another index or thread also holds.
    pub(crate) fn from_shared(
        project_root: &Path,
        files: Arc<HashMap<String, DbFileIndex>>,
        caller_data: HashMap<String, &'a FileCallData>,
        workspace_crate_prefixes: WorkspaceCratePrefixCache,
        facts: Rc<dyn ProjectFacts + 'a>,
    ) -> Self {
        Self {
            facts,
            project_root: project_root.to_path_buf(),
            files,
            caller_data,
            lazy: None,
            workspace_crate_prefixes,
            rust_crate_roots: callgraph::RustCrateRootMemo::default(),
            crate_scans: std::cell::OnceCell::new(),
            lazy_module_parents: std::cell::OnceCell::new(),
            lazy_inline_candidates: std::cell::OnceCell::new(),
            rust_caller_paths: RefCell::default(),
            go_packages: RefCell::default(),
            local_symbols: RefCell::default(),
        }
    }

    /// The index of one file: the in-memory entry when there is one (every
    /// file of an eagerly built index, or a file this refresh re-resolves),
    /// otherwise the stored rows, loaded on first use.
    fn file_index(&self, rel_path: &str) -> Option<FileIndexRef<'_>> {
        if let Some(index) = self.files.get(rel_path) {
            return Some(FileIndexRef::Borrowed(index));
        }
        self.lazy
            .as_ref()
            .and_then(|lazy| lazy.file_index(rel_path))
            .map(FileIndexRef::Loaded)
    }

    fn lang_for(&self, rel_path: &str) -> Option<LangId> {
        self.file_index(rel_path).and_then(|file| file.lang)
    }

    fn module_target(&self, caller_file: &str, module_path: &str) -> Option<String> {
        self.file_index(caller_file)
            .and_then(|file| file.module_targets.get(module_path).cloned().flatten())
    }

    fn reexports_for(&self, rel_path: &str) -> Vec<ReexportIndex> {
        self.file_index(rel_path)
            .map(|file| file.reexports.clone())
            .unwrap_or_default()
    }

    fn node_for_symbol(&self, rel_path: &str, symbol: &str) -> Option<String> {
        self.file_index(rel_path).and_then(|file| {
            file.node_by_scoped.get(symbol).cloned().or_else(|| {
                (file.lang != Some(LangId::Python))
                    .then(|| file.node_by_bare.get(symbol).cloned())
                    .flatten()
            })
        })
    }

    fn node_is_callable(&self, rel_path: &str, node_id: &str) -> bool {
        self.file_index(rel_path)
            .and_then(|file| file.node_kind_by_id.get(node_id).cloned())
            .is_some_and(|kind| matches!(kind.as_str(), "function" | "kernel" | "method"))
    }

    /// Every indexed file path an index-wide scan must consider, in no order:
    /// the in-memory entries plus the stored files `stored` lists that have no
    /// in-memory entry (an in-memory entry replaces the stored rows).
    fn scan_paths(
        &self,
        stored: impl FnOnce(&dyn LoadedFileIndexes) -> Vec<String>,
    ) -> Vec<String> {
        let mut paths = self.files.keys().cloned().collect::<Vec<_>>();
        if let Some(lazy) = &self.lazy {
            paths.extend(
                stored(lazy.as_ref())
                    .into_iter()
                    .filter(|path| !self.files.contains_key(path)),
            );
        }
        paths
    }
}

/// A file index borrowed from an in-memory map or loaded on demand.
enum FileIndexRef<'a> {
    Borrowed(&'a DbFileIndex),
    Loaded(LoadedIndex),
}

impl std::ops::Deref for FileIndexRef<'_> {
    type Target = DbFileIndex;

    fn deref(&self) -> &DbFileIndex {
        match self {
            Self::Borrowed(index) => index,
            Self::Loaded(index) => index,
        }
    }
}

/// A file index loaded on demand and shared across resolver threads.
#[derive(Clone)]
pub(crate) enum LoadedIndex {
    Shared(Arc<DbFileIndex>),
}

impl std::ops::Deref for LoadedIndex {
    type Target = DbFileIndex;

    fn deref(&self) -> &DbFileIndex {
        match self {
            Self::Shared(index) => index,
        }
    }
}

/// File indexes a [`ProjectIndex`] loads when resolution first reaches
/// them, rather than binding every file eagerly.
pub(crate) trait LoadedFileIndexes {
    /// Whether `rel_path` is an indexed file.
    fn contains_file(&self, rel_path: &str) -> bool;
    /// `rel_path`'s index, or `None` when it is not an indexed file.
    fn file_index(&self, rel_path: &str) -> Option<LoadedIndex>;
    /// Every indexed Rust file, in no order.
    fn rust_files(&self) -> Vec<String>;
    /// Every indexed file that declares a module, in no order.
    fn module_declaring_files(&self) -> Vec<String>;
}

impl DbFileIndex {
    fn from_extract(project_root: &Path, extract: &FileExtract, facts: &FactPaths<'_>) -> Self {
        let module_memo = callgraph::ModuleResolutionMemo::default();
        let mut node_by_scoped = HashMap::new();
        let mut node_by_bare = HashMap::new();
        for node in &extract.nodes {
            node_by_scoped.insert(node.scoped_name.clone(), node.id.clone());
            node_by_bare
                .entry(node.name.clone())
                .or_insert(node.id.clone());
        }
        let node_kind_by_id = extract
            .nodes
            .iter()
            .map(|node| (node.id.clone(), node.kind.clone()))
            .collect();
        let mut export_aliases = HashMap::new();
        for raw_ref in &extract.raw_refs {
            if raw_ref.kind == "export_alias" {
                if let (Some(exported), Some(source_symbol)) =
                    (&raw_ref.local_name, &raw_ref.requested_name)
                {
                    export_aliases.insert(exported.clone(), source_symbol.clone());
                }
            }
        }
        let mut module_targets: HashMap<String, Option<String>> = HashMap::new();
        let mut declared_module_targets = HashMap::<String, Option<String>>::new();
        let mut macro_use_module_targets = BTreeSet::new();
        let mut reexports = Vec::new();
        for raw_ref in &extract.raw_refs {
            if !matches!(raw_ref.kind.as_str(), "import" | "reexport" | "module") {
                continue;
            }
            let Some(module_path) = &raw_ref.module_path else {
                continue;
            };
            // An import and its re-export select the same Python module.
            let target_file = if extract.lang == LangId::Python {
                if module_targets.contains_key(module_path) {
                    module_targets.get(module_path).cloned().flatten()
                } else {
                    // Python packages take precedence over same-named module
                    // files, unlike the lexical order of dependency candidates.
                    module_ref_target(
                        project_root,
                        &extract.rel_path,
                        &raw_ref.kind,
                        module_path,
                        &module_memo,
                        facts,
                        |file| facts.is_file(&project_root.join(file)),
                    )
                }
            } else if extract.lang == LangId::Rust && raw_ref.kind == "reexport" {
                // Parsed module refs remain available when immutable facts do
                // not contain raw source for reparsing module declarations.
                // Dependency sets include parent-module candidates. Their lexical
                // order is not Rust resolution order: retain the most-specific
                // module target for each flattened re-export leaf.
                declared_module_targets
                    .get(module_path)
                    .cloned()
                    .flatten()
                    .or_else(|| {
                        module_ref_target(
                            project_root,
                            &extract.rel_path,
                            &raw_ref.kind,
                            module_path,
                            &module_memo,
                            facts,
                            |file| facts.is_file(&project_root.join(file)),
                        )
                    })
            } else {
                module_target_from_dependencies(project_root, &raw_ref.dependencies, facts)
            };
            module_targets
                .entry(module_path.clone())
                .or_insert_with(|| target_file.clone());
            if raw_ref.kind == "module" {
                if raw_ref
                    .full_ref
                    .as_deref()
                    .is_some_and(|text| text.starts_with("#[macro_use]"))
                {
                    macro_use_module_targets.extend(target_file.iter().cloned());
                }
                declared_module_targets
                    .entry(module_path.clone())
                    .or_insert_with(|| target_file.clone());
            }
            if raw_ref.kind == "reexport" {
                reexports.push(reexport_index_from_raw(raw_ref, target_file));
            }
        }
        Self {
            lang: Some(extract.lang),
            exports: extract.data.exported_symbols.iter().cloned().collect(),
            default_export: extract.data.default_export_symbol.clone(),
            export_aliases,
            node_by_scoped,
            node_by_bare,
            node_kind_by_id,
            module_targets,
            declared_module_targets,
            macro_use_module_targets,
            reexports,
        }
    }
}

fn parse_method_dispatch(full_ref: &str) -> Option<(String, String, String, bool)> {
    let dot = full_ref.rfind('.').map(|index| (index, 1usize, false));
    let colon = full_ref.rfind("::").map(|index| (index, 2usize, true));
    let arrow = full_ref.rfind("->").map(|index| (index, 2usize, false));
    let (delimiter, delimiter_len, colon_dispatch) = [dot, colon, arrow]
        .into_iter()
        .flatten()
        .max_by_key(|(index, _, _)| *index)?;
    if delimiter == 0 {
        return None;
    }
    let member_start = delimiter + delimiter_len;
    if member_start >= full_ref.len() {
        return None;
    }
    let receiver_expression = full_ref[..delimiter].trim();
    let receiver = last_name_segment(receiver_expression).trim();
    let member = &full_ref[member_start..];
    if receiver.is_empty() || member.is_empty() {
        return None;
    }
    Some((
        receiver_expression.to_string(),
        receiver.to_string(),
        member.to_string(),
        colon_dispatch,
    ))
}

fn last_name_segment(value: &str) -> &str {
    value
        .rsplit(['.', ':', '/', '\\', '-', '>'])
        .find(|segment| !segment.is_empty())
        .unwrap_or(value)
}

fn node_text<'source>(node: tree_sitter::Node<'_>, source: &'source str) -> &'source str {
    &source[node.byte_range()]
}

fn infer_cpp_receiver_type_from_scope(
    scope: tree_sitter::Node<'_>,
    source: &str,
    receiver: &str,
    call_line: u32,
) -> Option<String> {
    let lines = source.lines().collect::<Vec<_>>();
    if lines.is_empty() {
        return None;
    }
    let scope_start = scope.start_position().row as usize;
    let call_index = (call_line as usize)
        .saturating_sub(1)
        .min(lines.len().saturating_sub(1));
    for index in (scope_start..=call_index).rev() {
        if let Some(receiver_type) = infer_cpp_receiver_type_from_line(lines[index], receiver) {
            return Some(receiver_type);
        }
    }
    None
}

fn infer_cpp_receiver_type_from_line(line: &str, receiver: &str) -> Option<String> {
    for receiver_start in identifier_occurrences(line, receiver) {
        let after = line[receiver_start + receiver.len()..].trim_start();
        if after
            .chars()
            .next()
            .is_some_and(|ch| !matches!(ch, ';' | '=' | ',' | ')' | '[' | '{' | '('))
        {
            continue;
        }
        let type_text = cpp_type_before_receiver(&line[..receiver_start])?;
        let normalized = normalize_cpp_type_name(type_text)?;
        if normalized == "auto" {
            if let Some(rhs) = after.strip_prefix('=') {
                return infer_cpp_auto_receiver_type(rhs);
            }
            continue;
        }
        return Some(normalized);
    }
    None
}

fn cpp_type_before_receiver(prefix: &str) -> Option<&str> {
    let candidate = prefix
        .rsplit([';', '{', '}', '('])
        .next()
        .unwrap_or(prefix)
        .trim();
    if candidate.is_empty() || candidate.ends_with(',') {
        None
    } else {
        Some(candidate)
    }
}

fn normalize_cpp_type_name(type_text: &str) -> Option<String> {
    let without_templates = strip_angle_groups(type_text);
    let mut cleaned = String::with_capacity(without_templates.len());
    for token in without_templates.split_whitespace() {
        if matches!(
            token,
            "const" | "volatile" | "mutable" | "typename" | "class" | "struct"
        ) {
            continue;
        }
        if !cleaned.is_empty() {
            cleaned.push(' ');
        }
        cleaned.push_str(token);
    }
    let token = cleaned
        .split_whitespace()
        .last()
        .unwrap_or(cleaned.trim())
        .trim_matches(|ch: char| !(is_code_ident_char(ch) || ch == ':' || ch == '.'))
        .trim_matches(['*', '&']);
    let simple = token.rsplit("::").next().unwrap_or(token).trim();
    if simple.is_empty() || cpp_non_type_token(simple) {
        None
    } else {
        Some(simple.to_string())
    }
}

fn infer_cpp_auto_receiver_type(rhs: &str) -> Option<String> {
    let rhs = rhs.trim_start();
    if let Some(after_new) = rhs.strip_prefix("new ") {
        return infer_cpp_constructor_type(after_new);
    }
    infer_cpp_make_template_type(rhs)
        .or_else(|| infer_cpp_constructor_type(rhs))
        .or_else(|| infer_cpp_factory_type(rhs))
}

fn infer_cpp_constructor_type(rhs: &str) -> Option<String> {
    let (head, rest) = read_invocation_head(rhs.trim_start())?;
    let normalized = normalize_cpp_type_name(head)?;
    if !normalized
        .chars()
        .next()
        .is_some_and(|ch| ch == '_' || ch.is_ascii_uppercase())
    {
        return None;
    }
    if matches!(rest.trim_start().chars().next(), Some('(' | '{')) {
        Some(normalized)
    } else {
        None
    }
}

fn infer_cpp_make_template_type(rhs: &str) -> Option<String> {
    let (head, rest) = read_invocation_head(rhs.trim_start())?;
    if !rest.trim_start().starts_with('(') {
        return None;
    }
    let base = head.split('<').next().unwrap_or(head);
    let base_simple = base.rsplit("::").next().unwrap_or(base);
    if !matches!(base_simple, "make_unique" | "make_shared") {
        return None;
    }
    first_angle_arg(head).and_then(normalize_cpp_type_name)
}

fn infer_cpp_factory_type(rhs: &str) -> Option<String> {
    let (head, rest) = read_invocation_head(rhs.trim_start())?;
    if !rest.trim_start().starts_with('(') {
        return None;
    }
    let simple = head
        .split('<')
        .next()
        .unwrap_or(head)
        .rsplit("::")
        .next()
        .unwrap_or(head);
    for prefix in ["make", "create", "build"] {
        if let Some(suffix) = simple.strip_prefix(prefix) {
            if suffix
                .chars()
                .next()
                .is_some_and(|ch| ch == '_' || ch.is_ascii_uppercase())
            {
                return normalize_cpp_type_name(suffix);
            }
        }
    }
    None
}

fn read_invocation_head(value: &str) -> Option<(&str, &str)> {
    let value = value.trim_start();
    let mut end = 0usize;
    for (index, ch) in value.char_indices() {
        let allowed_separator = ch == ':' || ch == '.';
        if is_code_ident_char(ch) || allowed_separator {
            end = index + ch.len_utf8();
            continue;
        }
        break;
    }
    if end == 0 {
        return None;
    }
    let mut rest = &value[end..];
    if let Some(stripped) = rest.trim_start().strip_prefix('<') {
        let skipped = skip_balanced_angle(stripped)?;
        let rest_start = rest.len() - rest.trim_start().len();
        let angle_len = 1 + skipped;
        end += rest_start + angle_len;
        rest = &value[end..];
    }
    Some((value[..end].trim(), rest))
}

fn skip_balanced_angle(value_after_open: &str) -> Option<usize> {
    let mut depth = 1usize;
    for (index, ch) in value_after_open.char_indices() {
        match ch {
            '<' => depth += 1,
            '>' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(index + ch.len_utf8());
                }
            }
            _ => {}
        }
    }
    None
}

fn first_angle_arg(value: &str) -> Option<&str> {
    let open = value.find('<')?;
    let inner_len = skip_balanced_angle(&value[open + 1..])?;
    let inner = &value[open + 1..open + inner_len];
    split_top_level_commas(inner).into_iter().next()
}

fn strip_angle_groups(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut depth = 0usize;
    for ch in value.chars() {
        match ch {
            '<' => {
                if depth == 0 {
                    output.push(' ');
                }
                depth += 1;
            }
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => output.push(ch),
            _ => {}
        }
    }
    output
}

fn cpp_non_type_token(value: &str) -> bool {
    matches!(
        value,
        "return"
            | "if"
            | "else"
            | "for"
            | "while"
            | "do"
            | "switch"
            | "case"
            | "default"
            | "break"
            | "continue"
            | "goto"
            | "throw"
            | "new"
            | "delete"
            | "co_await"
            | "co_yield"
            | "co_return"
            | "static_cast"
            | "const_cast"
            | "dynamic_cast"
            | "reinterpret_cast"
            | "sizeof"
            | "alignof"
            | "typeid"
            | "and"
            | "or"
            | "not"
            | "xor"
    )
}

fn identifier_occurrences(value: &str, needle: &str) -> Vec<usize> {
    value
        .match_indices(needle)
        .filter_map(|(index, _)| identifier_boundary(value, index, needle.len()).then_some(index))
        .collect()
}

fn identifier_boundary(value: &str, start: usize, len: usize) -> bool {
    let before = value[..start].chars().next_back();
    let after = value[start + len..].chars().next();
    !before.is_some_and(is_code_ident_char) && !after.is_some_and(is_code_ident_char)
}

fn is_code_ident_char(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphanumeric()
}

fn signature_parameter_text(signature: &str) -> Option<&str> {
    let open = signature.find('(')?;
    let mut depth = 0usize;
    for (offset, ch) in signature[open..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(&signature[open + 1..open + offset]);
                }
            }
            _ => {}
        }
    }
    None
}

fn split_top_level_commas(value: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut angle_depth = 0usize;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    for (index, ch) in value.char_indices() {
        match ch {
            '<' => angle_depth += 1,
            '>' => angle_depth = angle_depth.saturating_sub(1),
            '(' => paren_depth += 1,
            ')' => paren_depth = paren_depth.saturating_sub(1),
            '[' => bracket_depth += 1,
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            ',' if angle_depth == 0 && paren_depth == 0 && bracket_depth == 0 => {
                let part = value[start..index].trim();
                if !part.is_empty() {
                    parts.push(part);
                }
                start = index + ch.len_utf8();
            }
            _ => {}
        }
    }
    let part = value[start..].trim();
    if !part.is_empty() {
        parts.push(part);
    }
    parts
}

fn rust_parameter_name(pattern: &str) -> Option<&str> {
    let mut pattern = pattern.trim();
    if let Some(stripped) = pattern.strip_prefix("mut ") {
        pattern = stripped.trim_start();
    }
    pattern
        .rsplit(|ch: char| !is_rust_ident_char(ch))
        .find(|part| !part.is_empty())
}

fn is_rust_ident_char(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphanumeric()
}

#[cfg(test)]
thread_local! {
    pub(crate) static STALE_PATH_CENSUS_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The indexed file that an `import`, `reexport` or Rust `module` ref names.
///
/// Cold builds and incremental refreshes both answer this through this one
/// function, so a store gives the same answer however it was built. The
/// answer comes from the module resolver itself, not from the file's
/// dependency set: that set also holds candidate paths recorded only so that
/// creating one of them invalidates the importer, and picking the first
/// existing member can name a file the import never refers to.
///
/// Rust `use` paths are not handled by the JS/TS resolver; for Rust callers
/// the first indexed file among the module's Rust path candidates is used.
/// No other file is ever substituted: an import the resolver cannot place
/// (`bun:test`, `node:fs`, an npm package) has no target.
fn module_ref_target(
    project_root: &Path,
    caller_file: &str,
    kind: &str,
    module_path: &str,
    memo: &callgraph::ModuleResolutionMemo,
    facts: &FactPaths<'_>,
    is_indexed: impl Fn(&str) -> bool,
) -> Option<String> {
    if kind == "module" {
        return rust_declared_module_target(project_root, caller_file, module_path, memo, facts);
    }
    let caller_path = project_root.join(caller_file);
    if caller_path.extension().and_then(|ext| ext.to_str()) == Some("py") {
        return python_module_candidates(project_root, &caller_path, module_path)
            .into_iter()
            .find(|candidate| facts.is_file(candidate))
            .map(|path| relative_path(project_root, &path))
            .filter(|path| is_indexed(path));
    }
    if Path::new(caller_file)
        .extension()
        .and_then(|ext| ext.to_str())
        == Some("rs")
    {
        // `pub use cli::{..}` in a file that declares `mod cli;` names that
        // module (Rust 2018 paths start from names in scope), wherever the
        // path candidates below would look.
        if let Some(target) =
            rust_declared_module_path_target(project_root, caller_file, module_path, memo, facts)
                .filter(|target| is_indexed(target))
        {
            return Some(target);
        }
        return rust_module_candidates(project_root, &caller_path, module_path, memo, facts)
            .into_iter()
            .find(|candidate| is_indexed(candidate));
    }
    let caller_dir = caller_path.parent()?;
    let resolved = callgraph::resolve_module_path_with_memo(caller_dir, module_path, memo, facts)?;
    let rel_path = relative_path(project_root, &resolved);
    is_indexed(&rel_path).then_some(rel_path)
}

/// The file a Rust `use` path reaches by following `mod` declarations from
/// `caller_file`: its first segment (after an optional `self::`) must be a
/// module the caller declares, and each further segment that the reached file
/// declares as a module is followed too. The remaining segments name items.
fn rust_declared_module_path_target(
    project_root: &Path,
    caller_file: &str,
    module_path: &str,
    memo: &callgraph::ModuleResolutionMemo,
    facts: &FactPaths<'_>,
) -> Option<String> {
    let path = rust_module_path_without_alias_or_use_list(module_path);
    let path = path.strip_prefix("self::").unwrap_or(path);
    let mut segments = path
        .split("::")
        .map(str::trim)
        .filter(|segment| !segment.is_empty());
    let first = segments.next()?;
    let mut current = rust_declared_module_target(project_root, caller_file, first, memo, facts)?;
    for segment in segments {
        match rust_declared_module_target(project_root, &current, segment, memo, facts) {
            Some(next) => current = next,
            None => break,
        }
    }
    Some(current)
}

fn module_target_from_dependencies(
    project_root: &Path,
    dependencies: &BTreeSet<String>,
    facts: &FactPaths<'_>,
) -> Option<String> {
    dependencies.iter().find_map(|dep| {
        let path = project_root.join(dep);
        if facts.is_file(&path) {
            Some(relative_path(
                project_root,
                &facts.canonical(&path).unwrap_or(path.clone()),
            ))
        } else {
            None
        }
    })
}

fn reexport_index_from_raw(raw_ref: &RawRef, target_file: Option<String>) -> ReexportIndex {
    let mut named = HashMap::new();
    if let Some(full_ref) = &raw_ref.full_ref {
        named = parse_reexport_names(full_ref);
    }
    ReexportIndex {
        target_file,
        named,
        wildcard: raw_ref.wildcard,
    }
}

fn parse_reexport_names(statement: &str) -> HashMap<String, String> {
    let mut names = HashMap::new();
    let Some(open) = statement.find('{') else {
        return names;
    };
    let Some(close) = statement[open + 1..]
        .find('}')
        .map(|offset| open + 1 + offset)
    else {
        return names;
    };
    for spec in statement[open + 1..close].split(',') {
        let spec = spec.trim();
        if spec.is_empty() {
            continue;
        }
        if let Some((source, local)) = spec.split_once(" as ") {
            names.insert(local.trim().to_string(), source.trim().to_string());
        } else {
            names.insert(spec.to_string(), spec.to_string());
        }
    }
    names
}

fn module_dependencies(
    project_root: &Path,
    abs_path: &Path,
    module_path: &str,
    facts: &FactPaths<'_>,
) -> BTreeSet<String> {
    module_dependencies_with_memo(
        project_root,
        abs_path,
        module_path,
        &callgraph::ModuleResolutionMemo::default(),
        facts,
    )
}

fn module_dependencies_with_memo(
    project_root: &Path,
    abs_path: &Path,
    module_path: &str,
    memo: &callgraph::ModuleResolutionMemo,
    facts: &FactPaths<'_>,
) -> BTreeSet<String> {
    if abs_path.extension().and_then(|ext| ext.to_str()) == Some("py") {
        // Keep missing candidates too, so creating a module rebinds its importers.
        return python_module_candidates(project_root, abs_path, module_path)
            .into_iter()
            .map(|path| relative_path(project_root, &path))
            .collect();
    }
    let mut deps = rust_module_dependencies(project_root, abs_path, module_path, memo, facts);
    let caller_dir = abs_path.parent().unwrap_or(project_root);
    if let Some(resolved) =
        callgraph::resolve_module_path_with_memo(caller_dir, module_path, memo, facts)
    {
        deps.insert(relative_path(project_root, &resolved));
    }
    if module_path.starts_with('.') {
        let base = caller_dir.join(module_path);
        for candidate in relative_module_candidates(&base) {
            deps.insert(relative_path(project_root, &candidate));
        }
    }
    deps
}

/// Absolute Python imports start at the analyzed project root. Relative dots
/// walk package directories; neither form searches arbitrary same-named files
/// or guesses runtime changes to sys.path.
fn python_module_candidates(project_root: &Path, caller: &Path, module: &str) -> Vec<PathBuf> {
    let dots = module.bytes().take_while(|byte| *byte == b'.').count();
    let mut base = if dots == 0 {
        project_root.to_path_buf()
    } else {
        let Some(parent) = caller.parent() else {
            return Vec::new();
        };
        parent.to_path_buf()
    };
    for _ in 1..dots {
        let Some(parent) = base.parent() else {
            return Vec::new();
        };
        base = parent.to_path_buf();
    }
    if !base.starts_with(project_root) || (dots > 0 && base == project_root) {
        return Vec::new();
    }
    let rest = &module[dots..];
    if !rest.is_empty() {
        for segment in rest.split('.') {
            if segment.is_empty() || !segment.chars().all(|c| c == '_' || c.is_alphanumeric()) {
                return Vec::new();
            }
            base.push(segment);
        }
    } else if dots == 0 {
        return Vec::new();
    }
    // A regular package takes precedence over a sibling module, as in Python.
    vec![base.join("__init__.py"), base.with_extension("py")]
}

fn rust_module_dependencies(
    project_root: &Path,
    abs_path: &Path,
    module_path: &str,
    memo: &callgraph::ModuleResolutionMemo,
    facts: &FactPaths<'_>,
) -> BTreeSet<String> {
    rust_module_candidates(project_root, abs_path, module_path, memo, facts)
        .into_iter()
        .collect()
}

/// Existing files a Rust `use` path can name, most specific first: the file for
/// the full path (`a/b.rs`, `a/b/mod.rs`), then the file for its parent module,
/// which is where an item such as `a::b::Item` lives.
fn rust_module_candidates(
    project_root: &Path,
    abs_path: &Path,
    module_path: &str,
    memo: &callgraph::ModuleResolutionMemo,
    facts: &FactPaths<'_>,
) -> Vec<String> {
    let mut candidates = Vec::new();
    let rel_path = relative_path(
        project_root,
        &facts
            .canonical(abs_path)
            .unwrap_or_else(|| abs_path.to_path_buf()),
    );
    // A manifest can put a crate root outside src/, as ripgrep's binary does.
    // Resolve use paths from that target's directory, not from a guessed src/.
    let root_dir = memo
        .rust_crate_root_file(project_root, abs_path, facts)
        .and_then(|root| root.parent().map(Path::to_path_buf));
    let (src_prefix, path_segments) = if let Some(root_dir) = root_dir {
        let caller_rel = relative_path(
            &root_dir,
            &facts
                .canonical(abs_path)
                .unwrap_or_else(|| abs_path.to_path_buf()),
        );
        let path = rust_module_path_without_alias_or_use_list(module_path);
        let segments = path
            .split("::")
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>();
        if segments.is_empty() || matches!(segments[0], "std" | "core" | "alloc") {
            return candidates;
        }
        (
            relative_path(project_root, &root_dir),
            rust_resolve_segments_from(rust_module_segments_for_rel(&caller_rel), &segments),
        )
    } else {
        (
            rust_src_prefix(&rel_path),
            rust_module_dependency_segments(&rel_path, module_path),
        )
    };
    let Some(path_segments) = path_segments else {
        return candidates;
    };
    rust_push_module_dependency_candidate(
        project_root,
        &mut candidates,
        &src_prefix,
        &path_segments,
        facts,
    );
    if !path_segments.is_empty() {
        rust_push_module_dependency_candidate(
            project_root,
            &mut candidates,
            &src_prefix,
            &path_segments[..path_segments.len() - 1],
            facts,
        );
    }
    candidates
}

fn rust_module_dependency_segments(rel_path: &str, module_path: &str) -> Option<Vec<String>> {
    let path = rust_module_path_without_alias_or_use_list(module_path);
    let segments = path
        .split("::")
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if segments.is_empty() || matches!(segments[0], "std" | "core" | "alloc") {
        return None;
    }
    rust_resolve_segments(rel_path, &segments)
}

fn rust_module_path_without_alias_or_use_list(module_path: &str) -> &str {
    let path = module_path
        .trim()
        .trim_end_matches(';')
        .split_once(" as ")
        .map(|(left, _)| left.trim())
        .unwrap_or_else(|| module_path.trim().trim_end_matches(';'));
    path.find("::{").map(|brace| &path[..brace]).unwrap_or(path)
}

fn rust_push_module_dependency_candidate(
    project_root: &Path,
    deps: &mut Vec<String>,
    src_prefix: &str,
    segments: &[String],
    facts: &FactPaths<'_>,
) {
    let prefix = if src_prefix.is_empty() {
        String::new()
    } else {
        format!("{src_prefix}/")
    };
    let candidates = if segments.is_empty() {
        vec![format!("{prefix}lib.rs"), format!("{prefix}main.rs")]
    } else {
        vec![
            format!("{}{}.rs", prefix, segments.join("/")),
            format!("{}{}/mod.rs", prefix, segments.join("/")),
        ]
    };
    for candidate in candidates {
        if facts.is_file(&project_root.join(&candidate)) && !deps.contains(&candidate) {
            deps.push(candidate);
        }
    }
}

fn relative_module_candidates(base: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if base.extension().is_some() {
        candidates.push(base.to_path_buf());
        return candidates;
    }
    for ext in JS_TS_EXTENSIONS {
        candidates.push(base.with_extension(ext));
    }
    for ext in JS_TS_EXTENSIONS {
        candidates.push(base.join(format!("index.{ext}")));
    }
    candidates
}

fn import_local_names(import: &ImportStatement) -> Vec<String> {
    let mut names = Vec::new();
    if let Some(default) = &import.default_import {
        names.push(default.clone());
    }
    if let Some(namespace) = &import.namespace_import {
        names.push(namespace.clone());
    }
    for name in &import.names {
        names.push(crate::imports::specifier_local_name(name).to_string());
    }
    names
}

fn import_requested_names(import: &ImportStatement) -> Vec<String> {
    import
        .names
        .iter()
        .map(|name| crate::imports::specifier_imported_name(name).to_string())
        .collect()
}

fn import_is_wildcard(import: &ImportStatement) -> bool {
    import.namespace_import.is_some() || import.raw_text.contains('*')
}

fn import_kind_label(kind: ImportKind) -> &'static str {
    match kind {
        ImportKind::Value => "value",
        ImportKind::Type => "type",
        ImportKind::SideEffect => "side_effect",
    }
}

pub(crate) fn lang_label(lang: LangId) -> &'static str {
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

pub(crate) fn lang_from_label(label: &str) -> Option<LangId> {
    match label {
        "typescript" => Some(LangId::TypeScript),
        "tsx" => Some(LangId::Tsx),
        "javascript" => Some(LangId::JavaScript),
        "python" => Some(LangId::Python),
        "rust" => Some(LangId::Rust),
        "go" => Some(LangId::Go),
        "c" => Some(LangId::C),
        "cpp" => Some(LangId::Cpp),
        "cuda" => Some(LangId::Cuda),
        "metal" => Some(LangId::Metal),
        "zig" => Some(LangId::Zig),
        "csharp" => Some(LangId::CSharp),
        "bash" => Some(LangId::Bash),
        "html" => Some(LangId::Html),
        "markdown" => Some(LangId::Markdown),
        "solidity" => Some(LangId::Solidity),
        "scss" => Some(LangId::Scss),
        "vue" => Some(LangId::Vue),
        "json" => Some(LangId::Json),
        "scala" => Some(LangId::Scala),
        "java" => Some(LangId::Java),
        "ruby" => Some(LangId::Ruby),
        "kotlin" => Some(LangId::Kotlin),
        "swift" => Some(LangId::Swift),
        "php" => Some(LangId::Php),
        "lua" => Some(LangId::Lua),
        "perl" => Some(LangId::Perl),
        "yaml" => Some(LangId::Yaml),
        "pascal" => Some(LangId::Pascal),
        "r" => Some(LangId::R),
        "groovy" => Some(LangId::Groovy),
        "objc" => Some(LangId::ObjC),
        "toml" => Some(LangId::Toml),
        _ => None,
    }
}

pub(crate) fn normalize_file_path(project_root: &Path, path: &Path) -> Result<PathBuf> {
    let full_path = if path.is_relative() {
        project_root.join(path)
    } else {
        path.to_path_buf()
    };
    Ok(canonicalize_path(&full_path))
}

#[cfg(test)]
thread_local! {
    static CANONICALIZE_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// `std::fs::canonicalize`, counted in tests. Each call is a `realpath`, which
/// on macOS costs one `getattrlist` per path component.
fn fs_canonicalize(path: &Path) -> std::io::Result<PathBuf> {
    #[cfg(test)]
    CANONICALIZE_CALLS.with(|calls| calls.set(calls.get() + 1));
    #[cfg(test)]
    work_counts::note_realpath(path);
    std::fs::canonicalize(path)
}

/// Canonicalize an existing path or the deepest existing ancestor of a deleted
/// one. This keeps watcher deletion events in the same identity domain as the
/// files indexed before the deletion.
///
/// Uncached resolver probes also call this (`tsconfig.json` in each ancestor
/// directory, `index.ts` beside each import), and most name a missing file.
fn canonicalize_path(path: &Path) -> PathBuf {
    #[cfg(test)]
    work_counts::note_canonical_path(path);
    if let Ok(canonical) = fs_canonicalize(path) {
        return canonical;
    }
    canonicalize_missing_absolute_path(path).unwrap_or_else(|| canonicalize_missing_path(path))
}

/// The deepest-existing-ancestor form of an absolute path made only of normal
/// components, found by walking up from the parent. A missing probe usually
/// sits in an existing directory, so this costs one more `canonicalize`; the
/// component-by-component walk in [`canonicalize_missing_path`] costs one per
/// component, each of them a full `realpath` from the root.
///
/// It returns what that walk returns: canonicalizing an ancestor also resolves
/// every ancestor above it, so the first ancestor that canonicalizes is the
/// deepest one the walk would reach. `None` hands paths with `..`, relative
/// paths or no parent to the walk. A Windows drive or verbatim prefix before
/// the root is allowed, since `canonicalize` returns such paths there.
fn canonicalize_missing_absolute_path(path: &Path) -> Option<PathBuf> {
    let mut components = path.components().peekable();
    if matches!(components.peek(), Some(std::path::Component::Prefix(_))) {
        components.next();
    }
    if components.next() != Some(std::path::Component::RootDir) {
        return None;
    }
    let names = components
        .map(|component| match component {
            std::path::Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    let (existing, canonical) =
        path.ancestors()
            .skip(1)
            .enumerate()
            .find_map(|(depth_above, ancestor)| {
                fs_canonicalize(ancestor)
                    .ok()
                    .map(|canonical| (names.len() - 1 - depth_above, canonical))
            })?;
    // The walk checks the first missing component the same way: a name that
    // exists but cannot be resolved (a dangling symlink, a file used as a
    // directory) leaves the whole path as given.
    match std::fs::symlink_metadata(canonical.join(names[existing])) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut resolved = canonical;
            resolved.extend(&names[existing..]);
            Some(resolved)
        }
        _ => Some(path.to_path_buf()),
    }
}

/// Resolve a missing path component by component, keeping the unresolved tail
/// and applying `..` to it lexically.
fn canonicalize_missing_path(path: &Path) -> PathBuf {
    let mut resolved = PathBuf::new();
    let mut missing = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::Prefix(_) | std::path::Component::RootDir => {
                resolved.push(component.as_os_str());
                if let Ok(canonical) = fs_canonicalize(&resolved) {
                    resolved = canonical;
                }
            }
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if missing.pop().is_none() {
                    if !resolved.as_os_str().is_empty() && !resolved.is_dir() {
                        return path.to_path_buf();
                    }
                    resolved.pop();
                }
            }
            std::path::Component::Normal(name) => {
                if missing.is_empty() {
                    let candidate = resolved.join(name);
                    match fs_canonicalize(&candidate) {
                        Ok(canonical) => resolved = canonical,
                        Err(_) => match std::fs::symlink_metadata(&candidate) {
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                                missing.push(name.to_owned());
                            }
                            _ => return path.to_path_buf(),
                        },
                    }
                } else {
                    missing.push(name.to_owned());
                }
            }
        }
    }
    resolved.extend(missing);
    resolved
}

pub(crate) fn relative_path(project_root: &Path, path: &Path) -> String {
    if let Ok(stripped) = path.strip_prefix(project_root) {
        return stripped.to_string_lossy().replace('\\', "/");
    }
    let canon_root = canonicalize_path(project_root);
    let canon_path = canonicalize_path(path);
    if let Ok(stripped) = canon_path.strip_prefix(&canon_root) {
        return stripped.to_string_lossy().replace('\\', "/");
    }
    canon_path.to_string_lossy().replace('\\', "/")
}

fn ref_id(parts: &[&str]) -> String {
    let joined = parts.join("\0");
    hash_to_hex(blake3::hash(joined.as_bytes()))
}

fn hash_to_hex(hash: blake3::Hash) -> String {
    hash.to_hex().to_string()
}

/// The first 64 bits of a digest as 16 hex digits.
fn short_hash_hex(hash: blake3::Hash) -> String {
    hash.to_hex()[..16].to_string()
}

#[derive(Debug, Clone)]
pub(crate) struct LineIndex {
    starts: Vec<usize>,
    ends: Vec<usize>,
    source_len: usize,
}

impl LineIndex {
    pub(crate) fn new(source: &str) -> Self {
        #[cfg(test)]
        join::extraction_work::note(|work| work.position_bytes += source.len());
        let mut starts = vec![0];
        let mut ends = Vec::new();
        for (offset, byte) in source.bytes().enumerate() {
            if byte == b'\n' {
                // Call attribution clamps columns before CRLF terminators;
                // blob ranges retain the older inclusive segment convention.
                ends.push(if offset > 0 && source.as_bytes()[offset - 1] == b'\r' {
                    offset - 1
                } else {
                    offset
                });
                starts.push(offset + 1);
            }
        }
        ends.push(source.len());
        Self {
            starts,
            ends,
            source_len: source.len(),
        }
    }

    fn byte_to_line(&self, byte_offset: usize) -> u32 {
        let byte_offset = byte_offset.min(self.source_len);
        self.starts.partition_point(|start| *start <= byte_offset) as u32
    }

    pub(crate) fn byte_offset(&self, line: u32, column: u32) -> usize {
        let Some(&start) = self.starts.get(line as usize) else {
            return self.source_len;
        };
        start + (column as usize).min(self.ends[line as usize] - start)
    }

    fn inclusive_byte_offset(&self, line: u32, column: u32) -> usize {
        let Some(&start) = self.starts.get(line as usize) else {
            return self.source_len;
        };
        let end = self
            .starts
            .get(line as usize + 1)
            .copied()
            .unwrap_or(self.source_len);
        start + (column as usize).min(end - start)
    }
}

fn empty_to_none(value: String) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

pub(crate) fn unix_millis_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod rust_use_parser_tests {
    use super::*;

    fn import(raw_text: &str) -> ImportStatement {
        ImportStatement {
            module_path: String::new(),
            names: Vec::new(),
            default_import: Some("pub".to_string()),
            namespace_import: None,
            kind: ImportKind::Value,
            group: ImportGroup::Internal,
            byte_range: 0..raw_text.len(),
            raw_text: raw_text.to_string(),
            form: ImportForm::RustUse {
                visibility: Some("pub".to_string()),
                named: Vec::new(),
            },
        }
    }

    #[test]
    fn rust_reexport_rejects_close_before_unclosed_group() {
        let raw_text = format!("pub use macro! {{}} {}::{{name;", "長".repeat(180));
        assert!(raw_text.len() > 541);
        assert_eq!(rust_pub_use_reexport_parts(&import(&raw_text)), None);
    }

    // Frozen pre-fix implementation: valid use declarations must retain their
    // existing extraction output, even when the text parser is imperfect.
    fn old_rust_pub_use_reexport_parts(
        import: &ImportStatement,
    ) -> Option<(String, HashMap<String, String>, bool)> {
        let body = rust_use_body(&import.raw_text).unwrap_or(import.module_path.as_str());
        let body = body.trim();
        if let Some(module_path) = body.strip_suffix("::*") {
            return Some((module_path.trim().to_string(), HashMap::new(), true));
        }

        if let Some(brace_start) = body.find("::{") {
            let module_path = body[..brace_start].trim().to_string();
            let names = rust_reexport_names_from_specs(&body[brace_start + 3..body.rfind('}')?]);
            if names.is_empty() {
                return None;
            }
            return Some((module_path, names, false));
        }

        let (module_path, spec) = body.rsplit_once("::")?;
        let names = rust_reexport_names_from_specs(spec);
        if names.is_empty() {
            return None;
        }
        Some((module_path.trim().to_string(), names, false))
    }

    #[test]
    fn rust_reexport_rejects_unclosed_groups() {
        for raw_text in [
            "pub use a::{",
            "pub use a::{b, c;",
            "pub use a::{b::{c, d};",
            "pub use a::{b /* } */;",
            "pub use a::{b // }\n;",
            "pub use a::{b /* nested /* } */ } */;",
        ] {
            assert_eq!(
                rust_pub_use_reexport_parts(&import(raw_text)),
                None,
                "{raw_text}"
            );
        }
    }

    #[test]
    fn rust_reexport_preserves_nested_groups_and_unicode() {
        let nested = rust_pub_use_reexport_parts(&import("pub use a::{b::{c, d}, e};"));
        assert_eq!(
            nested,
            Some((
                "a".into(),
                [
                    ("b::{c".into(), "b::{c".into()),
                    ("d}".into(), "d}".into()),
                    ("e".into(), "e".into())
                ]
                .into_iter()
                .collect(),
                false,
            ))
        );
        let unicode = rust_pub_use_reexport_parts(&import("pub use 長::道::{名 as 別名, café};"));
        assert_eq!(
            unicode,
            Some((
                "長::道".into(),
                [("別名".into(), "名".into()), ("café".into(), "café".into())]
                    .into_iter()
                    .collect(),
                false
            ))
        );
    }

    #[test]
    fn rust_use_group_matching_ignores_comments_and_literals() {
        for specs in [
            "b::{c, d}, e",
            "b /* } { /* } */ } */",
            "b // } {\n, c",
            r#"m!("} {", "escaped \" }", b"}", c"{")"#,
            r####"m!(r"}", r###"}"# {"###, br#"}"#, cr#"{"#)"####,
            r#"m!('}', '{', '\'', '\\', '\u{7d}', '長', 'a)"#,
        ] {
            let body = format!("a::{{{specs}}} trailing {{}}");
            assert_eq!(rust_use_group_close(&body), Some(4 + specs.len()), "{body}");
            let unclosed = format!("pub use a::{{{specs};");
            assert_eq!(
                rust_pub_use_reexport_parts(&import(&unclosed)),
                None,
                "{unclosed}"
            );
        }
        for raw_text in [
            "pub use a::{m!(\"}\");",
            "pub use a::{m!(\"unterminated }",
            "pub use a::{m!(r###\"unterminated }\"##);",
            "pub use a::{b /* unterminated }",
            "pub use a /* ::{b} */ ::c;",
            "pub use macro!(\"::{b}\");",
        ] {
            assert_eq!(
                rust_pub_use_reexport_parts(&import(raw_text)),
                None,
                "{raw_text}"
            );
        }
    }

    #[test]
    fn rust_reexport_preserves_legacy_comment_ranges() {
        // These imperfect names are kept for output compatibility.
        for (raw_text, module, name) in [
            ("pub use a::{b} /* } */;", "a", "b} /*"),
            ("pub use a /* ::{ */ ::{b};", "a /*", "*/ ::{b"),
        ] {
            let mut parser = tree_sitter::Parser::new();
            parser
                .set_language(&tree_sitter_rust::LANGUAGE.into())
                .unwrap();
            let tree = parser.parse(raw_text, None).unwrap();
            assert!(!tree.root_node().has_error(), "{raw_text}");
            let node = tree.root_node().named_child(0).unwrap();
            let parsed = rust_import_from_use_node(raw_text, node).unwrap();
            assert_eq!(parsed.raw_text, raw_text);
            assert_eq!(
                rust_pub_use_reexport_parts(&parsed),
                Some((
                    module.into(),
                    [(name.into(), name.into())].into_iter().collect(),
                    false
                ))
            );
            assert_eq!(
                rust_pub_use_reexport_parts(&parsed),
                old_rust_pub_use_reexport_parts(&parsed)
            );
        }
    }

    struct Rng(u64);

    impl Rng {
        fn pick<'a>(&mut self, choices: &'a [&'a str]) -> &'a str {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            choices[(self.0 % choices.len() as u64) as usize]
        }
    }

    fn generated_use_spec(rng: &mut Rng, depth: usize) -> String {
        let name = rng.pick(&["b", "名", "café", "r#type", "long_name"]);
        match rng.pick(&["plain", "alias", "nested", "comment"]) {
            "alias" => format!("{name} as {}", rng.pick(&["別名", "renamed", "r#match"])),
            "nested" if depth > 0 => format!(
                "{name}::{{{}, {}}}",
                generated_use_spec(rng, depth - 1),
                generated_use_spec(rng, depth - 1)
            ),
            "comment" => format!(
                "{name} /* {} */",
                rng.pick(&["}", "{", "/* } */", "\"::{\"", "長"])
            ),
            _ => name.to_string(),
        }
    }

    #[test]
    fn rust_use_generated_inputs_never_panic_and_preserve_well_formed_output() {
        let mut rng = Rng(0x937a_b501_4cdf_628e);
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        for _ in 0..4096 {
            let prefix = rng.pick(&["a", "長::道", "crate::r#type", "::外部"]);
            let spec = generated_use_spec(&mut rng, 3);
            let gap = rng.pick(&["", " /* ::{ */", " /* } */"]);
            let suffix = rng.pick(&["", " /* } */", " /* { */", " // }\n"]);
            let body = match rng.pick(&["group", "named", "wildcard"]) {
                "named" => format!("{prefix}::{spec}"),
                "wildcard" => format!("{prefix}::*"),
                _ => format!("{prefix}{gap}::{{{spec}, self,}}{suffix}"),
            };
            let raw_text = format!(
                "{}use {body};",
                rng.pick(&["pub ", "pub(crate) ", "pub(super) ", ""])
            );
            let parsed = import(&raw_text);
            assert!(
                !parser
                    .parse(&raw_text, None)
                    .unwrap()
                    .root_node()
                    .has_error(),
                "{raw_text}"
            );
            assert_eq!(
                rust_pub_use_reexport_parts(&parsed),
                old_rust_pub_use_reexport_parts(&parsed),
                "{raw_text}"
            );

            let mut malformed = format!("pub use {body}");
            for _ in 0..12 {
                malformed.push_str(rng.pick(&[
                    "::{", "}", "{", ";", "'", "\"", "\\", "/*", "*/", "//", "\n", "名", "🦀",
                    " as ", "r###\"", "#", ",",
                ]));
            }
            let result = std::panic::catch_unwind(|| {
                let parsed = import(&malformed);
                rust_use_body(&malformed);
                rust_use_visibility(&malformed);
                rust_use_list_names(&malformed);
                rust_reexport_names_from_specs(&malformed);
                quoted_module_path(&malformed);
                parse_reexport_names(&malformed);
                rust_pub_use_reexport_parts(&parsed);
            });
            assert!(result.is_ok(), "{malformed}");
        }
    }
}

#[cfg(test)]
mod crate_scan_memo_tests {
    //! The in-memory index answers `module_parent` and
    //! `inline_scoped_target` from a memo built once per index. These tests
    //! pin the memo to the scans it replaces, including their first-match
    //! order, and show that a changed file set (a new index) never reuses an
    //! earlier index's answers.
    use super::*;

    fn rust_file(declared: &[(&str, &str)], scoped: &[&str]) -> DbFileIndex {
        let mut index = DbFileIndex::empty(Some(LangId::Rust));
        for (module, target) in declared {
            index
                .declared_module_targets
                .insert((*module).to_string(), Some((*target).to_string()));
        }
        for symbol in scoped {
            index
                .node_by_scoped
                .insert((*symbol).to_string(), format!("node:{symbol}"));
        }
        index
    }

    fn index(files: Vec<(&str, DbFileIndex)>) -> ProjectIndex<'static> {
        ProjectIndex::from_parts(
            Path::new("/fixture"),
            files
                .into_iter()
                .map(|(path, index)| (path.to_string(), index))
                .collect(),
            HashMap::new(),
            WorkspaceCratePrefixCache::default(),
            Rc::new(DiskFacts::new(Path::new("/fixture"))),
        )
    }

    fn fixture_files() -> Vec<(&'static str, DbFileIndex)> {
        // A non-Rust file whose path sorts before the Rust match; the
        // language check must skip it.
        let mut untyped = DbFileIndex::empty(None);
        untyped
            .node_by_scoped
            .insert("deep::f".to_string(), "node:deep::f".to_string());
        vec![
            (
                "crates/a/src/lib.rs",
                rust_file(
                    &[
                        ("foo", "crates/a/src/foo.rs"),
                        ("alias_foo", "crates/a/src/foo.rs"),
                    ],
                    &["foo::inner::helper", "bar::deep::f"],
                ),
            ),
            (
                "crates/a/src/zed.rs",
                rust_file(&[("foo", "crates/a/src/foo.rs")], &[]),
            ),
            ("crates/a/src/foo.rs", rust_file(&[], &["inner::helper"])),
            ("crates/a/src/other.rs", rust_file(&[], &[])),
            ("crates/a/src/bar.rs", untyped),
            (
                "crates/b/src/lib.rs",
                rust_file(&[], &["foo::inner::helper"]),
            ),
            ("crates/b/src/x.rs", rust_file(&[], &[])),
        ]
    }

    fn segments(path: &[&str]) -> Vec<String> {
        path.iter().map(|segment| segment.to_string()).collect()
    }

    /// Compares every memo answer with the scan over the same index and
    /// returns the memo answers for further checks.
    fn assert_memo_matches_scan(
        index: &ProjectIndex<'_>,
    ) -> Vec<(String, Option<(String, String)>)> {
        let mut answers = Vec::new();
        let targets = [
            "crates/a/src/foo.rs",
            "crates/a/src/zed.rs",
            "crates/a/src/missing.rs",
        ];
        for target in targets {
            let memo = ResolverIndex::module_parent(index, target);
            assert_eq!(memo, index.module_parent_by_scan(target), "{target}");
            answers.push((format!("parent {target}"), memo));
        }
        let mut callers = index.files.keys().cloned().collect::<Vec<_>>();
        callers.push("crates/a/src/not_indexed.rs".to_string());
        callers.sort();
        let queries = [
            (segments(&["foo", "inner"]), "helper"),
            (segments(&["bar", "deep"]), "f"),
            (segments(&["foo"]), "absent"),
            (segments(&[]), "helper"),
        ];
        for caller in &callers {
            for (module_segments, name) in &queries {
                let memo =
                    ResolverIndex::inline_scoped_target(index, caller, module_segments, name);
                assert_eq!(
                    memo,
                    index.inline_scoped_target_by_scan(caller, module_segments, name),
                    "{caller} {module_segments:?} {name}"
                );
                answers.push((format!("inline {caller} {module_segments:?} {name}"), memo));
            }
        }
        answers
    }

    fn pair(file: &str, item: &str) -> Option<(String, String)> {
        Some((file.to_string(), item.to_string()))
    }

    #[test]
    fn rust_module_path_memo_preserves_cycles_missing_parents_and_path_fallbacks() {
        let index = index(vec![
            ("src/a.rs", rust_file(&[("b", "src/b.rs")], &[])),
            ("src/b.rs", rust_file(&[("a", "src/a.rs")], &[])),
            ("src/loose.rs", rust_file(&[], &[])),
            ("src/lib.rs", rust_file(&[], &[])),
        ]);
        for caller in [
            "src/a.rs",
            "src/b.rs",
            "src/loose.rs",
            "src/lib.rs",
            "src/missing.rs",
        ] {
            let expected = rust_caller_module_segments_uncached(&index, caller);
            assert_eq!(rust_caller_module_segments(&index, caller), expected);
            assert_eq!(rust_caller_module_segments(&index, caller), expected);
        }
    }

    #[test]
    fn memoized_crate_scans_answer_exactly_like_the_scans() {
        let index = index(fixture_files());
        assert_memo_matches_scan(&index);
        // Several declarations of one file: the smallest (file, module) pair.
        assert_eq!(
            ResolverIndex::module_parent(&index, "crates/a/src/foo.rs"),
            pair("crates/a/src/lib.rs", "alias_foo")
        );
        let inner = segments(&["foo", "inner"]);
        // The caller's own file wins over an earlier path.
        assert_eq!(
            ResolverIndex::inline_scoped_target(&index, "crates/a/src/lib.rs", &inner, "helper"),
            pair("crates/a/src/lib.rs", "foo::inner::helper")
        );
        // Otherwise the first path in order, within the caller's crate only.
        assert_eq!(
            ResolverIndex::inline_scoped_target(&index, "crates/a/src/other.rs", &inner, "helper"),
            pair("crates/a/src/foo.rs", "inner::helper")
        );
        assert_eq!(
            ResolverIndex::inline_scoped_target(&index, "crates/b/src/x.rs", &inner, "helper"),
            pair("crates/b/src/lib.rs", "foo::inner::helper")
        );
        // A non-Rust file that sorts first is skipped.
        assert_eq!(
            ResolverIndex::inline_scoped_target(
                &index,
                "crates/a/src/other.rs",
                &segments(&["bar", "deep"]),
                "f"
            ),
            pair("crates/a/src/lib.rs", "bar::deep::f")
        );
    }

    #[test]
    fn a_changed_file_set_never_reuses_an_earlier_index_memo() {
        let first = index(fixture_files());
        let inner = segments(&["foo", "inner"]);
        // Build the first index's memo before the file set changes.
        let before = assert_memo_matches_scan(&first);
        assert_eq!(
            ResolverIndex::module_parent(&first, "crates/a/src/foo.rs"),
            pair("crates/a/src/lib.rs", "alias_foo")
        );
        assert_eq!(
            ResolverIndex::inline_scoped_target(&first, "crates/a/src/other.rs", &inner, "helper"),
            pair("crates/a/src/foo.rs", "inner::helper")
        );

        // Remove two files and add two, the way a join rebuilds its index
        // from the previous index's files.
        let mut files = (*first.files).clone();
        files.remove("crates/a/src/lib.rs");
        files.remove("crates/a/src/foo.rs");
        files.insert(
            "crates/a/src/aaa.rs".to_string(),
            rust_file(&[("foo", "crates/a/src/foo.rs")], &[]),
        );
        files.insert(
            "crates/a/src/foo/mod.rs".to_string(),
            rust_file(&[], &["inner::helper"]),
        );
        let second = ProjectIndex::from_parts(
            Path::new("/fixture"),
            files,
            HashMap::new(),
            WorkspaceCratePrefixCache::default(),
            Rc::new(DiskFacts::new(Path::new("/fixture"))),
        );
        let after = assert_memo_matches_scan(&second);
        assert_ne!(before, after, "the fixture must change some answers");
        assert_eq!(
            ResolverIndex::module_parent(&second, "crates/a/src/foo.rs"),
            pair("crates/a/src/aaa.rs", "foo")
        );
        assert_eq!(
            ResolverIndex::inline_scoped_target(&second, "crates/a/src/other.rs", &inner, "helper"),
            pair("crates/a/src/foo/mod.rs", "inner::helper")
        );
        // The first index still answers for its own, unchanged file set.
        assert_eq!(assert_memo_matches_scan(&first), before);
    }
}

#[cfg(test)]
mod reexport_resolution_tests {
    use super::*;

    fn barrel_index(files: Vec<(String, DbFileIndex)>) -> ProjectIndex<'static> {
        ProjectIndex {
            facts: Rc::new(DiskFacts::new(Path::new("/fixture"))),
            project_root: PathBuf::from("/fixture"),
            files: Arc::new(files.into_iter().collect()),
            caller_data: HashMap::new(),
            lazy: None,
            workspace_crate_prefixes: WorkspaceCratePrefixCache::default(),
            rust_crate_roots: callgraph::RustCrateRootMemo::default(),
            crate_scans: std::cell::OnceCell::new(),
            lazy_module_parents: std::cell::OnceCell::new(),
            lazy_inline_candidates: std::cell::OnceCell::new(),
            rust_caller_paths: RefCell::default(),
            go_packages: RefCell::default(),
            local_symbols: RefCell::default(),
        }
    }

    fn barrel_file(reexport_targets: &[&str]) -> DbFileIndex {
        DbFileIndex {
            lang: None,
            exports: HashSet::new(),
            default_export: None,
            export_aliases: HashMap::new(),
            node_by_scoped: HashMap::new(),
            node_by_bare: HashMap::new(),
            node_kind_by_id: HashMap::new(),
            module_targets: HashMap::new(),
            declared_module_targets: HashMap::new(),
            macro_use_module_targets: BTreeSet::new(),
            reexports: reexport_targets
                .iter()
                .map(|target| ReexportIndex {
                    target_file: Some((*target).to_string()),
                    named: HashMap::new(),
                    wildcard: true,
                })
                .collect(),
        }
    }

    /// A dense wildcard re-export cycle (barrel files re-exporting each
    /// other) must resolve in O(files), not O(branching^depth). Without the
    /// resolver's memoization, resolving a MISSING symbol through this
    /// 12-file complete digraph explores ~11^16 paths and this test never
    /// finishes: the depth cap bounds path length, not path count, and one
    /// such resolution can pin a worker thread at 100% CPU indefinitely.
    #[test]
    fn missing_symbol_in_dense_wildcard_reexport_cycle_terminates() {
        let names: Vec<String> = (0..12).map(|i| format!("src/barrel{i}.ts")).collect();
        let files = names
            .iter()
            .map(|name| {
                let targets: Vec<&str> = names
                    .iter()
                    .filter(|other| *other != name)
                    .map(String::as_str)
                    .collect();
                (name.clone(), barrel_file(&targets))
            })
            .collect();
        let index = barrel_index(files);

        assert_eq!(
            resolve_exported_symbol(&index, "src/barrel0.ts", "does_not_exist", 0),
            None
        );
    }

    /// Depth-dominance counterexample: the walk first reaches `shared` down a
    /// 16-hop chain (no budget left for its leaf), then reaches it again
    /// directly at depth 1. Plain visited-set pruning would skip the second
    /// visit and lose a resolution the capped resolver finds; the
    /// depth-dominance memo revisits because the second arrival is shallower.
    #[test]
    fn shallow_revisit_after_deep_capped_visit_still_resolves() {
        let mut leaf = barrel_file(&[]);
        leaf.exports.insert("deep_symbol".to_string());
        let mut files: Vec<(String, DbFileIndex)> = Vec::new();
        // entry -> chain0 -> chain1 -> ... -> chain14 -> shared -> leaf
        // entry's SECOND reexport goes straight to shared.
        files.push((
            "src/entry.ts".to_string(),
            barrel_file(&["src/chain0.ts", "src/shared.ts"]),
        ));
        for i in 0..15 {
            let next = if i == 14 {
                "src/shared.ts".to_string()
            } else {
                format!("src/chain{}.ts", i + 1)
            };
            files.push((format!("src/chain{i}.ts"), barrel_file(&[&next])));
        }
        files.push(("src/shared.ts".to_string(), barrel_file(&["src/leaf.ts"])));
        files.push(("src/leaf.ts".to_string(), leaf));
        let index = barrel_index(files);

        assert_eq!(
            resolve_exported_symbol(&index, "src/entry.ts", "deep_symbol", 0),
            Some(("src/leaf.ts".to_string(), "deep_symbol".to_string())),
            "a shallower re-visit must not be pruned by a deeper capped visit"
        );
    }

    #[test]
    fn symbol_reachable_through_reexport_cycle_still_resolves() {
        let mut leaf = barrel_file(&[]);
        leaf.exports.insert("real_symbol".to_string());
        let index = barrel_index(vec![
            (
                "src/a.ts".to_string(),
                barrel_file(&["src/b.ts", "src/a.ts"]),
            ),
            (
                "src/b.ts".to_string(),
                barrel_file(&["src/a.ts", "src/leaf.ts"]),
            ),
            ("src/leaf.ts".to_string(), leaf),
        ]);

        assert_eq!(
            resolve_exported_symbol(&index, "src/a.ts", "real_symbol", 0),
            Some(("src/leaf.ts".to_string(), "real_symbol".to_string()))
        );
    }
}

#[cfg(test)]
mod canonicalize_path_tests {
    use super::*;

    fn canonicalize_calls<T>(run: impl FnOnce() -> T) -> (T, usize) {
        let before = CANONICALIZE_CALLS.with(std::cell::Cell::get);
        let value = run();
        (
            value,
            CANONICALIZE_CALLS.with(std::cell::Cell::get) - before,
        )
    }

    fn deep_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(dir.path()).expect("canonical root");
        let deep = root.join("packages/plugin/src/features/magic-context/dreamer/nested");
        std::fs::create_dir_all(&deep).expect("deep dirs");
        (dir, root, deep)
    }

    // The resolver probes a missing `tsconfig.json` in every ancestor of every
    // importing file. Each probe must cost a fixed number of `realpath` calls,
    // not one per path component: on a real checkout the per-component walk
    // made a callgraph refresh issue millions of `getattrlist` calls a minute.
    #[test]
    fn missing_probe_canonicalizes_its_parent_not_every_ancestor() {
        let (_dir, root, deep) = deep_fixture();
        let probe = deep.join("tsconfig.json");

        let (canonical, calls) = canonicalize_calls(|| canonicalize_path(&probe));
        assert_eq!(canonical, probe);
        assert!(
            calls <= 2,
            "one missing probe took {calls} canonicalize calls for {} components",
            probe.components().count()
        );

        let disk = disk_facts::DiskFacts::new(&root);
        let facts = facts::FactPaths {
            root: &root,
            facts: &disk,
        };
        let (found, calls) = canonicalize_calls(|| facts.is_file(&probe));
        assert!(!found);
        assert!(calls <= 2, "resolver probe took {calls} canonicalize calls");
    }

    #[test]
    fn missing_path_shortcut_matches_the_component_walk() {
        let (_dir, root, deep) = deep_fixture();
        std::fs::write(deep.join("file.ts"), "").expect("file");
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut cases = vec![
            deep.join("tsconfig.json"),
            deep.join("missing/also-missing/index.ts"),
            root.join("missing"),
            // A regular file used as a directory is not a missing ancestor.
            deep.join("file.ts/index.ts"),
        ];
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&deep, root.join("linked")).expect("dir link");
            std::os::unix::fs::symlink(root.join("nowhere"), root.join("dangling"))
                .expect("dangling link");
            cases.push(root.join("linked/tsconfig.json"));
            cases.push(root.join("linked/missing/index.ts"));
            cases.push(root.join("dangling/index.ts"));
        }
        for case in cases {
            assert_eq!(
                canonicalize_path(&case),
                canonicalize_missing_path(&case),
                "{}",
                case.display()
            );
        }
        #[cfg(unix)]
        assert_eq!(
            canonicalize_path(&root.join("linked/tsconfig.json")),
            deep.join("tsconfig.json")
        );
    }
}

#[cfg(test)]
impl DbFileIndex {
    fn empty(lang: Option<LangId>) -> Self {
        Self {
            lang,
            exports: HashSet::new(),
            default_export: None,
            export_aliases: HashMap::new(),
            node_by_scoped: HashMap::new(),
            node_by_bare: HashMap::new(),
            node_kind_by_id: HashMap::new(),
            module_targets: HashMap::new(),
            declared_module_targets: HashMap::new(),
            macro_use_module_targets: BTreeSet::new(),
            reexports: Vec::new(),
        }
    }
}
