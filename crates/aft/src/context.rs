use std::cell::{Ref, RefCell, RefMut};
use std::io::{self, BufWriter};
use std::path::{Component, Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
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
use crate::language::LanguageProvider;
use crate::lsp::manager::LspManager;
use crate::lsp::registry::is_config_file_path_with_custom;
use crate::parser::{SharedSymbolCache, SymbolCache};
use crate::protocol::{ProgressFrame, PushFrame, StatusChangedFrame, StatusPayload};

pub type ProgressSender = Arc<Box<dyn Fn(PushFrame) + Send + Sync>>;
pub type SharedProgressSender = Arc<Mutex<Option<ProgressSender>>>;
pub type SharedStdoutWriter = Arc<Mutex<BufWriter<io::Stdout>>>;
const STATUS_DEBOUNCE_MS: u64 = 1_000;

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
use crate::search_index::SearchIndex;
use crate::semantic_index::SemanticIndex;

#[derive(Debug, Clone)]
pub enum SemanticIndexStatus {
    Disabled,
    Building {
        stage: String,
        files: Option<usize>,
        entries_done: Option<usize>,
        entries_total: Option<usize>,
    },
    Ready,
    Failed(String),
}

pub enum SemanticIndexEvent {
    Progress {
        stage: String,
        files: Option<usize>,
        entries_done: Option<usize>,
        entries_total: Option<usize>,
    },
    Ready(SemanticIndex),
    Failed(String),
}

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
    symbol_cache: SharedSymbolCache,
    semantic_index: RefCell<Option<SemanticIndex>>,
    semantic_index_rx: RefCell<Option<crossbeam_channel::Receiver<SemanticIndexEvent>>>,
    semantic_index_status: RefCell<SemanticIndexStatus>,
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
}

impl AppContext {
    pub fn new(provider: Box<dyn LanguageProvider>, config: Config) -> Self {
        let bash_compress_enabled = config.experimental_bash_compress;
        let progress_sender = Arc::new(Mutex::new(None));
        let stdout_writer = Arc::new(Mutex::new(BufWriter::new(io::stdout())));
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
            symbol_cache,
            semantic_index: RefCell::new(None),
            semantic_index_rx: RefCell::new(None),
            semantic_index_status: RefCell::new(SemanticIndexStatus::Disabled),
            semantic_embedding_model: RefCell::new(None),
            watcher: RefCell::new(None),
            watcher_rx: RefCell::new(None),
            lsp_manager: RefCell::new(lsp_manager),
            lsp_child_registry,
            stdout_writer,
            progress_sender: Arc::clone(&progress_sender),
            status_emitter,
            bash_background: BgTaskRegistry::new(progress_sender),
            filter_registry: Arc::new(std::sync::RwLock::new(
                crate::compress::toml_filter::FilterRegistry::default(),
            )),
            filter_registry_loaded: std::sync::atomic::AtomicBool::new(false),
            bash_compress_flag: Arc::new(std::sync::atomic::AtomicBool::new(bash_compress_enabled)),
            gitignore: RefCell::new(None),
        }
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
    /// - `<project_root>/.git/info/exclude` (loaded explicitly because
    ///   `GitignoreBuilder::new` does not auto-discover it)
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
        // .git/info/exclude — manually added because GitignoreBuilder::new()
        // does not auto-discover it (verified against ignore-0.4.25 source).
        let info_exclude = Path::new(&root).join(".git").join("info").join("exclude");
        if info_exclude.exists() {
            if let Some(err) = builder.add(&info_exclude) {
                crate::slog_warn!(
                    "gitignore parse error in {}: {}",
                    info_exclude.display(),
                    err
                );
            }
        }
        // Walk the project to pick up nested .gitignore files. Cap the walk
        // at the same SOURCE_WALK_LIMIT used by other configure-time walks
        // (currently 20000 files); gitignore lookup-cost stays bounded for
        // huge monorepos. Skip the obvious infra dirs so we don't accidentally
        // load a vendored repo's .gitignore that doesn't apply to ours.
        let walker = ignore::WalkBuilder::new(&root)
            .standard_filters(true)
            // Hidden files are filtered by default, but `.gitignore` starts with
            // `.` so we need to traverse "hidden" entries to find nested ones.
            .hidden(false)
            .max_depth(Some(8))
            .filter_entry(|entry| {
                let name = entry.file_name().to_string_lossy();
                !matches!(
                    name.as_ref(),
                    "node_modules" | "target" | ".git" | ".opencode" | ".alfonso"
                )
            })
            .build();
        for entry in walker.flatten() {
            if entry.file_name() == ".gitignore" && entry.path() != root_ignore {
                if let Some(err) = builder.add(entry.path()) {
                    crate::slog_warn!(
                        "nested gitignore parse error in {}: {}",
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

    pub fn harness(&self) -> Harness {
        self.harness
            .borrow()
            .expect("harness set by configure before any tool call")
    }

    pub fn storage_dir(&self) -> PathBuf {
        crate::bash_background::storage_dir(self.config().storage_dir.as_deref())
    }

    pub fn harness_dir(&self) -> PathBuf {
        self.storage_dir().join(self.harness().as_str())
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

    #[test]
    fn rebuild_gitignore_returns_none_without_project_root() {
        let provider = Box::new(crate::parser::TreeSitterProvider::new());
        let ctx = AppContext::new(provider, Config::default());
        ctx.rebuild_gitignore();
        assert!(ctx.gitignore().is_none());
    }

    #[test]
    fn rebuild_gitignore_returns_none_for_project_with_no_gitignore() {
        let tmp = TempDir::new().unwrap();
        let ctx = make_ctx_with_root(tmp.path());
        ctx.rebuild_gitignore();
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
