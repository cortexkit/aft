use std::cell::{Ref, RefCell, RefMut};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, BufWriter};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use lsp_types::FileChangeType;
use notify::RecommendedWatcher;
use rusqlite::Connection;

use crate::backup::hash_session;
use crate::backup::BackupStore;
use crate::bash_background::{BgCompletion, BgTaskRegistry};
use crate::callgraph::CallGraph;
use crate::checkpoint::CheckpointStore;
use crate::config::Config;
use crate::harness::Harness;
use crate::inspect::{
    InspectCategory, InspectManager, InspectSnapshot, Tier2RefreshScheduler, Tier2TriggerReason,
};
use crate::language::LanguageProvider;
use crate::lsp::manager::LspManager;
use crate::lsp::registry::is_config_file_path_with_custom;
use crate::parser::{SharedSymbolCache, SymbolCache};
use crate::protocol::{
    ConfigureWarningsFrame, ProgressFrame, PushFrame, StatusChangedFrame, StatusPayload,
};

pub type ProgressSender = Arc<Box<dyn Fn(PushFrame) + Send + Sync>>;
pub type SharedProgressSender = Arc<Mutex<Option<ProgressSender>>>;
pub type SharedStdoutWriter = Arc<Mutex<BufWriter<io::Stdout>>>;
const STATUS_DEBOUNCE_MS: u64 = 1_000;

/// Agent status-bar counts — the IDE-style "status bar" surfaced to the agent
/// on every tool result (emit-on-change). `errors`/`warnings` are read LIVE
/// from the continuously-drained LSP diagnostics store; the Tier-2 counts
/// (`dead_code`/`unused_exports`/`duplicates`) and `todos` are last-known,
/// refreshed when `aft_inspect` runs or a background Tier-2 scan completes.
/// `tier2_stale` marks the Tier-2 counts as not-yet-reconciled with the latest
/// edits (rendered with a `~` marker so the agent never reads them as live).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatusBarCounts {
    pub errors: usize,
    pub warnings: usize,
    pub dead_code: usize,
    pub unused_exports: usize,
    pub duplicates: usize,
    pub todos: usize,
    pub tier2_stale: bool,
}

/// Last-known Tier-2 + todos counts, refreshed off the hot path. `errors` and
/// `warnings` are intentionally NOT cached here — they're read live per attach.
///
/// Each Tier-2 category is `Option`: `None` means "no scan has ever produced a
/// count for this category", so we never fabricate a `0`. The bar is only
/// surfaced once all three Tier-2 categories hold a real value — a partially
/// completed cold scan (e.g. dead_code done, unused_exports/duplicates still
/// running) must not render `D<real> U0 C0` and lie about project health (#1).
#[derive(Debug, Clone, Default)]
struct StatusBarTier2 {
    dead_code: Option<usize>,
    unused_exports: Option<usize>,
    duplicates: Option<usize>,
    todos: Option<usize>,
    stale: bool,
}

pub struct StatusEmitter {
    latest: Arc<Mutex<Option<StatusPayload>>>,
    notify: mpsc::Sender<()>,
}

impl StatusEmitter {
    fn new(progress_sender: SharedProgressSender) -> Self {
        let (notify, rx) = mpsc::channel();
        let latest = Arc::new(Mutex::new(None));
        let latest_for_thread = Arc::clone(&latest);
        std::thread::spawn(move || {
            status_debounce_loop(rx, latest_for_thread, progress_sender);
        });
        Self { latest, notify }
    }

    pub fn signal(&self, snapshot: StatusPayload) {
        if let Ok(mut latest) = self.latest.lock() {
            *latest = Some(snapshot);
        }
        let _ = self.notify.send(());
    }
}

fn status_debounce_loop(
    rx: mpsc::Receiver<()>,
    latest: Arc<Mutex<Option<StatusPayload>>>,
    progress_sender: SharedProgressSender,
) {
    while rx.recv().is_ok() {
        let deadline = Instant::now() + Duration::from_millis(STATUS_DEBOUNCE_MS);
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(remaining) {
                Ok(()) => continue,
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }

        let snapshot = latest.lock().ok().and_then(|mut latest| latest.take());
        let Some(snapshot) = snapshot else { continue };
        let sender = progress_sender
            .lock()
            .ok()
            .and_then(|sender| sender.clone());
        if let Some(sender) = sender {
            sender(PushFrame::StatusChanged(StatusChangedFrame::new(
                None, snapshot,
            )));
        }
    }
}
use crate::cache_freshness::FileFreshness;
use crate::search_index::SearchIndex;
use crate::semantic_index::{EmbeddingEntry, SemanticIndex};

// `SemanticIndexStatus::Ready` exposes a unique `refreshing` path list. Keep
// per-path queue accounting separately so repeated edits to the same file do not
// let an older refresh completion remove the path while newer work is pending.
#[derive(Debug, Default)]
struct SemanticRefreshAccounting {
    pending: usize,
    in_flight: usize,
}

static SEMANTIC_REFRESH_ACCOUNTING: OnceLock<Mutex<BTreeMap<PathBuf, SemanticRefreshAccounting>>> =
    OnceLock::new();

fn semantic_refresh_accounting() -> &'static Mutex<BTreeMap<PathBuf, SemanticRefreshAccounting>> {
    SEMANTIC_REFRESH_ACCOUNTING.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn clear_semantic_refresh_accounting() {
    if let Some(accounting) = SEMANTIC_REFRESH_ACCOUNTING.get() {
        if let Ok(mut accounting) = accounting.lock() {
            accounting.clear();
        }
    }
}

fn ensure_refreshing_path(refreshing: &mut Vec<PathBuf>, path: PathBuf) {
    if !refreshing.iter().any(|existing| existing == &path) {
        refreshing.push(path);
        refreshing.sort();
    }
}

fn remove_refreshing_path(refreshing: &mut Vec<PathBuf>, path: &Path) {
    refreshing.retain(|existing| existing != path);
}

#[derive(Debug, Clone)]
pub enum SemanticIndexStatus {
    Disabled,
    Building {
        /// Cold-build only — index is not queryable.
        stage: String,
        files: Option<usize>,
        entries_done: Option<usize>,
        entries_total: Option<usize>,
        stats: Option<SemanticProgressStats>,
    },
    Ready {
        /// Files currently being re-embedded after recent edits. The index is
        /// still queryable; results for these files may be temporarily missing.
        refreshing: Vec<PathBuf>,
    },
    Failed(String),
}

impl SemanticIndexStatus {
    pub fn ready() -> Self {
        clear_semantic_refresh_accounting();
        Self::Ready {
            refreshing: Vec::new(),
        }
    }

    pub fn add_refreshing_file(&mut self, path: PathBuf) {
        if let Self::Ready { refreshing } = self {
            if let Ok(mut accounting) = semantic_refresh_accounting().lock() {
                let state = accounting.entry(path.clone()).or_default();
                state.pending = state.pending.saturating_add(1);
            }
            ensure_refreshing_path(refreshing, path);
        }
    }

    pub fn start_refreshing_file(&mut self, path: PathBuf) {
        if let Self::Ready { refreshing } = self {
            if let Ok(mut accounting) = semantic_refresh_accounting().lock() {
                let state = accounting.entry(path.clone()).or_default();
                if state.pending == 0 {
                    state.pending = 1;
                }
                if state.in_flight == 0 {
                    state.in_flight = state.pending;
                }
            }
            ensure_refreshing_path(refreshing, path);
        }
    }

    pub fn cancel_refreshing_file(&mut self, path: &Path) {
        self.finish_refreshing_file(path, false);
    }

    pub fn complete_refreshing_file(&mut self, path: &Path) {
        self.finish_refreshing_file(path, true);
    }

    pub fn remove_refreshing_file(&mut self, path: &Path) {
        self.complete_refreshing_file(path);
    }

    fn finish_refreshing_file(&mut self, path: &Path, complete_in_flight: bool) {
        if let Self::Ready { refreshing } = self {
            let mut keep_refreshing = false;
            let mut accounting_checked = false;
            if let Ok(mut accounting) = semantic_refresh_accounting().lock() {
                accounting_checked = true;
                if let Some(state) = accounting.get_mut(path) {
                    let finished = if complete_in_flight {
                        state.in_flight.max(1)
                    } else {
                        1
                    };
                    state.pending = state.pending.saturating_sub(finished);
                    if complete_in_flight {
                        state.in_flight = 0;
                    } else {
                        state.in_flight = state.in_flight.min(state.pending);
                    }
                    keep_refreshing = state.pending > 0;
                    if !keep_refreshing {
                        accounting.remove(path);
                    }
                }
            }

            if !accounting_checked || !keep_refreshing {
                remove_refreshing_path(refreshing, path);
            }
        }
    }

    pub fn refreshing_count(&self) -> usize {
        match self {
            Self::Ready { refreshing } => refreshing.len(),
            _ => 0,
        }
    }
}

pub enum SemanticIndexEvent {
    Progress {
        stage: String,
        files: Option<usize>,
        entries_done: Option<usize>,
        entries_total: Option<usize>,
        stats: Option<SemanticProgressStats>,
    },
    Ready(SemanticIndex),
    Failed(String),
}

/// Optional diagnostic counters attached to semantic-index progress updates.
///
/// These are intentionally estimates: they expose payload/cache/clone pressure
/// without requiring a platform-specific heap profiler in the Rust worker.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SemanticProgressStats {
    pub indexed_files: Option<usize>,
    pub entries: Option<usize>,
    pub vector_bytes: Option<u64>,
    pub snippet_bytes: Option<u64>,
    pub embed_text_bytes: Option<u64>,
    pub metadata_bytes: Option<u64>,
    pub estimated_payload_bytes: Option<u64>,
    pub cache_bytes: Option<u64>,
    pub clone_estimated_bytes: Option<u64>,
}

#[derive(Debug, Clone)]
pub enum SemanticRefreshRequest {
    Files { paths: Vec<PathBuf> },
    Corpus { current_files: Vec<PathBuf> },
}

#[derive(Debug)]
pub enum SemanticRefreshEvent {
    Started {
        paths: Vec<PathBuf>,
    },
    Completed {
        added_entries: Vec<EmbeddingEntry>,
        updated_metadata: Vec<(PathBuf, FileFreshness)>,
        completed_paths: Vec<PathBuf>,
    },
    CorpusCompleted {
        index: SemanticIndex,
        changed: usize,
        added: usize,
        deleted: usize,
        total_processed: usize,
    },
    Failed {
        paths: Vec<PathBuf>,
        error: String,
    },
    CorpusFailed {
        error: String,
    },
}

pub type SemanticRefreshWorkerSlot = Arc<Mutex<Option<std::thread::JoinHandle<()>>>>;

/// Normalize a path by resolving `.` and `..` components lexically,
/// without touching the filesystem. This prevents path traversal
/// attacks when `fs::canonicalize` fails (e.g. for non-existent paths).
fn normalize_path(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                // Pop the last component unless we're at root or have no components
                if !result.pop() {
                    result.push(component);
                }
            }
            Component::CurDir => {} // Skip `.`
            _ => result.push(component),
        }
    }
    result
}

fn resolve_with_existing_ancestors(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut tail_segments = Vec::new();

    while !existing.exists() {
        if let Some(name) = existing.file_name() {
            tail_segments.push(name.to_owned());
        } else {
            break;
        }

        existing = match existing.parent() {
            Some(parent) => parent.to_path_buf(),
            None => break,
        };
    }

    let mut resolved = std::fs::canonicalize(&existing).unwrap_or(existing);
    for segment in tail_segments.into_iter().rev() {
        resolved.push(segment);
    }

    resolved
}

fn path_error_response(
    req_id: &str,
    path: &Path,
    resolved_root: &Path,
) -> crate::protocol::Response {
    crate::protocol::Response::error(
        req_id,
        "path_outside_root",
        format!(
            "path '{}' is outside the project root '{}'",
            path.display(),
            resolved_root.display()
        ),
    )
}

/// Walk `candidate` component-by-component. For any component that is a
/// symlink on disk, iteratively follow the full chain (up to 40 hops) and
/// reject if any hop's resolved target lies outside `resolved_root`.
///
/// This is the fallback path used when `fs::canonicalize` fails (e.g. on
/// Linux with broken symlink chains pointing to non-existent destinations).
/// On macOS `canonicalize` also fails for broken symlinks but the returned
/// `/var/...` tempdir paths diverge from `resolved_root`'s `/private/var/...`
/// form, so we must accept either form when deciding which symlinks to check.
fn reject_escaping_symlink(
    req_id: &str,
    original_path: &Path,
    candidate: &Path,
    resolved_root: &Path,
    raw_root: &Path,
) -> Result<(), crate::protocol::Response> {
    let mut current = PathBuf::new();

    for component in candidate.components() {
        current.push(component);

        let Ok(metadata) = std::fs::symlink_metadata(&current) else {
            continue;
        };

        if !metadata.file_type().is_symlink() {
            continue;
        }

        // Only check symlinks that live inside the project root. This skips
        // OS-level prefix symlinks (macOS /var → /private/var) that are not
        // inside our project directory and whose "escaping" is harmless.
        //
        // We compare against BOTH the canonicalized root (resolved_root, e.g.
        // /private/var/.../project) AND the raw root (e.g. /var/.../project)
        // because tempdir() returns raw paths while fs::canonicalize returns
        // the resolved form — and our `current` may be in either form.
        let inside_root = current.starts_with(resolved_root) || current.starts_with(raw_root);
        if !inside_root {
            continue;
        }

        iterative_follow_chain(req_id, original_path, &current, resolved_root)?;
    }

    Ok(())
}

/// Iteratively follow a symlink chain from `link` and reject if any hop's
/// resolved target is outside `resolved_root`. Depth-capped at 40 hops.
fn iterative_follow_chain(
    req_id: &str,
    original_path: &Path,
    start: &Path,
    resolved_root: &Path,
) -> Result<(), crate::protocol::Response> {
    let mut link = start.to_path_buf();
    let mut depth = 0usize;

    loop {
        if depth > 40 {
            return Err(path_error_response(req_id, original_path, resolved_root));
        }

        let target = match std::fs::read_link(&link) {
            Ok(t) => t,
            Err(_) => {
                // Can't read the link — treat as escaping to be safe.
                return Err(path_error_response(req_id, original_path, resolved_root));
            }
        };

        let resolved_target = if target.is_absolute() {
            normalize_path(&target)
        } else {
            let parent = link.parent().unwrap_or_else(|| Path::new(""));
            normalize_path(&parent.join(&target))
        };

        // Check boundary: use canonicalized target when available (handles
        // macOS /var → /private/var aliasing), fall back to the normalized
        // path when canonicalize fails (e.g. broken symlink on Linux).
        let canonical_target =
            std::fs::canonicalize(&resolved_target).unwrap_or_else(|_| resolved_target.clone());

        if !canonical_target.starts_with(resolved_root)
            && !resolved_target.starts_with(resolved_root)
        {
            return Err(path_error_response(req_id, original_path, resolved_root));
        }

        // If the target is itself a symlink, follow the next hop.
        match std::fs::symlink_metadata(&resolved_target) {
            Ok(meta) if meta.file_type().is_symlink() => {
                link = resolved_target;
                depth += 1;
            }
            _ => break, // Non-symlink or non-existent target — chain ends here.
        }
    }

    Ok(())
}

/// Shared application context threaded through all command handlers.
///
/// Holds the language provider, backup/checkpoint stores, configuration,
/// and call graph engine. Constructed once at startup and passed by
/// reference to `dispatch`.
///
/// Stores use `RefCell` for interior mutability — the binary is single-threaded
/// (one request at a time on the stdin read loop) so runtime borrow checking
/// is safe and never contended.
pub struct AppContext {
    provider: Box<dyn LanguageProvider>,
    backup: RefCell<BackupStore>,
    checkpoint: RefCell<CheckpointStore>,
    db: RefCell<Option<Arc<Mutex<Connection>>>>,
    config: RefCell<Config>,
    pub harness: RefCell<Option<Harness>>,
    canonical_cache_root: RefCell<Option<PathBuf>>,
    is_worktree_bridge: RefCell<bool>,
    git_common_dir: RefCell<Option<PathBuf>>,
    /// Reasons (if any) why heavy AFT subsystems were auto-disabled for the
    /// current project root. Populated by `handle_configure` based on the
    /// canonical project root and synchronous file count. Each reason is a
    /// stable machine-readable string suffix (`"home_root"`,
    /// `"search_too_many_files:N"`, etc.) so the plugin can render distinct
    /// degraded-mode UI states without re-deriving the reason locally.
    /// Empty when the project is healthy / full-featured.
    degraded_reasons: RefCell<Vec<String>>,
    callgraph: RefCell<Option<CallGraph>>,
    search_index: RefCell<Option<SearchIndex>>,
    search_index_rx: RefCell<Option<crossbeam_channel::Receiver<SearchIndex>>>,
    pending_search_index_paths: RefCell<BTreeSet<PathBuf>>,
    symbol_cache: SharedSymbolCache,
    inspect_manager: Arc<InspectManager>,
    tier2_refresh_scheduler: RefCell<Tier2RefreshScheduler>,
    semantic_index: RefCell<Option<SemanticIndex>>,
    semantic_index_rx: RefCell<Option<crossbeam_channel::Receiver<SemanticIndexEvent>>>,
    semantic_index_status: RefCell<SemanticIndexStatus>,
    pending_semantic_index_paths: RefCell<BTreeSet<PathBuf>>,
    pending_semantic_corpus_refresh: RefCell<bool>,
    semantic_refresh_tx: RefCell<Option<crossbeam_channel::Sender<SemanticRefreshRequest>>>,
    semantic_refresh_event_rx: RefCell<Option<crossbeam_channel::Receiver<SemanticRefreshEvent>>>,
    semantic_refresh_worker: RefCell<Option<SemanticRefreshWorkerSlot>>,
    semantic_embedding_model: RefCell<Option<crate::semantic_index::EmbeddingModel>>,
    watcher: RefCell<Option<RecommendedWatcher>>,
    watcher_rx: RefCell<Option<mpsc::Receiver<notify::Result<notify::Event>>>>,
    lsp_manager: RefCell<LspManager>,
    /// Shared registry of LSP child PIDs. Cloned and passed to the signal
    /// handler so it can SIGKILL all children before aft exits, preventing
    /// orphaned LSP processes when bridge.shutdown() SIGTERMs aft.
    lsp_child_registry: crate::lsp::child_registry::LspChildRegistry,
    stdout_writer: SharedStdoutWriter,
    progress_sender: SharedProgressSender,
    configure_generation: AtomicU64,
    /// Last-seen value of `InspectManager::reuse_completion_count()`, so the
    /// per-request inspect drain can detect watcher-driven Tier-2 scans that
    /// finished since the previous tick and refresh the status bar (#3).
    last_seen_reuse_completions: AtomicU64,
    configure_warnings_tx: mpsc::Sender<(u64, ConfigureWarningsFrame)>,
    configure_warnings_rx: mpsc::Receiver<(u64, ConfigureWarningsFrame)>,
    status_emitter: StatusEmitter,
    bash_background: BgTaskRegistry,
    /// Thread-safe registry of TOML output filters. Lazy-built on first
    /// access; populated atomically via `RwLock`. Shared between command
    /// handlers (which use it through `filter_registry()` -> read guard) and
    /// the `BgTaskRegistry` watchdog thread (which uses it through
    /// `compress::compress_with_registry`). Reloaded when configure changes
    /// the project root or storage_dir; see [`AppContext::reset_filter_registry`].
    filter_registry: crate::compress::SharedFilterRegistry,
    /// Set to true once the filter_registry has been populated. Avoids
    /// double-loading on hot paths without holding a write lock.
    filter_registry_loaded: std::sync::atomic::AtomicBool,
    /// Live `experimental.bash.compress` flag, kept in sync with `config`
    /// from the configure handler. Exposed via [`AppContext::bash_compress_flag`]
    /// so the BgTaskRegistry's watchdog-thread compressor can read it without
    /// holding the config refcell.
    bash_compress_flag: Arc<std::sync::atomic::AtomicBool>,
    /// Project gitignore matcher, rebuilt by [`AppContext::rebuild_gitignore`]
    /// whenever `project_root` changes or a watcher event reports a
    /// `.gitignore` write. Used by the watcher event filter to decide which
    /// path-changes are interesting to AFT's caches. `None` when no project
    /// root is configured or when the project has no gitignore files; in that
    /// case the watcher falls back to a small hardcoded infra-directory skip.
    gitignore: RefCell<Option<Arc<ignore::gitignore::Gitignore>>>,
    /// Last-known Tier-2 + todos counts for the agent status bar, refreshed off
    /// the hot path (on `aft_inspect` reads and background Tier-2 completions).
    /// Errors/warnings are read live and not stored here.
    status_bar_tier2: RefCell<StatusBarTier2>,
}

impl AppContext {
    pub fn new(provider: Box<dyn LanguageProvider>, config: Config) -> Self {
        let bash_compress_enabled = config.experimental_bash_compress;
        let progress_sender = Arc::new(Mutex::new(None));
        let stdout_writer = Arc::new(Mutex::new(BufWriter::new(io::stdout())));
        let (configure_warnings_tx, configure_warnings_rx) = mpsc::channel();
        let status_emitter = StatusEmitter::new(Arc::clone(&progress_sender));
        let symbol_cache = provider
            .as_any()
            .downcast_ref::<crate::parser::TreeSitterProvider>()
            .map(|provider| provider.symbol_cache())
            .unwrap_or_else(|| Arc::new(std::sync::RwLock::new(SymbolCache::new())));
        let lsp_child_registry = crate::lsp::child_registry::LspChildRegistry::new();
        let mut lsp_manager = LspManager::new();
        lsp_manager.set_child_registry(lsp_child_registry.clone());
        AppContext {
            provider,
            backup: RefCell::new(BackupStore::new()),
            checkpoint: RefCell::new(CheckpointStore::new()),
            db: RefCell::new(None),
            config: RefCell::new(config),
            harness: RefCell::new(None),
            canonical_cache_root: RefCell::new(None),
            is_worktree_bridge: RefCell::new(false),
            git_common_dir: RefCell::new(None),
            degraded_reasons: RefCell::new(Vec::new()),
            callgraph: RefCell::new(None),
            search_index: RefCell::new(None),
            search_index_rx: RefCell::new(None),
            pending_search_index_paths: RefCell::new(BTreeSet::new()),
            symbol_cache,
            inspect_manager: Arc::new(InspectManager::new()),
            tier2_refresh_scheduler: RefCell::new(Tier2RefreshScheduler::new()),
            semantic_index: RefCell::new(None),
            semantic_index_rx: RefCell::new(None),
            semantic_index_status: RefCell::new(SemanticIndexStatus::Disabled),
            pending_semantic_index_paths: RefCell::new(BTreeSet::new()),
            pending_semantic_corpus_refresh: RefCell::new(false),
            semantic_refresh_tx: RefCell::new(None),
            semantic_refresh_event_rx: RefCell::new(None),
            semantic_refresh_worker: RefCell::new(None),
            semantic_embedding_model: RefCell::new(None),
            watcher: RefCell::new(None),
            watcher_rx: RefCell::new(None),
            lsp_manager: RefCell::new(lsp_manager),
            lsp_child_registry,
            stdout_writer,
            progress_sender: Arc::clone(&progress_sender),
            configure_generation: AtomicU64::new(0),
            last_seen_reuse_completions: AtomicU64::new(0),
            configure_warnings_tx,
            configure_warnings_rx,
            status_emitter,
            bash_background: BgTaskRegistry::new(progress_sender),
            filter_registry: Arc::new(std::sync::RwLock::new(
                crate::compress::toml_filter::FilterRegistry::default(),
            )),
            filter_registry_loaded: std::sync::atomic::AtomicBool::new(false),
            bash_compress_flag: Arc::new(std::sync::atomic::AtomicBool::new(bash_compress_enabled)),
            gitignore: RefCell::new(None),
            status_bar_tier2: RefCell::new(StatusBarTier2::default()),
        }
    }

    /// Current agent status-bar counts. `errors`/`warnings` are read LIVE from
    /// the LSP diagnostics store (continuously drained, no round-trip); the
    /// Tier-2 + todos counts are the last-known cached values. Returns `None`
    /// until the Tier-2 cache has been populated at least once, so we never
    /// surface a bar that misleadingly claims "0 dead code" before any scan.
    pub fn status_bar_counts(&self) -> Option<StatusBarCounts> {
        let tier2 = self.status_bar_tier2.borrow();
        // All three Tier-2 categories must hold a real value before the bar is
        // surfaced — otherwise a partially-scanned cold run would render a
        // fabricated `0` for the not-yet-completed categories (#1).
        let (Some(dead_code), Some(unused_exports), Some(duplicates)) =
            (tier2.dead_code, tier2.unused_exports, tier2.duplicates)
        else {
            return None;
        };
        let (errors, warnings) = self.lsp_manager.borrow().warm_error_warning_counts();
        Some(StatusBarCounts {
            errors,
            warnings,
            dead_code,
            unused_exports,
            duplicates,
            todos: tier2.todos.unwrap_or(0),
            tier2_stale: tier2.stale,
        })
    }

    /// Mark the status-bar Tier-2 counts stale (rendered with `~`) without
    /// changing the numbers — called when the watcher sees a source-file change,
    /// so the bar honestly signals the counts predate the latest edit until the
    /// next background scan completes. No-op before the first populate.
    pub fn mark_status_bar_tier2_stale(&self) {
        let mut tier2 = self.status_bar_tier2.borrow_mut();
        // No-op before the first full populate (nothing real to mark stale).
        if tier2.dead_code.is_some() && tier2.unused_exports.is_some() && tier2.duplicates.is_some()
        {
            tier2.stale = true;
        }
    }

    /// Refresh the cached Tier-2 + todos counts for the status bar. Each count
    /// is `Option`: `None` preserves the last-known value (the category wasn't
    /// recomputed or has no real aggregate yet) so we never overwrite a real
    /// count with a fabricated `0`. `stale` marks the Tier-2 numbers as
    /// not-yet-reconciled with the latest edits.
    pub fn update_status_bar_tier2(
        &self,
        dead_code: Option<usize>,
        unused_exports: Option<usize>,
        duplicates: Option<usize>,
        todos: Option<usize>,
        stale: bool,
    ) {
        let mut tier2 = self.status_bar_tier2.borrow_mut();
        if let Some(dead_code) = dead_code {
            tier2.dead_code = Some(dead_code);
        }
        if let Some(unused_exports) = unused_exports {
            tier2.unused_exports = Some(unused_exports);
        }
        if let Some(duplicates) = duplicates {
            tier2.duplicates = Some(duplicates);
        }
        if let Some(todos) = todos {
            tier2.todos = Some(todos);
        }
        tier2.stale = stale;
    }

    /// Borrow the cached project gitignore matcher. Returns `None` when no
    /// project_root is configured or when the project has no gitignore files.
    pub fn gitignore(&self) -> Option<Arc<ignore::gitignore::Gitignore>> {
        self.gitignore.borrow().clone()
    }

    /// Rebuild the gitignore matcher from the current `project_root` and
    /// cache it. Called by the configure handler whenever the project root
    /// changes, and by the watcher event drain when a `.gitignore` file
    /// itself is modified.
    ///
    /// The builder honors:
    /// - `<project_root>/.gitignore`
    /// - Git's global excludes file (the same source used by `ignore::WalkBuilder`)
    /// - the repository's real `info/exclude` file, resolved through Git's
    ///   common dir for linked worktrees
    /// - nested `.gitignore` files (each `.gitignore` discovered during
    ///   the recursive walk)
    ///
    /// Stores `None` if there's no project_root or no matchable gitignore
    /// files. Logs build errors but never fails configure.
    /// Clear any cached gitignore matcher without rebuilding.
    ///
    /// Used by `handle_configure` in degraded mode (e.g. `project_root == $HOME`)
    /// where running the gitignore-discovery walk would exceed the configure
    /// budget. The watcher event filter falls back to the hardcoded infra-dir
    /// skip list when no matcher is present.
    pub fn clear_gitignore(&self) {
        *self.gitignore.borrow_mut() = None;
    }

    pub fn rebuild_gitignore(&self) {
        use ignore::gitignore::GitignoreBuilder;
        use std::path::Path;
        let root_raw = match self.config().project_root.clone() {
            Some(r) => r,
            None => {
                *self.gitignore.borrow_mut() = None;
                return;
            }
        };
        // Canonicalize the root so symlink-prefix mismatches don't cause
        // `Gitignore::matched_path_or_any_parents` to panic on watcher event
        // paths. macOS routinely surfaces `/private/var/...` while `project_root`
        // arrives as `/var/...` (a symlink to `/private/var`); the `ignore`
        // crate's matcher panics when a query path isn't lexically under the
        // matcher's root. Canonicalizing both ends (here for root, naturally
        // for watcher events on macOS) keeps them in the same prefix space.
        let root = std::fs::canonicalize(&root_raw).unwrap_or(root_raw);
        let mut builder = GitignoreBuilder::new(&root);
        // Git's global excludes file — keep the live watcher matcher aligned
        // with the project walkers (`WalkBuilder::git_global(true)`). The
        // ignore crate exposes the same path discovery it uses internally, so
        // this handles the default XDG location and configured excludesFile.
        if let Some(global_ignore) = ignore::gitignore::gitconfig_excludes_path() {
            if global_ignore.is_file() {
                if let Some(err) = builder.add(&global_ignore) {
                    crate::slog_warn!(
                        "global gitignore parse error in {}: {}",
                        global_ignore.display(),
                        err
                    );
                }
            }
        }
        // Add root .gitignore (the most common case)
        let root_ignore = Path::new(&root).join(".gitignore");
        if root_ignore.exists() {
            if let Some(err) = builder.add(&root_ignore) {
                crate::slog_warn!(
                    "gitignore parse error in {}: {}",
                    root_ignore.display(),
                    err
                );
            }
        }
        // Root .aftignore — AFT-specific ignores layered on top of .gitignore.
        // Lets users exclude paths git can't (e.g. submodules) from AFT's
        // walks/indexes. Honored by the watcher matcher too, so edits under an
        // aftignored path don't trigger reindexing.
        let root_aftignore = Path::new(&root).join(".aftignore");
        if root_aftignore.exists() {
            if let Some(err) = builder.add(&root_aftignore) {
                crate::slog_warn!(
                    "aftignore parse error in {}: {}",
                    root_aftignore.display(),
                    err
                );
            }
        }
        // .git/info/exclude — manually added because GitignoreBuilder::new()
        // does not auto-discover it (verified against ignore-0.4.25 source).
        // In linked worktrees this lives under the repository common dir, not
        // under `<worktree>/.git/info/exclude` (where `.git` is only a file).
        let info_exclude = self
            .git_common_dir
            .borrow()
            .clone()
            .unwrap_or_else(|| Path::new(&root).join(".git"))
            .join("info")
            .join("exclude");
        if info_exclude.exists() {
            if let Some(err) = builder.add(&info_exclude) {
                crate::slog_warn!(
                    "gitignore parse error in {}: {}",
                    info_exclude.display(),
                    err
                );
            }
        }
        // Walk the project to pick up nested .gitignore/.aftignore files at
        // arbitrary depth. The main project walkers honor deeply nested ignore
        // files, so the watcher matcher must do the same or live invalidation
        // can disagree with startup indexing. Skip obvious infra dirs so we
        // don't accidentally load a vendored repo's ignore file as ours.
        let walker = ignore::WalkBuilder::new(&root)
            .standard_filters(true)
            // Hidden files are filtered by default, but `.gitignore` starts with
            // `.` so we need to traverse "hidden" entries to find nested ones.
            // No `max_depth`: nested `.gitignore`/`.aftignore` files are honored
            // at arbitrary depth (see configure_watcher_honors_deep_nested_aftignore).
            // The walk is pruned by standard gitignore filters plus the infra
            // skip below; configure never runs this against `$HOME` (guarded by
            // `home_match`), and tests use bounded roots rather than `/`.
            .hidden(false)
            .filter_entry(|entry| {
                let name = entry.file_name().to_string_lossy();
                !matches!(
                    name.as_ref(),
                    "node_modules" | "target" | ".git" | ".opencode" | ".alfonso"
                )
            })
            .build();
        for entry in walker.flatten() {
            let file_name = entry.file_name();
            let is_nested_gitignore = file_name == ".gitignore" && entry.path() != root_ignore;
            let is_nested_aftignore = file_name == ".aftignore" && entry.path() != root_aftignore;
            if is_nested_gitignore || is_nested_aftignore {
                if let Some(err) = builder.add(entry.path()) {
                    crate::slog_warn!(
                        "nested ignore parse error in {}: {}",
                        entry.path().display(),
                        err
                    );
                }
            }
        }
        match builder.build() {
            Ok(gi) => {
                let count = gi.num_ignores();
                if count > 0 {
                    crate::slog_info!("gitignore matcher built: {} pattern(s)", count);
                    *self.gitignore.borrow_mut() = Some(Arc::new(gi));
                } else {
                    *self.gitignore.borrow_mut() = None;
                }
            }
            Err(err) => {
                crate::slog_warn!("gitignore matcher build failed: {}", err);
                *self.gitignore.borrow_mut() = None;
            }
        }
    }

    /// Shared atomic mirror of `experimental.bash.compress`. Updated by the
    /// configure handler. Read by the BgTaskRegistry compressor closure.
    pub fn bash_compress_flag(&self) -> Arc<std::sync::atomic::AtomicBool> {
        Arc::clone(&self.bash_compress_flag)
    }

    /// Update the shared `bash_compress_flag` mirror. Call this from the
    /// configure handler whenever `experimental.bash.compress` changes so the
    /// BgTaskRegistry watchdog sees the new value on the next completion.
    pub fn sync_bash_compress_flag(&self) {
        let value = self.config().experimental_bash_compress;
        self.bash_compress_flag
            .store(value, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn set_bash_compress_enabled(&self, enabled: bool) {
        self.config_mut().experimental_bash_compress = enabled;
        self.bash_compress_flag
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
    }

    /// Read-only access to the TOML filter registry, building it lazily on
    /// first use. Returns an `RwLockReadGuard` that callers can `lookup`
    /// against directly.
    pub fn filter_registry(
        &self,
    ) -> std::sync::RwLockReadGuard<'_, crate::compress::toml_filter::FilterRegistry> {
        self.ensure_filter_registry_loaded();
        match self.filter_registry.read() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Returns the shared `Arc<RwLock<FilterRegistry>>` handle so threads
    /// outside `AppContext` (notably the bash watchdog) can read it without
    /// touching the rest of the context.
    pub fn shared_filter_registry(&self) -> crate::compress::SharedFilterRegistry {
        self.ensure_filter_registry_loaded();
        Arc::clone(&self.filter_registry)
    }

    /// Force a fresh load of the TOML filter registry. Called when configure
    /// changes the project root, storage_dir, or trust state so subsequent
    /// `compress::compress` calls pick up new filters.
    pub fn reset_filter_registry(&self) {
        let new_registry = crate::compress::build_registry_for_context(self);
        match self.filter_registry.write() {
            Ok(mut slot) => *slot = new_registry,
            Err(poisoned) => *poisoned.into_inner() = new_registry,
        }
        self.filter_registry_loaded
            .store(true, std::sync::atomic::Ordering::Release);
    }

    fn ensure_filter_registry_loaded(&self) {
        use std::sync::atomic::Ordering;
        if self.filter_registry_loaded.load(Ordering::Acquire) {
            return;
        }
        // Build outside the lock to avoid blocking other readers during a
        // multi-file TOML parse.
        let new_registry = crate::compress::build_registry_for_context(self);
        if let Ok(mut slot) = self.filter_registry.write() {
            *slot = new_registry;
            self.filter_registry_loaded.store(true, Ordering::Release);
        }
    }

    /// Clone the LSP child registry handle. Used by main.rs to give the
    /// signal handler thread a way to SIGKILL LSP children on shutdown.
    pub fn lsp_child_registry(&self) -> crate::lsp::child_registry::LspChildRegistry {
        self.lsp_child_registry.clone()
    }

    pub fn stdout_writer(&self) -> SharedStdoutWriter {
        Arc::clone(&self.stdout_writer)
    }

    pub fn set_progress_sender(&self, sender: Option<ProgressSender>) {
        if let Ok(mut progress_sender) = self.progress_sender.lock() {
            *progress_sender = sender;
        }
    }

    pub fn emit_progress(&self, frame: ProgressFrame) {
        let Ok(progress_sender) = self.progress_sender.lock().map(|sender| sender.clone()) else {
            return;
        };
        if let Some(sender) = progress_sender.as_ref() {
            sender(PushFrame::Progress(frame));
        }
    }

    pub fn status_emitter(&self) -> &StatusEmitter {
        &self.status_emitter
    }

    /// Get a clone of the current progress sender for use from background
    /// threads. Returns `None` when the main loop hasn't installed one (tests,
    /// CLI without push frames).
    ///
    /// Used by `configure`'s deferred file-walk thread to push warnings after
    /// configure has already returned, so configure latency stays sub-100 ms
    /// even on huge directories.
    pub fn progress_sender_handle(&self) -> Option<ProgressSender> {
        self.progress_sender
            .lock()
            .ok()
            .and_then(|sender| sender.clone())
    }

    pub fn advance_configure_generation(&self) -> u64 {
        self.configure_generation
            .fetch_add(1, Ordering::SeqCst)
            .wrapping_add(1)
    }

    pub fn configure_generation(&self) -> u64 {
        self.configure_generation.load(Ordering::SeqCst)
    }

    pub fn configure_warnings_sender(&self) -> mpsc::Sender<(u64, ConfigureWarningsFrame)> {
        self.configure_warnings_tx.clone()
    }

    pub fn drain_configure_warnings(&self) -> Vec<(u64, ConfigureWarningsFrame)> {
        let mut warnings = Vec::new();
        while let Ok(warning) = self.configure_warnings_rx.try_recv() {
            warnings.push(warning);
        }
        warnings
    }

    pub fn bash_background(&self) -> &BgTaskRegistry {
        &self.bash_background
    }

    pub fn drain_bg_completions(&self) -> Vec<BgCompletion> {
        self.bash_background.drain_completions()
    }

    /// Access the language provider.
    pub fn provider(&self) -> &dyn LanguageProvider {
        self.provider.as_ref()
    }

    /// Access the backup store.
    pub fn backup(&self) -> &RefCell<BackupStore> {
        &self.backup
    }

    /// Access the checkpoint store.
    pub fn checkpoint(&self) -> &RefCell<CheckpointStore> {
        &self.checkpoint
    }

    pub fn set_db(&self, conn: Arc<Mutex<Connection>>) {
        *self.db.borrow_mut() = Some(conn);
    }

    pub fn clear_db(&self) {
        *self.db.borrow_mut() = None;
    }

    pub fn db(&self) -> Option<Arc<Mutex<Connection>>> {
        self.db.borrow().clone()
    }

    /// Access the configuration (shared borrow).
    pub fn config(&self) -> Ref<'_, Config> {
        self.config.borrow()
    }

    /// Access the configuration (mutable borrow).
    pub fn config_mut(&self) -> RefMut<'_, Config> {
        self.config.borrow_mut()
    }

    pub fn set_harness(&self, harness: Harness) {
        *self.harness.borrow_mut() = Some(harness);
        self.bash_background.set_harness(harness);
    }

    pub fn harness_opt(&self) -> Option<Harness> {
        *self.harness.borrow()
    }

    pub fn harness(&self) -> Harness {
        self.harness_opt()
            .expect("harness set by configure before any tool call")
    }

    pub fn storage_dir(&self) -> PathBuf {
        crate::bash_background::storage_dir(self.config().storage_dir.as_deref())
    }

    pub fn harness_dir(&self) -> PathBuf {
        self.storage_dir().join(self.harness().as_str())
    }

    pub fn inspect_dir(&self) -> PathBuf {
        self.harness_dir().join("inspect")
    }

    pub fn bash_tasks_dir(&self, session_id: &str) -> PathBuf {
        self.harness_dir()
            .join("bash-tasks")
            .join(hash_session(session_id))
    }

    pub fn backups_dir(&self, session_id: &str, path_hash: &str) -> PathBuf {
        self.harness_dir()
            .join("backups")
            .join(hash_session(session_id))
            .join(path_hash)
    }

    pub fn filters_dir(&self) -> PathBuf {
        self.harness_dir().join("filters")
    }

    /// HOST-GLOBAL — NOT under harness_dir. Read by trust.rs across both harnesses.
    pub fn trust_file(&self) -> PathBuf {
        self.storage_dir().join("trusted-filter-projects.json")
    }

    pub fn set_canonical_cache_root(&self, root: PathBuf) {
        debug_assert!(root.is_absolute());
        *self.canonical_cache_root.borrow_mut() = Some(root);
    }

    pub fn canonical_cache_root(&self) -> PathBuf {
        self.canonical_cache_root
            .borrow()
            .clone()
            .expect("canonical_cache_root accessed before handle_configure")
    }

    pub fn canonical_cache_root_opt(&self) -> Option<PathBuf> {
        self.canonical_cache_root.borrow().clone()
    }

    pub fn set_cache_role(&self, is_worktree_bridge: bool, git_common_dir: Option<PathBuf>) {
        *self.is_worktree_bridge.borrow_mut() = is_worktree_bridge;
        *self.git_common_dir.borrow_mut() = git_common_dir;
    }

    pub fn is_worktree_bridge(&self) -> bool {
        *self.is_worktree_bridge.borrow()
    }

    pub fn git_common_dir(&self) -> Option<PathBuf> {
        self.git_common_dir.borrow().clone()
    }

    /// Replace the current degraded-mode reasons. Empty vec = full-featured
    /// mode (no degradation). Called by `handle_configure` after deciding
    /// which subsystems to disable for this project root.
    pub fn set_degraded_reasons(&self, reasons: Vec<String>) {
        *self.degraded_reasons.borrow_mut() = reasons;
    }

    /// Snapshot of current degraded-mode reasons. Order is stable
    /// (insertion order from `set_degraded_reasons`) so UI rendering and
    /// snapshot diffs are deterministic.
    pub fn degraded_reasons(&self) -> Vec<String> {
        self.degraded_reasons.borrow().clone()
    }

    /// True iff at least one degraded reason is recorded.
    pub fn is_degraded(&self) -> bool {
        !self.degraded_reasons.borrow().is_empty()
    }

    pub fn cache_role(&self) -> &'static str {
        if self.canonical_cache_root.borrow().is_none() {
            "not_initialized"
        } else if self.is_worktree_bridge() {
            "worktree"
        } else {
            "main"
        }
    }

    /// Access the call graph engine.
    pub fn callgraph(&self) -> &RefCell<Option<CallGraph>> {
        &self.callgraph
    }

    /// Access the search index.
    pub fn search_index(&self) -> &RefCell<Option<SearchIndex>> {
        &self.search_index
    }

    /// Access the search-index build receiver.
    pub fn search_index_rx(&self) -> &RefCell<Option<crossbeam_channel::Receiver<SearchIndex>>> {
        &self.search_index_rx
    }

    pub fn add_pending_search_index_paths<I>(&self, paths: I)
    where
        I: IntoIterator<Item = PathBuf>,
    {
        self.pending_search_index_paths.borrow_mut().extend(paths);
    }

    pub fn take_pending_search_index_paths(&self) -> Vec<PathBuf> {
        std::mem::take(&mut *self.pending_search_index_paths.borrow_mut())
            .into_iter()
            .collect()
    }

    pub fn add_pending_semantic_index_paths<I>(&self, paths: I)
    where
        I: IntoIterator<Item = PathBuf>,
    {
        self.pending_semantic_index_paths.borrow_mut().extend(paths);
    }

    pub fn take_pending_semantic_index_paths(&self) -> Vec<PathBuf> {
        std::mem::take(&mut *self.pending_semantic_index_paths.borrow_mut())
            .into_iter()
            .collect()
    }

    pub fn mark_pending_semantic_corpus_refresh(&self) {
        *self.pending_semantic_corpus_refresh.borrow_mut() = true;
    }

    pub fn take_pending_semantic_corpus_refresh(&self) -> bool {
        std::mem::take(&mut *self.pending_semantic_corpus_refresh.borrow_mut())
    }

    pub fn clear_pending_index_updates(&self) {
        self.pending_search_index_paths.borrow_mut().clear();
        self.pending_semantic_index_paths.borrow_mut().clear();
        *self.pending_semantic_corpus_refresh.borrow_mut() = false;
    }

    pub fn inspect_manager(&self) -> Arc<InspectManager> {
        Arc::clone(&self.inspect_manager)
    }

    /// Returns true when one or more watcher-driven (reuse-path) Tier-2 scans
    /// have completed since the last call, advancing the last-seen marker. The
    /// per-request inspect drain uses this to refresh the status bar after a
    /// background scan — those completions bypass `drain_completions`.
    pub fn take_new_reuse_completions(&self) -> bool {
        let current = self.inspect_manager.reuse_completion_count();
        let previous = self
            .last_seen_reuse_completions
            .swap(current, Ordering::SeqCst);
        current != previous
    }

    pub fn reset_tier2_refresh_scheduler(&self) {
        self.reset_tier2_refresh_scheduler_at(Instant::now());
    }

    #[doc(hidden)]
    pub fn reset_tier2_refresh_scheduler_at(&self, now: Instant) {
        self.tier2_refresh_scheduler
            .borrow_mut()
            .reset_after_configure(now);
    }

    pub fn request_tier2_refresh_pull(&self) -> bool {
        self.tier2_refresh_scheduler
            .borrow_mut()
            .request_pull(!self.is_worktree_bridge())
    }

    pub fn tick_tier2_refresh_scheduler(
        &self,
        changed_path_count: usize,
    ) -> Option<Tier2TriggerReason> {
        self.tick_tier2_refresh_scheduler_at(Instant::now(), changed_path_count)
    }

    #[doc(hidden)]
    pub fn tick_tier2_refresh_scheduler_at(
        &self,
        now: Instant,
        changed_path_count: usize,
    ) -> Option<Tier2TriggerReason> {
        let manager = self.inspect_manager();
        let can_write = !self.is_worktree_bridge();
        let in_flight = manager.tier2_any_in_flight();
        let decision = self.tier2_refresh_scheduler.borrow_mut().tick(
            now,
            changed_path_count,
            can_write,
            in_flight,
        );

        if let Some(reason) = decision {
            self.start_tier2_refresh(reason, manager);
        }

        decision
    }

    pub fn note_tier2_refresh_started(&self) {
        self.note_tier2_refresh_started_at(Instant::now());
    }

    #[doc(hidden)]
    pub fn note_tier2_refresh_started_at(&self, now: Instant) {
        self.tier2_refresh_scheduler
            .borrow_mut()
            .note_external_scan_started(now);
    }

    pub fn tier2_trigger_reason(&self) -> Option<&'static str> {
        self.tier2_refresh_scheduler
            .borrow()
            .last_trigger_reason()
            .map(Tier2TriggerReason::as_str)
    }

    #[doc(hidden)]
    pub fn tier2_pull_demand_pending(&self) -> bool {
        self.tier2_refresh_scheduler.borrow().pull_demand_pending()
    }

    fn start_tier2_refresh(&self, reason: Tier2TriggerReason, manager: Arc<InspectManager>) {
        if self.is_worktree_bridge() {
            return;
        }
        let Some(snapshot) = self.tier2_refresh_snapshot() else {
            return;
        };
        let categories = InspectCategory::active()
            .iter()
            .copied()
            .filter(|category| category.is_tier2())
            .collect::<Vec<_>>();
        let submission =
            manager.submit_tier2_run_with_reuse_serial_background(snapshot, categories);
        if submission.has_new_work() {
            crate::slog_info!(
                "tier2 refresh scheduled: reason={}, categories={:?}",
                reason.as_str(),
                submission
                    .newly_queued_categories
                    .iter()
                    .map(|category| category.as_str())
                    .collect::<Vec<_>>()
            );
        }
        for error in submission.errors {
            crate::slog_warn!(
                "tier2 refresh schedule failed for {}: {}",
                error.category,
                error.message
            );
        }
    }

    fn tier2_refresh_snapshot(&self) -> Option<InspectSnapshot> {
        self.harness_opt()?;
        let config = self.config().clone();
        let project_root = config
            .project_root
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let project_root = std::fs::canonicalize(&project_root).unwrap_or(project_root);
        Some(InspectSnapshot::new(
            project_root,
            self.inspect_dir(),
            Arc::new(config),
            self.symbol_cache(),
        ))
    }

    /// Access the shared symbol cache.
    pub fn symbol_cache(&self) -> SharedSymbolCache {
        Arc::clone(&self.symbol_cache)
    }

    /// Clear the shared symbol cache and return the new active generation.
    pub fn reset_symbol_cache(&self) -> u64 {
        self.symbol_cache
            .write()
            .map(|mut cache| cache.reset())
            .unwrap_or(0)
    }

    /// Access the semantic search index.
    pub fn semantic_index(&self) -> &RefCell<Option<SemanticIndex>> {
        &self.semantic_index
    }

    /// Access the semantic-index build receiver.
    pub fn semantic_index_rx(
        &self,
    ) -> &RefCell<Option<crossbeam_channel::Receiver<SemanticIndexEvent>>> {
        &self.semantic_index_rx
    }

    pub fn semantic_index_status(&self) -> &RefCell<SemanticIndexStatus> {
        &self.semantic_index_status
    }

    pub fn install_semantic_refresh_worker(
        &self,
        sender: crossbeam_channel::Sender<SemanticRefreshRequest>,
        event_rx: crossbeam_channel::Receiver<SemanticRefreshEvent>,
        worker_slot: SemanticRefreshWorkerSlot,
    ) {
        self.clear_semantic_refresh_worker();
        *self.semantic_refresh_tx.borrow_mut() = Some(sender);
        *self.semantic_refresh_event_rx.borrow_mut() = Some(event_rx);
        *self.semantic_refresh_worker.borrow_mut() = Some(worker_slot);
    }

    pub fn clear_semantic_refresh_worker(&self) {
        *self.semantic_refresh_tx.borrow_mut() = None;
        *self.semantic_refresh_event_rx.borrow_mut() = None;
        if let Some(worker_slot) = self.semantic_refresh_worker.borrow_mut().take() {
            if let Ok(mut handle) = worker_slot.lock() {
                drop(handle.take());
            }
        }
    }

    pub fn semantic_refresh_sender(
        &self,
    ) -> Option<crossbeam_channel::Sender<SemanticRefreshRequest>> {
        self.semantic_refresh_tx.borrow().clone()
    }

    pub fn semantic_refresh_event_rx(
        &self,
    ) -> &RefCell<Option<crossbeam_channel::Receiver<SemanticRefreshEvent>>> {
        &self.semantic_refresh_event_rx
    }

    /// Access the cached semantic embedding model.
    pub fn semantic_embedding_model(
        &self,
    ) -> &RefCell<Option<crate::semantic_index::EmbeddingModel>> {
        &self.semantic_embedding_model
    }

    /// Access the file watcher handle (kept alive to continue watching).
    pub fn watcher(&self) -> &RefCell<Option<RecommendedWatcher>> {
        &self.watcher
    }

    /// Access the watcher event receiver.
    pub fn watcher_rx(&self) -> &RefCell<Option<mpsc::Receiver<notify::Result<notify::Event>>>> {
        &self.watcher_rx
    }

    /// Access the LSP manager.
    pub fn lsp(&self) -> RefMut<'_, LspManager> {
        self.lsp_manager.borrow_mut()
    }

    /// Notify LSP servers that a file was written.
    /// Call this after write_format_validate in command handlers.
    pub fn lsp_notify_file_changed(&self, file_path: &Path, content: &str) {
        if let Ok(mut lsp) = self.lsp_manager.try_borrow_mut() {
            let config = self.config();
            if let Err(e) = lsp.notify_file_changed(file_path, content, &config) {
                crate::slog_warn!("sync error for {}: {}", file_path.display(), e);
            }
        }
    }

    /// Drop cached LSP diagnostics for a deleted/renamed-away file so its
    /// errors/warnings don't linger in the warm set (no server republishes for
    /// a vanished path), keeping the status bar and `aft_inspect` honest.
    /// Returns true if any entry was removed. Best-effort: a contended borrow is
    /// skipped silently (the watcher drain retries on subsequent events).
    pub fn lsp_clear_diagnostics_for_file(&self, file_path: &Path) -> bool {
        if let Ok(mut lsp) = self.lsp_manager.try_borrow_mut() {
            lsp.clear_diagnostics_for_file(file_path)
        } else {
            false
        }
    }

    /// Notify LSP and optionally wait for diagnostics.
    ///
    /// Call this after `write_format_validate` when the request has `"diagnostics": true`.
    /// Sends didChange to the server, waits briefly for publishDiagnostics, and returns
    /// any diagnostics for the file. If no server is running, returns empty immediately.
    ///
    /// v0.17.3: this is the version-aware path. Pre-edit cached diagnostics
    /// are NEVER returned — only entries whose `version` matches the
    /// post-edit document version (or, for unversioned servers, whose
    /// `epoch` advanced past the pre-edit snapshot).
    pub fn lsp_notify_and_collect_diagnostics(
        &self,
        file_path: &Path,
        content: &str,
        timeout: std::time::Duration,
    ) -> crate::lsp::manager::PostEditWaitOutcome {
        let Ok(mut lsp) = self.lsp_manager.try_borrow_mut() else {
            return crate::lsp::manager::PostEditWaitOutcome::default();
        };

        // Clear any queued notifications before this write so the wait loop only
        // observes diagnostics triggered by the current change.
        lsp.drain_events();

        // Snapshot per-server epochs and document versions BEFORE sending
        // didChange so the wait loop can prove freshness without accepting
        // stale pre-edit publishes that arrived late.
        let pre_snapshot = lsp.snapshot_pre_edit_state(file_path);

        // Send didChange/didOpen and capture per-server target version.
        let config = self.config();
        let expected_versions = match lsp.notify_file_changed_versioned(file_path, content, &config)
        {
            Ok(v) => v,
            Err(e) => {
                crate::slog_warn!("sync error for {}: {}", file_path.display(), e);
                return crate::lsp::manager::PostEditWaitOutcome::default();
            }
        };

        // No server matched this file — return an empty outcome that's
        // honestly `complete: true` (nothing to wait for).
        if expected_versions.is_empty() {
            return crate::lsp::manager::PostEditWaitOutcome::default();
        }

        lsp.wait_for_post_edit_diagnostics(
            file_path,
            &config,
            &expected_versions,
            &pre_snapshot,
            timeout,
        )
    }

    /// Collect custom server root_markers from user config for use in
    /// `is_config_file_path_with_custom` checks (#25).
    fn custom_lsp_root_markers(&self) -> Vec<String> {
        self.config()
            .lsp_servers
            .iter()
            .flat_map(|s| s.root_markers.iter().cloned())
            .collect()
    }

    fn notify_watched_config_files(&self, file_paths: &[PathBuf]) {
        let custom_markers = self.custom_lsp_root_markers();
        let config_paths: Vec<(PathBuf, FileChangeType)> = file_paths
            .iter()
            .filter(|path| is_config_file_path_with_custom(path, &custom_markers))
            .cloned()
            .map(|path| {
                let change_type = if path.exists() {
                    FileChangeType::CHANGED
                } else {
                    FileChangeType::DELETED
                };
                (path, change_type)
            })
            .collect();

        self.notify_watched_config_events(&config_paths);
    }

    fn multi_file_write_paths(params: &serde_json::Value) -> Option<Vec<PathBuf>> {
        let paths = params
            .get("multi_file_write_paths")
            .and_then(|value| value.as_array())?
            .iter()
            .filter_map(|value| value.as_str())
            .map(PathBuf::from)
            .collect::<Vec<_>>();

        (!paths.is_empty()).then_some(paths)
    }

    /// Parse config-file watched events from `multi_file_write_paths` when the
    /// array contains object entries `{ "path": "...", "type": "created|changed|deleted" }`.
    ///
    /// This handles the OBJECT variant of `multi_file_write_paths`. The STRING
    /// variant (bare path strings) is handled by `multi_file_write_paths()` and
    /// `notify_watched_config_files()`. Both variants read the same JSON key but
    /// with different per-entry schemas — they are NOT redundant.
    ///
    /// #18 note: in older code this function also existed alongside `multi_file_write_paths()`
    /// and was reachable via the `else if` branch when all entries were objects.
    /// Restoring both is correct.
    fn watched_file_events_from_params(
        params: &serde_json::Value,
        extra_markers: &[String],
    ) -> Option<Vec<(PathBuf, FileChangeType)>> {
        let events = params
            .get("multi_file_write_paths")
            .and_then(|value| value.as_array())?
            .iter()
            .filter_map(|entry| {
                // Only handle object entries — string entries go through multi_file_write_paths()
                let path = entry
                    .get("path")
                    .and_then(|value| value.as_str())
                    .map(PathBuf::from)?;

                if !is_config_file_path_with_custom(&path, extra_markers) {
                    return None;
                }

                let change_type = entry
                    .get("type")
                    .and_then(|value| value.as_str())
                    .and_then(Self::parse_file_change_type)
                    .unwrap_or_else(|| Self::change_type_from_current_state(&path));

                Some((path, change_type))
            })
            .collect::<Vec<_>>();

        (!events.is_empty()).then_some(events)
    }

    fn parse_file_change_type(value: &str) -> Option<FileChangeType> {
        match value {
            "created" | "CREATED" | "Created" => Some(FileChangeType::CREATED),
            "changed" | "CHANGED" | "Changed" => Some(FileChangeType::CHANGED),
            "deleted" | "DELETED" | "Deleted" => Some(FileChangeType::DELETED),
            _ => None,
        }
    }

    fn change_type_from_current_state(path: &Path) -> FileChangeType {
        if path.exists() {
            FileChangeType::CHANGED
        } else {
            FileChangeType::DELETED
        }
    }

    fn notify_watched_config_events(&self, config_paths: &[(PathBuf, FileChangeType)]) {
        if config_paths.is_empty() {
            return;
        }

        if let Ok(mut lsp) = self.lsp_manager.try_borrow_mut() {
            let config = self.config();
            if let Err(e) = lsp.notify_files_watched_changed(config_paths, &config) {
                crate::slog_warn!("watched-file sync error: {}", e);
            }
        }
    }

    pub fn lsp_notify_watched_config_file(&self, file_path: &Path, change_type: FileChangeType) {
        let custom_markers = self.custom_lsp_root_markers();
        if !is_config_file_path_with_custom(file_path, &custom_markers) {
            return;
        }

        self.notify_watched_config_events(&[(file_path.to_path_buf(), change_type)]);
    }

    /// Post-write LSP hook for multi-file edits. When the patch includes
    /// config-file edits, notify active workspace servers via
    /// `workspace/didChangeWatchedFiles` before sending the per-document
    /// didOpen/didChange for the current file.
    pub fn lsp_post_multi_file_write(
        &self,
        file_path: &Path,
        content: &str,
        file_paths: &[PathBuf],
        params: &serde_json::Value,
    ) -> Option<crate::lsp::manager::PostEditWaitOutcome> {
        self.notify_watched_config_files(file_paths);

        let wants_diagnostics = params
            .get("diagnostics")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if !wants_diagnostics {
            self.lsp_notify_file_changed(file_path, content);
            return None;
        }

        let wait_ms = params
            .get("wait_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(3000)
            .min(10_000);

        Some(self.lsp_notify_and_collect_diagnostics(
            file_path,
            content,
            std::time::Duration::from_millis(wait_ms),
        ))
    }

    /// Post-write LSP hook: notify server and optionally collect diagnostics.
    ///
    /// This is the single call site for all command handlers after `write_format_validate`.
    /// Behavior:
    /// - When `diagnostics: true` is in `params`, notifies the server, waits
    ///   until matching diagnostics arrive or the timeout expires, and returns
    ///   `Some(outcome)` with the verified-fresh diagnostics + per-server
    ///   status.
    /// - When `diagnostics: false` (or absent), just notifies (fire-and-forget)
    ///   and returns `None`. Callers must NOT wrap this in `Some(...)`; the
    ///   `None` is what tells the response builder to omit the LSP fields
    ///   entirely (preserves the no-diagnostics-requested response shape).
    ///
    /// v0.17.3: default `wait_ms` raised from 1500 to 3000 because real-world
    /// tsserver re-analysis on monorepo files routinely takes 2-5s. Still
    /// capped at 10000ms.
    pub fn lsp_post_write(
        &self,
        file_path: &Path,
        content: &str,
        params: &serde_json::Value,
    ) -> Option<crate::lsp::manager::PostEditWaitOutcome> {
        let wants_diagnostics = params
            .get("diagnostics")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let custom_markers = self.custom_lsp_root_markers();

        if !wants_diagnostics {
            if let Some(file_paths) = Self::multi_file_write_paths(params) {
                self.notify_watched_config_files(&file_paths);
            } else if let Some(config_events) =
                Self::watched_file_events_from_params(params, &custom_markers)
            {
                self.notify_watched_config_events(&config_events);
            }
            self.lsp_notify_file_changed(file_path, content);
            return None;
        }

        let wait_ms = params
            .get("wait_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(3000)
            .min(10_000); // Cap at 10 seconds to prevent hangs from adversarial input

        if let Some(file_paths) = Self::multi_file_write_paths(params) {
            return self.lsp_post_multi_file_write(file_path, content, &file_paths, params);
        }

        if let Some(config_events) = Self::watched_file_events_from_params(params, &custom_markers)
        {
            self.notify_watched_config_events(&config_events);
        }

        Some(self.lsp_notify_and_collect_diagnostics(
            file_path,
            content,
            std::time::Duration::from_millis(wait_ms),
        ))
    }

    /// Validate that a file path falls within the configured project root.
    ///
    /// When `project_root` is configured (normal plugin usage), this resolves the
    /// path and checks it starts with the root. Returns the canonicalized path on
    /// success, or an error response on violation.
    ///
    /// When no `project_root` is configured (direct CLI usage), all paths pass
    /// through unrestricted for backward compatibility.
    pub fn validate_path(
        &self,
        req_id: &str,
        path: &Path,
    ) -> Result<std::path::PathBuf, crate::protocol::Response> {
        let config = self.config();
        // When restrict_to_project_root is false (default), allow all paths
        if !config.restrict_to_project_root {
            return Ok(path.to_path_buf());
        }
        let root = match &config.project_root {
            Some(r) => r.clone(),
            None => return Ok(path.to_path_buf()), // No root configured, allow all
        };
        drop(config);

        // Keep the raw root for symlink-guard comparisons. On macOS, tempdir()
        // returns /var/... paths while canonicalize gives /private/var/...; we
        // need both forms so reject_escaping_symlink can recognise in-root
        // symlinks regardless of which prefix form `current` happens to have.
        let raw_root = root.clone();
        let resolved_root = std::fs::canonicalize(&root).unwrap_or(root);

        // Resolve the path (follow symlinks, normalize ..). If canonicalization
        // fails (e.g. path does not exist or traverses a broken symlink), inspect
        // every existing component with lstat before falling back lexically so a
        // broken in-root symlink cannot be used to write outside project_root.
        let path_for_resolution = if path.is_relative() {
            raw_root.join(path)
        } else {
            path.to_path_buf()
        };
        let resolved = match std::fs::canonicalize(&path_for_resolution) {
            Ok(resolved) => resolved,
            Err(_) => {
                let normalized = normalize_path(&path_for_resolution);
                reject_escaping_symlink(
                    req_id,
                    &path_for_resolution,
                    &normalized,
                    &resolved_root,
                    &raw_root,
                )?;
                resolve_with_existing_ancestors(&normalized)
            }
        };

        if !resolved.starts_with(&resolved_root) {
            return Err(path_error_response(req_id, path, &resolved_root));
        }

        Ok(resolved)
    }

    /// Count active LSP server instances.
    pub fn lsp_server_count(&self) -> usize {
        self.lsp_manager
            .try_borrow()
            .map(|lsp| lsp.server_count())
            .unwrap_or(0)
    }

    /// Symbol cache statistics from the language provider.
    pub fn symbol_cache_stats(&self) -> serde_json::Value {
        let entries = self
            .symbol_cache
            .read()
            .map(|cache| cache.len())
            .unwrap_or(0);
        serde_json::json!({
            "local_entries": entries,
            "warm_entries": 0,
        })
    }
}

#[cfg(test)]
mod status_emitter_tests {
    use super::*;
    use crate::parser::TreeSitterProvider;

    fn ctx_with_frame_rx() -> (AppContext, mpsc::Receiver<PushFrame>) {
        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        let (tx, rx) = mpsc::channel();
        ctx.set_progress_sender(Some(Arc::new(Box::new(move |frame| {
            let _ = tx.send(frame);
        }))));
        (ctx, rx)
    }

    #[test]
    fn status_emitter_signal_triggers_push() {
        let (ctx, rx) = ctx_with_frame_rx();
        ctx.status_emitter().signal(ctx.build_status_snapshot());
        let frame = rx
            .recv_timeout(Duration::from_millis(STATUS_DEBOUNCE_MS + 500))
            .expect("status_changed push");
        assert!(matches!(frame, PushFrame::StatusChanged(_)));
    }

    #[test]
    fn status_emitter_debounces_burst() {
        let (ctx, rx) = ctx_with_frame_rx();
        for _ in 0..10 {
            ctx.status_emitter().signal(ctx.build_status_snapshot());
        }
        let frame = rx
            .recv_timeout(Duration::from_millis(STATUS_DEBOUNCE_MS + 500))
            .expect("status_changed push");
        assert!(matches!(frame, PushFrame::StatusChanged(_)));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn status_emitter_separate_windows_separate_pushes() {
        let (ctx, rx) = ctx_with_frame_rx();
        ctx.status_emitter().signal(ctx.build_status_snapshot());
        rx.recv_timeout(Duration::from_millis(STATUS_DEBOUNCE_MS + 500))
            .expect("first push");
        ctx.status_emitter().signal(ctx.build_status_snapshot());
        rx.recv_timeout(Duration::from_millis(STATUS_DEBOUNCE_MS + 500))
            .expect("second push");
    }

    #[test]
    fn status_emitter_no_signal_no_push() {
        let (_ctx, rx) = ctx_with_frame_rx();
        assert!(rx
            .recv_timeout(Duration::from_millis(STATUS_DEBOUNCE_MS + 100))
            .is_err());
    }

    #[test]
    fn status_emitter_shutdown_cleanly_exits_debounce_thread() {
        let (ctx, rx) = ctx_with_frame_rx();
        drop(ctx);
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
    }
}

#[cfg(test)]
mod status_bar_tests {
    use super::*;
    use crate::parser::TreeSitterProvider;

    fn ctx() -> AppContext {
        AppContext::new(Box::new(TreeSitterProvider::new()), Config::default())
    }

    #[test]
    fn status_bar_counts_none_until_tier2_populated() {
        let ctx = ctx();
        // No scan has run yet — never surface a bar claiming "0 dead code".
        assert!(ctx.status_bar_counts().is_none());

        ctx.update_status_bar_tier2(Some(5), Some(3), Some(7), Some(2), false);
        let counts = ctx.status_bar_counts().expect("populated");
        assert_eq!(counts.dead_code, 5);
        assert_eq!(counts.unused_exports, 3);
        assert_eq!(counts.duplicates, 7);
        assert_eq!(counts.todos, 2);
        assert!(!counts.tier2_stale);
        // Errors/warnings are read live from an empty LSP store → 0.
        assert_eq!(counts.errors, 0);
        assert_eq!(counts.warnings, 0);
    }

    #[test]
    fn partial_tier2_does_not_fabricate_zeros() {
        let ctx = ctx();
        // Only dead_code has completed (the slow first serial category); the
        // other two are still in flight. The bar must stay suppressed rather
        // than render `D5 U0 C0` with fabricated zeros (#1).
        ctx.update_status_bar_tier2(Some(5), None, None, None, true);
        assert!(
            ctx.status_bar_counts().is_none(),
            "bar must not surface until all three Tier-2 categories are real"
        );

        // Second category completes — still incomplete, still suppressed.
        ctx.update_status_bar_tier2(None, Some(3), None, None, true);
        assert!(ctx.status_bar_counts().is_none());

        // Final category completes → bar surfaces with all real counts, and
        // none of them were ever fabricated.
        ctx.update_status_bar_tier2(None, None, Some(7), None, false);
        let counts = ctx.status_bar_counts().expect("all three real now");
        assert_eq!(counts.dead_code, 5);
        assert_eq!(counts.unused_exports, 3);
        assert_eq!(counts.duplicates, 7);
    }

    #[test]
    fn update_with_none_todos_preserves_last_known_todos() {
        let ctx = ctx();
        ctx.update_status_bar_tier2(Some(1), Some(1), Some(1), Some(9), false);
        // A background-scan refresh passes todos=None → todo count preserved.
        ctx.update_status_bar_tier2(Some(2), Some(2), Some(2), None, false);
        let counts = ctx.status_bar_counts().expect("populated");
        assert_eq!(counts.todos, 9);
        assert_eq!(counts.dead_code, 2);
    }

    #[test]
    fn update_with_none_count_preserves_last_known_count() {
        let ctx = ctx();
        ctx.update_status_bar_tier2(Some(10), Some(20), Some(30), None, false);
        // A refresh that only recomputed dead_code preserves the other two
        // real counts rather than overwriting them with a fabricated 0.
        ctx.update_status_bar_tier2(Some(11), None, None, None, false);
        let counts = ctx.status_bar_counts().expect("populated");
        assert_eq!(counts.dead_code, 11);
        assert_eq!(counts.unused_exports, 20);
        assert_eq!(counts.duplicates, 30);
    }

    #[test]
    fn mark_stale_sets_flag_only_after_populate() {
        let ctx = ctx();
        // No-op before first populate.
        ctx.mark_status_bar_tier2_stale();
        assert!(ctx.status_bar_counts().is_none());

        ctx.update_status_bar_tier2(Some(4), Some(0), Some(0), Some(0), false);
        ctx.mark_status_bar_tier2_stale();
        assert!(ctx.status_bar_counts().expect("populated").tier2_stale);

        // A completed scan clears stale.
        ctx.update_status_bar_tier2(Some(4), Some(0), Some(0), None, false);
        assert!(!ctx.status_bar_counts().expect("populated").tier2_stale);
    }

    // End-to-end wiring: a diagnostic for a file inflates the status-bar `E`
    // count (read live from the warm LSP set); clearing that file's diagnostics
    // (the deleted-file path) drops it back. This is the AppContext glue between
    // the watcher-drain clear and the agent-visible bar.
    #[test]
    fn clearing_diagnostics_for_deleted_file_drops_status_bar_errors() {
        use crate::lsp::diagnostics::{DiagnosticSeverity, StoredDiagnostic};
        use crate::lsp::registry::ServerKind;
        use crate::lsp::roots::ServerKey;

        let ctx = ctx();
        ctx.update_status_bar_tier2(Some(0), Some(0), Some(0), Some(0), false); // populate so the bar surfaces

        let file = std::path::PathBuf::from("/proj/gone.ts");
        {
            let mut lsp = ctx.lsp();
            lsp.diagnostics_store_mut_for_test().publish(
                ServerKey {
                    kind: ServerKind::TypeScript,
                    root: std::path::PathBuf::from("/proj"),
                },
                file.clone(),
                vec![StoredDiagnostic {
                    file: file.clone(),
                    line: 1,
                    column: 1,
                    end_line: 1,
                    end_column: 2,
                    severity: DiagnosticSeverity::Error,
                    message: "boom".into(),
                    code: None,
                    source: None,
                }],
            );
        }

        // Bar reflects the live warm-set error.
        assert_eq!(ctx.status_bar_counts().expect("populated").errors, 1);

        // Clearing the (now-deleted) file's diagnostics drops the count.
        let removed = ctx.lsp_clear_diagnostics_for_file(&file);
        assert!(removed);
        assert_eq!(ctx.status_bar_counts().expect("populated").errors, 0);
    }
}

#[cfg(test)]
mod harness_path_tests {
    use super::*;
    use crate::harness::Harness;
    use crate::parser::TreeSitterProvider;

    fn ctx_with_storage_and_harness(storage_dir: PathBuf, harness: Harness) -> AppContext {
        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        ctx.config_mut().storage_dir = Some(storage_dir);
        ctx.set_harness(harness);
        ctx
    }

    #[test]
    fn harness_dir_resolves_correctly() {
        let storage = PathBuf::from("/tmp/cortexkit/aft");
        let ctx = ctx_with_storage_and_harness(storage.clone(), Harness::Pi);

        assert_eq!(ctx.harness_dir(), storage.join("pi"));
    }

    #[test]
    fn bash_tasks_dir_uses_hash_session() {
        let storage = PathBuf::from("/tmp/cortexkit/aft");
        let ctx = ctx_with_storage_and_harness(storage.clone(), Harness::Opencode);

        assert_eq!(
            ctx.bash_tasks_dir("ses_abc"),
            storage
                .join("opencode")
                .join("bash-tasks")
                .join(hash_session("ses_abc"))
        );
    }

    #[test]
    fn backups_dir_includes_path_hash() {
        let storage = PathBuf::from("/tmp/cortexkit/aft");
        let ctx = ctx_with_storage_and_harness(storage.clone(), Harness::Pi);

        assert_eq!(
            ctx.backups_dir("ses_abc", "pathhash"),
            storage
                .join("pi")
                .join("backups")
                .join(hash_session("ses_abc"))
                .join("pathhash")
        );
    }

    #[test]
    fn filters_dir_under_harness() {
        let storage = PathBuf::from("/tmp/cortexkit/aft");
        let ctx = ctx_with_storage_and_harness(storage.clone(), Harness::Opencode);

        assert_eq!(ctx.filters_dir(), storage.join("opencode").join("filters"));
    }

    #[test]
    fn trust_file_is_host_global() {
        let storage = PathBuf::from("/tmp/cortexkit/aft");
        let ctx = ctx_with_storage_and_harness(storage.clone(), Harness::Pi);

        assert_eq!(
            ctx.trust_file(),
            storage.join("trusted-filter-projects.json")
        );
    }

    #[test]
    fn same_session_different_harness_resolve_different_paths() {
        let storage = PathBuf::from("/tmp/cortexkit/aft");
        let opencode = ctx_with_storage_and_harness(storage.clone(), Harness::Opencode);
        let pi = ctx_with_storage_and_harness(storage, Harness::Pi);

        assert_ne!(
            opencode.bash_tasks_dir("ses_same"),
            pi.bash_tasks_dir("ses_same")
        );
    }
}

#[cfg(test)]
mod gitignore_tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    fn make_ctx_with_root(root: &Path) -> AppContext {
        let provider = Box::new(crate::parser::TreeSitterProvider::new());
        let config = Config {
            project_root: Some(root.to_path_buf()),
            ..Config::default()
        };
        AppContext::new(provider, config)
    }

    /// Helper: returns true when the matcher would skip `path` (as if it
    /// arrived via a watcher event for this project root). Canonicalizes
    /// the query path so symlink prefixes (e.g. macOS `/var` → `/private/var`)
    /// don't trip the `ignore` crate's "path is expected to be under the
    /// root" panic — production code does the same guard via
    /// `path.starts_with(matcher.path())` in `drain_watcher_events`.
    fn is_ignored(ctx: &AppContext, path: &Path) -> bool {
        let Some(matcher) = ctx.gitignore() else {
            return false;
        };
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if !canonical.starts_with(matcher.path()) {
            return false;
        }
        let is_dir = canonical.is_dir();
        matcher
            .matched_path_or_any_parents(&canonical, is_dir)
            .is_ignore()
    }

    /// Run `f` with global git-ignore discovery neutralized.
    ///
    /// `rebuild_gitignore` loads git's global excludes (the `ignore` crate
    /// resolves `$XDG_CONFIG_HOME/git/ignore`, falling back to
    /// `$HOME/.config/git/ignore`). A developer machine commonly has that file,
    /// so a "no project ignore → None" assertion is only deterministic when
    /// global discovery is pointed at an empty directory. Pointing
    /// `XDG_CONFIG_HOME` at a fresh tempdir does that without touching `HOME`
    /// (so it can't race the `HOME`-mutating configure tests). Serialized by a
    /// process-local mutex; env is restored before the closure result is used.
    fn with_neutralized_global_gitignore<R>(f: impl FnOnce() -> R) -> R {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let _guard = LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let prev = std::env::var_os("XDG_CONFIG_HOME");
        // SAFETY: serialized by LOCK above; restored immediately after `f`.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", tmp.path());
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        unsafe {
            match prev {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
        match result {
            Ok(r) => r,
            Err(p) => std::panic::resume_unwind(p),
        }
    }

    #[test]
    fn rebuild_gitignore_returns_none_without_project_root() {
        let provider = Box::new(crate::parser::TreeSitterProvider::new());
        let ctx = AppContext::new(provider, Config::default());
        with_neutralized_global_gitignore(|| ctx.rebuild_gitignore());
        assert!(ctx.gitignore().is_none());
    }

    #[test]
    fn rebuild_gitignore_returns_none_for_project_with_no_gitignore() {
        let tmp = TempDir::new().unwrap();
        let ctx = make_ctx_with_root(tmp.path());
        with_neutralized_global_gitignore(|| ctx.rebuild_gitignore());
        assert!(ctx.gitignore().is_none());
    }

    #[test]
    fn matcher_filters_files_in_ignored_dist_dir() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".gitignore"), "dist/\nbuild/\n").unwrap();
        fs::create_dir_all(tmp.path().join("dist")).unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        let dist_file = tmp.path().join("dist").join("bundle.js");
        let src_file = tmp.path().join("src").join("app.ts");
        fs::write(&dist_file, "x").unwrap();
        fs::write(&src_file, "y").unwrap();

        let ctx = make_ctx_with_root(tmp.path());
        ctx.rebuild_gitignore();

        assert!(ctx.gitignore().is_some());
        assert!(
            is_ignored(&ctx, &dist_file),
            "dist/bundle.js should be ignored"
        );
        assert!(
            !is_ignored(&ctx, &src_file),
            "src/app.ts should NOT be ignored"
        );
    }

    #[test]
    fn matcher_handles_node_modules_and_target() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".gitignore"), "node_modules/\ntarget/\n").unwrap();
        fs::create_dir_all(tmp.path().join("node_modules/foo")).unwrap();
        fs::create_dir_all(tmp.path().join("target/debug")).unwrap();
        let nm_file = tmp.path().join("node_modules/foo/index.js");
        let target_file = tmp.path().join("target/debug/aft");
        fs::write(&nm_file, "x").unwrap();
        fs::write(&target_file, "x").unwrap();

        let ctx = make_ctx_with_root(tmp.path());
        ctx.rebuild_gitignore();

        assert!(is_ignored(&ctx, &nm_file));
        assert!(is_ignored(&ctx, &target_file));
    }

    #[test]
    fn matcher_honors_negation_pattern() {
        // .gitignore: ignore all *.log files EXCEPT important.log
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".gitignore"), "*.log\n!important.log\n").unwrap();
        let random_log = tmp.path().join("random.log");
        let important_log = tmp.path().join("important.log");
        fs::write(&random_log, "x").unwrap();
        fs::write(&important_log, "y").unwrap();

        let ctx = make_ctx_with_root(tmp.path());
        ctx.rebuild_gitignore();

        assert!(is_ignored(&ctx, &random_log));
        assert!(
            !is_ignored(&ctx, &important_log),
            "negation pattern should un-ignore important.log"
        );
    }

    #[test]
    fn rebuild_picks_up_gitignore_changes() {
        let tmp = TempDir::new().unwrap();
        let ignore_path = tmp.path().join(".gitignore");
        fs::write(&ignore_path, "foo.txt\n").unwrap();
        let foo = tmp.path().join("foo.txt");
        let bar = tmp.path().join("bar.txt");
        fs::write(&foo, "").unwrap();
        fs::write(&bar, "").unwrap();

        let ctx = make_ctx_with_root(tmp.path());
        ctx.rebuild_gitignore();
        assert!(is_ignored(&ctx, &foo));
        assert!(!is_ignored(&ctx, &bar));

        // Now flip the rules: ignore bar.txt instead of foo.txt
        fs::write(&ignore_path, "bar.txt\n").unwrap();
        ctx.rebuild_gitignore();
        assert!(!is_ignored(&ctx, &foo));
        assert!(is_ignored(&ctx, &bar));
    }

    #[test]
    fn gitignore_loads_info_exclude_when_present() {
        let tmp = TempDir::new().unwrap();
        let info_dir = tmp.path().join(".git/info");
        fs::create_dir_all(&info_dir).unwrap();
        fs::write(info_dir.join("exclude"), "secrets.txt\n").unwrap();
        let secrets = tmp.path().join("secrets.txt");
        let public = tmp.path().join("public.txt");
        fs::write(&secrets, "token").unwrap();
        fs::write(&public, "ok").unwrap();

        let ctx = make_ctx_with_root(tmp.path());
        ctx.rebuild_gitignore();

        assert!(is_ignored(&ctx, &secrets));
        assert!(!is_ignored(&ctx, &public));
    }

    #[test]
    fn matcher_picks_up_nested_gitignore() {
        let tmp = TempDir::new().unwrap();
        // Root .gitignore is intentionally empty — only the nested one ignores
        fs::write(tmp.path().join(".gitignore"), "").unwrap();
        let sub = tmp.path().join("packages/foo");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join(".gitignore"), "generated/\n").unwrap();
        let generated_file = sub.join("generated").join("out.js");
        fs::create_dir_all(generated_file.parent().unwrap()).unwrap();
        fs::write(&generated_file, "x").unwrap();

        let ctx = make_ctx_with_root(tmp.path());
        ctx.rebuild_gitignore();

        assert!(
            is_ignored(&ctx, &generated_file),
            "nested gitignore in packages/foo/.gitignore should ignore generated/"
        );
    }
}
