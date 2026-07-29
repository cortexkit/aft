use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{self, BufWriter};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, RwLock, TryLockError, Weak};
use std::time::{Duration, Instant, SystemTime};

use lsp_types::FileChangeType;
use notify::RecommendedWatcher;
use rusqlite::Connection;
use serde::Serialize;

use crate::artifact_owner::{
    ArtifactOwnerLease, ArtifactOwnerLeaseRegistration, ArtifactOwnerMode, ArtifactOwnerStatus,
};
use crate::backup::hash_session;
use crate::backup::BackupStore;
use crate::bash_background::{BgCompletion, BgTaskHealthCounts, BgTaskRegistry};
use crate::callgraph_store::{CallGraphStore, CallGraphStoreError, ReadonlyCallGraphStore};
use crate::checkpoint::CheckpointStore;
use crate::config::Config;
use crate::harness::Harness;
use crate::inspect::{
    InspectCategory, InspectManager, InspectSnapshot, Tier2RefreshScheduler, Tier2TriggerReason,
};
use crate::language::LanguageProvider;
use crate::lsp::manager::{LspManager, StaleDiagnosticsMark};
use crate::lsp::registry::is_config_file_path_with_custom;
use crate::parser::{SharedSymbolCache, SymbolCache, TreeSitterProvider};
use crate::protocol::{
    ConfigureWarningsFrame, ProgressFrame, PushFrame, StatusChangedFrame, StatusPayload,
};
use crate::watcher_filter::WatcherJoinOutcome;
use crate::watcher_filter::{SharedGitignore, WatcherDispatchEvent, WatcherThreadHandle};

pub type ProgressSender = Arc<Box<dyn Fn(PushFrame) + Send + Sync>>;
pub type SharedProgressSender = Arc<Mutex<Option<ProgressSender>>>;
pub type SharedStdoutWriter = Arc<Mutex<BufWriter<io::Stdout>>>;
const STATUS_DEBOUNCE_MS: u64 = 1_000;

/// Canonicalize a path that may no longer exist (pending callgraph paths
/// legitimately include deleted files): canonicalize the nearest existing
/// ancestor of the ORIGINAL spelling and re-append the missing tail, so alias
/// spellings (macOS /var vs /private/var) normalize even for dead paths.
///
/// Symlink semantics match the callgraph store's `normalize_file_path`
/// (filesystem-first): `root/link/../x` where `link` targets a foreign
/// directory canonicalizes to the FOREIGN parent, not a lexical `root/x`.
/// Lexical `.`/`..` resolution applies only past the deepest existing
/// component (a nonexistent component cannot be a symlink) and to the tail
/// appended onto an already-canonical, symlink-free base.
/// Component-wise lenient canonicalization with filesystem-first semantics
/// (matching the callgraph store's `normalize_file_path`): each existing
/// component — including symlinks — resolves through the filesystem; genuinely
/// absent components accumulate on a missing stack and resolve lexically. `..`
/// pops the missing stack first, and only when the stack is empty does it take
/// the parent of the canonical base (symlink-free, so a lexical parent is
/// sound there). Handles re-entry: in `dead/../link/../x`, `dead/..` drains
/// back to the existing base and `link` (a symlink) resolves through the
/// filesystem instead of being erased lexically.
///
/// Returns `None` — and containment fails closed — where realpath would not
/// resolve either: a dangling symlink or other filesystem error on an existing
/// component (the store falls back to the raw spelling for those, which
/// `relative_path` keeps as an absolute out-of-root key), and `..` traversal
/// through a non-directory (realpath ENOTDIR).
fn canonicalize_lenient(path: &Path) -> Option<PathBuf> {
    use std::path::Component;
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return Some(canonical);
    }
    let mut resolved = PathBuf::new();
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => {
                resolved.push(component.as_os_str());
                // Canonicalize the anchor so a missing child directly under a
                // drive/UNC root compares in the same (verbatim) spelling as a
                // canonicalized root on Windows; "/" is a no-op on Unix.
                if let Ok(canonical_anchor) = std::fs::canonicalize(&resolved) {
                    resolved = canonical_anchor;
                }
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if missing.pop().is_none() {
                    if !resolved.as_os_str().is_empty() && !resolved.is_dir() {
                        // `file/..` — realpath rejects with ENOTDIR.
                        return None;
                    }
                    resolved.pop();
                }
            }
            Component::Normal(name) => {
                if missing.is_empty() {
                    let candidate = resolved.join(name);
                    match std::fs::canonicalize(&candidate) {
                        Ok(canonical) => resolved = canonical,
                        Err(_) => match std::fs::symlink_metadata(&candidate) {
                            // Genuinely absent: lexical from here until `..`
                            // drains back.
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                                missing.push(name.to_owned())
                            }
                            // Exists but does not canonicalize (dangling
                            // symlink) or the probe itself failed: fail closed.
                            _ => return None,
                        },
                    }
                } else {
                    missing.push(name.to_owned());
                }
            }
        }
    }
    for name in missing {
        resolved.push(name);
    }
    Some(resolved)
}

/// Root-containment check for pending callgraph replay paths.
///
/// Relative paths are project-root-relative by the callgraph store's own
/// contract (`normalize_file_path`), so they are resolved against each root
/// rather than the process CWD. Both sides are lenient-canonicalized
/// (component-wise, filesystem-first) before the prefix comparison: raw-spelling
/// acceptance would let `root/../foreign` or a symlinked escape pass, and a
/// bare textual check false-drops alias spellings (macOS /var vs
/// /private/var) and deleted files.
fn pending_path_in_roots(path: &Path, roots: &[PathBuf]) -> bool {
    if path.is_relative() {
        // Project-root-relative by contract, and only for prefix-free
        // spellings: Windows drive-relative (`C:foo`) and root-relative
        // (`\foo`) forms are "relative" to std but `join` replaces the root
        // for them, resolving through the drive CWD instead of the project.
        let has_prefix_or_root = path.components().next().is_some_and(|component| {
            matches!(
                component,
                std::path::Component::Prefix(_) | std::path::Component::RootDir
            )
        });
        if has_prefix_or_root {
            return false;
        }
        // A lexical escape via `..` still fails the canonical prefix check
        // after joining. Unresolvable spellings fail closed.
        return roots.iter().any(|root| {
            let joined = root.join(path);
            match (canonicalize_lenient(&joined), canonicalize_lenient(root)) {
                (Some(path), Some(root)) => path.starts_with(&root),
                _ => false,
            }
        });
    }
    let Some(canonical_path) = canonicalize_lenient(path) else {
        return false;
    };
    roots.iter().any(|root| {
        canonicalize_lenient(root)
            .is_some_and(|canonical_root| canonical_path.starts_with(&canonical_root))
    })
}

/// Serializes the daemon's bound/unbound transition with admission of deferred
/// root work. The lock covers only the bounded decision and worker-start commit;
/// call sites must not wait for worker completion or run a scan while holding it.
#[derive(Clone, Default)]
pub(crate) struct SubcLifecycleAdmission {
    unbound: Arc<parking_lot::Mutex<bool>>,
}

impl SubcLifecycleAdmission {
    fn mark_bound(&self) {
        *self.unbound.lock() = false;
    }

    fn mark_unbound(&self, configure_generation: &AtomicU64) {
        let mut unbound = self.unbound.lock();
        if !*unbound {
            *unbound = true;
            configure_generation.fetch_add(1, Ordering::SeqCst);
        }
    }

    pub(crate) fn is_current(&self, generation: &AtomicU64, expected: u64) -> bool {
        let unbound = self.unbound.lock();
        !*unbound && generation.load(Ordering::SeqCst) == expected
    }

    fn advance_generation(&self, generation: &AtomicU64) -> u64 {
        let _unbound = self.unbound.lock();
        generation.fetch_add(1, Ordering::SeqCst).wrapping_add(1)
    }

    pub(crate) fn run_if_current<R>(
        &self,
        generation: &AtomicU64,
        expected: u64,
        action: impl FnOnce() -> R,
    ) -> Option<R> {
        let unbound = self.unbound.lock();
        if *unbound || generation.load(Ordering::SeqCst) != expected {
            return None;
        }
        Some(action())
    }

    pub(crate) fn is_bound(&self) -> bool {
        !*self.unbound.lock()
    }

    fn try_is_bound(&self) -> Option<bool> {
        self.unbound.try_lock().map(|unbound| !*unbound)
    }

    fn is_unbound(&self) -> bool {
        !self.is_bound()
    }

    fn run_if_unbound<R>(&self, action: impl FnOnce() -> R) -> Option<R> {
        let unbound = self.unbound.lock();
        if !*unbound {
            return None;
        }
        Some(action())
    }
}

const GRACEFUL_SHUTDOWN_SEARCH_BUILD_WAIT: Duration = Duration::from_secs(5);
const GRACEFUL_SHUTDOWN_SEARCH_BUILD_POLL: Duration = Duration::from_millis(10);

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
    generation: u64,
    /// True when the latest dead_code aggregate reported `callgraph_available:
    /// false` (the callgraph store was not ready when dead_code scanned). Health
    /// uses this to tell "tier2 still building" apart from "tier2 complete except
    /// dead_code, which is blocked on the callgraph store" — the latter must not
    /// report "building" forever, because nothing recomputes dead_code until the
    /// callgraph store becomes ready.
    dead_code_blocked_on_callgraph: bool,
}

#[derive(Debug, Clone, Default)]
struct StatusBarCache {
    valid: bool,
    diagnostics_generation: u64,
    tier2_generation: u64,
    tsconfig_generation: u64,
    counts: Option<StatusBarCounts>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RootHealthState {
    Ready,
    Busy,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct HealthComponentSnapshot {
    pub status: &'static str,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Tier2HealthSnapshot {
    pub status: &'static str,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RootHealthSnapshot {
    pub project_root: String,
    pub actor_count: usize,
    pub state: RootHealthState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search_index: Option<HealthComponentSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub semantic_index: Option<HealthComponentSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callgraph_store: Option<HealthComponentSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier2: Option<Tier2HealthSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bash: Option<BgTaskHealthCounts>,
}

impl RootHealthSnapshot {
    fn busy(project_root: &Path) -> Self {
        Self {
            project_root: project_root.display().to_string(),
            actor_count: 1,
            state: RootHealthState::Busy,
            search_index: None,
            semantic_index: None,
            callgraph_store: None,
            tier2: None,
            bash: None,
        }
    }

    pub fn is_fully_ready(&self) -> bool {
        let component_is_satisfied =
            |status: &HealthComponentSnapshot| matches!(status.status, "ready" | "disabled");
        let tier2_is_satisfied =
            |tier2: &Tier2HealthSnapshot| matches!(tier2.status, "ready" | "disabled");

        matches!(self.state, RootHealthState::Ready)
            && self
                .search_index
                .as_ref()
                .is_some_and(component_is_satisfied)
            && self
                .semantic_index
                .as_ref()
                .is_some_and(component_is_satisfied)
            && self
                .callgraph_store
                .as_ref()
                .is_some_and(component_is_satisfied)
            && self.tier2.as_ref().is_some_and(tier2_is_satisfied)
    }
}

pub struct StatusEmitter {
    latest: Arc<Mutex<Option<StatusPayload>>>,
    notify: mpsc::Sender<()>,
}

#[derive(Clone, Debug, Default)]
struct ConfigureWarmState {
    generation: u64,
    key: Option<String>,
}

#[derive(Debug)]
struct ConfigurePhaseTiming {
    phase: &'static str,
    started_at: Instant,
    completed: Vec<(&'static str, Duration)>,
}

impl Default for ConfigurePhaseTiming {
    fn default() -> Self {
        Self {
            phase: "idle",
            started_at: Instant::now(),
            completed: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum WatcherDrainApplyPhase {
    #[default]
    PendingTier2,
    PendingIndexes,
    SymbolCache,
    Callgraph,
    SearchIndex,
    SemanticIndex,
    LspDiagnostics,
    Complete,
}

#[derive(Debug, Default)]
pub(crate) enum WatcherDrainPhase {
    #[default]
    Collect,
    Apply {
        stage: WatcherDrainApplyPhase,
        paths: VecDeque<PathBuf>,
        remaining: usize,
        oversized_inline_batch: bool,
    },
}

#[derive(Debug)]
pub(crate) struct WatcherDrainSliceState {
    pub(crate) configure_generation: u64,
    /// Content identity of the configuration this continuation was built
    /// under. A lifecycle-only generation change (route unbind/rebind with an
    /// equivalent config) preserves the continuation by REBASING it onto the
    /// new generation; a content change discards it (the new configuration
    /// rebuilds artifacts wholesale).
    pub(crate) configure_content_generation: u64,
    pub(crate) phase: WatcherDrainPhase,
    pub(crate) pending_paths: VecDeque<PathBuf>,
    pub(crate) ignore_changed: bool,
    pub(crate) rescan_required: bool,
    pub(crate) status_changed: bool,
    pub(crate) scheduler_changed_path_count: usize,
    pub(crate) semantic_refresh_paths: Vec<PathBuf>,
    pub(crate) path_slice_count: usize,
}

/// Pending watcher-derived reconciliation state taken out of the context for
/// a transactional TTL teardown: committed (dropped) once eviction succeeds,
/// restored when a secondary blocker aborts the eviction.
pub(crate) struct PendingReconciliationState {
    search: BTreeSet<PathBuf>,
    callgraph: BTreeSet<PathBuf>,
    tier2: BTreeSet<PathBuf>,
    semantic: BTreeSet<PathBuf>,
    corpus_refresh: bool,
}

impl WatcherDrainSliceState {
    pub(crate) fn new(configure_generation: u64, configure_content_generation: u64) -> Self {
        Self {
            configure_generation,
            configure_content_generation,
            phase: WatcherDrainPhase::Collect,
            pending_paths: VecDeque::new(),
            ignore_changed: false,
            rescan_required: false,
            status_changed: false,
            scheduler_changed_path_count: 0,
            semantic_refresh_paths: Vec::new(),
            path_slice_count: 0,
        }
    }

    pub(crate) fn has_pending_work(&self) -> bool {
        !matches!(self.phase, WatcherDrainPhase::Collect)
            || !self.pending_paths.is_empty()
            || self.ignore_changed
            || self.rescan_required
    }
}

#[doc(hidden)]
pub enum CallGraphStoreBuildEvent {
    Ready {
        store: CallGraphStore,
        fulfilled_force_token: Option<u64>,
        publication_epoch: u64,
    },
    Settled,
}

struct CallGraphStoreBuildSettlement {
    tx: crossbeam_channel::Sender<CallGraphStoreBuildEvent>,
    sent: bool,
    force_token: Option<u64>,
    publication_epoch: u64,
}

impl CallGraphStoreBuildSettlement {
    fn new(
        tx: crossbeam_channel::Sender<CallGraphStoreBuildEvent>,
        force_token: Option<u64>,
        publication_epoch: u64,
    ) -> Self {
        Self {
            tx,
            sent: false,
            force_token,
            publication_epoch,
        }
    }

    fn ready(&mut self, store: CallGraphStore) {
        let _ = self.tx.send(CallGraphStoreBuildEvent::Ready {
            store,
            fulfilled_force_token: self.force_token,
            publication_epoch: self.publication_epoch,
        });
        self.sent = true;
    }
}

impl Drop for CallGraphStoreBuildSettlement {
    fn drop(&mut self) {
        if !self.sent {
            let _ = self.tx.send(CallGraphStoreBuildEvent::Settled);
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ConfigureMaintenanceJob {
    pub(crate) generation: u64,
    pub(crate) root_path: PathBuf,
    pub(crate) canonical_cache_root: PathBuf,
    pub(crate) harness: Harness,
    pub(crate) storage_root: PathBuf,
    pub(crate) harness_dir: PathBuf,
    pub(crate) session_id: String,
    pub(crate) home_match: bool,
    pub(crate) format_tool_cache_clear_needed: bool,
    pub(crate) run_bash_replay: bool,
    pub(crate) refresh_project_runtime: bool,
    pub(crate) sync_bash_compress_flag: bool,
    pub(crate) reset_filter_registry: bool,
    pub(crate) clear_failed_spawns: bool,
    pub(crate) warm_callgraph_store: bool,
    /// Advance disk-publication epochs in the post-ack configure tail. This can
    /// wait for an already-committing writer, so it must never run on bind.
    pub(crate) supersede_artifact_persistence: bool,
    /// One-shot gates for artifact workers created during configure. The
    /// configure tail opens them only after the bind response has been produced.
    pub(crate) artifact_load_starts: Vec<crossbeam_channel::Sender<()>>,
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
#[derive(Debug, Default, Clone)]
#[doc(hidden)]
pub struct SemanticRefreshAccounting {
    #[doc(hidden)]
    pub pending: usize,
    #[doc(hidden)]
    pub in_flight: usize,
}

#[derive(Debug, Default)]
struct SemanticRefreshCircuit {
    consecutive_transient_failures: AtomicUsize,
    open: AtomicBool,
    probe_in_flight: AtomicBool,
    probe_ready: AtomicBool,
    probe_token: AtomicU64,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SemanticColdSeedResume {
    request_tier2: bool,
    warm_callgraph: bool,
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
    },
    Ready {
        /// Files currently being re-embedded after recent edits. The index is
        /// still queryable; results for these files may be temporarily missing.
        refreshing: Vec<PathBuf>,
        /// Per-root queue accounting for repeated refreshes of the same path.
        /// Kept on the status value so two AppContexts in one process cannot
        /// share refresh-completion state.
        #[doc(hidden)]
        accounting: BTreeMap<PathBuf, SemanticRefreshAccounting>,
    },
    Failed(String),
}

impl SemanticIndexStatus {
    pub fn ready() -> Self {
        Self::Ready {
            refreshing: Vec::new(),
            accounting: BTreeMap::new(),
        }
    }

    pub fn add_refreshing_file(&mut self, path: PathBuf) {
        if let Self::Ready {
            refreshing,
            accounting,
        } = self
        {
            let state = accounting.entry(path.clone()).or_default();
            state.pending = state.pending.saturating_add(1);
            ensure_refreshing_path(refreshing, path);
        }
    }

    pub fn start_refreshing_file(&mut self, path: PathBuf) {
        if let Self::Ready {
            refreshing,
            accounting,
        } = self
        {
            let state = accounting.entry(path.clone()).or_default();
            if state.pending == 0 {
                state.pending = 1;
            }
            if state.in_flight == 0 {
                state.in_flight = state.pending;
            }
            ensure_refreshing_path(refreshing, path);
        }
    }

    pub fn cancel_refreshing_file(&mut self, path: &Path) {
        self.finish_refreshing_file(path, false);
    }

    /// Take every file currently tracked as refreshing, clearing the
    /// accounting. Used when the refresh worker is cancelled outright: the
    /// caller re-queues the paths for a replacement worker.
    pub fn take_refreshing_files(&mut self) -> Vec<PathBuf> {
        if let Self::Ready {
            refreshing,
            accounting,
        } = self
        {
            accounting.clear();
            std::mem::take(refreshing)
        } else {
            Vec::new()
        }
    }

    /// True while a corpus-wide (not per-file) refresh is running.
    pub fn corpus_refresh_in_flight(&self) -> bool {
        matches!(self, Self::Building { stage, .. } if stage == "refreshing_corpus")
    }

    pub fn complete_refreshing_file(&mut self, path: &Path) {
        self.finish_refreshing_file(path, true);
    }

    pub fn remove_refreshing_file(&mut self, path: &Path) {
        self.complete_refreshing_file(path);
    }

    fn finish_refreshing_file(&mut self, path: &Path, complete_in_flight: bool) {
        if let Self::Ready {
            refreshing,
            accounting,
        } = self
        {
            let mut keep_refreshing = false;
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

            if !keep_refreshing {
                remove_refreshing_path(refreshing, path);
            }
        }
    }

    pub fn refreshing_count(&self) -> usize {
        match self {
            Self::Ready { refreshing, .. } => refreshing.len(),
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
    },
    /// Emitted when the semantic worker avoids or pauses full project corpus
    /// collection before reaching terminal Ready/Failed, such as after loading a
    /// cached index or while waiting to retry an embedding backend with no vectors
    /// retained. Work that was waiting for the full index can proceed.
    ColdSeedGateCleared,
    Ready(SemanticIndex),
    Failed(String),
}

#[derive(Debug, Clone)]
pub enum SemanticRefreshRequest {
    Files {
        paths: Vec<PathBuf>,
    },
    /// Refresh the whole semantic corpus on the refresh worker. The worker owns
    /// the project walk so watcher/configure drains never do corpus-scale work
    /// on the single dispatch thread before scheduling embedding.
    Corpus,
}

#[derive(Debug)]
pub enum SemanticRefreshEvent {
    Started {
        paths: Vec<PathBuf>,
    },
    CorpusStarted {
        files: usize,
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

pub(crate) struct ReceiverTerminalGuard {
    terminal_epoch: Arc<AtomicU64>,
    epoch: u64,
}

impl ReceiverTerminalGuard {
    fn new(terminal_epoch: Arc<AtomicU64>, epoch: u64) -> Self {
        Self {
            terminal_epoch,
            epoch,
        }
    }
}

impl Drop for ReceiverTerminalGuard {
    fn drop(&mut self) {
        self.terminal_epoch.fetch_max(self.epoch, Ordering::SeqCst);
    }
}

pub type SemanticRefreshWorkerSlot = Arc<Mutex<Option<std::thread::JoinHandle<()>>>>;

struct PathRestrictionContext {
    raw_root: PathBuf,
    resolved_root: PathBuf,
    path_for_resolution: PathBuf,
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

pub type LanguageProviderFactory = fn() -> Box<dyn LanguageProvider>;

pub fn default_language_provider_factory() -> Box<dyn LanguageProvider> {
    Box::new(TreeSitterProvider::new())
}

fn database_path_key(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }
    let Some(parent) = path.parent() else {
        return path.to_path_buf();
    };
    let canonical_parent = std::fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
    path.file_name()
        .map(|name| canonical_parent.join(name))
        .unwrap_or_else(|| canonical_parent.join(path))
}

/// Process-global services shared by all project actors in this AFT process.
///
/// `App` owns only true process services. Per-root caches and the live
/// language provider instance stay in [`AppContext`].
pub struct App {
    /// One process-wide handle for the current AFT database. Every project
    /// actor points at this handle so roots do not open duplicate SQLite/WAL
    /// descriptors for the same database.
    db: parking_lot::Mutex<Option<(PathBuf, Arc<Mutex<Connection>>)>>,
    active_watchers: AtomicUsize,
    active_actor_roots: AtomicUsize,
    open_routes: AtomicUsize,
    lsp_child_registry: crate::lsp::child_registry::LspChildRegistry,
    stdout_writer: SharedStdoutWriter,
    provider_factory: LanguageProviderFactory,
    /// Weak actor references let status attribute process RSS across roots
    /// without making the process-global App own per-root caches.
    memory_contexts: parking_lot::Mutex<BTreeMap<PathBuf, Weak<AppContext>>>,
}

impl App {
    pub fn new(provider_factory: LanguageProviderFactory) -> Self {
        Self {
            db: parking_lot::Mutex::new(None),
            active_watchers: AtomicUsize::new(0),
            active_actor_roots: AtomicUsize::new(0),
            open_routes: AtomicUsize::new(0),
            lsp_child_registry: crate::lsp::child_registry::LspChildRegistry::new(),
            stdout_writer: Arc::new(Mutex::new(BufWriter::new(io::stdout()))),
            provider_factory,
            memory_contexts: parking_lot::Mutex::new(BTreeMap::new()),
        }
    }

    /// Create the shared process `App` handle required by the actor split.
    pub fn shared(provider_factory: LanguageProviderFactory) -> Arc<Self> {
        Arc::new(Self::new(provider_factory))
    }

    pub fn default_shared() -> Arc<Self> {
        Self::shared(default_language_provider_factory)
    }

    pub fn create_provider(&self) -> Box<dyn LanguageProvider> {
        (self.provider_factory)()
    }

    pub fn lsp_child_registry(&self) -> crate::lsp::child_registry::LspChildRegistry {
        self.lsp_child_registry.clone()
    }

    pub fn stdout_writer(&self) -> SharedStdoutWriter {
        Arc::clone(&self.stdout_writer)
    }

    pub(crate) fn register_memory_context(&self, root: PathBuf, ctx: &Arc<AppContext>) {
        let mut contexts = self.memory_contexts.lock();
        contexts.retain(|_, context| context.strong_count() > 0);
        contexts.insert(root, Arc::downgrade(ctx));
    }

    pub(crate) fn unregister_memory_context(&self, root: &Path, ctx: &Arc<AppContext>) {
        let mut contexts = self.memory_contexts.lock();
        let removes_current = contexts
            .get(root)
            .and_then(Weak::upgrade)
            .is_some_and(|registered| Arc::ptr_eq(&registered, ctx));
        if removes_current {
            contexts.remove(root);
        }
    }

    /// Snapshot process roots without waiting behind actor registration. A busy
    /// registry is surfaced as a named status gap by the memory snapshot.
    pub(crate) fn try_memory_contexts(&self) -> Option<Vec<(PathBuf, Arc<AppContext>)>> {
        let contexts = self.memory_contexts.try_lock()?;
        Some(
            contexts
                .iter()
                .filter_map(|(root, context)| {
                    context.upgrade().map(|context| (root.clone(), context))
                })
                .collect(),
        )
    }

    /// Return the process-shared database handle, opening it only when the
    /// requested path is not already resident. The connection mutex serializes
    /// transactions from all roots; callers never hold the App lock while using
    /// the returned connection.
    pub fn open_db(&self, path: &Path) -> Result<Arc<Mutex<Connection>>, crate::db::OpenError> {
        let key = database_path_key(path);
        let mut slot = self.db.lock();
        if let Some((existing_path, conn)) = slot.as_ref() {
            if existing_path == &key {
                return Ok(Arc::clone(conn));
            }
        }

        let conn = Arc::new(Mutex::new(crate::db::open(path)?));
        *slot = Some((key, Arc::clone(&conn)));
        Ok(conn)
    }

    pub fn set_db(&self, conn: Arc<Mutex<Connection>>) {
        *self.db.lock() = Some((PathBuf::new(), conn));
    }

    pub fn clear_db(&self) {
        *self.db.lock() = None;
    }

    /// Clear the shared handle only when it still refers to `path`. A failed
    /// reconfigure for one root must not tear down a database used by another
    /// root.
    pub fn clear_db_for_path(&self, path: &Path) {
        let key = database_path_key(path);
        let mut slot = self.db.lock();
        if slot.as_ref().is_some_and(|(existing_path, _)| {
            existing_path.as_os_str().is_empty() || existing_path == &key
        }) {
            *slot = None;
        }
    }

    pub fn db(&self) -> Option<Arc<Mutex<Connection>>> {
        self.db.lock().as_ref().map(|(_, conn)| Arc::clone(conn))
    }

    pub(crate) fn watcher_started(&self) {
        self.active_watchers.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) fn watcher_stopped(&self) {
        self.active_watchers
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                Some(count.saturating_sub(1))
            })
            .ok();
    }

    /// Number of live watcher filter runtimes registered by this process.
    /// A runtime remains counted until its OS watcher thread has actually exited.
    pub fn watcher_count(&self) -> usize {
        self.active_watchers.load(Ordering::SeqCst)
    }

    pub(crate) fn actor_root_registered(&self) {
        self.active_actor_roots.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) fn actor_root_unregistered(&self) {
        self.active_actor_roots
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                Some(count.saturating_sub(1))
            })
            .ok();
    }

    pub fn actor_root_count(&self) -> usize {
        self.active_actor_roots.load(Ordering::SeqCst)
    }

    pub(crate) fn set_open_route_count(&self, count: usize) {
        self.open_routes.store(count, Ordering::SeqCst);
    }

    pub fn open_route_count(&self) -> usize {
        self.open_routes.load(Ordering::SeqCst)
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new(default_language_provider_factory)
    }
}

const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    fn assert_send<T: Send>() {}

    assert_send_sync::<App>();
    assert_send_sync::<AppContext>();
    assert_send::<crate::lsp::manager::LspManager>();
    assert_send::<crate::semantic_index::EmbeddingModel>();
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GitEntryKind {
    Missing,
    File,
    Directory,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct GitEntrySignature {
    kind: GitEntryKind,
    modified: Option<SystemTime>,
}

#[derive(Clone, Debug)]
struct WorktreeBridgeCacheEntry {
    git_entry: GitEntrySignature,
    is_worktree_bridge: bool,
    git_common_dir: Option<PathBuf>,
}

pub(crate) const BORROWED_INDEX_CACHE_CAPACITY: usize = 4;

#[derive(Clone, Debug, PartialEq, Eq)]
struct BorrowedIndexCacheKey {
    canonical_root: PathBuf,
    artifact: crate::readonly_artifacts::BorrowedArtifactGeneration,
}

#[derive(Clone, Debug)]
enum BorrowedIndexCacheValue {
    Search(crate::readonly_artifacts::ReadOnlyArtifact<Arc<SearchIndex>>),
    Semantic(crate::readonly_artifacts::ReadOnlyArtifact<Arc<SemanticIndex>>),
}

#[derive(Debug, Default)]
struct BorrowedIndexCache {
    entries: VecDeque<(BorrowedIndexCacheKey, BorrowedIndexCacheValue)>,
    resolved_roots: VecDeque<(PathBuf, GitEntrySignature)>,
}

impl BorrowedIndexCache {
    fn search(
        &mut self,
        key: &BorrowedIndexCacheKey,
    ) -> Option<crate::readonly_artifacts::ReadOnlyArtifact<Arc<SearchIndex>>> {
        let position = self.entries.iter().position(|(candidate, value)| {
            candidate == key && matches!(value, BorrowedIndexCacheValue::Search(_))
        })?;
        let entry = self.entries.remove(position)?;
        let BorrowedIndexCacheValue::Search(index) = &entry.1 else {
            return None;
        };
        let index = (*index).clone();
        self.entries.push_back(entry);
        Some(index)
    }

    fn semantic(
        &mut self,
        key: &BorrowedIndexCacheKey,
    ) -> Option<crate::readonly_artifacts::ReadOnlyArtifact<Arc<SemanticIndex>>> {
        let position = self.entries.iter().position(|(candidate, value)| {
            candidate == key && matches!(value, BorrowedIndexCacheValue::Semantic(_))
        })?;
        let entry = self.entries.remove(position)?;
        let BorrowedIndexCacheValue::Semantic(index) = &entry.1 else {
            return None;
        };
        let index = (*index).clone();
        self.entries.push_back(entry);
        Some(index)
    }

    fn insert(&mut self, key: BorrowedIndexCacheKey, value: BorrowedIndexCacheValue) {
        self.entries.retain(|(candidate, _)| {
            candidate.canonical_root != key.canonical_root
                || candidate.artifact.path != key.artifact.path
        });
        self.entries.push_back((key, value));
        while self.entries.len() > BORROWED_INDEX_CACHE_CAPACITY {
            self.entries.pop_front();
        }
    }

    fn resolved_root(&mut self, requested_root: &Path) -> Option<PathBuf> {
        let position = self
            .resolved_roots
            .iter()
            .position(|(candidate, _)| candidate == requested_root)?;
        let entry = self.resolved_roots.remove(position)?;
        if entry.1 != git_entry_signature(requested_root) {
            return None;
        }
        let root = entry.0.clone();
        self.resolved_roots.push_back(entry);
        Some(root)
    }

    fn remember_resolved_root(&mut self, root: PathBuf) {
        self.resolved_roots
            .retain(|(candidate, _)| candidate != &root);
        let signature = git_entry_signature(&root);
        self.resolved_roots.push_back((root, signature));
        while self.resolved_roots.len() > BORROWED_INDEX_CACHE_CAPACITY {
            self.resolved_roots.pop_front();
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.resolved_roots.clear();
    }
}

fn git_entry_signature(project_root: &Path) -> GitEntrySignature {
    match std::fs::symlink_metadata(project_root.join(".git")) {
        Ok(metadata) => GitEntrySignature {
            kind: if metadata.file_type().is_file() {
                GitEntryKind::File
            } else if metadata.file_type().is_dir() {
                GitEntryKind::Directory
            } else {
                GitEntryKind::Other
            },
            modified: metadata.modified().ok(),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => GitEntrySignature {
            kind: GitEntryKind::Missing,
            modified: None,
        },
        Err(_) => GitEntrySignature {
            kind: GitEntryKind::Other,
            modified: None,
        },
    }
}

/// Shared application context threaded through all command handlers.
///
/// Holds the language provider, backup/checkpoint stores, and configuration.
/// Constructed once at startup and passed by
/// reference to `dispatch`.
///
/// Write-rarely stores use `parking_lot::Mutex` for interior mutability so this
/// context can become thread-safe while preserving the current single-request
/// dispatch behavior. `config` is a thread-safe owned snapshot so future
/// read-only dispatch can hold configuration across other work without holding
/// a lock guard.
pub struct AppContext {
    app: Arc<App>,
    provider: Box<dyn LanguageProvider>,
    backup: parking_lot::Mutex<BackupStore>,
    checkpoint: parking_lot::Mutex<CheckpointStore>,
    config: RwLock<Arc<Config>>,
    force_restrict_requests: parking_lot::Mutex<BTreeMap<String, usize>>,
    pub harness: parking_lot::Mutex<Option<Harness>>,
    canonical_cache_root: parking_lot::Mutex<Option<PathBuf>>,
    is_worktree_bridge: parking_lot::Mutex<bool>,
    git_common_dir: parking_lot::Mutex<Option<PathBuf>>,
    shared_artifacts_read_only: AtomicBool,
    callgraph_writer: AtomicBool,
    inspect_writer: AtomicBool,
    artifact_owner_status: parking_lot::Mutex<Option<ArtifactOwnerStatus>>,
    artifact_owner_lease: parking_lot::Mutex<Option<ArtifactOwnerLeaseRegistration>>,
    /// Reasons (if any) why heavy AFT subsystems were auto-disabled for the
    /// current project root. Populated by `handle_configure` based on the
    /// canonical project root. Each reason is a stable machine-readable string
    /// (e.g. `"home_root"`, `"watcher_unavailable"`) so the plugin can render
    /// distinct degraded-mode UI states without re-deriving the reason locally.
    /// Empty when the project is healthy / full-featured.
    degraded_reasons: parking_lot::Mutex<Vec<String>>,
    /// Configure-time gate for project-wide scans, builds, and watcher-driven
    /// refreshes that would otherwise walk the whole root. `handle_configure`
    /// closes it for degraded home roots and every heavy-work entry point reads
    /// the same atomic so the decision cannot drift after configure returns.
    heavy_root_work_allowed: Arc<AtomicBool>,
    callgraph_store: RwLock<Option<Arc<ReadonlyCallGraphStore>>>,
    callgraph_store_force_requested: AtomicU64,
    callgraph_store_force_fulfilled: AtomicU64,
    callgraph_store_rx:
        parking_lot::Mutex<Option<crossbeam_channel::Receiver<CallGraphStoreBuildEvent>>>,
    callgraph_store_rx_generation: AtomicU64,
    callgraph_store_rx_epoch: AtomicU64,
    callgraph_persist_epoch: crate::root_cache::ArtifactPublishEpoch,
    callgraph_legacy_migration_summary_logged: Arc<AtomicBool>,
    pending_callgraph_store_paths: crate::callgraph_store::PendingCallGraphStorePaths,
    search_index: RwLock<Option<SearchIndex>>,
    search_index_rx: RwLock<Option<crossbeam_channel::Receiver<SearchIndex>>>,
    search_index_rx_generation: AtomicU64,
    search_index_rx_epoch: AtomicU64,
    search_index_rx_terminal_epoch: Arc<AtomicU64>,
    /// `(configure_generation, automatic_replacement_attempts)`. Caps the
    /// drain-path replacement of a search-index load whose worker disconnected
    /// without delivering an index, so a persistently failing worker cannot be
    /// relaunched in a loop on the drain thread. Resets when the configure
    /// generation advances.
    search_index_disconnect_reschedule: parking_lot::Mutex<(u64, u32)>,
    search_persist_epoch: crate::root_cache::ArtifactPublishEpoch,
    pending_search_index_paths: parking_lot::Mutex<BTreeSet<PathBuf>>,
    symbol_cache: SharedSymbolCache,
    inspect_manager: Arc<InspectManager>,
    tier2_refresh_scheduler: parking_lot::Mutex<Tier2RefreshScheduler>,
    pending_tier2_paths: parking_lot::Mutex<BTreeSet<PathBuf>>,
    semantic_index: RwLock<Option<SemanticIndex>>,
    semantic_index_rx: parking_lot::Mutex<Option<crossbeam_channel::Receiver<SemanticIndexEvent>>>,
    semantic_index_rx_generation: AtomicU64,
    semantic_index_rx_epoch: AtomicU64,
    semantic_index_rx_terminal_epoch: Arc<AtomicU64>,
    semantic_persist_epoch: crate::root_cache::ArtifactPublishEpoch,
    semantic_persist_lock: Arc<parking_lot::Mutex<()>>,
    semantic_index_status: RwLock<SemanticIndexStatus>,
    /// Serializes missing-artifact checks with receiver installation so
    /// concurrent fallback queries cannot start duplicate reload workers.
    artifact_reload_lock: parking_lot::Mutex<()>,
    /// True while this context has a cold semantic seed scheduled or actively
    /// collecting/embedding/persisting the full project corpus. The semantic
    /// worker clears it as soon as it proves the cached/incremental path is in use.
    semantic_cold_seed_active: Arc<AtomicBool>,
    /// Monotonic generation that prevents a superseded semantic worker from
    /// reopening the cold-seed gate after a later configure has reset it.
    semantic_cold_seed_generation: Arc<AtomicU64>,
    semantic_fingerprint_generation: Arc<AtomicU64>,
    semantic_callgraph_warm_deferred: AtomicBool,
    pending_semantic_index_paths: Arc<parking_lot::Mutex<BTreeSet<PathBuf>>>,
    pending_semantic_corpus_refresh: parking_lot::Mutex<bool>,
    semantic_refresh_tx:
        Arc<parking_lot::Mutex<Option<crossbeam_channel::Sender<SemanticRefreshRequest>>>>,
    semantic_refresh_event_rx:
        parking_lot::Mutex<Option<crossbeam_channel::Receiver<SemanticRefreshEvent>>>,
    semantic_refresh_generation: AtomicU64,
    semantic_refresh_epoch: AtomicU64,
    semantic_refresh_build_epoch: AtomicU64,
    semantic_refresh_worker: parking_lot::Mutex<Option<SemanticRefreshWorkerSlot>>,
    semantic_refresh_retry_attempts: parking_lot::Mutex<BTreeMap<PathBuf, usize>>,
    semantic_refresh_circuit: Arc<SemanticRefreshCircuit>,
    semantic_embedding_model: parking_lot::Mutex<Option<crate::semantic_index::EmbeddingModel>>,
    watcher_runtime_lock: parking_lot::Mutex<()>,
    watcher: parking_lot::Mutex<Option<RecommendedWatcher>>,
    watcher_rx: parking_lot::Mutex<Option<crossbeam_channel::Receiver<WatcherDispatchEvent>>>,
    watcher_drain_slice: parking_lot::Mutex<Option<WatcherDrainSliceState>>,
    watcher_thread: parking_lot::Mutex<Option<WatcherThreadHandle>>,
    lsp_manager: parking_lot::Mutex<LspManager>,
    configure_generation: Arc<AtomicU64>,
    /// Advances only when the warm configuration changes, not on route
    /// teardown. Already-admitted workers use it to decide whether their disk
    /// artifact is still configuration-compatible after becoming unbound.
    configure_content_generation: Arc<AtomicU64>,
    /// Set only by the daemon route lifecycle. Standalone contexts remain bound.
    /// Deferred maintenance uses the same gate for the state check and admission.
    subc_lifecycle: SubcLifecycleAdmission,
    configure_warm_state: parking_lot::Mutex<ConfigureWarmState>,
    configure_phase_timing: parking_lot::Mutex<ConfigurePhaseTiming>,
    configured_session_roots: parking_lot::Mutex<BTreeSet<(PathBuf, String)>>,
    configure_maintenance_jobs: parking_lot::Mutex<VecDeque<ConfigureMaintenanceJob>>,
    artifact_cache_keys: parking_lot::Mutex<BTreeMap<PathBuf, String>>,
    artifact_cache_key_derivations: AtomicU64,
    borrowed_index_cache: parking_lot::Mutex<BorrowedIndexCache>,
    /// Successful git worktree probes, keyed by canonical root and guarded by
    /// the root's `.git` entry shape and modification time.
    worktree_bridge_cache: parking_lot::Mutex<BTreeMap<PathBuf, WorktreeBridgeCacheEntry>>,
    #[cfg(test)]
    worktree_bridge_probe_spawns: AtomicU64,
    #[cfg(test)]
    force_worktree_bridge_reprobe: AtomicBool,
    /// Last-seen value of `InspectManager::reuse_completion_count()`, so the
    /// per-request inspect drain can detect watcher-driven Tier-2 scans that
    /// finished since the previous tick and refresh the status bar (#3).
    last_seen_reuse_completions: AtomicU64,
    configure_warnings_tx: crossbeam_channel::Sender<(u64, ConfigureWarningsFrame)>,
    configure_warnings_rx: crossbeam_channel::Receiver<(u64, ConfigureWarningsFrame)>,
    /// Per-context push sender slot. Status and background-bash emitters share
    /// this Arc so a sender installed after construction is observed at emit time.
    progress_sender: SharedProgressSender,
    status_emitter: StatusEmitter,
    /// Last status-bar payload attached to a tool response for this project root.
    /// Deduping here (not in a process-global static) lets daemon roots emit the
    /// same counts independently.
    status_bar_last_emitted: RwLock<Option<StatusBarCounts>>,
    status_bar_cached: RwLock<StatusBarCache>,
    compression_aggregates: Arc<crate::db::compression_events::CompressionAggregateCache>,
    bash_background: BgTaskRegistry,
    #[cfg(unix)]
    escalation_grants: parking_lot::Mutex<crate::sandbox_spawn::EscalationGrantStore>,
    /// Thread-safe registry of TOML output filters. Lazy-built on first
    /// access; populated atomically via `RwLock`. Shared between command
    /// handlers (which use it through `filter_registry()` -> read guard) and
    /// the `BgTaskRegistry` watchdog thread (which uses it through
    /// `compress::compress_with_registry`). Reloaded when configure changes
    /// the project root or storage_dir; see [`AppContext::reset_filter_registry`].
    filter_registry: crate::compress::SharedFilterRegistry,
    filter_registry_rebuild_count: AtomicU64,
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
    gitignore: SharedGitignore,
    gitignore_generation: Arc<AtomicU64>,
    /// Last-known Tier-2 + todos counts for the agent status bar, refreshed off
    /// the hot path (on `aft_inspect` reads and background Tier-2 completions).
    /// Errors/warnings are read live and not stored here.
    status_bar_tier2: RwLock<StatusBarTier2>,
    /// Persistent TypeScript-project membership cache for the status-bar E/W
    /// count. The bar reads E/W live on every tool result, so resolving the
    /// nearest tsconfig (read + parse + glob-compile) per drain is too costly;
    /// this memoizes per tsconfig dir. Invalidated wholesale on any
    /// tsconfig-like watcher event and on `configure`. Owned here (not in
    /// `DiagnosticsStore`, which stays raw policy-free) per the v0.35 council.
    tsconfig_membership:
        parking_lot::Mutex<crate::lsp::tsconfig_membership::TsconfigMembershipCache>,
}

/// RAII guard for a server-owned request-scoped path-restriction override.
///
/// Guards are refcounted by request id so duplicated ids over-restrict until the
/// last worker exits, rather than letting one completion disable another
/// in-flight request's containment.
pub struct ForceRestrictGuard<'a> {
    ctx: &'a AppContext,
    req_id: String,
}

impl Drop for ForceRestrictGuard<'_> {
    fn drop(&mut self) {
        self.ctx.release_force_restrict(&self.req_id);
    }
}

impl Drop for AppContext {
    fn drop(&mut self) {
        self.artifact_owner_lease.get_mut().take();
        if let Some(runtime) = self.watcher_thread.get_mut().take() {
            let root = self
                .canonical_cache_root
                .get_mut()
                .clone()
                .or_else(|| {
                    self.config
                        .get_mut()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .project_root
                        .clone()
                })
                .unwrap_or_else(|| PathBuf::from("<unconfigured>"));
            Self::spawn_watcher_shutdown(Arc::clone(&self.app), root, runtime);
        }
    }
}

/// Result of requesting the persisted callgraph store for a store-backed op.
///
/// The five edge-query ops never block the request thread on a cold build:
/// a genuine cold build is kicked off in the background and `Building` is
/// returned so the agent retries, mirroring how semantic search reports a
/// build in progress. Warm restarts open the on-disk DB synchronously, so
/// `Building` is only ever seen during a true first cold build.
pub enum CallgraphStoreAccess {
    /// Store is resident and queryable.
    Ready(Arc<ReadonlyCallGraphStore>),
    /// A cold build is in flight (or was just started); retry shortly.
    Building,
    /// Not configured, or a read-only worktree whose store was never built.
    Unavailable,
    /// A store open/build check failed with a real error (DB/IO).
    Error(CallGraphStoreError),
}

#[derive(Clone, Copy)]
enum CallgraphBackgroundWork {
    Ensure,
    ForceRebuild(u64),
    LegacyMigration,
}

#[cfg(test)]
struct CallgraphBuildStartGate {
    root: PathBuf,
    reached: crossbeam_channel::Sender<()>,
    release: crossbeam_channel::Receiver<()>,
}

#[cfg(test)]
static CALLGRAPH_BUILD_START_GATE: std::sync::OnceLock<
    parking_lot::Mutex<Option<CallgraphBuildStartGate>>,
> = std::sync::OnceLock::new();

#[cfg(test)]
fn install_callgraph_build_start_gate(
    root: PathBuf,
) -> (
    crossbeam_channel::Receiver<()>,
    crossbeam_channel::Sender<()>,
) {
    let (reached_tx, reached_rx) = crossbeam_channel::bounded(1);
    let (release_tx, release_rx) = crossbeam_channel::bounded(1);
    *CALLGRAPH_BUILD_START_GATE
        .get_or_init(|| parking_lot::Mutex::new(None))
        .lock() = Some(CallgraphBuildStartGate {
        root,
        reached: reached_tx,
        release: release_rx,
    });
    (reached_rx, release_tx)
}

#[cfg(test)]
fn wait_on_callgraph_build_start_gate(root: &Path) {
    let mut slot = CALLGRAPH_BUILD_START_GATE
        .get_or_init(|| parking_lot::Mutex::new(None))
        .lock();
    if !slot.as_ref().is_some_and(|gate| gate.root == root) {
        return;
    }
    let gate = slot.take();
    drop(slot);
    if let Some(gate) = gate {
        let _ = gate.reached.send(());
        let _ = gate.release.recv_timeout(Duration::from_secs(5));
    }
}

#[cfg(not(test))]
fn wait_on_callgraph_build_start_gate(_root: &Path) {}

#[cfg(test)]
static REMOVE_CALLGRAPH_POINTER_BEFORE_INLINE_REOPEN: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
struct RemoveCallgraphPointerBeforeInlineReopenGuard;

#[cfg(test)]
impl Drop for RemoveCallgraphPointerBeforeInlineReopenGuard {
    fn drop(&mut self) {
        REMOVE_CALLGRAPH_POINTER_BEFORE_INLINE_REOPEN.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
fn remove_callgraph_pointer_before_inline_reopen_for_test(
    callgraph_dir: &Path,
    store: &CallGraphStore,
) {
    if REMOVE_CALLGRAPH_POINTER_BEFORE_INLINE_REOPEN.swap(false, Ordering::SeqCst) {
        let pointer = callgraph_dir.join(format!("{}.current", store.project_key()));
        std::fs::remove_file(pointer).expect("remove callgraph pointer before inline reopen");
    }
}

#[cfg(not(test))]
fn remove_callgraph_pointer_before_inline_reopen_for_test(
    _callgraph_dir: &Path,
    _store: &CallGraphStore,
) {
}

/// Inline wait window for a callgraph-store cold build before returning
/// `Building`. Default `0` (pure-async: never block the request thread).
/// Tests set `AFT_CALLGRAPH_BUILD_WAIT_MS` large so small fixture builds
/// resolve to `Ready` synchronously and exercise query correctness directly.
fn callgraph_build_wait_window() -> Duration {
    std::env::var("AFT_CALLGRAPH_BUILD_WAIT_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::ZERO)
}

static CALLGRAPH_COLD_BUILD_SPAWN_COUNT: AtomicUsize = AtomicUsize::new(0);

#[doc(hidden)]
pub fn reset_callgraph_cold_build_spawn_count_for_test() {
    CALLGRAPH_COLD_BUILD_SPAWN_COUNT.store(0, Ordering::SeqCst);
}

#[doc(hidden)]
pub fn callgraph_cold_build_spawn_count_for_test() -> usize {
    CALLGRAPH_COLD_BUILD_SPAWN_COUNT.load(Ordering::SeqCst)
}

impl AppContext {
    pub fn new(provider: Box<dyn LanguageProvider>, config: Config) -> Self {
        Self::with_app_and_provider(App::default_shared(), provider, config)
    }

    pub fn from_app(app: Arc<App>, config: Config) -> Self {
        let provider = app.create_provider();
        Self::with_app_and_provider(app, provider, config)
    }

    pub fn with_app_and_provider(
        app: Arc<App>,
        provider: Box<dyn LanguageProvider>,
        config: Config,
    ) -> Self {
        let bash_compress_enabled = config.experimental_bash_compress;
        let (configure_warnings_tx, configure_warnings_rx) = crossbeam_channel::unbounded();
        let progress_sender: SharedProgressSender = Arc::new(Mutex::new(None));
        let status_emitter = StatusEmitter::new(Arc::clone(&progress_sender));
        let heavy_root_work_allowed = Arc::new(AtomicBool::new(true));
        let symbol_cache = provider
            .as_any()
            .downcast_ref::<TreeSitterProvider>()
            .map(|provider| provider.symbol_cache())
            .unwrap_or_else(|| Arc::new(std::sync::RwLock::new(SymbolCache::new())));
        let mut lsp_manager = LspManager::new();
        lsp_manager.set_child_registry(app.lsp_child_registry());
        // Apply the configured diagnostic LRU cap (default 5000, 0 = unbounded)
        // so the documented `lsp.diagnostic_cache_size` knob takes effect.
        lsp_manager.set_diagnostic_capacity(config.diagnostic_cache_size);
        let bash_background = BgTaskRegistry::new(Arc::clone(&progress_sender));
        let compression_aggregates = bash_background.compression_aggregate_cache();
        let context = AppContext {
            app: Arc::clone(&app),
            provider,
            backup: parking_lot::Mutex::new(BackupStore::new()),
            checkpoint: parking_lot::Mutex::new(CheckpointStore::new()),
            config: RwLock::new(Arc::new(config)),
            force_restrict_requests: parking_lot::Mutex::new(BTreeMap::new()),
            harness: parking_lot::Mutex::new(None),
            canonical_cache_root: parking_lot::Mutex::new(None),
            is_worktree_bridge: parking_lot::Mutex::new(false),
            git_common_dir: parking_lot::Mutex::new(None),
            shared_artifacts_read_only: AtomicBool::new(false),
            callgraph_writer: AtomicBool::new(true),
            inspect_writer: AtomicBool::new(true),
            artifact_owner_status: parking_lot::Mutex::new(None),
            artifact_owner_lease: parking_lot::Mutex::new(None),
            degraded_reasons: parking_lot::Mutex::new(Vec::new()),
            heavy_root_work_allowed: Arc::clone(&heavy_root_work_allowed),
            callgraph_store: RwLock::new(None),
            callgraph_store_force_requested: AtomicU64::new(0),
            callgraph_store_force_fulfilled: AtomicU64::new(0),
            callgraph_store_rx: parking_lot::Mutex::new(None),
            callgraph_store_rx_generation: AtomicU64::new(0),
            callgraph_store_rx_epoch: AtomicU64::new(0),
            callgraph_persist_epoch: crate::root_cache::ArtifactPublishEpoch::default(),
            callgraph_legacy_migration_summary_logged: Arc::new(AtomicBool::new(false)),
            pending_callgraph_store_paths: Arc::new(parking_lot::Mutex::new(BTreeSet::new())),
            search_index: RwLock::new(None),
            search_index_rx: RwLock::new(None),
            search_index_rx_generation: AtomicU64::new(0),
            search_index_rx_epoch: AtomicU64::new(0),
            search_index_rx_terminal_epoch: Arc::new(AtomicU64::new(0)),
            search_index_disconnect_reschedule: parking_lot::Mutex::new((0, 0)),
            search_persist_epoch: crate::root_cache::ArtifactPublishEpoch::default(),
            pending_search_index_paths: parking_lot::Mutex::new(BTreeSet::new()),
            symbol_cache,
            inspect_manager: Arc::new(InspectManager::with_heavy_root_work_gate(Arc::clone(
                &heavy_root_work_allowed,
            ))),
            tier2_refresh_scheduler: parking_lot::Mutex::new(Tier2RefreshScheduler::new()),
            pending_tier2_paths: parking_lot::Mutex::new(BTreeSet::new()),
            semantic_index: RwLock::new(None),
            semantic_index_rx: parking_lot::Mutex::new(None),
            semantic_index_rx_generation: AtomicU64::new(0),
            semantic_index_rx_epoch: AtomicU64::new(0),
            semantic_index_rx_terminal_epoch: Arc::new(AtomicU64::new(0)),
            semantic_persist_epoch: crate::root_cache::ArtifactPublishEpoch::default(),
            semantic_persist_lock: Arc::new(parking_lot::Mutex::new(())),
            semantic_index_status: RwLock::new(SemanticIndexStatus::Disabled),
            artifact_reload_lock: parking_lot::Mutex::new(()),
            semantic_cold_seed_active: Arc::new(AtomicBool::new(false)),
            semantic_cold_seed_generation: Arc::new(AtomicU64::new(0)),
            semantic_fingerprint_generation: Arc::new(AtomicU64::new(0)),
            semantic_callgraph_warm_deferred: AtomicBool::new(false),
            pending_semantic_index_paths: Arc::new(parking_lot::Mutex::new(BTreeSet::new())),
            pending_semantic_corpus_refresh: parking_lot::Mutex::new(false),
            semantic_refresh_tx: Arc::new(parking_lot::Mutex::new(None)),
            semantic_refresh_event_rx: parking_lot::Mutex::new(None),
            semantic_refresh_generation: AtomicU64::new(0),
            semantic_refresh_epoch: AtomicU64::new(0),
            semantic_refresh_build_epoch: AtomicU64::new(0),
            semantic_refresh_worker: parking_lot::Mutex::new(None),
            semantic_refresh_retry_attempts: parking_lot::Mutex::new(BTreeMap::new()),
            semantic_refresh_circuit: Arc::new(SemanticRefreshCircuit::default()),
            semantic_embedding_model: parking_lot::Mutex::new(None),
            watcher_runtime_lock: parking_lot::Mutex::new(()),
            watcher: parking_lot::Mutex::new(None),
            watcher_rx: parking_lot::Mutex::new(None),
            watcher_drain_slice: parking_lot::Mutex::new(None),
            watcher_thread: parking_lot::Mutex::new(None),
            lsp_manager: parking_lot::Mutex::new(lsp_manager),
            configure_generation: Arc::new(AtomicU64::new(0)),
            configure_content_generation: Arc::new(AtomicU64::new(0)),
            subc_lifecycle: SubcLifecycleAdmission::default(),
            configure_warm_state: parking_lot::Mutex::new(ConfigureWarmState::default()),
            configure_phase_timing: parking_lot::Mutex::new(ConfigurePhaseTiming::default()),
            configured_session_roots: parking_lot::Mutex::new(BTreeSet::new()),
            configure_maintenance_jobs: parking_lot::Mutex::new(VecDeque::new()),
            artifact_cache_keys: parking_lot::Mutex::new(BTreeMap::new()),
            artifact_cache_key_derivations: AtomicU64::new(0),
            borrowed_index_cache: parking_lot::Mutex::new(BorrowedIndexCache::default()),
            worktree_bridge_cache: parking_lot::Mutex::new(BTreeMap::new()),
            #[cfg(test)]
            worktree_bridge_probe_spawns: AtomicU64::new(0),
            #[cfg(test)]
            force_worktree_bridge_reprobe: AtomicBool::new(false),
            last_seen_reuse_completions: AtomicU64::new(0),
            configure_warnings_tx,
            configure_warnings_rx,
            progress_sender: Arc::clone(&progress_sender),
            status_emitter,
            status_bar_last_emitted: RwLock::new(None),
            status_bar_cached: RwLock::new(StatusBarCache::default()),
            compression_aggregates,
            bash_background,
            #[cfg(unix)]
            escalation_grants: parking_lot::Mutex::new(
                crate::sandbox_spawn::EscalationGrantStore::default(),
            ),
            filter_registry: Arc::new(std::sync::RwLock::new(
                crate::compress::toml_filter::FilterRegistry::default(),
            )),
            filter_registry_rebuild_count: AtomicU64::new(0),
            filter_registry_loaded: std::sync::atomic::AtomicBool::new(false),
            bash_compress_flag: Arc::new(std::sync::atomic::AtomicBool::new(bash_compress_enabled)),
            gitignore: Arc::new(std::sync::RwLock::new(None)),
            gitignore_generation: Arc::new(AtomicU64::new(0)),
            status_bar_tier2: RwLock::new(StatusBarTier2::default()),
            tsconfig_membership: parking_lot::Mutex::new(
                crate::lsp::tsconfig_membership::TsconfigMembershipCache::new(),
            ),
        };
        crate::logging::sync_storage_root(context.storage_dir());
        context
    }

    /// Current agent status-bar counts. Generation identities are checked before
    /// project scoping or tsconfig membership work, so unchanged responses reuse
    /// the last honest aggregate from the continuously drained stores.
    pub fn status_bar_counts(&self) -> Option<StatusBarCounts> {
        let tier2 = self
            .status_bar_tier2
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let tsconfig_generation = self.tsconfig_membership.lock().generation();
        let lsp = self.lsp_manager.lock();
        let diagnostics_generation = lsp.diagnostics_generation();

        {
            let cached = self
                .status_bar_cached
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if cached.valid
                && cached.diagnostics_generation == diagnostics_generation
                && cached.tier2_generation == tier2.generation
                && cached.tsconfig_generation == tsconfig_generation
            {
                return cached.counts.clone();
            }
        }

        let counts = match (tier2.dead_code, tier2.unused_exports, tier2.duplicates) {
            (Some(dead_code), Some(unused_exports), Some(duplicates)) => {
                let (errors, warnings) = match self.canonical_cache_root_opt() {
                    Some(root) => {
                        let mut membership = self.tsconfig_membership.lock();
                        lsp.filtered_error_warning_counts(|file| {
                            file.starts_with(&root) && !membership.should_skip_diagnostics(file)
                        })
                    }
                    None => lsp.warm_error_warning_counts(),
                };
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
            _ => None,
        };

        *self
            .status_bar_cached
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = StatusBarCache {
            valid: true,
            diagnostics_generation,
            tier2_generation: tier2.generation,
            tsconfig_generation,
            counts: counts.clone(),
        };
        counts
    }

    pub fn try_health_snapshot(&self, project_root: &Path) -> RootHealthSnapshot {
        // Read lifecycle state before taking artifact locks. Worker admission takes
        // the lifecycle lock first and then installs artifact receivers, so the
        // reverse order here would deadlock a health poll against worker startup.
        let heavy_root_work_allowed = match self.try_heavy_root_work_allowed() {
            Some(allowed) => allowed,
            None => return RootHealthSnapshot::busy(project_root),
        };
        let config = match self.config.try_read() {
            Ok(guard) => Arc::clone(&*guard),
            Err(_) => return RootHealthSnapshot::busy(project_root),
        };
        let search_index = match self.search_index.try_read() {
            Ok(guard) => guard,
            Err(_) => return RootHealthSnapshot::busy(project_root),
        };
        let search_index_rx = match self.search_index_rx.try_read() {
            Ok(guard) => guard,
            Err(_) => return RootHealthSnapshot::busy(project_root),
        };
        let semantic_status = match self.semantic_index_status.try_read() {
            Ok(guard) => guard,
            Err(_) => return RootHealthSnapshot::busy(project_root),
        };
        let callgraph_store = match self.callgraph_store.try_read() {
            Ok(guard) => guard,
            Err(_) => return RootHealthSnapshot::busy(project_root),
        };
        let callgraph_store_rx = match self.callgraph_store_rx.try_lock() {
            Some(guard) => guard,
            None => return RootHealthSnapshot::busy(project_root),
        };
        let tier2 = match self.status_bar_tier2.try_read() {
            Ok(guard) => guard,
            Err(_) => return RootHealthSnapshot::busy(project_root),
        };
        let bash = match self.bash_background.try_health_counts() {
            Some(counts) => counts,
            None => return RootHealthSnapshot::busy(project_root),
        };

        // Borrow-only roots (mason worktrees, read-only siblings) never
        // materialize an in-RAM index or spawn a build: queries go through the
        // read-only disk openers against the shared artifact. Reporting them
        // as "building" is a permanent lie that keeps module health degraded
        // whenever any worktree is bound.
        let borrows_shared_artifacts = self.shared_artifacts_read_only.load(Ordering::SeqCst);
        let search_index_status = if search_index
            .as_ref()
            .is_some_and(|index| index.ready || index.build_denied)
            || (borrows_shared_artifacts && config.search_index)
        {
            "ready"
        } else if config.search_index
            || search_index.as_ref().is_some()
            || search_index_rx.as_ref().is_some()
        {
            "building"
        } else {
            "disabled"
        };
        let semantic_index_status = match &*semantic_status {
            SemanticIndexStatus::Ready { .. } => "ready",
            SemanticIndexStatus::Building { .. } => "building",
            SemanticIndexStatus::Disabled => "disabled",
            SemanticIndexStatus::Failed(_) => "degraded",
        };
        let callgraph_writer = self.callgraph_writer.load(Ordering::SeqCst);
        let callgraph_store_status = if !heavy_root_work_allowed {
            "disabled"
        } else if callgraph_store.as_ref().is_some() {
            "ready"
        } else if !callgraph_writer && config.callgraph_store {
            // Read-only roots never cold-build; they query the shared store
            // via ReadonlyCallGraphStore on demand. "building" would never
            // resolve.
            "ready"
        } else if callgraph_store_rx.is_some() || config.callgraph_store {
            "building"
        } else {
            "disabled"
        };
        // dead_code is suppressed (reports `None`) while the callgraph store is
        // not ready, so a root whose only missing category is dead_code would
        // otherwise stay "building" forever: nothing recomputes dead_code until
        // the callgraph store becomes ready. Treat that blocked-on-callgraph case
        // as complete and let the callgraph component's own status tell the
        // callgraph story, instead of double-reporting it here as a permanent
        // "building".
        let dead_code_blocked_on_callgraph = tier2.dead_code_blocked_on_callgraph;
        let tier2_complete = (tier2.dead_code.is_some() || dead_code_blocked_on_callgraph)
            && tier2.unused_exports.is_some()
            && tier2.duplicates.is_some()
            && !tier2.stale;
        let tier2_has_aggregates = tier2.dead_code.is_some()
            || tier2.unused_exports.is_some()
            || tier2.duplicates.is_some();
        let tier2_refresh_gated = borrows_shared_artifacts
            || !heavy_root_work_allowed
            || !self.inspect_writer.load(Ordering::SeqCst)
            || !self.inspect_manager.automatic_tier2_refresh_enabled();
        let tier2_status = if tier2_complete {
            "ready"
        } else if !config.inspect.enabled || !tier2_has_aggregates || tier2_refresh_gated {
            // A partial snapshot can only be "building" when this root is
            // allowed to run the refresh that would complete it.
            "disabled"
        } else {
            "building"
        };

        RootHealthSnapshot {
            project_root: project_root.display().to_string(),
            actor_count: 1,
            state: RootHealthState::Ready,
            search_index: Some(HealthComponentSnapshot {
                status: search_index_status,
            }),
            semantic_index: Some(HealthComponentSnapshot {
                status: semantic_index_status,
            }),
            callgraph_store: Some(HealthComponentSnapshot {
                status: callgraph_store_status,
            }),
            tier2: Some(Tier2HealthSnapshot {
                status: tier2_status,
            }),
            bash: Some(bash),
        }
    }

    pub fn should_emit_status_bar(&self, counts: &StatusBarCounts) -> bool {
        let mut last = self
            .status_bar_last_emitted
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if last.as_ref() == Some(counts) {
            return false;
        }
        *last = Some(counts.clone());
        true
    }

    /// Invalidate the status-bar tsconfig-membership cache. Called from the
    /// watcher seam when a tsconfig-like file changes and from `configure`
    /// when the project root changes, so the next bar count re-reads from disk.
    pub fn clear_tsconfig_membership_cache(&self) {
        self.tsconfig_membership.lock().clear();
    }

    #[cfg(test)]
    pub fn tsconfig_membership_clear_generation_for_test(&self) -> u64 {
        self.tsconfig_membership.lock().generation()
    }

    /// Mark the status-bar Tier-2 counts stale (rendered with `~`) without
    /// changing the numbers — called when the watcher sees a source-file change,
    /// so the bar honestly signals the counts predate the latest edit until the
    /// next background scan completes. Returns true only when the visible stale
    /// bit flips. No-op before the first populate.
    pub fn mark_status_bar_tier2_stale(&self) -> bool {
        let mut tier2 = self
            .status_bar_tier2
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // No-op before the first full populate (nothing real to mark stale).
        if tier2.dead_code.is_some() && tier2.unused_exports.is_some() && tier2.duplicates.is_some()
        {
            let changed = !tier2.stale;
            tier2.stale = true;
            if changed {
                tier2.generation = tier2.generation.wrapping_add(1);
            }
            return changed;
        }
        false
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
        let mut tier2 = self
            .status_bar_tier2
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = (
            tier2.dead_code,
            tier2.unused_exports,
            tier2.duplicates,
            tier2.todos,
            tier2.stale,
        );
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
        let current = (
            tier2.dead_code,
            tier2.unused_exports,
            tier2.duplicates,
            tier2.todos,
            tier2.stale,
        );
        if current != previous {
            tier2.generation = tier2.generation.wrapping_add(1);
        }
    }

    /// Record whether the latest dead_code aggregate was suppressed because the
    /// callgraph store was not ready (`callgraph_available:false`). Kept separate
    /// from [`update_status_bar_tier2`] because the flag is health metadata, not a
    /// status-bar count: it never renders in the bar and need not bump the
    /// count-generation used for status-bar cache invalidation.
    pub(crate) fn set_status_bar_tier2_dead_code_blocked_on_callgraph(&self, blocked: bool) {
        let mut tier2 = self
            .status_bar_tier2
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tier2.dead_code_blocked_on_callgraph = blocked;
    }

    /// Borrow the cached project gitignore matcher. Returns `None` when no
    /// project_root is configured or when the project has no gitignore files.
    pub fn gitignore(&self) -> Option<Arc<ignore::gitignore::Gitignore>> {
        self.gitignore
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Shared gitignore matcher handle for the watcher filter thread.
    pub fn shared_gitignore(&self) -> SharedGitignore {
        Arc::clone(&self.gitignore)
    }

    /// Monotonic generation bumped after every matcher rebuild/clear. The
    /// watcher filter thread uses it to wait until the main thread has rebuilt
    /// ignore rules after it reports an ignore-file change.
    pub fn gitignore_generation(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.gitignore_generation)
    }

    fn set_gitignore(&self, matcher: Option<Arc<ignore::gitignore::Gitignore>>) {
        *self
            .gitignore
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = matcher;
        self.gitignore_generation.fetch_add(1, Ordering::SeqCst);
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
        self.set_gitignore(None);
    }

    pub fn rebuild_gitignore(&self) {
        use ignore::gitignore::GitignoreBuilder;
        use std::path::Path;
        let root_raw = match self.config().project_root.clone() {
            Some(r) => r,
            None => {
                self.set_gitignore(None);
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
            .lock()
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
                    self.set_gitignore(Some(Arc::new(gi)));
                } else {
                    self.set_gitignore(None);
                }
            }
            Err(err) => {
                crate::slog_warn!("gitignore matcher build failed: {}", err);
                self.set_gitignore(None);
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
        self.update_config(|config| {
            config.experimental_bash_compress = enabled;
        });
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
        self.filter_registry_rebuild_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
        self.filter_registry_rebuild_count
            .fetch_add(1, Ordering::SeqCst);
        if let Ok(mut slot) = self.filter_registry.write() {
            *slot = new_registry;
            self.filter_registry_loaded.store(true, Ordering::Release);
        }
    }

    #[cfg(test)]
    pub fn filter_registry_rebuild_count_for_test(&self) -> u64 {
        self.filter_registry_rebuild_count.load(Ordering::SeqCst)
    }

    pub fn app(&self) -> Arc<App> {
        Arc::clone(&self.app)
    }

    /// Clone the LSP child registry handle. Used by main.rs to give the
    /// signal handler thread a way to SIGKILL LSP children on shutdown.
    pub fn lsp_child_registry(&self) -> crate::lsp::child_registry::LspChildRegistry {
        self.app.lsp_child_registry()
    }

    pub fn stdout_writer(&self) -> SharedStdoutWriter {
        self.app.stdout_writer()
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
        self.subc_lifecycle
            .advance_generation(self.configure_generation.as_ref())
    }

    pub(crate) fn mark_subc_bound(&self) {
        self.subc_lifecycle.mark_bound();
    }

    pub(crate) fn mark_subc_unbound(&self) {
        self.subc_lifecycle
            .mark_unbound(self.configure_generation.as_ref());
    }

    #[doc(hidden)]
    pub fn subc_unbound_quiesced(&self) -> bool {
        self.subc_lifecycle.is_unbound()
    }

    pub(crate) fn subc_lifecycle_admission(&self) -> SubcLifecycleAdmission {
        self.subc_lifecycle.clone()
    }

    pub(crate) fn run_if_subc_bound_generation<R>(
        &self,
        expected_generation: u64,
        action: impl FnOnce() -> R,
    ) -> Option<R> {
        self.subc_lifecycle.run_if_current(
            self.configure_generation.as_ref(),
            expected_generation,
            action,
        )
    }

    /// Record the warm-maintenance key for a successful configure and return
    /// the generation this configure operates under.
    ///
    /// An unchanged key ADOPTS the running generation without advancing it:
    /// in-flight build workers gate their publish on the generation flag being
    /// unchanged, so advancing on an equivalent rebind would silently discard
    /// every adopted build's result at completion (the receiver never
    /// resolves, and long builds can never finish under rebind traffic). Only
    /// a genuinely different warm config advances the generation, which is
    /// what cancels superseded in-flight builds.
    pub fn note_configure_warm_key(&self, key: String) -> (u64, bool) {
        let mut state = self.configure_warm_state.lock();
        let equivalent = state.key.as_ref().is_some_and(|previous| *previous == key);
        let generation = if equivalent {
            self.configure_generation()
        } else {
            self.configure_content_generation
                .fetch_add(1, Ordering::SeqCst);
            self.advance_configure_generation()
        };
        state.generation = generation;
        state.key = Some(key);
        (generation, equivalent)
    }

    pub(crate) fn configure_warm_key_matches(&self, key: &str) -> bool {
        self.configure_warm_state
            .lock()
            .key
            .as_deref()
            .is_some_and(|current| current == key)
    }

    pub(crate) fn invalidate_configure_warm_state(&self) {
        self.configure_warm_state.lock().key = None;
    }

    pub fn note_configure_session_binding(&self, root: PathBuf, session_id: String) -> bool {
        self.configured_session_roots
            .lock()
            .insert((root, session_id))
    }

    /// Undo [`Self::note_configure_session_binding`] when the maintenance job
    /// carrying the session's bash replay was dropped as stale: the session has
    /// not actually been replayed, so its next bind must count as first again.
    pub fn forget_configure_session_binding(&self, root: &Path, session_id: &str) {
        self.configured_session_roots
            .lock()
            .remove(&(root.to_path_buf(), session_id.to_string()));
    }

    /// Cheap emptiness probes for the maintenance scheduler: a drain kind with
    /// no pending work is not enqueued at all, so idle roots stop paying a
    /// dispatch cycle per kind per tick. Every probe is lock-free or try-lock
    /// (a contended source reports "maybe work" and the kind is enqueued —
    /// fail-open keeps the skip an optimization, never a correctness gate).
    pub fn watcher_drain_has_work(&self) -> bool {
        let receiver_pending = self
            .watcher_rx
            .lock()
            .as_ref()
            .is_some_and(|rx| !rx.is_empty());
        receiver_pending
            || self
                .watcher_drain_slice
                .lock()
                .as_ref()
                .is_some_and(WatcherDrainSliceState::has_pending_work)
    }

    pub fn lsp_drain_has_work(&self) -> bool {
        match self.lsp_manager.try_lock() {
            Some(lsp) => lsp.has_pending_events(),
            // Contended: the manager is busy, so events may be queuing.
            None => true,
        }
    }

    pub fn completion_drains_have_work(&self) -> bool {
        let search_pending = self
            .search_index_rx
            .try_read()
            .map(|slot| {
                slot.as_ref().is_some_and(|receiver| {
                    !receiver.is_empty()
                        || self.search_index_rx_terminal_epoch.load(Ordering::SeqCst)
                            == self.search_index_rx_epoch()
                })
            })
            .unwrap_or(true);
        if search_pending {
            return true;
        }
        if self
            .callgraph_store_rx
            .lock()
            .as_ref()
            .is_some_and(|rx| !rx.is_empty())
        {
            return true;
        }
        if self
            .semantic_index_rx
            .lock()
            .as_ref()
            .is_some_and(|receiver| {
                !receiver.is_empty()
                    || self.semantic_index_rx_terminal_epoch.load(Ordering::SeqCst)
                        == self.semantic_index_rx_epoch()
            })
        {
            return true;
        }
        if self
            .semantic_refresh_event_rx
            .lock()
            .as_ref()
            .is_some_and(|rx| !rx.is_empty())
        {
            return true;
        }
        if self.semantic_refresh_probe_ready() && self.semantic_refresh_event_rx.lock().is_some() {
            return true;
        }
        if self
            .semantic_refresh_worker
            .lock()
            .as_ref()
            .is_some_and(|worker_slot| match worker_slot.try_lock() {
                Ok(handle) => handle
                    .as_ref()
                    .is_some_and(std::thread::JoinHandle::is_finished),
                Err(std::sync::TryLockError::WouldBlock) => true,
                Err(std::sync::TryLockError::Poisoned(_)) => true,
            })
        {
            return true;
        }
        self.inspect_manager().has_pending_completions() || self.has_new_reuse_completions()
    }

    pub fn configure_tail_has_work(&self) -> bool {
        !self.configure_maintenance_jobs.lock().is_empty() || !self.configure_warnings_rx.is_empty()
    }

    pub(crate) fn enqueue_configure_maintenance(&self, job: ConfigureMaintenanceJob) {
        self.configure_maintenance_jobs.lock().push_back(job);
    }

    pub(crate) fn drain_configure_maintenance(&self) -> Vec<ConfigureMaintenanceJob> {
        self.configure_maintenance_jobs.lock().drain(..).collect()
    }

    #[cfg(test)]
    pub(crate) fn configure_maintenance_job_count_for_test(&self) -> usize {
        self.configure_maintenance_jobs.lock().len()
    }

    /// Peek the memoized artifact key without deriving it. Passive readers
    /// (status snapshots) use this so reporting never spawns a git probe.
    pub fn cached_artifact_cache_key(&self, canonical_root: &Path) -> Option<String> {
        self.artifact_cache_keys.lock().get(canonical_root).cloned()
    }

    /// Return a worktree probe result only while the root's `.git` marker still
    /// matches the marker present when the successful probe was cached.
    pub(crate) fn cached_worktree_bridge(
        &self,
        canonical_root: &Path,
    ) -> Option<(bool, Option<PathBuf>)> {
        #[cfg(test)]
        if self.force_worktree_bridge_reprobe.load(Ordering::SeqCst) {
            return None;
        }

        let signature = git_entry_signature(canonical_root);
        self.worktree_bridge_cache
            .lock()
            .get(canonical_root)
            .filter(|entry| entry.git_entry == signature)
            .map(|entry| (entry.is_worktree_bridge, entry.git_common_dir.clone()))
    }

    /// Cache only successful git worktree probes. Failed probes remain retryable
    /// because a transient process or filesystem error must not become sticky.
    pub(crate) fn cache_worktree_bridge(
        &self,
        canonical_root: &Path,
        is_worktree_bridge: bool,
        git_common_dir: PathBuf,
    ) {
        self.worktree_bridge_cache.lock().insert(
            canonical_root.to_path_buf(),
            WorktreeBridgeCacheEntry {
                git_entry: git_entry_signature(canonical_root),
                is_worktree_bridge,
                git_common_dir: Some(git_common_dir),
            },
        );
    }

    #[cfg(test)]
    pub(crate) fn record_worktree_bridge_probe_spawn_for_test(&self) {
        self.worktree_bridge_probe_spawns
            .fetch_add(1, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn worktree_bridge_probe_spawns_for_test(&self) -> u64 {
        self.worktree_bridge_probe_spawns.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn force_worktree_bridge_reprobe_for_test(&self, enabled: bool) {
        self.force_worktree_bridge_reprobe
            .store(enabled, Ordering::SeqCst);
    }

    pub fn memoized_artifact_cache_key(&self, canonical_root: &Path) -> String {
        let mut keys = self.artifact_cache_keys.lock();
        if let Some(key) = keys.get(canonical_root).cloned() {
            return key;
        }
        let key = crate::search_index::artifact_cache_key(canonical_root);
        self.artifact_cache_key_derivations
            .fetch_add(1, Ordering::SeqCst);
        keys.insert(canonical_root.to_path_buf(), key.clone());
        key
    }

    pub fn memoized_artifact_cache_key_for_configure(
        &self,
        raw_root: &Path,
        canonical_root: &Path,
        storage_root: &Path,
        git_common_dir: Option<&Path>,
    ) -> Result<String, crate::search_index::ArtifactCacheKeyProbeError> {
        {
            let keys = self.artifact_cache_keys.lock();
            if let Some(key) = keys
                .get(canonical_root)
                .or_else(|| keys.get(raw_root))
                .cloned()
            {
                return Ok(key);
            }
        }

        let key = crate::search_index::artifact_cache_key_with_memo(
            canonical_root,
            raw_root,
            storage_root,
            git_common_dir,
        )?;
        self.artifact_cache_key_derivations
            .fetch_add(1, Ordering::SeqCst);
        let mut keys = self.artifact_cache_keys.lock();
        keys.insert(canonical_root.to_path_buf(), key.clone());
        keys.insert(raw_root.to_path_buf(), key.clone());
        Ok(key)
    }

    #[cfg(test)]
    pub fn artifact_cache_key_derivation_count_for_test(&self) -> u64 {
        self.artifact_cache_key_derivations.load(Ordering::SeqCst)
    }

    pub(crate) fn resolve_external_git_root(
        &self,
        project_root: &Path,
        requested_path: &str,
    ) -> Result<PathBuf, crate::readonly_artifacts::GitRootResolutionError> {
        let raw_path = Path::new(requested_path);
        let canonical_requested = if raw_path.is_absolute() {
            std::fs::canonicalize(raw_path).ok()
        } else {
            None
        };
        if let Some(root) = canonical_requested
            .as_deref()
            .and_then(|root| self.borrowed_index_cache.lock().resolved_root(root))
        {
            return Ok(root);
        }

        let root = crate::readonly_artifacts::resolve_git_root_from_user_path(
            project_root,
            requested_path,
        )?;
        if canonical_requested.as_deref() == Some(root.as_path()) {
            self.borrowed_index_cache
                .lock()
                .remember_resolved_root(root.clone());
        }
        Ok(root)
    }

    pub(crate) fn open_borrowed_search_index(
        &self,
        external_root: &Path,
        storage_dir: Option<&Path>,
    ) -> crate::readonly_artifacts::ReadOnlyArtifact<Arc<SearchIndex>> {
        let canonical_root =
            std::fs::canonicalize(external_root).unwrap_or_else(|_| external_root.to_path_buf());
        let project_key = self.memoized_artifact_cache_key(&canonical_root);
        let Some(artifact) = crate::readonly_artifacts::search_index_artifact_generation_with_key(
            &project_key,
            storage_dir,
        ) else {
            return crate::readonly_artifacts::ReadOnlyArtifact::Absent;
        };
        let key = BorrowedIndexCacheKey {
            canonical_root: canonical_root.clone(),
            artifact,
        };
        let mut cache = self.borrowed_index_cache.lock();
        if let Some(index) = cache.search(&key) {
            return index;
        }

        let opened = crate::readonly_artifacts::open_search_index_read_only_with_key(
            &canonical_root,
            storage_dir,
            &project_key,
        )
        .map(Arc::new);
        if !matches!(opened, crate::readonly_artifacts::ReadOnlyArtifact::Absent) {
            cache.insert(key, BorrowedIndexCacheValue::Search(opened.clone()));
        }
        opened
    }

    pub(crate) fn open_borrowed_semantic_index(
        &self,
        external_root: &Path,
        storage_dir: Option<&Path>,
    ) -> crate::readonly_artifacts::ReadOnlyArtifact<Arc<SemanticIndex>> {
        let canonical_root =
            std::fs::canonicalize(external_root).unwrap_or_else(|_| external_root.to_path_buf());
        let project_key = self.memoized_artifact_cache_key(&canonical_root);
        let Some(artifact) = crate::readonly_artifacts::semantic_index_artifact_generation_with_key(
            &project_key,
            storage_dir,
        ) else {
            return crate::readonly_artifacts::ReadOnlyArtifact::Absent;
        };
        let key = BorrowedIndexCacheKey {
            canonical_root: canonical_root.clone(),
            artifact,
        };
        let mut cache = self.borrowed_index_cache.lock();
        if let Some(index) = cache.semantic(&key) {
            return index;
        }

        let opened = crate::readonly_artifacts::open_semantic_index_read_only_with_key(
            &canonical_root,
            storage_dir,
            &project_key,
        )
        .map(Arc::new);
        if !matches!(opened, crate::readonly_artifacts::ReadOnlyArtifact::Absent) {
            cache.insert(key, BorrowedIndexCacheValue::Semantic(opened.clone()));
        }
        opened
    }

    #[cfg(test)]
    pub(crate) fn borrowed_index_cache_len_for_test(&self) -> usize {
        self.borrowed_index_cache.lock().entries.len()
    }

    pub fn configure_generation(&self) -> u64 {
        self.configure_generation.load(Ordering::SeqCst)
    }

    pub fn configure_generation_flag(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.configure_generation)
    }

    pub(crate) fn configure_content_generation(&self) -> u64 {
        self.configure_content_generation.load(Ordering::SeqCst)
    }

    pub(crate) fn configure_content_generation_flag(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.configure_content_generation)
    }

    pub(crate) fn begin_configure_ack_phase(&self, phase: &'static str) {
        let now = Instant::now();
        let mut timing = self.configure_phase_timing.lock();
        if phase == "canonicalize" {
            timing.completed.clear();
        } else if timing.phase != "idle" && timing.phase != "ack_ready" {
            let previous = timing.phase;
            let elapsed = now.saturating_duration_since(timing.started_at);
            timing.completed.push((previous, elapsed));
        }
        timing.phase = phase;
        timing.started_at = now;
    }

    pub(crate) fn configure_ack_phase_snapshot(&self) -> String {
        let timing = self.configure_phase_timing.lock();
        let mut parts = timing
            .completed
            .iter()
            .map(|(phase, elapsed)| format!("{phase}={}ms", elapsed.as_millis()))
            .collect::<Vec<_>>();
        parts.push(format!(
            "{}={}ms",
            timing.phase,
            timing.started_at.elapsed().as_millis()
        ));
        parts.join(",")
    }

    pub fn advance_semantic_fingerprint_generation(&self) -> u64 {
        self.semantic_fingerprint_generation
            .fetch_add(1, Ordering::SeqCst)
            .wrapping_add(1)
    }

    pub fn semantic_fingerprint_generation(&self) -> u64 {
        self.semantic_fingerprint_generation.load(Ordering::SeqCst)
    }

    pub fn semantic_fingerprint_generation_flag(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.semantic_fingerprint_generation)
    }

    pub fn configure_warnings_sender(
        &self,
    ) -> crossbeam_channel::Sender<(u64, ConfigureWarningsFrame)> {
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

    #[cfg(unix)]
    pub(crate) fn escalation_grants(
        &self,
    ) -> &parking_lot::Mutex<crate::sandbox_spawn::EscalationGrantStore> {
        &self.escalation_grants
    }

    pub fn drain_bg_completions(&self) -> Vec<BgCompletion> {
        self.bash_background.drain_completions()
    }

    /// Access the language provider.
    pub fn provider(&self) -> &dyn LanguageProvider {
        self.provider.as_ref()
    }

    /// Access the backup store.
    pub fn backup(&self) -> &parking_lot::Mutex<BackupStore> {
        &self.backup
    }

    /// Access the checkpoint store.
    pub fn checkpoint(&self) -> &parking_lot::Mutex<CheckpointStore> {
        &self.checkpoint
    }

    pub fn set_db(&self, conn: Arc<Mutex<Connection>>) {
        self.app.set_db(conn);
        self.compression_aggregates.clear();
    }

    pub fn clear_db(&self) {
        self.app.clear_db();
        self.compression_aggregates.clear();
    }

    pub fn db(&self) -> Option<Arc<Mutex<Connection>>> {
        self.app.db()
    }

    pub(crate) fn compression_aggregate_cache(
        &self,
    ) -> &crate::db::compression_events::CompressionAggregateCache {
        self.compression_aggregates.as_ref()
    }

    /// Access an owned configuration snapshot.
    pub fn config(&self) -> Arc<Config> {
        let guard = match self.config.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        Arc::clone(&*guard)
    }

    /// Atomically publish a fully-built configuration snapshot.
    pub fn set_config(&self, config: Config) {
        let next = Arc::new(config);
        match self.config.write() {
            Ok(mut guard) => *guard = next,
            Err(poisoned) => *poisoned.into_inner() = next,
        }
    }

    /// Clone-mutate-publish the current configuration without returning a guard.
    pub fn update_config(&self, update: impl FnOnce(&mut Config)) {
        let mut next = self.config().as_ref().clone();
        update(&mut next);
        self.set_config(next);
    }

    pub fn force_restrict_guard(&self, req_id: &str) -> ForceRestrictGuard<'_> {
        let mut requests = self.force_restrict_requests.lock();
        *requests.entry(req_id.to_string()).or_insert(0) += 1;
        ForceRestrictGuard {
            ctx: self,
            req_id: req_id.to_string(),
        }
    }

    pub fn with_force_restrict<R>(&self, req_id: &str, f: impl FnOnce() -> R) -> R {
        let _guard = self.force_restrict_guard(req_id);
        f()
    }

    pub fn request_force_restrict(&self, req_id: &str) -> bool {
        self.force_restrict_requests.lock().contains_key(req_id)
    }

    fn release_force_restrict(&self, req_id: &str) {
        let mut requests = self.force_restrict_requests.lock();
        match requests.get_mut(req_id) {
            Some(count) if *count > 1 => *count -= 1,
            Some(_) => {
                requests.remove(req_id);
            }
            None => {}
        }
    }

    pub fn set_harness(&self, harness: Harness) {
        self.bash_background.set_harness(harness.clone());
        *self.harness.lock() = Some(harness);
    }

    pub fn harness_opt(&self) -> Option<Harness> {
        self.harness.lock().clone()
    }

    pub fn harness(&self) -> Harness {
        self.harness_opt()
            .expect("harness set by configure before any tool call")
    }

    pub fn storage_dir(&self) -> PathBuf {
        crate::bash_background::storage_dir(self.config().storage_dir.as_deref())
    }

    pub fn harness_dir(&self) -> PathBuf {
        self.storage_dir().join(self.harness().storage_segment())
    }

    pub fn inspect_dir(&self) -> PathBuf {
        if let Some(root) = self
            .canonical_cache_root_opt()
            .or_else(|| self.config().project_root.clone())
        {
            self.storage_dir()
                .join("inspect")
                .join(crate::path_identity::project_scope_key(&root))
        } else {
            self.storage_dir().join("inspect").join("unconfigured")
        }
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
        let root_changed = {
            let mut current = self.canonical_cache_root.lock();
            let changed = current.as_deref() != Some(root.as_path());
            *current = Some(root);
            changed
        };
        if root_changed {
            let mut tier2 = self
                .status_bar_tier2
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let generation = tier2.generation.wrapping_add(1);
            *tier2 = StatusBarTier2 {
                generation,
                ..StatusBarTier2::default()
            };
            *self
                .status_bar_last_emitted
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        }
    }

    pub fn canonical_cache_root(&self) -> PathBuf {
        self.canonical_cache_root
            .lock()
            .clone()
            .expect("canonical_cache_root accessed before handle_configure")
    }

    pub fn canonical_cache_root_opt(&self) -> Option<PathBuf> {
        self.canonical_cache_root.lock().clone()
    }

    pub fn set_cache_role(&self, is_worktree_bridge: bool, git_common_dir: Option<PathBuf>) {
        *self.is_worktree_bridge.lock() = is_worktree_bridge;
        *self.git_common_dir.lock() = git_common_dir;
        // The configure-time worktree probe already applies the test seam, so
        // automatic Tier-2 scheduling follows the same effective root role as
        // callgraph cold-build gating while explicit inspect demand stays enabled.
        self.inspect_manager
            .set_automatic_tier2_refresh_allowed(!is_worktree_bridge);
        let artifact_read_only = self.shared_artifacts_read_only.load(Ordering::SeqCst);
        self.callgraph_writer
            .store(!is_worktree_bridge && !artifact_read_only, Ordering::SeqCst);
    }

    pub fn set_artifact_owner(
        &self,
        status: Option<ArtifactOwnerStatus>,
        lease: Option<ArtifactOwnerLease>,
    ) {
        let read_only = status
            .as_ref()
            .is_some_and(|status| status.mode == ArtifactOwnerMode::ReadOnly);
        self.shared_artifacts_read_only
            .store(read_only, Ordering::SeqCst);
        self.callgraph_writer
            .store(!self.is_worktree_bridge() && !read_only, Ordering::SeqCst);
        self.inspect_writer.store(true, Ordering::SeqCst);
        *self.artifact_owner_status.lock() = status;
        *self.artifact_owner_lease.lock() = lease.map(crate::artifact_owner::register_heartbeat);
    }

    pub fn set_cache_writer_capabilities(&self, callgraph_writer: bool, inspect_writer: bool) {
        self.callgraph_writer
            .store(callgraph_writer, Ordering::SeqCst);
        self.inspect_writer.store(inspect_writer, Ordering::SeqCst);
    }

    pub fn callgraph_writer(&self) -> bool {
        self.callgraph_writer.load(Ordering::SeqCst)
    }

    pub fn inspect_writer(&self) -> bool {
        self.inspect_writer.load(Ordering::SeqCst)
    }

    pub fn shared_artifacts_read_only(&self) -> bool {
        !self.callgraph_writer()
    }

    pub fn artifact_owner_status(&self) -> Option<ArtifactOwnerStatus> {
        self.artifact_owner_status.lock().clone()
    }

    pub fn is_worktree_bridge(&self) -> bool {
        *self.is_worktree_bridge.lock()
    }

    pub fn git_common_dir(&self) -> Option<PathBuf> {
        self.git_common_dir.lock().clone()
    }

    /// Replace the current degraded-mode reasons. Empty vec = full-featured
    /// mode (no degradation). Called by `handle_configure` after deciding
    /// which subsystems to disable for this project root.
    pub fn set_degraded_reasons(&self, reasons: Vec<String>) {
        *self.degraded_reasons.lock() = reasons;
    }

    pub fn set_heavy_root_work_allowed(&self, allowed: bool) {
        self.heavy_root_work_allowed
            .store(allowed, Ordering::SeqCst);
    }

    pub fn heavy_root_work_allowed(&self) -> bool {
        self.heavy_root_work_allowed.load(Ordering::SeqCst) && !self.subc_lifecycle.is_unbound()
    }

    fn try_heavy_root_work_allowed(&self) -> Option<bool> {
        if !self.heavy_root_work_allowed.load(Ordering::SeqCst) {
            return Some(false);
        }
        self.subc_lifecycle.try_is_bound()
    }

    pub fn add_degraded_reason(&self, reason: impl Into<String>) -> bool {
        let reason = reason.into();
        let mut reasons = self.degraded_reasons.lock();
        if reasons.iter().any(|existing| existing == &reason) {
            return false;
        }
        reasons.push(reason);
        true
    }

    /// Snapshot of current degraded-mode reasons. Order is stable
    /// (insertion order from `set_degraded_reasons`) so UI rendering and
    /// snapshot diffs are deterministic.
    pub fn degraded_reasons(&self) -> Vec<String> {
        self.degraded_reasons.lock().clone()
    }

    /// True iff at least one degraded reason is recorded.
    pub fn is_degraded(&self) -> bool {
        !self.degraded_reasons.lock().is_empty()
    }

    pub fn cache_role(&self) -> &'static str {
        if self.canonical_cache_root.lock().is_none() {
            "not_initialized"
        } else if self.is_worktree_bridge() {
            "worktree"
        } else if self.shared_artifacts_read_only.load(Ordering::SeqCst) {
            "read_only"
        } else {
            "main"
        }
    }

    /// Access the persisted call graph store.
    pub fn callgraph_store(&self) -> &RwLock<Option<Arc<ReadonlyCallGraphStore>>> {
        &self.callgraph_store
    }

    pub fn mark_callgraph_store_force_rebuild(&self) -> u64 {
        self.callgraph_store_force_requested
            .fetch_add(1, Ordering::SeqCst)
            .wrapping_add(1)
    }

    pub(crate) fn pending_callgraph_store_force_token(&self) -> Option<u64> {
        let requested = self.callgraph_store_force_requested.load(Ordering::SeqCst);
        let fulfilled = self.callgraph_store_force_fulfilled.load(Ordering::SeqCst);
        (requested > fulfilled).then_some(requested)
    }

    pub fn fulfill_callgraph_store_force_token(&self, token: u64) {
        self.callgraph_store_force_fulfilled
            .fetch_max(token, Ordering::SeqCst);
    }

    pub fn callgraph_store_dir(&self) -> PathBuf {
        if let Some(root) = self.callgraph_project_root() {
            self.storage_dir()
                .join("callgraph")
                .join(self.memoized_artifact_cache_key(&root))
        } else {
            self.storage_dir().join("callgraph").join("unconfigured")
        }
    }

    pub fn ensure_callgraph_store(
        &self,
    ) -> Result<Option<Arc<ReadonlyCallGraphStore>>, CallGraphStoreError> {
        self.ensure_callgraph_store_with_flag(true)
    }

    fn ensure_callgraph_store_with_flag(
        &self,
        respect_config_flag: bool,
    ) -> Result<Option<Arc<ReadonlyCallGraphStore>>, CallGraphStoreError> {
        if respect_config_flag && !self.config().callgraph_store {
            return Ok(None);
        }
        if !self.heavy_root_work_allowed() {
            return Ok(None);
        }
        self.revalidate_callgraph_store_generation();
        let force_token = self.pending_callgraph_store_force_token();
        if force_token.is_none() {
            if let Some(store) = {
                let guard = self
                    .callgraph_store
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                guard.as_ref().map(Arc::clone)
            } {
                self.schedule_legacy_callgraph_migration_if_needed(
                    store.as_ref(),
                    store.project_root().to_path_buf(),
                    self.callgraph_store_dir(),
                );
                return Ok(Some(store));
            }
        }

        let Some(project_root) = self.callgraph_project_root() else {
            return Ok(None);
        };
        let callgraph_dir = self.callgraph_store_dir();

        // Preserve a readable legacy fallback while writer-capable processes
        // migrate it on the cold-build lane. Opening before the writer path is
        // also the cheap fast path for an already-published root generation.
        if force_token.is_none() {
            if let Some(store) =
                CallGraphStore::open_readonly(callgraph_dir.clone(), project_root.clone())?
            {
                let store = Arc::new(store);
                {
                    let mut guard = self
                        .callgraph_store
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    *guard = Some(Arc::clone(&store));
                }
                self.schedule_legacy_callgraph_migration_if_needed(
                    store.as_ref(),
                    project_root,
                    callgraph_dir,
                );
                return Ok(Some(store));
            }
        }

        if !self.callgraph_writer() {
            return Ok(None);
        }
        let build_generation = self.configure_generation();
        let persist_epoch_flag = self.callgraph_persist_epoch_flag();
        let Some(persist_epoch) = self
            .run_if_subc_bound_generation(build_generation, || self.next_callgraph_persist_epoch())
        else {
            return Ok(None);
        };
        let files = crate::callgraph::walk_project_files(&project_root).collect::<Vec<_>>();
        let (store, _stats) = crate::callgraph_store::with_publish_epoch(
            persist_epoch_flag.clone(),
            persist_epoch,
            || {
                if force_token.is_some() {
                    CallGraphStore::force_cold_build_with_lease_chunked(
                        callgraph_dir.clone(),
                        project_root.clone(),
                        &files,
                        self.config().callgraph_chunk_size,
                    )
                    .map(|(store, _stats)| (store, ()))
                } else {
                    CallGraphStore::ensure_built_with_lease_chunked(
                        callgraph_dir.clone(),
                        project_root.clone(),
                        &files,
                        self.config().callgraph_chunk_size,
                    )
                    .map(|(store, _stats)| (store, ()))
                }
            },
        )?;
        drop(store);

        let Some(store) = CallGraphStore::open_readonly(callgraph_dir, project_root)? else {
            return Ok(None);
        };
        let store = Arc::new(store);
        self.run_if_subc_bound_generation(build_generation, || {
            if persist_epoch_flag.current() != persist_epoch {
                return None;
            }
            let mut guard = self
                .callgraph_store
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *guard = Some(Arc::clone(&store));
            if let Some(force_token) = force_token {
                self.fulfill_callgraph_store_force_token(force_token);
            }
            Some(Arc::clone(&store))
        })
        .flatten()
        .map_or(Ok(None), |store| Ok(Some(store)))
    }

    /// Resolve the project root used for the callgraph store: prefer the
    /// canonical cache root, falling back to the configured project root.
    pub fn callgraph_project_root(&self) -> Option<PathBuf> {
        self.canonical_cache_root_opt().or_else(|| {
            self.config()
                .project_root
                .clone()
                .map(|root| std::fs::canonicalize(&root).unwrap_or(root))
        })
    }

    /// Drop a cached reader when another process published a newer generation.
    /// The next access reopens through the pointer and converges to that
    /// generation instead of serving a stale long-lived connection.
    pub fn revalidate_callgraph_store_generation(&self) {
        let (superseded, legacy_fallback) = {
            let guard = self
                .callgraph_store
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard
                .as_ref()
                .map(|store| (!store.is_current(), store.is_legacy_fallback()))
                .unwrap_or((false, false))
        };
        if !superseded {
            return;
        }
        // A local migration publishes its pointer just before sending the new
        // store to the main-loop drain. Keep queries on the fallback during that
        // narrow handoff instead of reporting a transient Building state.
        if legacy_fallback && self.callgraph_store_rx.lock().is_some() {
            return;
        }
        let mut guard = self
            .callgraph_store
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = None;
    }

    pub fn callgraph_store_for_ops(&self) -> CallgraphStoreAccess {
        if !self.heavy_root_work_allowed() {
            return CallgraphStoreAccess::Unavailable;
        }
        let operation_generation = self.configure_generation();

        // Converge to a newer generation another process (or a local cold
        // rebuild) may have published: if our resident store is superseded, drop
        // it so the open path below reopens via the pointer. Cheap pointer read.
        self.revalidate_callgraph_store_generation();
        let force_token = self.pending_callgraph_store_force_token();
        if force_token.is_none() {
            if let Some(store) = {
                let guard = self
                    .callgraph_store
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                guard.as_ref().map(Arc::clone)
            } {
                self.schedule_legacy_callgraph_migration_if_needed(
                    store.as_ref(),
                    store.project_root().to_path_buf(),
                    self.callgraph_store_dir(),
                );
                return CallgraphStoreAccess::Ready(store);
            }
        }

        // A background build is already running; don't start a second one.
        if self.callgraph_store_rx.lock().is_some() {
            return CallgraphStoreAccess::Building;
        }

        let Some(project_root) = self.callgraph_project_root() else {
            return CallgraphStoreAccess::Unavailable;
        };
        let callgraph_dir = self.callgraph_store_dir();

        if force_token.is_none() {
            match CallGraphStore::open_readonly(callgraph_dir.clone(), project_root.clone()) {
                Ok(Some(store)) => {
                    let store = Arc::new(store);
                    let installed = self.run_if_subc_bound_generation(operation_generation, || {
                        let mut guard = self
                            .callgraph_store
                            .write()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        *guard = Some(Arc::clone(&store));
                        Arc::clone(&store)
                    });
                    let Some(store) = installed else {
                        return CallgraphStoreAccess::Unavailable;
                    };
                    self.schedule_legacy_callgraph_migration_if_needed(
                        store.as_ref(),
                        project_root.clone(),
                        callgraph_dir.clone(),
                    );
                    return CallgraphStoreAccess::Ready(store);
                }
                Ok(None) => {
                    if !self.callgraph_writer() {
                        return CallgraphStoreAccess::Unavailable;
                    }
                }
                Err(error) => {
                    if !self.callgraph_writer() {
                        return CallgraphStoreAccess::Unavailable;
                    }
                    crate::slog_warn!(
                        "callgraph read-only open failed before writer promotion: {}",
                        error
                    );
                }
            }
        } else if !self.callgraph_writer() {
            return CallgraphStoreAccess::Unavailable;
        }

        if self.semantic_cold_seed_active() {
            self.defer_callgraph_store_warm_for_semantic_cold_seed();
            return CallgraphStoreAccess::Building;
        }

        // Cold build required: run it off the request thread and return
        // `Building` so the agent retries (the watcher keeps the store fresh
        // once it lands). By default this never blocks the request thread.
        //
        // `AFT_CALLGRAPH_BUILD_WAIT_MS` (default 0) optionally waits a bounded
        // window inline for the build to land before returning `Building`; tests
        // set it large so fixture builds resolve to `Ready` synchronously.
        let work = if let Some(force_token) = force_token {
            CallgraphBackgroundWork::ForceRebuild(force_token)
        } else {
            CallgraphBackgroundWork::Ensure
        };
        if !self.spawn_callgraph_store_cold_build(project_root.clone(), callgraph_dir.clone(), work)
        {
            return CallgraphStoreAccess::Building;
        }

        let wait = callgraph_build_wait_window();
        if !wait.is_zero() {
            let (received, receiver_generation, receiver_epoch) = {
                let rx_ref = self.callgraph_store_rx.lock();
                let Some(rx) = rx_ref.as_ref() else {
                    return CallgraphStoreAccess::Building;
                };
                (
                    rx.recv_timeout(wait),
                    self.callgraph_store_rx_generation(),
                    self.callgraph_store_rx_epoch(),
                )
            };
            match received {
                Ok(CallGraphStoreBuildEvent::Ready {
                    store,
                    fulfilled_force_token,
                    publication_epoch,
                }) => {
                    if self.callgraph_persist_epoch_flag().current() != publication_epoch {
                        // Superseded publication: a newer configure owns the
                        // pointer. Clear the receiver and report Building so the
                        // replacement build's event installs instead.
                        drop(store);
                        let _ = self.with_current_callgraph_store_rx(
                            receiver_generation,
                            receiver_epoch,
                            |receiver| {
                                *receiver = None;
                            },
                        );
                        return CallgraphStoreAccess::Building;
                    }
                    // The completed build owns the writer lease until dropped;
                    // release it before reopening the published generation.
                    remove_callgraph_pointer_before_inline_reopen_for_test(&callgraph_dir, &store);
                    drop(store);
                    let reopened =
                        CallGraphStore::open_readonly(callgraph_dir.clone(), project_root.clone());
                    let mut pending = Vec::new();
                    let outcome = self.with_current_callgraph_store_rx(
                        receiver_generation,
                        receiver_epoch,
                        |receiver| {
                            *receiver = None;
                            match reopened {
                                Ok(Some(store)) => {
                                    let ready = Arc::new(store);
                                    pending = self.take_pending_callgraph_store_paths();
                                    *self
                                        .callgraph_store
                                        .write()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                        Some(Arc::clone(&ready));
                                    if let Some(force_token) = fulfilled_force_token {
                                        self.fulfill_callgraph_store_force_token(force_token);
                                    }
                                    CallgraphStoreAccess::Ready(ready)
                                }
                                Ok(None) => CallgraphStoreAccess::Building,
                                Err(error) => CallgraphStoreAccess::Error(error),
                            }
                        },
                    );
                    let Some(outcome) = outcome else {
                        return if self.subc_unbound_quiesced()
                            || self.configure_generation() != receiver_generation
                        {
                            CallgraphStoreAccess::Unavailable
                        } else {
                            CallgraphStoreAccess::Building
                        };
                    };
                    if !pending.is_empty() {
                        let _ = self.enqueue_callgraph_store_refresh(pending);
                    }
                    if matches!(&outcome, CallgraphStoreAccess::Ready(_)) {
                        let _ = self.request_tier2_refresh_pull();
                    }
                    return outcome;
                }
                Ok(CallGraphStoreBuildEvent::Settled) => {
                    let _ = self.with_current_callgraph_store_rx(
                        receiver_generation,
                        receiver_epoch,
                        |receiver| *receiver = None,
                    );
                    return CallgraphStoreAccess::Building;
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    let _ = self.with_current_callgraph_store_rx(
                        receiver_generation,
                        receiver_epoch,
                        |receiver| *receiver = None,
                    );
                }
            }
        }
        CallgraphStoreAccess::Building
    }

    fn schedule_legacy_callgraph_migration_if_needed(
        &self,
        store: &ReadonlyCallGraphStore,
        project_root: PathBuf,
        callgraph_dir: PathBuf,
    ) {
        if !store.is_legacy_fallback()
            || !self.callgraph_writer()
            || !self.heavy_root_work_allowed()
        {
            return;
        }
        if self.semantic_cold_seed_active() {
            self.defer_callgraph_store_warm_for_semantic_cold_seed();
            return;
        }
        let _ = self.spawn_callgraph_store_cold_build(
            project_root,
            callgraph_dir,
            CallgraphBackgroundWork::LegacyMigration,
        );
    }

    fn configured_callgraph_keys(&self, current_root: &Path) -> BTreeSet<String> {
        let mut roots = self
            .configured_session_roots
            .lock()
            .iter()
            .map(|(root, _session)| root.clone())
            .collect::<BTreeSet<_>>();
        roots.insert(current_root.to_path_buf());
        roots
            .iter()
            .map(|root| crate::search_index::artifact_cache_key(root))
            .collect()
    }

    /// Atomically mark root-keyed callgraph maintenance in flight and spawn it
    /// on the cold-build lane. The same receiver/install path handles cold
    /// builds and legacy migrations, so watcher edits are queued and replayed
    /// against whichever root-keyed generation publishes.
    fn spawn_callgraph_store_cold_build(
        &self,
        project_root: PathBuf,
        callgraph_dir: PathBuf,
        work: CallgraphBackgroundWork,
    ) -> bool {
        if !self.heavy_root_work_allowed() || !self.callgraph_writer() {
            return false;
        }
        let generation = self.configure_generation();
        self.run_if_subc_bound_generation(generation, || {
            self.spawn_callgraph_store_cold_build_admitted(project_root, callgraph_dir, work)
        })
        .unwrap_or(false)
    }

    /// Start a callgraph worker after lifecycle admission has been acquired.
    fn spawn_callgraph_store_cold_build_admitted(
        &self,
        project_root: PathBuf,
        callgraph_dir: PathBuf,
        work: CallgraphBackgroundWork,
    ) -> bool {
        let session_id = crate::log_ctx::current_session();
        let chunk_size = self.config().callgraph_chunk_size;
        let build_generation = self.configure_generation();
        let generation_flag = self.configure_generation_flag();
        let configured_keys = self.configured_callgraph_keys(&project_root);
        let summary_logged = Arc::clone(&self.callgraph_legacy_migration_summary_logged);

        let mut rx_guard = self.callgraph_store_rx.lock();
        if rx_guard.is_some() {
            return false;
        }

        let Some(permit) = crate::cold_build_limiter::try_acquire() else {
            crate::slog_info!(
                "callgraph store background work deferred by cold build limit ({})",
                crate::cold_build_limiter::limit()
            );
            return false;
        };

        let force_token = match work {
            CallgraphBackgroundWork::ForceRebuild(token) => Some(token),
            CallgraphBackgroundWork::Ensure | CallgraphBackgroundWork::LegacyMigration => None,
        };
        let (tx, rx) = crossbeam_channel::unbounded::<CallGraphStoreBuildEvent>();
        self.note_callgraph_store_rx_generation(build_generation);
        self.next_callgraph_store_rx_epoch();
        *rx_guard = Some(rx);
        let persist_epoch = self.next_callgraph_persist_epoch();
        let persist_epoch_flag = self.callgraph_persist_epoch_flag();

        CALLGRAPH_COLD_BUILD_SPAWN_COUNT.fetch_add(1, Ordering::SeqCst);

        std::thread::spawn(move || {
            let _permit = permit;
            let mut settlement = CallGraphStoreBuildSettlement::new(tx, force_token, persist_epoch);
            crate::log_ctx::with_session(session_id, || {
                wait_on_callgraph_build_start_gate(&project_root);
                if persist_epoch_flag.current() != persist_epoch {
                    crate::slog_info!(
                        "callgraph store background work skipped for superseded epoch {}",
                        persist_epoch
                    );
                    return;
                }
                let built = crate::callgraph_store::with_publish_epoch(
                    persist_epoch_flag,
                    persist_epoch,
                    || match work {
                        CallgraphBackgroundWork::LegacyMigration => {
                            CallGraphStore::migrate_legacy_with_lease(
                                callgraph_dir.clone(),
                                project_root.clone(),
                            )
                        }
                        CallgraphBackgroundWork::ForceRebuild(_) => {
                            let files = crate::callgraph::walk_project_files(&project_root)
                                .collect::<Vec<_>>();
                            CallGraphStore::force_cold_build_with_lease_chunked(
                                callgraph_dir.clone(),
                                project_root.clone(),
                                &files,
                                chunk_size,
                            )
                            .map(|(store, _)| Some(store))
                        }
                        CallgraphBackgroundWork::Ensure => {
                            let files = crate::callgraph::walk_project_files(&project_root)
                                .collect::<Vec<_>>();
                            CallGraphStore::ensure_built_with_lease_chunked(
                                callgraph_dir.clone(),
                                project_root.clone(),
                                &files,
                                chunk_size,
                            )
                            .map(|(store, _)| Some(store))
                        }
                    },
                );
                match built {
                    Ok(Some(store)) => {
                        if store.is_legacy_migration() {
                            match crate::callgraph_store::all_legacy_partitions_migrated_for_keys(
                                &callgraph_dir,
                                &configured_keys,
                            ) {
                                Ok(true)
                                    if summary_logged
                                        .compare_exchange(
                                            false,
                                            true,
                                            Ordering::SeqCst,
                                            Ordering::SeqCst,
                                        )
                                        .is_ok() =>
                                {
                                    crate::slog_info!(
                                        "all legacy callgraph partitions migrated for configured roots"
                                    );
                                }
                                Ok(_) => {}
                                Err(error) => crate::slog_warn!(
                                    "failed to inspect legacy callgraph migration completion: {}",
                                    error
                                ),
                            }
                        }
                        if generation_flag.load(Ordering::SeqCst) == build_generation {
                            settlement.ready(store);
                        } else {
                            crate::slog_info!(
                                "callgraph store warm build result discarded for stale generation {}",
                                build_generation
                            );
                        }
                    }
                    Ok(None) => {}
                    Err(crate::callgraph_store::CallGraphStoreError::Superseded) => {
                        crate::slog_info!(
                            "callgraph store disk publication skipped for superseded epoch {}",
                            persist_epoch
                        );
                    }
                    Err(error) => {
                        crate::slog_warn!("callgraph store background work failed: {}", error);
                    }
                }
            });
        });
        true
    }

    /// Access the callgraph-store background-build receiver (drained by the
    /// main loop once the cold build completes).
    pub fn callgraph_store_rx(
        &self,
    ) -> &parking_lot::Mutex<Option<crossbeam_channel::Receiver<CallGraphStoreBuildEvent>>> {
        &self.callgraph_store_rx
    }

    /// Commit a dequeued result only while its lifecycle and receiver identity
    /// remain current. Lifecycle admission is intentionally acquired first,
    /// matching worker-start paths and preventing a lock-order cycle.
    #[doc(hidden)]
    pub fn with_current_callgraph_store_rx<R>(
        &self,
        generation: u64,
        epoch: u64,
        action: impl FnOnce(&mut Option<crossbeam_channel::Receiver<CallGraphStoreBuildEvent>>) -> R,
    ) -> Option<R> {
        self.run_if_subc_bound_generation(generation, || {
            let mut receiver = self.callgraph_store_rx.lock();
            if receiver.is_none()
                || self.callgraph_store_rx_generation() != generation
                || self.callgraph_store_rx_epoch() != epoch
            {
                return None;
            }
            Some(action(&mut receiver))
        })
        .flatten()
    }

    pub(crate) fn retire_callgraph_store_rx(&self) {
        let mut receiver = self.callgraph_store_rx.lock();
        *receiver = None;
        self.next_callgraph_store_rx_epoch();
    }

    pub(crate) fn note_callgraph_store_rx_generation(&self, generation: u64) {
        self.callgraph_store_rx_generation
            .store(generation, Ordering::SeqCst);
    }

    #[doc(hidden)]
    pub fn callgraph_store_rx_generation(&self) -> u64 {
        self.callgraph_store_rx_generation.load(Ordering::SeqCst)
    }

    pub(crate) fn next_callgraph_store_rx_epoch(&self) -> u64 {
        self.callgraph_store_rx_epoch
            .fetch_add(1, Ordering::SeqCst)
            .wrapping_add(1)
    }

    #[doc(hidden)]
    pub fn callgraph_store_rx_epoch(&self) -> u64 {
        self.callgraph_store_rx_epoch.load(Ordering::SeqCst)
    }

    pub(crate) fn next_callgraph_persist_epoch(&self) -> u64 {
        self.callgraph_persist_epoch.next()
    }

    #[doc(hidden)]
    pub fn callgraph_persist_epoch_flag(&self) -> crate::root_cache::ArtifactPublishEpoch {
        self.callgraph_persist_epoch.clone()
    }

    /// Record source-file paths that could not be applied to the writable store
    /// so the next ready-store replay can refresh them.
    pub fn add_pending_callgraph_store_paths<I>(&self, paths: I)
    where
        I: IntoIterator<Item = PathBuf>,
    {
        self.pending_callgraph_store_paths.lock().extend(paths);
    }

    pub fn enqueue_callgraph_store_refresh<I>(&self, paths: I) -> bool
    where
        I: IntoIterator<Item = PathBuf>,
    {
        let generation = self.configure_generation();
        self.enqueue_callgraph_store_refresh_for_generation(paths, generation)
    }

    pub(crate) fn enqueue_callgraph_store_refresh_for_generation<I>(
        &self,
        paths: I,
        generation: u64,
    ) -> bool
    where
        I: IntoIterator<Item = PathBuf>,
    {
        let paths = paths.into_iter().collect::<Vec<_>>();
        if paths.is_empty() {
            return true;
        }
        self.run_if_subc_bound_generation(generation, || {
            if !self.callgraph_writer() {
                self.add_pending_callgraph_store_paths(paths);
                return false;
            }
            let Some(project_root) = self.callgraph_project_root() else {
                self.add_pending_callgraph_store_paths(paths);
                return false;
            };

            // The ticket fences the batch against lifecycle transitions and
            // cold-build publications: a superseded batch defers its paths to
            // the pending sink instead of committing into a store generation
            // that a newer configure no longer owns.
            let ticket = crate::callgraph_store::CallgraphRefreshTicket::new(
                self.subc_lifecycle_admission(),
                self.configure_generation_flag(),
                generation,
                self.callgraph_persist_epoch_flag(),
                self.callgraph_persist_epoch_flag().current(),
            );
            crate::callgraph_store::enqueue_callgraph_store_refresh_fenced(
                self.callgraph_store_dir(),
                project_root,
                paths,
                Arc::clone(&self.pending_callgraph_store_paths),
                ticket,
            )
        })
        .unwrap_or(false)
    }

    /// Take and clear paths waiting for a ready writable store.
    ///
    /// Paths outside the current project root are dropped: the pending sink is
    /// shared with detached refresh batches, so a batch superseded by a root
    /// change can defer paths from the PREVIOUS root after configure cleared
    /// the sink. Replaying those would index foreign files into the new root's
    /// store (refresh accepts absolute out-of-root paths).
    pub fn take_pending_callgraph_store_paths(&self) -> Vec<PathBuf> {
        let roots: Vec<PathBuf> = [
            self.canonical_cache_root_opt(),
            self.config().project_root.clone(),
        ]
        .into_iter()
        .flatten()
        .collect();
        std::mem::take(&mut *self.pending_callgraph_store_paths.lock())
            .into_iter()
            .filter(|path| {
                let in_root = pending_path_in_roots(path, &roots);
                if !in_root {
                    crate::slog_debug!(
                        "dropping pending callgraph path outside current root: {}",
                        path.display()
                    );
                }
                in_root
            })
            .collect()
    }

    /// Access the search index.
    pub fn search_index(&self) -> &RwLock<Option<SearchIndex>> {
        &self.search_index
    }

    /// Access the search-index build receiver.
    pub fn search_index_rx(&self) -> &RwLock<Option<crossbeam_channel::Receiver<SearchIndex>>> {
        &self.search_index_rx
    }

    pub(crate) fn install_search_index_rx(
        &self,
        receiver: crossbeam_channel::Receiver<SearchIndex>,
        generation: u64,
    ) -> u64 {
        let mut slot = self
            .search_index_rx
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.note_search_index_rx_generation(generation);
        let epoch = self.next_search_index_rx_epoch();
        *slot = Some(receiver);
        epoch
    }

    pub(crate) fn search_index_rx_terminal_guard(&self, epoch: u64) -> ReceiverTerminalGuard {
        ReceiverTerminalGuard::new(Arc::clone(&self.search_index_rx_terminal_epoch), epoch)
    }

    /// Keep generation/epoch validation and receiver mutation under the same
    /// lock used by receiver installation.
    pub(crate) fn with_current_search_index_rx<R>(
        &self,
        generation: u64,
        epoch: u64,
        action: impl FnOnce(&mut Option<crossbeam_channel::Receiver<SearchIndex>>) -> R,
    ) -> Option<R> {
        self.run_if_subc_bound_generation(generation, || {
            let mut receiver = self
                .search_index_rx
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if receiver.is_none()
                || self.search_index_rx_generation() != generation
                || self.search_index_rx_epoch() != epoch
            {
                return None;
            }
            Some(action(&mut receiver))
        })
        .flatten()
    }

    pub(crate) fn retire_search_index_rx(&self) {
        let mut receiver = self
            .search_index_rx
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *receiver = None;
        self.next_search_index_rx_epoch();
    }

    pub(crate) fn note_search_index_rx_generation(&self, generation: u64) {
        self.search_index_rx_generation
            .store(generation, Ordering::SeqCst);
    }

    pub(crate) fn search_index_rx_generation(&self) -> u64 {
        self.search_index_rx_generation.load(Ordering::SeqCst)
    }

    pub(crate) fn next_search_index_rx_epoch(&self) -> u64 {
        self.search_index_rx_epoch
            .fetch_add(1, Ordering::SeqCst)
            .wrapping_add(1)
    }

    pub(crate) fn search_index_rx_epoch(&self) -> u64 {
        self.search_index_rx_epoch.load(Ordering::SeqCst)
    }

    /// Allow one automatic search-index replacement load per configure
    /// generation. The drain disconnect path calls this before rescheduling a
    /// load whose worker exited without delivering an index; capping it at one
    /// prevents a persistently failing worker from being relaunched in a loop on
    /// the drain thread. After the cap is hit, the query-triggered reload
    /// (`trigger_search_index_reload_if_evicted`) remains the recovery path.
    pub(crate) fn allow_search_index_disconnect_reschedule(&self) -> bool {
        const MAX_REPLACEMENTS_PER_GENERATION: u32 = 1;
        let generation = self.configure_generation();
        let mut state = self.search_index_disconnect_reschedule.lock();
        if state.0 != generation {
            *state = (generation, 0);
        }
        if state.1 >= MAX_REPLACEMENTS_PER_GENERATION {
            return false;
        }
        state.1 += 1;
        true
    }

    pub(crate) fn next_search_persist_epoch(&self) -> u64 {
        self.search_persist_epoch.next()
    }

    pub(crate) fn search_persist_epoch_flag(&self) -> crate::root_cache::ArtifactPublishEpoch {
        self.search_persist_epoch.clone()
    }

    pub fn add_pending_search_index_paths<I>(&self, paths: I)
    where
        I: IntoIterator<Item = PathBuf>,
    {
        let paths = paths.into_iter().collect::<Vec<_>>();
        if !paths.is_empty() {
            self.invalidate_warm_verify_memo();
            self.pending_search_index_paths.lock().extend(paths);
        }
    }

    pub fn take_pending_search_index_paths(&self) -> Vec<PathBuf> {
        std::mem::take(&mut *self.pending_search_index_paths.lock())
            .into_iter()
            .collect()
    }

    pub fn add_pending_semantic_index_paths<I>(&self, paths: I)
    where
        I: IntoIterator<Item = PathBuf>,
    {
        let paths = paths.into_iter().collect::<Vec<_>>();
        if !paths.is_empty() {
            self.invalidate_warm_verify_memo();
            self.pending_semantic_index_paths.lock().extend(paths);
        }
    }

    pub(crate) fn invalidate_warm_verify_memo(&self) {
        if let Some(root) = self.canonical_cache_root_opt() {
            crate::cache_freshness::invalidate_verify_memo(&root);
        }
    }

    pub fn take_pending_semantic_index_paths(&self) -> Vec<PathBuf> {
        std::mem::take(&mut *self.pending_semantic_index_paths.lock())
            .into_iter()
            .collect()
    }

    pub fn mark_pending_semantic_corpus_refresh(&self) {
        *self.pending_semantic_corpus_refresh.lock() = true;
    }

    pub fn take_pending_semantic_corpus_refresh(&self) -> bool {
        std::mem::take(&mut *self.pending_semantic_corpus_refresh.lock())
    }

    pub fn clear_pending_index_updates(&self) {
        self.pending_search_index_paths.lock().clear();
        self.pending_callgraph_store_paths.lock().clear();
        self.pending_tier2_paths.lock().clear();
        self.pending_semantic_index_paths.lock().clear();
        *self.pending_semantic_corpus_refresh.lock() = false;
    }

    /// Take the retained pending reconciliation state for a transactional
    /// teardown. The caller commits the disposal by dropping the returned
    /// state after eviction succeeds, or restores it with
    /// [`Self::restore_pending_reconciliation_state`] when eviction is blocked
    /// by a secondary blocker (running bash, in-flight builds): the paths are
    /// the only repair record for consumed watcher events, and the root may
    /// rebind before the next reap attempt.
    pub(crate) fn take_pending_reconciliation_state(&self) -> PendingReconciliationState {
        PendingReconciliationState {
            search: std::mem::take(&mut *self.pending_search_index_paths.lock()),
            callgraph: std::mem::take(&mut *self.pending_callgraph_store_paths.lock()),
            tier2: std::mem::take(&mut *self.pending_tier2_paths.lock()),
            semantic: std::mem::take(&mut *self.pending_semantic_index_paths.lock()),
            corpus_refresh: std::mem::take(&mut *self.pending_semantic_corpus_refresh.lock()),
        }
    }

    pub(crate) fn restore_pending_reconciliation_state(&self, state: PendingReconciliationState) {
        self.pending_search_index_paths.lock().extend(state.search);
        self.pending_callgraph_store_paths
            .lock()
            .extend(state.callgraph);
        self.pending_tier2_paths.lock().extend(state.tier2);
        self.pending_semantic_index_paths
            .lock()
            .extend(state.semantic);
        if state.corpus_refresh {
            *self.pending_semantic_corpus_refresh.lock() = true;
        }
    }

    /// Cancel artifact work that no longer has a bound daemon route to consume it.
    /// `mark_subc_unbound` advances the generation under the lifecycle admission
    /// gate before this cleanup runs. Clearing receivers lets a later rebind
    /// schedule fresh work instead of adopting a disconnected worker forever.
    ///
    /// Pending watcher-derived path sets are RETAINED: a pre-unbind artifact
    /// worker may legitimately finish generation-safe disk persistence during
    /// the unbound window (content generation and persist epochs deliberately
    /// do not advance on route teardown), and those paths are the only record
    /// that its artifact is content-stale. Rebind replays them. Disposal of
    /// pending state belongs to non-equivalent configure and TTL eviction
    /// (transactional take in the TTL reaper), whose strict invalidation
    /// subsumes their purpose.
    pub(crate) fn cancel_unbound_artifact_work(&self) {
        // A cancelled non-ready search corpus refresh left the resident index
        // marked not-ready; retiring its receiver alone would strand it
        // (equivalent rebind only reloads a MISSING index). Drop the resident
        // index too so the rebind's artifact setup reloads from disk and the
        // retained pending paths repair it on install.
        let search_refresh_cancelled = self
            .search_index_rx
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some();
        self.retire_search_index_rx();
        if search_refresh_cancelled {
            let mut resident = self
                .search_index
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if resident.as_ref().is_some_and(|index| !index.ready) {
                *resident = None;
            }
        }
        self.retire_callgraph_store_rx();
        let semantic_cancelled = self.semantic_index_rx.lock().is_some();
        self.retire_semantic_index_rx();
        let semantic_refresh_cancelled = self.semantic_refresh_event_rx.lock().is_some();
        self.clear_semantic_refresh_worker();
        self.reset_semantic_cold_seed_gate_for_configure();
        let _ = self.inspect_manager.discard_completions();
        let _ = self.take_new_reuse_completions();
        if semantic_cancelled || semantic_refresh_cancelled {
            let has_index = self
                .semantic_index
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some();
            // In-flight refreshing files were consumed from the watcher; the
            // cancelled worker will never re-embed them. Transfer them to the
            // retained pending set so the rebind's replacement worker does.
            {
                let mut status = self
                    .semantic_index_status
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let refreshing = status.take_refreshing_files();
                if !refreshing.is_empty() {
                    self.pending_semantic_index_paths.lock().extend(refreshing);
                }
                if status.corpus_refresh_in_flight() {
                    *self.pending_semantic_corpus_refresh.lock() = true;
                }
                *status = if has_index {
                    SemanticIndexStatus::ready()
                } else {
                    SemanticIndexStatus::Disabled
                };
            }
        }
    }

    /// Gate every watcher-maintained artifact after the last route detaches. Files
    /// may change before the watcher is restored, so a later bind must reconcile
    /// from disk instead of serving retained snapshots that missed those edits.
    pub(crate) fn invalidate_artifacts_after_watcher_gap(&self) {
        self.next_search_persist_epoch();
        self.next_semantic_persist_epoch();
        self.next_callgraph_persist_epoch();

        self.search_index
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        self.semantic_index
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        self.callgraph_store
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        // Keep semantic status reloadable when the feature is enabled: the
        // query path's self-healing reload only fires from Ready (or Failed on
        // read-only roots), so Disabled would strand an already-bound root
        // with no way back short of a reconfigure. The advanced persist epoch
        // and strict verify memo force the reload to re-verify from disk.
        *self
            .semantic_index_status
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = if self.config().semantic_search {
            SemanticIndexStatus::ready()
        } else {
            SemanticIndexStatus::Disabled
        };
        // A force token is only fulfillable by a local writer build; read-only
        // roots follow the owner's published pointer and would be stuck
        // permanently unavailable behind an unfulfillable token.
        if self.callgraph_writer() {
            self.mark_callgraph_store_force_rebuild();
        }

        if let Some(root) = self
            .canonical_cache_root_opt()
            .or_else(|| self.config().project_root.clone())
        {
            crate::cache_freshness::invalidate_verify_memo_strict(&root);
        }
        self.borrowed_index_cache.lock().clear();
        self.inspect_manager.evict_idle_caches();
        self.reset_symbol_cache();
        self.clear_tsconfig_membership_cache();
    }

    fn drain_search_index_events_for_graceful_shutdown(&self) {
        crate::runtime_drain::drain_watcher_events(self);
        crate::runtime_drain::drain_search_index_events(self);
    }

    fn search_index_build_in_progress(&self) -> bool {
        self.search_index_rx()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }

    /// Graceful EOF teardown can afford a bounded wait for an already running
    /// search rebuild to publish. Poll the observable receiver state
    /// directly instead of relying on fixed sleeps in callers or tests.
    fn wait_for_search_index_build_to_settle_on_graceful_shutdown(&self) {
        crate::runtime_drain::note_search_rebuild_shutdown_wait_for_test();
        let deadline = Instant::now() + GRACEFUL_SHUTDOWN_SEARCH_BUILD_WAIT;
        while self.search_index_build_in_progress() && Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            std::thread::sleep(remaining.min(GRACEFUL_SHUTDOWN_SEARCH_BUILD_POLL));
            self.drain_search_index_events_for_graceful_shutdown();
        }
    }

    /// Flush the owner-side trigram delta during an orderly transport shutdown.
    /// EOF/Goodbye teardown uses this best-effort path; signal and panic exits
    /// intentionally skip it so abrupt shutdown never waits on slow recovery work.
    #[doc(hidden)]
    pub fn flush_search_index_on_graceful_shutdown(&self) -> bool {
        if self.shared_artifacts_read_only() {
            return false;
        }

        self.drain_search_index_events_for_graceful_shutdown();
        if self.search_index_build_in_progress() {
            self.wait_for_search_index_build_to_settle_on_graceful_shutdown();
            self.drain_search_index_events_for_graceful_shutdown();
        }

        if self.search_index_build_in_progress() {
            return false;
        }

        let Some(canonical_root) = self.canonical_cache_root_opt() else {
            return false;
        };
        let config = self.config();
        let project_key = self.memoized_artifact_cache_key(&canonical_root);
        let cache_dir = crate::search_index::resolve_cache_dir_with_key(
            &project_key,
            config.storage_dir.as_deref(),
        );

        {
            let search_index = self
                .search_index()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(index) = search_index.as_ref() else {
                return false;
            };
            if !index.ready || !index.has_pending_disk_changes() {
                return false;
            }
        }

        let _cache_lock = match crate::search_index::CacheLock::try_acquire_for_shutdown(
            &cache_dir,
            &canonical_root,
        ) {
            Ok(lock) => lock,
            Err(error) => {
                crate::slog_warn!(
                    "search index: skipped shutdown flush because cache lock was unavailable: {}",
                    error
                );
                return false;
            }
        };

        let mut search_index = self
            .search_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(index) = search_index.as_mut() else {
            return false;
        };
        if !index.ready || !index.has_pending_disk_changes() {
            return false;
        }

        let git_head = index.stored_git_head().map(str::to_owned);
        index.write_to_disk(&cache_dir, git_head.as_deref())
    }

    pub fn inspect_manager(&self) -> Arc<InspectManager> {
        Arc::clone(&self.inspect_manager)
    }

    pub fn add_pending_tier2_paths<I>(&self, paths: I)
    where
        I: IntoIterator<Item = PathBuf>,
    {
        self.pending_tier2_paths.lock().extend(paths);
    }

    pub fn pending_tier2_paths(&self) -> Vec<PathBuf> {
        self.pending_tier2_paths.lock().iter().cloned().collect()
    }

    pub fn remove_pending_tier2_paths<I>(&self, paths: I)
    where
        I: IntoIterator<Item = PathBuf>,
    {
        let mut pending = self.pending_tier2_paths.lock();
        for path in paths {
            pending.remove(&path);
        }
    }

    /// Returns true when one or more watcher-driven (reuse-path) Tier-2 scans
    /// have completed since the last call, advancing the last-seen marker. The
    /// per-request inspect drain uses this to refresh the status bar after a
    /// background scan — those completions bypass `drain_completions`.
    /// Peek variant of `take_new_reuse_completions`: reports whether new reuse
    /// completions exist WITHOUT consuming the observation, so the maintenance
    /// scheduler's skip probe cannot swallow a status-bar refresh.
    pub fn has_new_reuse_completions(&self) -> bool {
        self.inspect_manager.reuse_completion_count()
            != self.last_seen_reuse_completions.load(Ordering::SeqCst)
    }

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
            .lock()
            .reset_after_configure(now);
    }

    pub fn request_tier2_refresh_pull(&self) -> bool {
        let can_schedule = self.inspect_writer()
            && self.heavy_root_work_allowed()
            && self.inspect_manager.automatic_tier2_refresh_allowed();
        self.tier2_refresh_scheduler
            .lock()
            .request_pull(can_schedule)
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
        let can_write = self.inspect_writer()
            && self.heavy_root_work_allowed()
            && manager.automatic_tier2_refresh_allowed();
        let in_flight = manager.tier2_any_in_flight();
        let semantic_cold_seed_active = self.semantic_cold_seed_active();
        let decision = self.tier2_refresh_scheduler.lock().tick_with_semantic_gate(
            now,
            changed_path_count,
            can_write,
            in_flight,
            semantic_cold_seed_active,
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
            .lock()
            .note_external_scan_started(now);
    }

    pub fn tier2_trigger_reason(&self) -> Option<&'static str> {
        self.tier2_refresh_scheduler
            .lock()
            .last_trigger_reason()
            .map(Tier2TriggerReason::as_str)
    }

    #[doc(hidden)]
    pub fn tier2_pull_demand_pending(&self) -> bool {
        self.tier2_refresh_scheduler.lock().pull_demand_pending()
    }

    fn start_tier2_refresh(&self, reason: Tier2TriggerReason, manager: Arc<InspectManager>) {
        let generation = self.configure_generation();
        if !self.inspect_writer()
            || !self.heavy_root_work_allowed()
            || !manager.automatic_tier2_refresh_allowed()
            || !self.config().inspect.enabled
        {
            return;
        }
        let _ = self.run_if_subc_bound_generation(generation, || {
            self.start_tier2_refresh_admitted(reason, manager);
        });
    }

    fn start_tier2_refresh_admitted(
        &self,
        reason: Tier2TriggerReason,
        manager: Arc<InspectManager>,
    ) {
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
        if !submission.deferred_categories.is_empty() {
            self.tier2_refresh_scheduler.lock().note_dispatch_deferred();
            crate::slog_info!(
                "tier2 refresh deferred by cold build limit: categories={:?}",
                submission
                    .deferred_categories
                    .iter()
                    .map(|category| category.as_str())
                    .collect::<Vec<_>>()
            );
        }
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
        let config = self.config();
        let project_root = config
            .project_root
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let project_root = std::fs::canonicalize(&project_root).unwrap_or(project_root);
        Some(InspectSnapshot::new(
            project_root,
            self.inspect_dir(),
            config,
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
    pub fn semantic_index(&self) -> &RwLock<Option<SemanticIndex>> {
        &self.semantic_index
    }

    /// Access the semantic-index build receiver.
    pub fn semantic_index_rx(
        &self,
    ) -> &parking_lot::Mutex<Option<crossbeam_channel::Receiver<SemanticIndexEvent>>> {
        &self.semantic_index_rx
    }

    pub(crate) fn install_semantic_index_rx(
        &self,
        receiver: crossbeam_channel::Receiver<SemanticIndexEvent>,
        generation: u64,
    ) -> u64 {
        let mut slot = self.semantic_index_rx.lock();
        self.note_semantic_index_rx_generation(generation);
        let epoch = self.next_semantic_index_rx_epoch();
        *slot = Some(receiver);
        epoch
    }

    pub(crate) fn semantic_index_rx_terminal_guard(&self, epoch: u64) -> ReceiverTerminalGuard {
        ReceiverTerminalGuard::new(Arc::clone(&self.semantic_index_rx_terminal_epoch), epoch)
    }

    /// Keep generation/epoch validation and receiver mutation under the same
    /// lock used by receiver installation.
    pub(crate) fn with_current_semantic_index_rx<R>(
        &self,
        generation: u64,
        epoch: u64,
        action: impl FnOnce(&mut Option<crossbeam_channel::Receiver<SemanticIndexEvent>>) -> R,
    ) -> Option<R> {
        self.run_if_subc_bound_generation(generation, || {
            let mut receiver = self.semantic_index_rx.lock();
            if receiver.is_none()
                || self.semantic_index_rx_generation() != generation
                || self.semantic_index_rx_epoch() != epoch
            {
                return None;
            }
            Some(action(&mut receiver))
        })
        .flatten()
    }

    pub(crate) fn retire_semantic_index_rx(&self) {
        let mut receiver = self.semantic_index_rx.lock();
        *receiver = None;
        self.next_semantic_index_rx_epoch();
    }

    /// Retire a build receiver only if no replacement changed its epoch after
    /// the caller inspected it. `None` means a newer receiver won the race;
    /// `Some(false)` means the inspected epoch is still current but empty.
    pub(crate) fn retire_semantic_index_rx_if_epoch(&self, expected_epoch: u64) -> Option<bool> {
        let mut receiver = self.semantic_index_rx.lock();
        if self.semantic_index_rx_epoch() != expected_epoch {
            return None;
        }
        let retired = receiver.take().is_some();
        if retired {
            self.next_semantic_index_rx_epoch();
        }
        Some(retired)
    }

    pub(crate) fn note_semantic_index_rx_generation(&self, generation: u64) {
        self.semantic_index_rx_generation
            .store(generation, Ordering::SeqCst);
    }

    pub(crate) fn semantic_index_rx_generation(&self) -> u64 {
        self.semantic_index_rx_generation.load(Ordering::SeqCst)
    }

    pub(crate) fn next_semantic_index_rx_epoch(&self) -> u64 {
        self.semantic_index_rx_epoch
            .fetch_add(1, Ordering::SeqCst)
            .wrapping_add(1)
    }

    pub(crate) fn semantic_index_rx_epoch(&self) -> u64 {
        self.semantic_index_rx_epoch.load(Ordering::SeqCst)
    }

    pub(crate) fn next_semantic_persist_epoch(&self) -> u64 {
        self.semantic_persist_epoch.next()
    }

    pub(crate) fn semantic_persist_epoch_flag(&self) -> crate::root_cache::ArtifactPublishEpoch {
        self.semantic_persist_epoch.clone()
    }

    pub(crate) fn semantic_persist_lock(&self) -> Arc<parking_lot::Mutex<()>> {
        Arc::clone(&self.semantic_persist_lock)
    }

    pub fn semantic_index_status(&self) -> &RwLock<SemanticIndexStatus> {
        &self.semantic_index_status
    }

    pub(crate) fn artifact_reload_guard(&self) -> parking_lot::MutexGuard<'_, ()> {
        self.artifact_reload_lock.lock()
    }

    /// Reset this context's cold semantic seed gate for a newly accepted
    /// configure and return the generation token for the worker being spawned.
    pub fn reset_semantic_cold_seed_gate_for_configure(&self) -> u64 {
        self.semantic_cold_seed_active
            .store(false, Ordering::SeqCst);
        self.semantic_callgraph_warm_deferred
            .store(false, Ordering::SeqCst);
        self.semantic_cold_seed_generation
            .fetch_add(1, Ordering::SeqCst)
            .wrapping_add(1)
    }

    pub fn semantic_cold_seed_active_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.semantic_cold_seed_active)
    }

    pub fn semantic_cold_seed_generation_flag(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.semantic_cold_seed_generation)
    }

    pub fn semantic_cold_seed_generation(&self) -> u64 {
        self.semantic_cold_seed_generation.load(Ordering::SeqCst)
    }

    pub fn semantic_cold_seed_active(&self) -> bool {
        self.semantic_cold_seed_active.load(Ordering::SeqCst)
    }

    pub fn schedule_semantic_cold_seed_gate_for_configure(&self) {
        self.semantic_cold_seed_active.store(true, Ordering::SeqCst);
    }

    pub fn defer_callgraph_store_warm_for_semantic_cold_seed(&self) {
        self.semantic_callgraph_warm_deferred
            .store(true, Ordering::SeqCst);
    }

    fn semantic_callgraph_warm_deferred(&self) -> bool {
        self.semantic_callgraph_warm_deferred.load(Ordering::SeqCst)
    }

    /// Clear the cold-seed gate and resume work that was intentionally held back
    /// while the full semantic corpus was accumulating. This entry point is used
    /// by the code that drains events from the semantic worker.
    pub fn clear_semantic_cold_seed_gate_and_resume_deferred_work(&self) {
        self.resume_semantic_cold_seed_deferred_work(false);
    }

    /// Resume work after the semantic worker has already cleared the atomic gate
    /// itself, such as on cached-index load or before a retry backoff sleep.
    pub fn resume_deferred_work_after_semantic_cold_seed_gate_cleared(&self) {
        self.resume_semantic_cold_seed_deferred_work(true);
    }

    pub(crate) fn take_semantic_cold_seed_resume(&self, force: bool) -> SemanticColdSeedResume {
        let was_active = self.semantic_cold_seed_active.swap(false, Ordering::SeqCst);
        let warm_callgraph = self
            .semantic_callgraph_warm_deferred
            .swap(false, Ordering::SeqCst);
        SemanticColdSeedResume {
            request_tier2: force || was_active || warm_callgraph,
            warm_callgraph,
        }
    }

    pub(crate) fn apply_semantic_cold_seed_resume(&self, resume: SemanticColdSeedResume) {
        if resume.request_tier2 {
            let _ = self.request_tier2_refresh_pull();
        }

        if !resume.warm_callgraph
            || !self.config().callgraph_store
            || !self.heavy_root_work_allowed()
        {
            return;
        }

        match self.callgraph_store_for_ops() {
            CallgraphStoreAccess::Ready(_) => {
                crate::slog_debug!(
                    "deferred callgraph store warm completed after semantic cold seed gate cleared"
                );
            }
            CallgraphStoreAccess::Building => {
                crate::slog_info!(
                    "deferred callgraph store warm scheduled after semantic cold seed gate cleared"
                );
            }
            CallgraphStoreAccess::Unavailable => {
                crate::slog_info!(
                    "deferred callgraph store warm unavailable after semantic cold seed gate cleared"
                );
            }
            CallgraphStoreAccess::Error(error) => {
                crate::slog_warn!(
                    "deferred callgraph store warm failed after semantic cold seed gate cleared: {}",
                    error
                );
            }
        }
    }

    fn resume_semantic_cold_seed_deferred_work(&self, force: bool) {
        let resume = self.take_semantic_cold_seed_resume(force);
        self.apply_semantic_cold_seed_resume(resume);
    }

    #[doc(hidden)]
    pub fn set_semantic_cold_seed_active_for_test(&self, active: bool) {
        self.semantic_cold_seed_active
            .store(active, Ordering::SeqCst);
    }

    #[doc(hidden)]
    pub fn semantic_callgraph_warm_deferred_for_test(&self) -> bool {
        self.semantic_callgraph_warm_deferred()
    }

    pub fn install_semantic_refresh_worker(
        &self,
        sender: crossbeam_channel::Sender<SemanticRefreshRequest>,
        event_rx: crossbeam_channel::Receiver<SemanticRefreshEvent>,
        worker_slot: SemanticRefreshWorkerSlot,
    ) {
        self.install_semantic_refresh_worker_for_build_epoch(
            sender,
            event_rx,
            worker_slot,
            self.semantic_index_rx_epoch(),
        );
    }

    pub(crate) fn install_semantic_refresh_worker_for_build_epoch(
        &self,
        sender: crossbeam_channel::Sender<SemanticRefreshRequest>,
        event_rx: crossbeam_channel::Receiver<SemanticRefreshEvent>,
        worker_slot: SemanticRefreshWorkerSlot,
        build_epoch: u64,
    ) {
        self.clear_semantic_refresh_worker();
        {
            let mut receiver = self.semantic_refresh_event_rx.lock();
            let mut request = self.semantic_refresh_tx.lock();
            let mut worker = self.semantic_refresh_worker.lock();
            self.semantic_refresh_generation
                .store(self.configure_generation(), Ordering::SeqCst);
            self.semantic_refresh_epoch.fetch_add(1, Ordering::SeqCst);
            self.semantic_refresh_build_epoch
                .store(build_epoch, Ordering::SeqCst);
            *receiver = Some(event_rx);
            *request = Some(sender);
            *worker = Some(worker_slot);
        }
    }

    pub(crate) fn semantic_refresh_generation(&self) -> u64 {
        self.semantic_refresh_generation.load(Ordering::SeqCst)
    }

    pub(crate) fn semantic_refresh_epoch(&self) -> u64 {
        self.semantic_refresh_epoch.load(Ordering::SeqCst)
    }

    /// Serialize refresh event commit with worker replacement. The receiver
    /// lock also couples the generation and epoch to the dequeued channel.
    pub(crate) fn with_current_semantic_refresh_rx<R>(
        &self,
        generation: u64,
        epoch: u64,
        action: impl FnOnce() -> R,
    ) -> Option<R> {
        self.run_if_subc_bound_generation(generation, || {
            let receiver = self.semantic_refresh_event_rx.lock();
            if receiver.is_none()
                || self.semantic_refresh_generation() != generation
                || self.semantic_refresh_epoch() != epoch
            {
                return None;
            }
            Some(action())
        })
        .flatten()
    }

    pub(crate) fn clear_semantic_refresh_worker_if_current(
        &self,
        generation: u64,
        epoch: u64,
    ) -> Option<u64> {
        let worker_slot = {
            let mut receiver = self.semantic_refresh_event_rx.lock();
            if receiver.is_none()
                || self.semantic_refresh_generation() != generation
                || self.semantic_refresh_epoch() != epoch
            {
                return None;
            }
            let disconnected_build_epoch = self.semantic_refresh_build_epoch.load(Ordering::SeqCst);
            self.semantic_refresh_build_epoch.store(0, Ordering::SeqCst);
            let mut request = self.semantic_refresh_tx.lock();
            let mut worker = self.semantic_refresh_worker.lock();
            *receiver = None;
            *request = None;
            self.semantic_refresh_epoch.fetch_add(1, Ordering::SeqCst);
            self.invalidate_semantic_refresh_probe();
            (worker.take(), disconnected_build_epoch)
        };
        if let Some(worker_slot) = worker_slot.0 {
            if let Ok(mut handle) = worker_slot.lock() {
                drop(handle.take());
            }
        }
        Some(worker_slot.1)
    }

    pub fn clear_semantic_refresh_worker(&self) {
        let worker_slot = {
            let mut receiver = self.semantic_refresh_event_rx.lock();
            let mut request = self.semantic_refresh_tx.lock();
            let mut worker = self.semantic_refresh_worker.lock();
            *receiver = None;
            *request = None;
            self.semantic_refresh_epoch.fetch_add(1, Ordering::SeqCst);
            self.semantic_refresh_build_epoch.store(0, Ordering::SeqCst);
            self.invalidate_semantic_refresh_probe();
            worker.take()
        };
        if let Some(worker_slot) = worker_slot {
            if let Ok(mut handle) = worker_slot.lock() {
                drop(handle.take());
            }
        }
    }

    pub fn semantic_refresh_sender(
        &self,
    ) -> Option<crossbeam_channel::Sender<SemanticRefreshRequest>> {
        self.semantic_refresh_tx.lock().clone()
    }

    pub(crate) fn semantic_refresh_retry_slots(
        &self,
    ) -> (
        Arc<parking_lot::Mutex<Option<crossbeam_channel::Sender<SemanticRefreshRequest>>>>,
        Arc<parking_lot::Mutex<BTreeSet<PathBuf>>>,
    ) {
        (
            Arc::clone(&self.semantic_refresh_tx),
            Arc::clone(&self.pending_semantic_index_paths),
        )
    }

    pub fn semantic_refresh_event_rx(
        &self,
    ) -> &parking_lot::Mutex<Option<crossbeam_channel::Receiver<SemanticRefreshEvent>>> {
        &self.semantic_refresh_event_rx
    }

    pub fn with_semantic_refresh_retry_attempts_mut<R>(
        &self,
        f: impl FnOnce(&mut BTreeMap<PathBuf, usize>) -> R,
    ) -> R {
        let mut attempts = self.semantic_refresh_retry_attempts.lock();
        f(&mut attempts)
    }

    pub fn clear_semantic_refresh_retry_attempts(&self, paths: &[PathBuf]) {
        let mut attempts = self.semantic_refresh_retry_attempts.lock();
        for path in paths {
            attempts.remove(path);
        }
    }

    pub fn clear_all_semantic_refresh_retry_attempts(&self) {
        self.semantic_refresh_retry_attempts.lock().clear();
    }

    pub fn semantic_refresh_circuit_is_open(&self) -> bool {
        self.semantic_refresh_circuit.open.load(Ordering::SeqCst)
    }

    pub fn record_semantic_refresh_transient_failure(&self, trip_threshold: usize) -> bool {
        let failures = self
            .semantic_refresh_circuit
            .consecutive_transient_failures
            .fetch_add(1, Ordering::SeqCst)
            .saturating_add(1);
        if failures >= trip_threshold
            && !self
                .semantic_refresh_circuit
                .open
                .swap(true, Ordering::SeqCst)
        {
            crate::slog_warn!(
                "embedding backend appears down; suspending active retries, will resume on next change or successful probe"
            );
        }
        self.semantic_refresh_circuit_is_open()
    }

    pub fn trip_semantic_refresh_circuit(&self, trip_threshold: usize) {
        self.semantic_refresh_circuit
            .consecutive_transient_failures
            .store(trip_threshold, Ordering::SeqCst);
        if !self
            .semantic_refresh_circuit
            .open
            .swap(true, Ordering::SeqCst)
        {
            crate::slog_warn!(
                "embedding backend appears down; suspending active retries, will resume on next change or successful probe"
            );
        }
    }

    pub fn reset_semantic_refresh_transient_failure_count(&self) {
        self.semantic_refresh_circuit
            .consecutive_transient_failures
            .store(0, Ordering::SeqCst);
    }

    pub fn reset_semantic_refresh_circuit_after_success(&self) {
        self.reset_semantic_refresh_transient_failure_count();
        self.semantic_refresh_circuit
            .probe_ready
            .store(false, Ordering::SeqCst);
        if self
            .semantic_refresh_circuit
            .open
            .swap(false, Ordering::SeqCst)
        {
            crate::slog_info!("embedding backend recovered; resuming normal refresh retries");
        }
    }

    pub fn semantic_refresh_transient_failure_count(&self) -> usize {
        self.semantic_refresh_circuit
            .consecutive_transient_failures
            .load(Ordering::SeqCst)
    }

    pub fn semantic_refresh_probe_is_scheduled(&self) -> bool {
        self.semantic_refresh_circuit
            .probe_in_flight
            .load(Ordering::SeqCst)
            || self.semantic_refresh_probe_ready()
    }

    pub fn semantic_refresh_probe_ready(&self) -> bool {
        self.semantic_refresh_circuit
            .probe_ready
            .load(Ordering::SeqCst)
    }

    pub fn take_semantic_refresh_probe_ready(&self) -> bool {
        self.semantic_refresh_circuit
            .probe_ready
            .swap(false, Ordering::SeqCst)
    }

    fn invalidate_semantic_refresh_probe(&self) {
        self.semantic_refresh_circuit
            .probe_token
            .fetch_add(1, Ordering::SeqCst);
        self.semantic_refresh_circuit
            .probe_ready
            .store(false, Ordering::SeqCst);
        self.semantic_refresh_circuit
            .probe_in_flight
            .store(false, Ordering::SeqCst);
    }

    pub fn ensure_semantic_refresh_probe_scheduled(&self, delay: Duration) {
        let receiver = self.semantic_refresh_event_rx.lock();
        if receiver.is_none()
            || self
                .semantic_refresh_circuit
                .probe_ready
                .load(Ordering::SeqCst)
            || self
                .semantic_refresh_circuit
                .probe_in_flight
                .swap(true, Ordering::SeqCst)
        {
            return;
        }
        let probe_token = self
            .semantic_refresh_circuit
            .probe_token
            .fetch_add(1, Ordering::SeqCst)
            .wrapping_add(1);
        drop(receiver);

        let circuit = Arc::clone(&self.semantic_refresh_circuit);
        let session_id = crate::log_ctx::current_session();
        std::thread::spawn(move || {
            crate::log_ctx::with_session(session_id, || {
                std::thread::sleep(delay);
                if circuit.probe_token.load(Ordering::SeqCst) == probe_token {
                    circuit.probe_ready.store(true, Ordering::SeqCst);
                    circuit.probe_in_flight.store(false, Ordering::SeqCst);
                }
            });
        });
    }

    /// Access the cached semantic embedding model.
    pub fn semantic_embedding_model(
        &self,
    ) -> &parking_lot::Mutex<Option<crate::semantic_index::EmbeddingModel>> {
        &self.semantic_embedding_model
    }

    /// Access the file watcher handle (kept alive to continue watching).
    pub fn watcher(&self) -> &parking_lot::Mutex<Option<RecommendedWatcher>> {
        &self.watcher
    }

    /// Access the pre-filtered watcher event receiver.
    pub fn watcher_rx(
        &self,
    ) -> &parking_lot::Mutex<Option<crossbeam_channel::Receiver<WatcherDispatchEvent>>> {
        &self.watcher_rx
    }

    /// Access continuation state for the bounded watcher drain.
    pub(crate) fn watcher_drain_slice(
        &self,
    ) -> &parking_lot::Mutex<Option<WatcherDrainSliceState>> {
        &self.watcher_drain_slice
    }

    /// Include partially consumed dispatch events when reporting drain backlog.
    pub fn watcher_drain_pending_path_count(&self) -> usize {
        self.watcher_drain_slice.lock().as_ref().map_or(0, |state| {
            let active_paths = match &state.phase {
                WatcherDrainPhase::Collect => 0,
                WatcherDrainPhase::Apply { paths, .. } => paths.len(),
            };
            active_paths + state.pending_paths.len()
        })
    }

    /// Number of path-budgeted watcher batches since this runtime was installed.
    pub fn watcher_drain_path_slice_count(&self) -> usize {
        self.watcher_drain_slice
            .lock()
            .as_ref()
            .map_or(0, |state| state.path_slice_count)
    }

    /// Install a watcher filter thread and its dispatch receiver. The caller
    /// must have stopped any previous watcher runtime first.
    pub fn install_watcher_runtime(
        &self,
        rx: crossbeam_channel::Receiver<WatcherDispatchEvent>,
        runtime: WatcherThreadHandle,
    ) {
        let _runtime_guard = self.watcher_runtime_lock.lock();
        let replaced = self.watcher_thread.lock().replace(runtime);
        self.app.watcher_started();
        if let Some(runtime) = replaced {
            Self::spawn_watcher_shutdown(Arc::clone(&self.app), self.watcher_root_path(), runtime);
        }
        *self.watcher_rx.lock() = Some(rx);
        *self.watcher_drain_slice.lock() = None;
    }

    fn watcher_root_path(&self) -> PathBuf {
        self.canonical_cache_root_opt()
            .or_else(|| self.config().project_root.clone())
            .unwrap_or_else(|| PathBuf::from("<unconfigured>"))
    }

    fn spawn_watcher_shutdown(app: Arc<App>, root: PathBuf, runtime: WatcherThreadHandle) {
        const JOIN_TIMEOUT: Duration = Duration::from_secs(2);
        // Signal the watcher before scheduling the joiner so teardown does not
        // depend on a newly spawned thread winning CPU time under fleet load.
        runtime.request_shutdown();
        std::thread::spawn(
            move || match runtime.shutdown_and_join_timeout(JOIN_TIMEOUT) {
                WatcherJoinOutcome::Joined => {
                    app.watcher_stopped();
                    crate::slog_info!("watcher stopped: {}", root.display());
                }
                WatcherJoinOutcome::TimedOut(join) => {
                    crate::slog_warn!(
                        "watcher stop timed out after {} ms: {}",
                        JOIN_TIMEOUT.as_millis(),
                        root.display()
                    );
                    std::thread::spawn(move || {
                        let _ = join.join();
                        app.watcher_stopped();
                        crate::slog_info!("watcher stopped: {}", root.display());
                    });
                }
            },
        );
    }

    fn take_watcher_runtime(&self) -> Option<WatcherThreadHandle> {
        let _runtime_guard = self.watcher_runtime_lock.lock();
        let runtime = self.watcher_thread.lock().take();
        *self.watcher_rx.lock() = None;
        *self.watcher_drain_slice.lock() = None;
        *self.watcher.lock() = None;
        runtime
    }

    /// Stop the watcher runtime without waiting on its OS thread. Shutdown and
    /// the bounded join run on a detached reaper so configure and transport
    /// loops never wait on FSEvents or inotify teardown.
    pub fn stop_watcher_runtime(&self) {
        if let Some(runtime) = self.take_watcher_runtime() {
            Self::spawn_watcher_shutdown(Arc::clone(&self.app), self.watcher_root_path(), runtime);
        }
    }

    /// Request watcher shutdown without joining on the executor lane.
    pub fn stop_watcher_runtime_in_background(&self) {
        self.stop_watcher_runtime();
    }

    /// Remove a watcher runtime whose OS thread already exited (backend
    /// failure while the root was unbound and drains were suppressed).
    /// Returns true when a finished corpse was actually removed so the caller
    /// can apply watcher-gap invalidation exactly once.
    pub(crate) fn take_finished_watcher_runtime(&self) -> bool {
        let runtime = {
            let _runtime_guard = self.watcher_runtime_lock.lock();
            let finished = self
                .watcher_thread
                .lock()
                .as_ref()
                .is_some_and(|runtime| runtime.is_finished());
            if !finished {
                return false;
            }
            let runtime = self.watcher_thread.lock().take();
            *self.watcher_rx.lock() = None;
            *self.watcher_drain_slice.lock() = None;
            *self.watcher.lock() = None;
            runtime
        };
        if let Some(runtime) = runtime {
            Self::spawn_watcher_shutdown(Arc::clone(&self.app), self.watcher_root_path(), runtime);
        }
        true
    }

    /// Process-scoped watcher count used by maintenance diagnostics and
    /// regression tests. A runtime remains counted until its thread exits.
    pub fn watcher_registry_count(&self) -> usize {
        self.app.watcher_count()
    }

    pub(crate) fn watcher_runtime_active(&self) -> bool {
        let _runtime_guard = self.watcher_runtime_lock.lock();
        // A finished thread is a dead runtime even while its handle is still
        // installed (the backend can fail while drains are suppressed for an
        // unbound root, leaving the queued error undrained). Treating it as
        // active would block watcher restoration on rebind.
        let thread_live = self
            .watcher_thread
            .lock()
            .as_ref()
            .is_some_and(|runtime| !runtime.is_finished());
        thread_live && self.watcher_rx.lock().is_some()
    }

    /// Return whether artifact eviction would discard work that still needs a
    /// live handle. Callers use this as the single safety gate before clearing
    /// resident stores and inspect caches.
    pub fn artifact_eviction_blocked(&self) -> bool {
        let semantic_refresh_in_flight = match &*self
            .semantic_index_status
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            SemanticIndexStatus::Building { .. } => true,
            SemanticIndexStatus::Ready { refreshing, .. } => !refreshing.is_empty(),
            SemanticIndexStatus::Disabled | SemanticIndexStatus::Failed(_) => false,
        };
        if crate::runtime_drain::any_build_in_flight(self)
            || semantic_refresh_in_flight
            || self.inspect_manager.tier2_any_in_flight()
            || !self.bash_background.running_tasks().is_empty()
            || !self.pending_callgraph_store_paths.lock().is_empty()
            || !self.pending_search_index_paths.lock().is_empty()
            || !self.pending_tier2_paths.lock().is_empty()
            || !self.pending_semantic_index_paths.lock().is_empty()
            || *self.pending_semantic_corpus_refresh.lock()
        {
            return true;
        }

        let search_has_pending_disk_changes = self
            .search_index
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(SearchIndex::has_pending_disk_changes);
        search_has_pending_disk_changes
    }

    /// Drop idle root-scoped artifact handles. Persistent data remains on disk;
    /// artifact-backed query paths schedule a background reload on first use.
    /// Returns false when an active build, bash task, inspect scan, or pending
    /// disk update makes eviction unsafe.
    pub fn evict_idle_artifacts(&self) -> bool {
        if self.artifact_eviction_blocked() {
            return false;
        }

        self.callgraph_store
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        self.search_index
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        self.semantic_index
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        self.borrowed_index_cache.lock().clear();
        self.inspect_manager.evict_idle_caches();
        self.reset_symbol_cache();
        self.clear_tsconfig_membership_cache();
        true
    }

    /// Test seam for the serialized real-watcher integration suite. Production
    /// callers cannot trigger it without the explicit test-only environment flag.
    #[doc(hidden)]
    pub fn force_idle_teardown_for_test(self: &Arc<Self>) -> bool {
        if std::env::var("AFT_TEST_ALLOW_FORCE_IDLE_REAP").as_deref() != Ok("1") {
            return false;
        }
        if !self.evict_idle_artifacts() {
            return false;
        }
        self.stop_watcher_runtime_in_background();
        self.invalidate_artifacts_after_watcher_gap();
        true
    }

    /// Release resources that can be recreated by an equivalent later bind.
    /// LSP shutdown can wait on child processes, so all work stays off the
    /// executor and subc frame loops.
    pub(crate) fn release_idle_reopenable_resources_in_background(self: &Arc<Self>) {
        let ctx = Arc::clone(self);
        std::thread::spawn(move || {
            if !ctx.subc_unbound_quiesced() {
                return;
            }
            {
                let mut lsp = ctx.lsp_manager.lock();
                if !ctx.subc_unbound_quiesced() {
                    return;
                }
                lsp.shutdown_all();
            }
            let _ = ctx.subc_lifecycle.run_if_unbound(|| {
                ctx.bash_background.clear_db_pool();
                ctx.backup.lock().clear_db_pool();
            });
        });
    }

    /// Final cleanup for an actor whose project directory no longer exists.
    /// The executor invokes this only after proving the actor has no queued or
    /// running jobs, and always from a detached teardown thread.
    pub(crate) fn teardown_deleted_root(&self) {
        self.bash_background.detach();
        self.bash_background.clear_db_pool();
        self.backup.lock().clear_db_pool();
        self.lsp_manager.lock().shutdown_all();
    }

    /// Access the LSP manager.
    pub fn lsp(&self) -> parking_lot::MutexGuard<'_, LspManager> {
        self.lsp_manager.lock()
    }

    /// Notify LSP servers that a file was written.
    /// Call this after write_format_validate in command handlers.
    pub fn lsp_notify_file_changed(&self, file_path: &Path, content: &str) {
        let config = self.config();
        if let Some(mut lsp) = self.lsp_manager.try_lock() {
            if let Err(e) = lsp.notify_file_changed_if_running(file_path, content, &config) {
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
        if let Some(mut lsp) = self.lsp_manager.try_lock() {
            lsp.clear_diagnostics_for_file(file_path)
        } else {
            false
        }
    }

    /// Mark diagnostics stale for a file changed outside AFT's text-sync path.
    /// Best-effort: a contended LSP lock is skipped and the next watcher event
    /// or scoped diagnostics pull can reconcile the file.
    pub fn lsp_mark_diagnostics_stale_for_file(&self, file_path: &Path) -> StaleDiagnosticsMark {
        if let Some(mut lsp) = self.lsp_manager.try_lock() {
            lsp.mark_diagnostics_stale_for_file(file_path)
        } else {
            StaleDiagnosticsMark::default()
        }
    }

    /// Resync a watcher-stale diagnosed file with the active LSP server.
    ///
    /// `workspace/didChangeWatchedFiles` tells servers that the filesystem
    /// changed, but it does not update an already-open document's in-memory text.
    /// Sending the normal didOpen/didChange path gives push-only servers a chance
    /// to publish fresh diagnostics and keeps pull-capable servers' document state
    /// current for the next diagnostic request.
    pub fn lsp_resync_changed_file_for_diagnostics(&self, file_path: &Path) -> bool {
        if !file_path.is_file() {
            return false;
        }

        let content = match std::fs::read_to_string(file_path) {
            Ok(content) => content,
            Err(err) => {
                crate::slog_warn!(
                    "skipping LSP resync for {} after external edit: {}",
                    file_path.display(),
                    err
                );
                return false;
            }
        };

        let config = self.config();
        if let Some(mut lsp) = self.lsp_manager.try_lock() {
            if let Err(err) = lsp.notify_file_changed(file_path, &content, &config) {
                crate::slog_warn!(
                    "LSP resync failed for {} after external edit: {}",
                    file_path.display(),
                    err
                );
                return false;
            }
            true
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
        let config = self.config();
        let Some(mut lsp) = self.lsp_manager.try_lock() else {
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

        let config = self.config();
        if let Some(mut lsp) = self.lsp_manager.try_lock() {
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
        self.add_pending_tier2_paths(file_paths.iter().cloned());
        let _ = self.mark_status_bar_tier2_stale();

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
        if let Some(file_paths) = Self::multi_file_write_paths(params) {
            self.add_pending_tier2_paths(file_paths);
        } else {
            self.add_pending_tier2_paths([file_path.to_path_buf()]);
        }
        let _ = self.mark_status_bar_tier2_stale();

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

    fn path_restriction_context(
        &self,
        req_id: &str,
        path: &Path,
    ) -> Result<Option<PathRestrictionContext>, crate::protocol::Response> {
        let config = self.config();
        let force_restrict = self.request_force_restrict(req_id);
        if !config.restrict_to_project_root && !force_restrict {
            return Ok(None);
        }
        let root = match &config.project_root {
            Some(root) => root.clone(),
            None if force_restrict => {
                return Err(crate::protocol::Response::error(
                    req_id,
                    "path_outside_root",
                    "project root is required when path restriction is forced",
                ));
            }
            None => return Ok(None),
        };
        drop(config);

        let raw_root = root.clone();
        let resolved_root = std::fs::canonicalize(&root).unwrap_or(root);
        let path_for_resolution = if path.is_relative() {
            raw_root.join(path)
        } else {
            path.to_path_buf()
        };
        Ok(Some(PathRestrictionContext {
            raw_root,
            resolved_root,
            path_for_resolution,
        }))
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
        self.validate_path_with_artifact_session(req_id, path, None)
    }

    /// Validate a write location without following its final path component.
    ///
    /// Checkpoint creation and restore use this mode because the final component
    /// is the object being preserved or replaced. Following a symlink there would
    /// authorize its target and change the stored snapshot key. Every ancestor is
    /// still resolved so a symlinked parent cannot escape the project root.
    pub fn validate_write_location(
        &self,
        req_id: &str,
        path: &Path,
    ) -> Result<std::path::PathBuf, crate::protocol::Response> {
        let Some(PathRestrictionContext {
            raw_root,
            resolved_root,
            path_for_resolution,
        }) = self.path_restriction_context(req_id, path)?
        else {
            return Ok(path.to_path_buf());
        };
        let normalized = normalize_path(&path_for_resolution);
        let Some(file_name) = normalized.file_name() else {
            return self.validate_path(req_id, path);
        };
        let parent = normalized.parent().unwrap_or_else(|| Path::new(""));
        let resolved_parent = match std::fs::canonicalize(parent) {
            Ok(resolved) => resolved,
            Err(_) => {
                reject_escaping_symlink(req_id, path, parent, &resolved_root, &raw_root)?;
                resolve_with_existing_ancestors(parent)
            }
        };
        let resolved = normalize_path(&resolved_parent.join(file_name));

        if !resolved.starts_with(&resolved_root) {
            return Err(path_error_response(req_id, path, &resolved_root));
        }

        Ok(resolved)
    }

    /// Validate a read path. A file produced by a background bash task may live
    /// outside the project root, so the session that owns the registered output
    /// may read that specific file. Mutating tools deliberately use
    /// [`AppContext::validate_path`] or [`AppContext::validate_write_location`]
    /// and never receive this exception.
    pub fn validate_read_path(
        &self,
        req_id: &str,
        session_id: &str,
        path: &Path,
    ) -> Result<std::path::PathBuf, crate::protocol::Response> {
        self.validate_path_with_artifact_session(req_id, path, Some(session_id))
    }

    fn validate_path_with_artifact_session(
        &self,
        req_id: &str,
        path: &Path,
        artifact_session_id: Option<&str>,
    ) -> Result<std::path::PathBuf, crate::protocol::Response> {
        let Some(PathRestrictionContext {
            raw_root,
            resolved_root,
            path_for_resolution,
        }) = self.path_restriction_context(req_id, path)?
        else {
            // When path restriction is disabled, callers receive the input path
            // unchanged instead of an implicitly canonicalized filesystem path.
            return Ok(path.to_path_buf());
        };

        // Resolve the path (follow symlinks, normalize ..). If canonicalization
        // fails (e.g. path does not exist or traverses a broken symlink), inspect
        // every existing component with lstat before falling back lexically so a
        // broken in-root symlink cannot be used to write outside project_root.
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
            let is_owned_bash_artifact = artifact_session_id.is_some_and(|session_id| {
                self.bash_background
                    .is_session_owned_artifact_path(session_id, &resolved)
            });
            if !is_owned_bash_artifact {
                return Err(path_error_response(req_id, path, &resolved_root));
            }
        }

        Ok(resolved)
    }

    /// Count active LSP server instances.
    pub fn lsp_server_count(&self) -> usize {
        self.lsp_manager
            .try_lock()
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

    /// Build one root's memory estimate using only non-blocking lock attempts.
    /// A contended subsystem is represented as `busy` rather than delaying the
    /// status control path.
    pub fn memory_root_snapshot(&self) -> crate::memory::RootMemorySnapshot {
        let semantic = match self.semantic_index.try_read() {
            Ok(index) => index
                .as_ref()
                .map(SemanticIndex::estimated_memory)
                .unwrap_or_else(|| crate::memory::MemoryEstimate::estimated(0).count("entries", 0)),
            Err(TryLockError::Poisoned(error)) => error
                .into_inner()
                .as_ref()
                .map(SemanticIndex::estimated_memory)
                .unwrap_or_else(|| crate::memory::MemoryEstimate::estimated(0).count("entries", 0)),
            Err(TryLockError::WouldBlock) => crate::memory::MemoryEstimate::busy(),
        };
        let trigram = match self.search_index.try_read() {
            Ok(index) => index
                .as_ref()
                .map(SearchIndex::estimated_memory)
                .unwrap_or_else(|| crate::memory::MemoryEstimate::estimated(0).count("files", 0)),
            Err(TryLockError::Poisoned(error)) => error
                .into_inner()
                .as_ref()
                .map(SearchIndex::estimated_memory)
                .unwrap_or_else(|| crate::memory::MemoryEstimate::estimated(0).count("files", 0)),
            Err(TryLockError::WouldBlock) => crate::memory::MemoryEstimate::busy(),
        };
        let symbols = match self.symbol_cache.try_read() {
            Ok(cache) => cache.estimated_memory(),
            Err(TryLockError::Poisoned(error)) => error.into_inner().estimated_memory(),
            Err(TryLockError::WouldBlock) => crate::memory::MemoryEstimate::busy(),
        };
        let callgraph = match self.callgraph_store.try_read() {
            Ok(store) => store
                .as_ref()
                .map(|store| store.estimated_memory())
                .unwrap_or_else(|| {
                    crate::memory::MemoryEstimate::estimated(0).count("open_generation_handles", 0)
                }),
            Err(TryLockError::Poisoned(error)) => error
                .into_inner()
                .as_ref()
                .map(|store| store.estimated_memory())
                .unwrap_or_else(|| {
                    crate::memory::MemoryEstimate::estimated(0).count("open_generation_handles", 0)
                }),
            Err(TryLockError::WouldBlock) => crate::memory::MemoryEstimate::busy(),
        };
        let inspect = self.inspect_manager.estimated_memory();
        let bash = self.bash_background.estimated_memory();
        let lsp = self
            .lsp_manager
            .try_lock()
            .map(|lsp| lsp.estimated_memory())
            .unwrap_or_else(crate::memory::MemoryEstimate::busy);
        // AFT currently creates tree-sitter parsers per operation rather than
        // retaining a parser pool. Keep that fact explicit instead of assigning
        // a guessed byte size to tree-sitter internals.
        let parser_pool = crate::memory::MemoryEstimate::not_estimated()
            .count("pooled_parsers", 0)
            .gap("tree_sitter_parser_bytes");
        crate::memory::RootMemorySnapshot::new(
            semantic,
            trigram,
            symbols,
            callgraph,
            inspect,
            bash,
            lsp,
            parser_pool,
        )
    }

    /// Attribute all actor roots registered in this process. Standalone mode
    /// has no actor registry, so the current context is inserted directly.
    pub fn memory_snapshot(&self, current_root: Option<&Path>) -> crate::memory::MemorySnapshot {
        let mut roots = BTreeMap::new();
        let (roots_status, contexts) = match self.app.try_memory_contexts() {
            Some(contexts) => ("ready", contexts),
            None => ("busy", Vec::new()),
        };
        for (root, context) in contexts {
            roots.insert(root.display().to_string(), context.memory_root_snapshot());
        }
        // Normalize through the same identity the registry keys on: on Windows
        // a verbatim `\\?\` current root would otherwise land as a SECOND
        // entry for an already-registered root and double-count its memory.
        let current_label = current_root
            .map(|root| {
                cortexkit_paths::ProjectRootId::from_path(root)
                    .map(|id| id.as_path().display().to_string())
                    .unwrap_or_else(|_| root.display().to_string())
            })
            .unwrap_or_else(|| "<unconfigured>".to_string());
        roots
            .entry(current_label)
            .or_insert_with(|| self.memory_root_snapshot());
        crate::memory::MemorySnapshot::new(roots_status, roots)
    }
}

#[cfg(test)]
mod subc_lifecycle_admission_tests {
    use super::*;

    #[test]
    fn route_teardown_does_not_supersede_disk_artifact_compatibility() {
        let ctx = AppContext::new(default_language_provider_factory(), Config::default());
        ctx.note_configure_warm_key("config-a".to_string());
        let content_generation = ctx.configure_content_generation();
        let lifecycle_generation = ctx.configure_generation();
        let search_epoch = ctx.next_search_persist_epoch();
        let semantic_epoch = ctx.next_semantic_persist_epoch();
        let search_persist_epoch = ctx.search_persist_epoch_flag();
        let semantic_persist_epoch = ctx.semantic_persist_epoch_flag();

        ctx.mark_subc_unbound();
        assert!(ctx.configure_generation() > lifecycle_generation);
        assert_eq!(ctx.configure_content_generation(), content_generation);
        assert_eq!(search_persist_epoch.current(), search_epoch);
        assert_eq!(semantic_persist_epoch.current(), semantic_epoch);

        ctx.mark_subc_bound();
        ctx.note_configure_warm_key("config-b".to_string());
        assert!(ctx.configure_content_generation() > content_generation);
        let replacement_search_epoch = ctx.next_search_persist_epoch();
        let replacement_semantic_epoch = ctx.next_semantic_persist_epoch();
        assert!(replacement_search_epoch > search_epoch);
        assert!(replacement_semantic_epoch > semantic_epoch);
        assert_eq!(search_persist_epoch.current(), replacement_search_epoch);
        assert_eq!(semantic_persist_epoch.current(), replacement_semantic_epoch);
    }

    #[test]
    fn lifecycle_gate_serializes_unbind_with_worker_start_commit() {
        let admission = SubcLifecycleAdmission::default();
        let generation = Arc::new(AtomicU64::new(11));
        let expected = generation.load(Ordering::SeqCst);
        let starts = Arc::new(AtomicUsize::new(0));
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();

        let worker_admission = admission.clone();
        let worker_generation = Arc::clone(&generation);
        let worker_starts = Arc::clone(&starts);
        let worker = std::thread::spawn(move || {
            worker_admission.run_if_current(&worker_generation, expected, || {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                worker_starts.fetch_add(1, Ordering::SeqCst);
            })
        });
        entered_rx.recv().unwrap();

        let unbind_admission = admission.clone();
        let unbind_generation = Arc::clone(&generation);
        let (unbound_tx, unbound_rx) = std::sync::mpsc::channel();
        let unbind = std::thread::spawn(move || {
            unbind_admission.mark_unbound(&unbind_generation);
            unbound_tx.send(()).unwrap();
        });

        assert!(
            unbound_rx
                .recv_timeout(std::time::Duration::from_millis(50))
                .is_err(),
            "unbind must wait for an admitted worker-start commit"
        );
        release_tx.send(()).unwrap();
        assert!(worker.join().unwrap().is_some());
        unbound_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        unbind.join().unwrap();
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert!(
            admission
                .run_if_current(&generation, generation.load(Ordering::SeqCst), || {
                    starts.fetch_add(1, Ordering::SeqCst);
                })
                .is_none(),
            "worker starts after unbind must be denied"
        );
    }

    #[test]
    fn health_snapshot_returns_busy_before_locking_artifact_receivers() {
        let ctx = Arc::new(AppContext::new(
            default_language_provider_factory(),
            Config::default(),
        ));
        let lifecycle_guard = ctx.subc_lifecycle.unbound.lock();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (snapshot_tx, snapshot_rx) = std::sync::mpsc::channel();
        let worker_ctx = Arc::clone(&ctx);
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            snapshot_tx
                .send(worker_ctx.try_health_snapshot(Path::new("health-root")))
                .unwrap();
        });
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("health snapshot worker should start");

        let snapshot = snapshot_rx.recv_timeout(Duration::from_secs(2));
        let callgraph_receiver_available = ctx.callgraph_store_rx.try_lock().is_some();
        drop(lifecycle_guard);
        worker.join().unwrap();

        assert!(
            matches!(
                snapshot,
                Ok(RootHealthSnapshot {
                    state: RootHealthState::Busy,
                    ..
                })
            ),
            "health snapshots must report busy instead of waiting for lifecycle admission"
        );
        assert!(
            callgraph_receiver_available,
            "health snapshots must not hold the callgraph receiver while lifecycle admission is busy"
        );
    }

    #[test]
    fn borrow_only_root_with_partial_tier2_aggregates_reports_disabled() {
        let ctx = AppContext::new(default_language_provider_factory(), Config::default());
        ctx.set_artifact_owner(
            Some(crate::artifact_owner::ArtifactOwnerStatus {
                mode: crate::artifact_owner::ArtifactOwnerMode::ReadOnly,
                project_key: "borrowed".to_string(),
                manifest_path: "manifest.json".to_string(),
                owner_project_scope_key: "owner".to_string(),
                owner_checkout_path: "/owner".to_string(),
                note: None,
            }),
            None,
        );
        ctx.update_status_bar_tier2(Some(4), None, None, None, true);

        let snapshot = ctx.try_health_snapshot(Path::new("borrow-only-root"));

        assert_eq!(snapshot.tier2.expect("tier2 health").status, "disabled");
    }

    #[test]
    fn worktree_guard_prevents_partial_tier2_from_reporting_building() {
        let ctx = AppContext::new(default_language_provider_factory(), Config::default());
        ctx.set_cache_writer_capabilities(true, true);
        ctx.update_status_bar_tier2(Some(4), None, None, None, true);
        assert_eq!(
            ctx.try_health_snapshot(Path::new("writer-root"))
                .tier2
                .expect("tier2 health")
                .status,
            "building"
        );

        ctx.set_cache_role(true, None);

        assert_eq!(
            ctx.try_health_snapshot(Path::new("worktree-root"))
                .tier2
                .expect("tier2 health")
                .status,
            "disabled"
        );
    }

    #[test]
    fn unbound_artifact_cancellation_clears_semantic_refresh_state() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = AppContext::new(
            default_language_provider_factory(),
            Config {
                project_root: Some(temp.path().to_path_buf()),
                semantic_search: true,
                ..Config::default()
            },
        );
        *ctx.semantic_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(SemanticIndex::new(temp.path().to_path_buf(), 3));
        let mut status = SemanticIndexStatus::ready();
        status.add_refreshing_file(temp.path().join("changed.rs"));
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = status;
        let (request_tx, _request_rx) = crossbeam_channel::unbounded();
        let (_event_tx, event_rx) = crossbeam_channel::unbounded();
        ctx.install_semantic_refresh_worker_for_build_epoch(
            request_tx,
            event_rx,
            Arc::new(Mutex::new(None)),
            ctx.semantic_index_rx_epoch(),
        );

        ctx.cancel_unbound_artifact_work();

        assert!(ctx.semantic_refresh_event_rx().lock().is_none());
        assert!(matches!(
            &*ctx
                .semantic_index_status()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            SemanticIndexStatus::Ready { refreshing, .. } if refreshing.is_empty()
        ));
    }

    #[test]
    fn terminal_empty_search_receiver_reports_completion_work() {
        let ctx = AppContext::new(default_language_provider_factory(), Config::default());
        let (sender, receiver) = crossbeam_channel::unbounded();
        let epoch = ctx.install_search_index_rx(receiver, ctx.configure_generation());
        let terminal_guard = ctx.search_index_rx_terminal_guard(epoch);
        drop(sender);
        drop(terminal_guard);

        assert!(
            ctx.completion_drains_have_work(),
            "an empty disconnected one-shot receiver must wake the completion drain"
        );
    }

    #[test]
    fn conditional_semantic_receiver_retire_preserves_replacement_epoch() {
        let ctx = AppContext::new(default_language_provider_factory(), Config::default());
        let (_old_sender, old_receiver) = crossbeam_channel::unbounded();
        let old_epoch = ctx.install_semantic_index_rx(old_receiver, ctx.configure_generation());
        let (_replacement_sender, replacement_receiver) = crossbeam_channel::unbounded();
        let replacement_epoch =
            ctx.install_semantic_index_rx(replacement_receiver, ctx.configure_generation());

        assert!(replacement_epoch > old_epoch);
        assert_eq!(ctx.retire_semantic_index_rx_if_epoch(old_epoch), None);
        assert!(ctx.semantic_index_rx().lock().is_some());
        assert_eq!(ctx.semantic_index_rx_epoch(), replacement_epoch);
    }

    #[test]
    fn stale_terminal_guard_cannot_hide_newer_finished_receiver() {
        let ctx = AppContext::new(default_language_provider_factory(), Config::default());
        let (old_sender, old_receiver) = crossbeam_channel::unbounded();
        let old_epoch = ctx.install_search_index_rx(old_receiver, ctx.configure_generation());
        let old_guard = ctx.search_index_rx_terminal_guard(old_epoch);
        let (current_sender, current_receiver) = crossbeam_channel::unbounded();
        let current_epoch =
            ctx.install_search_index_rx(current_receiver, ctx.configure_generation());
        let current_guard = ctx.search_index_rx_terminal_guard(current_epoch);
        drop(old_sender);
        drop(current_sender);

        drop(current_guard);
        drop(old_guard);

        assert!(current_epoch > old_epoch);
        assert_eq!(
            ctx.search_index_rx_terminal_epoch.load(Ordering::SeqCst),
            current_epoch,
            "a stale worker must not move the terminal watermark backward"
        );
        assert!(ctx.completion_drains_have_work());
    }

    #[test]
    fn finished_semantic_refresh_worker_reports_completion_work() {
        let ctx = AppContext::new(default_language_provider_factory(), Config::default());
        let (request_tx, _request_rx) = crossbeam_channel::unbounded();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let worker_slot = Arc::new(Mutex::new(Some(std::thread::spawn(|| {}))));
        ctx.install_semantic_refresh_worker_for_build_epoch(
            request_tx,
            event_rx,
            Arc::clone(&worker_slot),
            ctx.semantic_index_rx_epoch(),
        );
        drop(event_tx);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while !worker_slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(std::thread::JoinHandle::is_finished)
        {
            assert!(
                std::time::Instant::now() < deadline,
                "worker did not finish"
            );
            std::thread::yield_now();
        }

        assert!(
            ctx.completion_drains_have_work(),
            "a finished refresh worker must wake the completion drain after its event queue empties"
        );
    }

    #[test]
    fn unbound_lifecycle_rejects_all_deferred_worker_starts() {
        let admission = SubcLifecycleAdmission::default();
        let generation = Arc::new(AtomicU64::new(7));
        admission.mark_unbound(&generation);
        let expected = generation.load(Ordering::SeqCst);
        let starts = Arc::new(AtomicUsize::new(0));

        let workers = (0..16)
            .map(|_| {
                let admission = admission.clone();
                let generation = Arc::clone(&generation);
                let starts = Arc::clone(&starts);
                std::thread::spawn(move || {
                    admission.run_if_current(&generation, expected, || {
                        starts.fetch_add(1, Ordering::SeqCst);
                    })
                })
            })
            .collect::<Vec<_>>();

        for worker in workers {
            assert!(worker.join().unwrap().is_none());
        }
        assert_eq!(starts.load(Ordering::SeqCst), 0);
    }
}

#[cfg(test)]
mod force_restrict_tests {
    use super::*;
    use crate::language::StubProvider;
    use tempfile::TempDir;

    fn test_context(project_root: Option<PathBuf>, restrict_to_project_root: bool) -> AppContext {
        AppContext::new(
            Box::new(StubProvider),
            Config {
                project_root,
                restrict_to_project_root,
                ..Config::default()
            },
        )
    }

    #[test]
    fn standalone_validate_path_parity_without_force_restrict() {
        let root = TempDir::new().expect("root tempdir");
        let outside = TempDir::new().expect("outside tempdir");
        let outside_path = outside.path().join("outside.txt");

        let unrestricted = test_context(Some(root.path().to_path_buf()), false);
        assert_eq!(
            unrestricted
                .validate_path("standalone-unrestricted", &outside_path)
                .expect("unrestricted standalone validates"),
            outside_path
        );

        let restricted = test_context(Some(root.path().to_path_buf()), true);
        let err = restricted
            .validate_path("standalone-restricted", &outside_path)
            .expect_err("restricted standalone rejects outside root");
        assert_eq!(
            serde_json::to_value(err).unwrap()["code"],
            "path_outside_root"
        );
    }

    #[test]
    fn force_restrict_guard_refcounts_duplicate_request_ids() {
        let root = TempDir::new().expect("root tempdir");
        let outside = TempDir::new().expect("outside tempdir");
        let outside_path = outside.path().join("outside.txt");
        let ctx = test_context(Some(root.path().to_path_buf()), false);

        assert!(ctx.validate_path("dup", &outside_path).is_ok());
        let guard1 = ctx.force_restrict_guard("dup");
        let guard2 = ctx.force_restrict_guard("dup");
        assert!(ctx.validate_path("dup", &outside_path).is_err());
        drop(guard1);
        assert!(
            ctx.validate_path("dup", &outside_path).is_err(),
            "duplicate guard must keep the request over-restricted"
        );
        drop(guard2);
        assert!(ctx.validate_path("dup", &outside_path).is_ok());
    }

    #[test]
    fn with_force_restrict_cleans_up_after_normal_completion_and_panic() {
        let root = TempDir::new().expect("root tempdir");
        let outside = TempDir::new().expect("outside tempdir");
        let outside_path = outside.path().join("outside.txt");
        let ctx = test_context(Some(root.path().to_path_buf()), false);

        ctx.with_force_restrict("normal", || {
            assert!(ctx.validate_path("normal", &outside_path).is_err());
        });
        assert!(!ctx.request_force_restrict("normal"));
        assert!(ctx.validate_path("normal", &outside_path).is_ok());

        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            ctx.with_force_restrict("panic", || {
                assert!(ctx.validate_path("panic", &outside_path).is_err());
                panic!("intentional force-restrict cleanup panic");
            });
        }));
        assert!(panicked.is_err());
        assert!(!ctx.request_force_restrict("panic"));
        assert!(ctx.validate_path("panic", &outside_path).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn validate_write_location_keeps_final_symlink_as_the_authorized_location() {
        let root = TempDir::new().expect("root tempdir");
        let outside = tempfile::NamedTempFile::new().expect("outside file");
        let link = root.path().join("file.txt");
        std::os::unix::fs::symlink(outside.path(), &link).expect("create final symlink");
        let ctx = test_context(Some(root.path().to_path_buf()), false);
        let _guard = ctx.force_restrict_guard("write-location-final-link");

        let validated = ctx
            .validate_write_location("write-location-final-link", &link)
            .expect("the in-root link location is writable");

        assert_eq!(
            validated,
            std::fs::canonicalize(root.path()).unwrap().join("file.txt")
        );
    }

    #[cfg(unix)]
    #[test]
    fn validate_write_location_rejects_symlinked_parent_escape() {
        let root = TempDir::new().expect("root tempdir");
        let outside = TempDir::new().expect("outside tempdir");
        let linked_parent = root.path().join("linked-parent");
        std::os::unix::fs::symlink(outside.path(), &linked_parent).expect("create parent symlink");
        let candidate = linked_parent.join("file.txt");
        let ctx = test_context(Some(root.path().to_path_buf()), false);
        let _guard = ctx.force_restrict_guard("write-location-parent-link");

        let error = ctx
            .validate_write_location("write-location-parent-link", &candidate)
            .expect_err("a symlinked parent must not escape the project root");

        assert_eq!(
            serde_json::to_value(error).unwrap()["code"],
            "path_outside_root"
        );
    }

    #[cfg(unix)]
    #[test]
    fn validate_write_location_rejects_outside_link_to_inside_file() {
        let root = TempDir::new().expect("root tempdir");
        let outside = TempDir::new().expect("outside tempdir");
        let inside = root.path().join("inside.txt");
        std::fs::write(&inside, "inside").unwrap();
        let outside_link = outside.path().join("outside-link.txt");
        std::os::unix::fs::symlink(&inside, &outside_link).expect("create outside symlink");
        let ctx = test_context(Some(root.path().to_path_buf()), false);
        let _guard = ctx.force_restrict_guard("write-location-outside-link");

        let error = ctx
            .validate_write_location("write-location-outside-link", &outside_link)
            .expect_err("an out-of-root lexical location must remain blocked");

        assert_eq!(
            serde_json::to_value(error).unwrap()["code"],
            "path_outside_root"
        );
    }

    #[test]
    fn forced_restrict_without_project_root_fails_closed() {
        let ctx = test_context(None, false);
        let _guard = ctx.force_restrict_guard("missing-root");
        let err = ctx
            .validate_path("missing-root", Path::new("relative.txt"))
            .expect_err("forced restriction without a root must fail closed");
        assert_eq!(
            serde_json::to_value(err).unwrap()["code"],
            "path_outside_root"
        );

        let write_err = ctx
            .validate_write_location("missing-root", Path::new("relative.txt"))
            .expect_err("write-location validation must also fail closed");
        assert_eq!(
            serde_json::to_value(write_err).unwrap()["code"],
            "path_outside_root"
        );
    }
}

#[cfg(test)]
mod callgraph_store_for_ops_tests {
    use super::*;
    use crate::inspect::{InspectCategory, InspectSnapshot, JobOutcome, JobScope};
    use crate::parser::TreeSitterProvider;
    use crate::protocol::RawRequest;
    use serde_json::json;
    use std::ffi::OsString;
    use std::path::Path;
    use std::sync::{Barrier, Mutex as StdMutex, MutexGuard, OnceLock};
    use tempfile::TempDir;

    struct CallgraphWaitWindowEnvGuard {
        _guard: MutexGuard<'static, ()>,
        previous: Option<OsString>,
    }

    impl Drop for CallgraphWaitWindowEnvGuard {
        fn drop(&mut self) {
            // SAFETY: serialized by the process-local guard held for this
            // helper's lifetime, and restored before the guard is released.
            unsafe {
                match &self.previous {
                    Some(value) => std::env::set_var("AFT_CALLGRAPH_BUILD_WAIT_MS", value),
                    None => std::env::remove_var("AFT_CALLGRAPH_BUILD_WAIT_MS"),
                }
            }
        }
    }

    fn callgraph_build_wait_ms(ms: u64) -> CallgraphWaitWindowEnvGuard {
        static LOCK: OnceLock<StdMutex<()>> = OnceLock::new();
        let guard = LOCK
            .get_or_init(|| StdMutex::new(()))
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous = std::env::var_os("AFT_CALLGRAPH_BUILD_WAIT_MS");
        // SAFETY: serialized by LOCK above and restored by the returned guard.
        unsafe {
            std::env::set_var("AFT_CALLGRAPH_BUILD_WAIT_MS", ms.to_string());
        }
        CallgraphWaitWindowEnvGuard {
            _guard: guard,
            previous,
        }
    }

    fn force_async_callgraph_builds() -> CallgraphWaitWindowEnvGuard {
        callgraph_build_wait_ms(0)
    }

    fn cold_build_context() -> Arc<AppContext> {
        let project = TempDir::new().expect("project tempdir");
        let storage = TempDir::new().expect("storage tempdir");
        let source_dir = project.path().join("src");
        std::fs::create_dir_all(&source_dir).expect("source dir");
        std::fs::write(
            source_dir.join("lib.rs"),
            "pub fn caller() { callee(); }\npub fn callee() {}\n",
        )
        .expect("source file");

        Arc::new(AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(project.keep()),
                storage_dir: Some(storage.keep()),
                callgraph_chunk_size: 1,
                ..Config::default()
            },
        ))
    }

    fn with_fake_home_env<R>(home: &Path, f: impl FnOnce() -> R) -> R {
        let _guard = crate::test_env::process_env_lock();
        let prev_home = std::env::var_os("HOME");
        let prev_userprofile = std::env::var_os("USERPROFILE");
        unsafe {
            std::env::set_var("HOME", home);
            std::env::set_var("USERPROFILE", home);
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        unsafe {
            match prev_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match prev_userprofile {
                Some(value) => std::env::set_var("USERPROFILE", value),
                None => std::env::remove_var("USERPROFILE"),
            }
        }
        match result {
            Ok(value) => value,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    fn configure_request_with_params(params: serde_json::Value) -> RawRequest {
        RawRequest {
            id: "cfg".to_string(),
            command: "configure".to_string(),
            lsp_hints: None,
            session_id: None,
            params,
        }
    }

    fn user_tier(doc: serde_json::Value) -> serde_json::Value {
        json!({
            "tier": "user",
            "source": "/u/aft.jsonc",
            "doc": doc.to_string(),
        })
    }

    fn configure_context(project_root: &Path, storage_dir: &Path) -> AppContext {
        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        let response = crate::commands::configure::handle_configure(
            &configure_request_with_params(json!({
                "project_root": project_root,
                "harness": "opencode",
                "storage_dir": storage_dir,
                "config": [user_tier(json!({
                    "callgraph_store": true,
                    "search_index": true,
                    "semantic_search": true,
                }))],
            })),
            &ctx,
        );
        assert!(response.success, "configure should succeed: {response:?}");
        ctx
    }

    fn inspect_snapshot(ctx: &AppContext) -> InspectSnapshot {
        InspectSnapshot::new(
            ctx.canonical_cache_root(),
            ctx.inspect_dir(),
            ctx.config(),
            ctx.symbol_cache(),
        )
    }

    fn empty_semantic_index_for_ctx(ctx: &AppContext) -> SemanticIndex {
        let project_root = ctx
            .config()
            .project_root
            .clone()
            .expect("test context has a project root");
        let files: Vec<PathBuf> = Vec::new();
        let mut embed = |_texts: Vec<String>| -> Result<Vec<Vec<f32>>, String> { Ok(Vec::new()) };
        SemanticIndex::build(&project_root, &files, &mut embed, 1)
            .expect("empty semantic index should build")
    }

    #[test]
    fn home_root_gate_blocks_callgraph_store_entry_points() {
        let _wait_guard = force_async_callgraph_builds();
        let home = TempDir::new().expect("home tempdir");
        let storage = TempDir::new().expect("storage tempdir");
        let source_dir = home.path().join("src");
        std::fs::create_dir_all(&source_dir).expect("source dir");
        std::fs::write(
            source_dir.join("lib.rs"),
            "pub fn caller() { callee(); }\npub fn callee() {}\n",
        )
        .expect("source file");

        with_fake_home_env(home.path(), || {
            let ctx = configure_context(home.path(), storage.path());
            assert!(
                !ctx.heavy_root_work_allowed(),
                "HOME root configure must close the heavy-root-work gate"
            );
            assert_eq!(
                ctx.try_health_snapshot(home.path())
                    .callgraph_store
                    .as_ref()
                    .map(|component| component.status),
                Some("disabled"),
                "HOME root health must not advertise callgraph building"
            );

            reset_callgraph_cold_build_spawn_count_for_test();
            assert!(matches!(
                ctx.callgraph_store_for_ops(),
                CallgraphStoreAccess::Unavailable
            ));
            assert!(
                ctx.ensure_callgraph_store()
                    .expect("ensure_callgraph_store should not error")
                    .is_none(),
                "shared gate must also block synchronous standalone callgraph builds"
            );
            assert_eq!(
                callgraph_cold_build_spawn_count_for_test(),
                0,
                "HOME root gate must not spawn a cold callgraph build"
            );
        });
    }

    #[test]
    fn home_root_gate_blocks_inspect_manager_submit_paths() {
        let home = TempDir::new().expect("home tempdir");
        let storage = TempDir::new().expect("storage tempdir");
        let source_dir = home.path().join("src");
        std::fs::create_dir_all(&source_dir).expect("source dir");
        std::fs::write(source_dir.join("lib.rs"), "pub fn one() {}\n").expect("source file");

        with_fake_home_env(home.path(), || {
            let ctx = configure_context(home.path(), storage.path());
            let snapshot = inspect_snapshot(&ctx);
            let scope = JobScope::for_project(snapshot.project_root.clone());
            let manager = ctx.inspect_manager();

            assert!(matches!(
                manager.submit_category(snapshot.clone(), InspectCategory::Metrics, scope.clone()),
                JobOutcome::Failed { .. }
            ));

            let submission = manager.submit_tier2_run_with_reuse_serial_background(
                snapshot,
                vec![InspectCategory::DeadCode],
            );
            assert!(submission.queued_categories.is_empty());
            assert!(submission.newly_queued_categories.is_empty());
            assert!(submission.deferred_categories.is_empty());
            assert_eq!(submission.errors.len(), 1);
            assert!(
                !manager.tier2_any_in_flight(),
                "HOME root gate must reject Tier-2 submission before any job is queued"
            );
        });
    }

    #[test]
    fn non_home_root_still_allows_callgraph_cold_builds() {
        let _env_guard = force_async_callgraph_builds();
        reset_callgraph_cold_build_spawn_count_for_test();
        let ctx = cold_build_context();

        assert!(ctx.heavy_root_work_allowed());
        assert!(matches!(
            ctx.callgraph_store_for_ops(),
            CallgraphStoreAccess::Building | CallgraphStoreAccess::Ready(_)
        ));
        assert_eq!(
            callgraph_cold_build_spawn_count_for_test(),
            1,
            "non-home roots must still be able to cold-build the callgraph store"
        );

        let rx = ctx
            .callgraph_store_rx
            .lock()
            .as_ref()
            .cloned()
            .expect("non-home cold build should install an in-flight receiver");
        rx.recv_timeout(Duration::from_secs(30))
            .expect("background cold build should complete");
        *ctx.callgraph_store_rx.lock() = None;
    }

    #[test]
    fn semantic_ready_event_resumes_deferred_callgraph_and_tier2() {
        let _env_guard = force_async_callgraph_builds();
        CALLGRAPH_COLD_BUILD_SPAWN_COUNT.store(0, Ordering::SeqCst);
        let ctx = cold_build_context();
        let (tx, rx) = crossbeam_channel::unbounded();
        *ctx.semantic_index_rx().lock() = Some(rx);
        ctx.schedule_semantic_cold_seed_gate_for_configure();

        assert!(matches!(
            ctx.callgraph_store_for_ops(),
            CallgraphStoreAccess::Building
        ));
        assert_eq!(CALLGRAPH_COLD_BUILD_SPAWN_COUNT.load(Ordering::SeqCst), 0);
        tx.send(SemanticIndexEvent::Ready(empty_semantic_index_for_ctx(
            &ctx,
        )))
        .expect("send ready event");

        crate::runtime_drain::drain_semantic_index_events(&ctx);

        assert!(
            !ctx.semantic_cold_seed_active(),
            "semantic Ready must clear the scheduled cold gate"
        );
        assert!(
            ctx.tier2_pull_demand_pending(),
            "semantic Ready must resume deferred Tier-2 work"
        );
        assert_eq!(
            CALLGRAPH_COLD_BUILD_SPAWN_COUNT.load(Ordering::SeqCst),
            1,
            "semantic Ready must resume the deferred callgraph warm"
        );
        let rx = ctx
            .callgraph_store_rx
            .lock()
            .as_ref()
            .cloned()
            .expect("ready resume should install an in-flight callgraph receiver");
        rx.recv_timeout(Duration::from_secs(30))
            .expect("background cold build should complete");
        *ctx.callgraph_store_rx.lock() = None;
    }

    #[test]
    fn semantic_gate_cleared_event_resumes_deferred_callgraph_and_tier2() {
        let _env_guard = force_async_callgraph_builds();
        CALLGRAPH_COLD_BUILD_SPAWN_COUNT.store(0, Ordering::SeqCst);
        let ctx = cold_build_context();
        ctx.schedule_semantic_cold_seed_gate_for_configure();

        assert!(matches!(
            ctx.callgraph_store_for_ops(),
            CallgraphStoreAccess::Building
        ));
        assert_eq!(CALLGRAPH_COLD_BUILD_SPAWN_COUNT.load(Ordering::SeqCst), 0);
        ctx.resume_deferred_work_after_semantic_cold_seed_gate_cleared();

        assert!(
            !ctx.semantic_cold_seed_active(),
            "cached-load or retry-wait clear must reopen the semantic cold gate"
        );
        assert!(
            ctx.tier2_pull_demand_pending(),
            "cached-load or retry-wait clear must resume deferred Tier-2 work"
        );
        assert_eq!(
            CALLGRAPH_COLD_BUILD_SPAWN_COUNT.load(Ordering::SeqCst),
            1,
            "cached-load or retry-wait clear must resume deferred callgraph warm"
        );
        let rx = ctx
            .callgraph_store_rx
            .lock()
            .as_ref()
            .cloned()
            .expect("gate-clear resume should install an in-flight callgraph receiver");
        rx.recv_timeout(Duration::from_secs(30))
            .expect("background cold build should complete");
        *ctx.callgraph_store_rx.lock() = None;
    }

    #[test]
    fn semantic_cold_seed_gate_defers_callgraph_cold_spawn_until_resume() {
        let _env_guard = force_async_callgraph_builds();
        CALLGRAPH_COLD_BUILD_SPAWN_COUNT.store(0, Ordering::SeqCst);
        let ctx = cold_build_context();

        ctx.set_semantic_cold_seed_active_for_test(true);
        assert!(
            matches!(
                ctx.callgraph_store_for_ops(),
                CallgraphStoreAccess::Building
            ),
            "callgraph ops should degrade as building while the semantic cold gate is active"
        );
        assert_eq!(
            CALLGRAPH_COLD_BUILD_SPAWN_COUNT.load(Ordering::SeqCst),
            0,
            "semantic cold gate must not spawn a competing callgraph cold build"
        );
        assert!(ctx.semantic_callgraph_warm_deferred_for_test());

        ctx.clear_semantic_cold_seed_gate_and_resume_deferred_work();
        assert_eq!(
            CALLGRAPH_COLD_BUILD_SPAWN_COUNT.load(Ordering::SeqCst),
            1,
            "clearing the semantic cold gate should resume the deferred callgraph warm"
        );

        let rx = ctx
            .callgraph_store_rx
            .lock()
            .as_ref()
            .cloned()
            .expect("deferred warm should install an in-flight receiver");
        rx.recv_timeout(Duration::from_secs(30))
            .expect("background cold build should complete");
        *ctx.callgraph_store_rx.lock() = None;
    }

    #[test]
    fn semantic_cold_seed_gate_clear_requests_tier2_pull() {
        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        ctx.schedule_semantic_cold_seed_gate_for_configure();

        ctx.resume_deferred_work_after_semantic_cold_seed_gate_cleared();

        assert!(
            !ctx.semantic_cold_seed_active(),
            "retry-wait or cached-load events must reopen the semantic cold gate"
        );
        assert!(
            ctx.tier2_pull_demand_pending(),
            "clearing the semantic cold gate should kick a Tier-2 pull refresh"
        );
    }

    #[test]
    fn semantic_failed_event_clears_scheduled_gate_and_requests_tier2_pull() {
        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        let (tx, rx) = crossbeam_channel::unbounded();
        *ctx.semantic_index_rx().lock() = Some(rx);
        ctx.schedule_semantic_cold_seed_gate_for_configure();
        tx.send(SemanticIndexEvent::Failed(
            "embedding backend failed".to_string(),
        ))
        .expect("send failed event");

        crate::runtime_drain::drain_semantic_index_events(&ctx);

        assert!(
            !ctx.semantic_cold_seed_active(),
            "semantic Failed must clear the scheduled cold gate"
        );
        assert!(
            ctx.tier2_pull_demand_pending(),
            "semantic Failed must resume deferred Tier-2 work"
        );
    }

    #[test]
    fn semantic_disconnect_clears_scheduled_gate_and_requests_tier2_pull() {
        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        let (tx, rx) = crossbeam_channel::unbounded::<SemanticIndexEvent>();
        *ctx.semantic_index_rx().lock() = Some(rx);
        ctx.schedule_semantic_cold_seed_gate_for_configure();
        drop(tx);

        crate::runtime_drain::drain_semantic_index_events(&ctx);

        assert!(
            !ctx.semantic_cold_seed_active(),
            "semantic worker disconnect must clear the scheduled cold gate"
        );
        assert!(
            ctx.tier2_pull_demand_pending(),
            "semantic worker disconnect must resume deferred Tier-2 work"
        );
    }

    #[test]
    fn semantic_cold_seed_gate_is_per_context_for_tier2_scheduler() {
        let ctx_a = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        let ctx_b = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        let base = Instant::now();
        ctx_a.reset_tier2_refresh_scheduler_at(base);
        ctx_b.reset_tier2_refresh_scheduler_at(base);
        ctx_a.set_semantic_cold_seed_active_for_test(true);

        assert_eq!(
            ctx_a.tick_tier2_refresh_scheduler_at(
                base + crate::inspect::tier2_scheduler::TIER2_REFRESH_COLD_CACHE_DELAY,
                0,
            ),
            None,
            "root A should defer Tier-2 while its semantic cold seed is active"
        );
        assert_eq!(
            ctx_b.tick_tier2_refresh_scheduler_at(
                base + crate::inspect::tier2_scheduler::TIER2_REFRESH_COLD_CACHE_DELAY,
                0,
            ),
            Some(Tier2TriggerReason::ConfigureWarm),
            "root B must not inherit root A's semantic cold gate"
        );
    }

    #[test]
    fn inline_wait_settled_event_clears_superseded_receiver() {
        let _env_guard = callgraph_build_wait_ms(2_000);
        let project = TempDir::new().expect("project tempdir");
        let storage = TempDir::new().expect("storage tempdir");
        std::fs::write(project.path().join("lib.rs"), "pub fn marker() {}\n").expect("source file");
        let project_root = std::fs::canonicalize(project.path()).expect("canonical project root");
        let ctx = Arc::new(AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(project.path().to_path_buf()),
                storage_dir: Some(storage.path().to_path_buf()),
                callgraph_chunk_size: 1,
                ..Config::default()
            },
        ));
        let (reached, release) = install_callgraph_build_start_gate(project_root);
        let request_ctx = Arc::clone(&ctx);
        let request = std::thread::spawn(move || request_ctx.callgraph_store_for_ops());
        reached
            .recv_timeout(Duration::from_secs(2))
            .expect("callgraph worker did not reach start barrier");

        ctx.next_callgraph_persist_epoch();
        release.send(()).unwrap();
        assert!(matches!(
            request.join().expect("callgraph request thread"),
            CallgraphStoreAccess::Building
        ));
        assert!(
            ctx.callgraph_store_rx().lock().is_none(),
            "inline Settled handling must retire the matching receiver"
        );
        assert!(
            ctx.callgraph_store()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_none(),
            "Settled must not reopen and install an older persisted store"
        );
    }

    #[test]
    fn inline_ready_without_published_pointer_settles_and_preserves_pending_paths() {
        let _env_guard = callgraph_build_wait_ms(2_000);
        let project = TempDir::new().expect("project tempdir");
        let storage = TempDir::new().expect("storage tempdir");
        std::fs::write(project.path().join("lib.rs"), "pub fn marker() {}\n").expect("source file");
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(project.path().to_path_buf()),
                storage_dir: Some(storage.path().to_path_buf()),
                callgraph_chunk_size: 1,
                ..Config::default()
            },
        );
        let pending = project.path().join("pending.rs");
        ctx.add_pending_callgraph_store_paths([pending.clone()]);
        REMOVE_CALLGRAPH_POINTER_BEFORE_INLINE_REOPEN.store(true, Ordering::SeqCst);
        let _remove_pointer_guard = RemoveCallgraphPointerBeforeInlineReopenGuard;

        assert!(matches!(
            ctx.callgraph_store_for_ops(),
            CallgraphStoreAccess::Building
        ));
        assert!(
            ctx.callgraph_store_rx().lock().is_none(),
            "inline Ready must settle after the published pointer disappears"
        );
        assert_eq!(
            ctx.take_pending_callgraph_store_paths(),
            vec![pending],
            "inline reopen failure must preserve pending watcher paths"
        );
    }

    #[test]
    fn take_pending_callgraph_store_paths_drops_paths_outside_current_root() {
        let project = TempDir::new().expect("project tempdir");
        let foreign = TempDir::new().expect("foreign tempdir");
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(project.path().to_path_buf()),
                ..Config::default()
            },
        );
        let inside = project.path().join("kept.rs");
        // A late-deferring batch from a superseded root writes into the shared
        // pending sink; replaying it into the NEW root's store would index a
        // foreign project's files.
        let outside = foreign.path().join("previous-root-file.rs");
        // Lexical escape: starts_with(project) is true on the raw spelling but
        // the path resolves outside the root.
        let dotdot_escape = project
            .path()
            .join("..")
            .join(
                foreign
                    .path()
                    .file_name()
                    .expect("foreign tempdir has a name"),
            )
            .join("escaped.rs");
        ctx.add_pending_callgraph_store_paths([inside.clone(), outside, dotdot_escape]);

        assert_eq!(
            ctx.take_pending_callgraph_store_paths(),
            vec![inside],
            "pending replay must drop foreign and dot-dot-escaping paths"
        );
    }

    #[test]
    fn watcher_gap_invalidation_keeps_semantic_reloadable_and_skips_readonly_force_token() {
        let project = TempDir::new().expect("project tempdir");
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(project.path().to_path_buf()),
                semantic_search: true,
                ..Config::default()
            },
        );
        ctx.set_canonical_cache_root(project.path().to_path_buf());
        // Read-only root: a force token could only be fulfilled by a local
        // writer build, which this root will never run.
        ctx.set_cache_writer_capabilities(false, true);
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::ready();

        ctx.invalidate_artifacts_after_watcher_gap();

        assert!(
            matches!(
                &*ctx
                    .semantic_index_status()
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                SemanticIndexStatus::Ready { .. }
            ),
            "semantic-enabled root must stay reloadable (Disabled has no self-healing path)"
        );
        assert_eq!(
            ctx.pending_callgraph_store_force_token(),
            None,
            "read-only root must not be stuck behind an unfulfillable force token"
        );
    }

    #[test]
    fn watcher_gap_invalidation_marks_force_rebuild_for_writer_roots() {
        let project = TempDir::new().expect("project tempdir");
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(project.path().to_path_buf()),
                ..Config::default()
            },
        );
        ctx.set_canonical_cache_root(project.path().to_path_buf());
        ctx.set_cache_writer_capabilities(true, true);

        ctx.invalidate_artifacts_after_watcher_gap();

        assert!(
            ctx.pending_callgraph_store_force_token().is_some(),
            "writer roots must still reconcile the store after the unobserved interval"
        );
        assert!(
            matches!(
                &*ctx
                    .semantic_index_status()
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                SemanticIndexStatus::Disabled
            ),
            "semantic-disabled config maps to Disabled status"
        );
    }

    #[cfg(unix)]
    #[test]
    fn take_pending_callgraph_store_paths_drops_symlink_dotdot_escape() {
        let project = TempDir::new().expect("project tempdir");
        let foreign = TempDir::new().expect("foreign tempdir");
        std::fs::create_dir_all(foreign.path().join("dir")).expect("foreign dir");
        std::fs::write(foreign.path().join("secret.rs"), "pub fn s() {}\n").expect("secret");
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(project.path().to_path_buf()),
                ..Config::default()
            },
        );
        // `root/link` targets a foreign directory; `root/link/../secret.rs`
        // therefore resolves to `foreign/secret.rs` under filesystem-first
        // semantics (matching the store's normalize_file_path). A lexical-first
        // filter would erase `link/..` and wrongly keep it as `root/secret.rs`.
        std::os::unix::fs::symlink(foreign.path().join("dir"), project.path().join("link"))
            .expect("plant symlink");
        let escape = project.path().join("link").join("..").join("secret.rs");
        // Dead component below the symlink: full canonicalization fails, so
        // the ancestor walk must reach and resolve `link` BEFORE any lexical
        // `..` resolution — a lexical-first pass would erase `dead/../..` and
        // wrongly keep this as `root/deep-secret.rs`.
        let dead_component_escape = project
            .path()
            .join("link")
            .join("dead")
            .join("..")
            .join("..")
            .join("deep-secret.rs");
        // Re-entry: `dead/..` drains back to the project root, then `link`
        // (an EXISTING symlink) must resolve through the filesystem — a
        // one-shot lexical pass over the dead tail would erase `link/..` too
        // and wrongly keep this as `root/reentry-secret.rs`.
        std::fs::write(foreign.path().join("reentry-secret.rs"), "pub fn r() {}\n")
            .expect("reentry secret");
        let reentry_escape = project
            .path()
            .join("dead")
            .join("..")
            .join("link")
            .join("..")
            .join("reentry-secret.rs");
        // Dangling symlink whose `..` re-enters the root: the store cannot
        // canonicalize it either and keeps the raw absolute spelling as an
        // out-of-root key, so containment must fail closed (a repaired-target
        // race could otherwise index outside the root).
        std::os::unix::fs::symlink(
            foreign.path().join("nonexistent-target"),
            project.path().join("dangling"),
        )
        .expect("plant dangling symlink");
        let dangling_reentry = project
            .path()
            .join("dangling")
            .join("..")
            .join("via-dangling.rs");
        // `..` traversal through a regular file: realpath rejects with
        // ENOTDIR; lexically popping the file would fabricate containment.
        std::fs::write(project.path().join("plain.rs"), "pub fn p() {}\n").expect("plain file");
        let through_file = project
            .path()
            .join("plain.rs")
            .join("..")
            .join("via-file.rs");
        let kept = project.path().join("kept.rs");
        ctx.add_pending_callgraph_store_paths([
            escape,
            dead_component_escape,
            reentry_escape,
            dangling_reentry,
            through_file,
            kept.clone(),
        ]);

        assert_eq!(
            ctx.take_pending_callgraph_store_paths(),
            vec![kept],
            "symlink-plus-dotdot escapes must be dropped with filesystem-first semantics"
        );
    }

    #[cfg(windows)]
    #[test]
    fn take_pending_callgraph_store_paths_drops_drive_relative_paths() {
        // Guard-sensitivity: exercise the classifier directly against a root
        // ON THE DRIVE CWD's drive, where join() replaces the root and the
        // joined path can genuinely resolve under the drive CWD — without the
        // early Prefix/RootDir rejection, a `C:file-under-cwd` spelling whose
        // drive CWD happens to sit inside the root would pass the post-join
        // prefix check.
        let cwd = std::env::current_dir().expect("drive cwd");
        let cwd_file = PathBuf::from(format!(
            "{}under-drive-cwd.rs",
            cwd.components()
                .next()
                .map(|prefix| prefix.as_os_str().to_string_lossy().into_owned())
                .expect("drive prefix")
        ));
        assert!(cwd_file.is_relative(), "C:foo must classify as relative");
        assert!(
            !pending_path_in_roots(&cwd_file, &[cwd.clone()]),
            "drive-relative spelling must be rejected even when the drive CWD is inside the root"
        );
        assert!(
            !pending_path_in_roots(Path::new(r"\root-relative.rs"), &[cwd]),
            "root-relative spelling must be rejected"
        );

        let project = TempDir::new().expect("project tempdir");
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(project.path().to_path_buf()),
                ..Config::default()
            },
        );
        let kept = project.path().join("kept.rs");
        ctx.add_pending_callgraph_store_paths([
            PathBuf::from("C:drive-relative.rs"),
            PathBuf::from(r"\root-relative.rs"),
            kept.clone(),
        ]);

        assert_eq!(
            ctx.take_pending_callgraph_store_paths(),
            vec![kept],
            "drive-relative and root-relative spellings must be rejected"
        );
    }

    #[test]
    fn take_pending_callgraph_store_paths_keeps_relative_and_deleted_paths() {
        let project = TempDir::new().expect("project tempdir");
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(project.path().to_path_buf()),
                ..Config::default()
            },
        );
        // Relative paths are project-root-relative by the callgraph store's
        // contract, and pending paths legitimately reference deleted files.
        let relative = PathBuf::from("src/relative.rs");
        let deleted = project.path().join("never-created.rs");
        ctx.add_pending_callgraph_store_paths([relative.clone(), deleted.clone()]);

        let mut taken = ctx.take_pending_callgraph_store_paths();
        taken.sort();
        let mut expected = vec![relative, deleted];
        expected.sort();
        assert_eq!(
            taken, expected,
            "root-relative and deleted in-root paths must survive the filter"
        );
    }

    #[test]
    fn concurrent_cold_callgraph_store_for_ops_spawns_one_build() {
        let _env_guard = force_async_callgraph_builds();
        CALLGRAPH_COLD_BUILD_SPAWN_COUNT.store(0, Ordering::SeqCst);

        let project = TempDir::new().expect("project tempdir");
        let storage = TempDir::new().expect("storage tempdir");
        let source_dir = project.path().join("src");
        std::fs::create_dir_all(&source_dir).expect("source dir");
        std::fs::write(
            source_dir.join("lib.rs"),
            "pub fn caller() { callee(); }\npub fn callee() {}\n",
        )
        .expect("source file");

        let ctx = Arc::new(AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(project.path().to_path_buf()),
                storage_dir: Some(storage.path().to_path_buf()),
                callgraph_chunk_size: 1,
                ..Config::default()
            },
        ));

        let barrier = Arc::new(Barrier::new(3));
        let handles = (0..2)
            .map(|_| {
                let ctx = Arc::clone(&ctx);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    matches!(
                        ctx.callgraph_store_for_ops(),
                        CallgraphStoreAccess::Building | CallgraphStoreAccess::Ready(_)
                    )
                })
            })
            .collect::<Vec<_>>();

        barrier.wait();
        for handle in handles {
            assert!(
                handle.join().expect("callgraph caller thread"),
                "cold callgraph ops should report Building or observe the installed store"
            );
        }

        assert_eq!(
            CALLGRAPH_COLD_BUILD_SPAWN_COUNT.load(Ordering::SeqCst),
            1,
            "concurrent cold callers must share one background build"
        );

        let rx = ctx
            .callgraph_store_rx
            .lock()
            .as_ref()
            .cloned()
            .expect("in-flight receiver installed before spawn");
        rx.recv_timeout(Duration::from_secs(30))
            .expect("background cold build should complete");
        *ctx.callgraph_store_rx.lock() = None;
    }

    #[test]
    fn watcher_gap_invalidation_gates_resident_artifacts_and_forces_strict_verify() {
        let root = TempDir::new().expect("project tempdir");
        let canonical_root = std::fs::canonicalize(root.path()).expect("canonical project root");
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(canonical_root.clone()),
                ..Config::default()
            },
        );
        *ctx.search_index
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(SearchIndex::build(&canonical_root));
        *ctx.semantic_index
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(SemanticIndex::new(canonical_root.clone(), 3));
        *ctx.semantic_index_status
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::ready();

        let artifact = canonical_root.join("verify-artifact.bin");
        std::fs::write(&artifact, b"same-size").expect("write verification artifact");
        let generation =
            crate::cache_freshness::artifact_generation(&artifact).expect("artifact generation");
        crate::cache_freshness::record_verify_completed(
            &canonical_root,
            crate::cache_freshness::VerifyArtifact::Search,
            Some(generation),
        );
        assert_eq!(
            crate::cache_freshness::warm_verify_plan(
                &canonical_root,
                crate::cache_freshness::VerifyArtifact::Search,
                Some(generation),
            ),
            crate::cache_freshness::WarmVerifyPlan::Skip
        );

        ctx.invalidate_artifacts_after_watcher_gap();

        assert!(ctx
            .search_index
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_none());
        assert!(ctx
            .semantic_index
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_none());
        assert!(ctx.pending_callgraph_store_force_token().is_some());
        assert_eq!(
            crate::cache_freshness::warm_verify_plan(
                &canonical_root,
                crate::cache_freshness::VerifyArtifact::Search,
                Some(generation),
            ),
            crate::cache_freshness::WarmVerifyPlan::Strict
        );
    }

    #[test]
    fn cancelled_semantic_refresh_transfers_refreshing_files_to_pending() {
        let root = TempDir::new().expect("project tempdir");
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(root.path().to_path_buf()),
                semantic_search: true,
                ..Config::default()
            },
        );
        *ctx.semantic_index
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(SemanticIndex::new(root.path().to_path_buf(), 3));
        let refreshing_path = root.path().join("src/lib.rs");
        {
            let mut status = ctx
                .semantic_index_status
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *status = SemanticIndexStatus::ready();
            status.start_refreshing_file(refreshing_path.clone());
        }
        let (request_tx, _request_rx) = crossbeam_channel::unbounded();
        let (_event_tx, event_rx) = crossbeam_channel::unbounded();
        ctx.install_semantic_refresh_worker_for_build_epoch(
            request_tx,
            event_rx,
            Arc::new(Mutex::new(None)),
            ctx.semantic_index_rx_epoch(),
        );

        ctx.cancel_unbound_artifact_work();

        // The cancelled worker will never re-embed the in-flight file; the
        // retained pending set is the only record for the replacement worker.
        assert_eq!(
            ctx.pending_semantic_index_paths
                .lock()
                .iter()
                .cloned()
                .collect::<Vec<_>>(),
            vec![refreshing_path],
            "cancelled in-flight refresh files must transfer to the pending set"
        );
        assert!(matches!(
            &*ctx
                .semantic_index_status
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            SemanticIndexStatus::Ready { refreshing, .. } if refreshing.is_empty()
        ));
    }

    #[test]
    fn unbind_before_corpus_started_preserves_corpus_intent() {
        // The probe stamps `refreshing_corpus` before sending, but the worker
        // emits CorpusStarted only after walking the project. An unbind in
        // that window must re-derive the corpus intent from the stamped
        // status, not lose it.
        let root = TempDir::new().expect("project tempdir");
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(root.path().to_path_buf()),
                semantic_search: true,
                ..Config::default()
            },
        );
        *ctx.semantic_index
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(SemanticIndex::new(root.path().to_path_buf(), 3));
        *ctx.semantic_index_status
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Building {
            stage: "refreshing_corpus".to_string(),
            files: None,
            entries_done: None,
            entries_total: None,
        };
        let (request_tx, _request_rx) = crossbeam_channel::unbounded();
        let (_event_tx, event_rx) = crossbeam_channel::unbounded();
        ctx.install_semantic_refresh_worker_for_build_epoch(
            request_tx,
            event_rx,
            Arc::new(Mutex::new(None)),
            ctx.semantic_index_rx_epoch(),
        );

        ctx.cancel_unbound_artifact_work();

        assert!(
            *ctx.pending_semantic_corpus_refresh.lock(),
            "corpus intent stamped before CorpusStarted must survive the cancellation"
        );
    }

    #[test]
    fn cancelled_search_corpus_refresh_drops_nonready_resident_index() {
        let root = TempDir::new().expect("project tempdir");
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(root.path().to_path_buf()),
                ..Config::default()
            },
        );
        // A corpus refresh in flight: resident index marked non-ready plus an
        // installed receiver. Cancelling only the receiver would strand the
        // non-ready resident (equivalent rebind reloads only a MISSING index).
        let mut refreshing = SearchIndex::new();
        refreshing.ready = false;
        *ctx.search_index
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(refreshing);
        let (_tx, rx) = crossbeam_channel::unbounded();
        ctx.install_search_index_rx(rx, ctx.configure_generation());

        ctx.cancel_unbound_artifact_work();

        assert!(
            ctx.search_index
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_none(),
            "a cancelled corpus refresh must drop the non-ready resident so rebind reloads it"
        );
        assert!(ctx
            .search_index_rx
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_none());
    }

    #[test]
    fn active_semantic_file_refresh_blocks_idle_eviction_until_completion() {
        let root = TempDir::new().expect("project tempdir");
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(root.path().to_path_buf()),
                ..Config::default()
            },
        );
        *ctx.semantic_index
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(SemanticIndex::new(root.path().to_path_buf(), 3));
        let refreshing_path = root.path().join("src/lib.rs");
        {
            let mut status = ctx
                .semantic_index_status
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *status = SemanticIndexStatus::ready();
            status.start_refreshing_file(refreshing_path.clone());
        }

        assert!(ctx.artifact_eviction_blocked());
        assert!(!ctx.evict_idle_artifacts());
        assert!(ctx
            .semantic_index
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some());

        ctx.semantic_index_status
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .complete_refreshing_file(&refreshing_path);
        assert!(ctx.evict_idle_artifacts());
        assert!(ctx
            .semantic_index
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_none());
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

    #[test]
    fn progress_sender_slot_is_per_context_for_shared_app() {
        let app = App::default_shared();
        let ctx_a = AppContext::from_app(Arc::clone(&app), Config::default());
        let ctx_b = AppContext::from_app(app, Config::default());
        let (tx_a, rx_a) = mpsc::channel();
        let (tx_b, rx_b) = mpsc::channel();

        ctx_a.set_progress_sender(Some(Arc::new(Box::new(move |frame| {
            let _ = tx_a.send(frame);
        }))));
        ctx_b.set_progress_sender(Some(Arc::new(Box::new(move |frame| {
            let _ = tx_b.send(frame);
        }))));

        ctx_a.emit_progress(ProgressFrame {
            frame_type: "progress",
            request_id: "ctx-a".to_string(),
            kind: crate::protocol::ProgressKind::Stdout,
            chunk: "a".to_string(),
        });
        ctx_b.emit_progress(ProgressFrame {
            frame_type: "progress",
            request_id: "ctx-b".to_string(),
            kind: crate::protocol::ProgressKind::Stdout,
            chunk: "b".to_string(),
        });

        match rx_a
            .recv_timeout(Duration::from_millis(50))
            .expect("ctx A progress frame")
        {
            PushFrame::Progress(frame) => assert_eq!(frame.request_id, "ctx-a"),
            other => panic!("unexpected frame for ctx A: {other:?}"),
        }
        assert!(rx_a.try_recv().is_err());

        match rx_b
            .recv_timeout(Duration::from_millis(50))
            .expect("ctx B progress frame")
        {
            PushFrame::Progress(frame) => assert_eq!(frame.request_id, "ctx-b"),
            other => panic!("unexpected frame for ctx B: {other:?}"),
        }
        assert!(rx_b.try_recv().is_err());
    }
}

#[cfg(test)]
mod health_warming_honesty_tests {
    use super::*;
    use crate::parser::TreeSitterProvider;

    fn ctx_with_config(config: Config) -> AppContext {
        AppContext::new(Box::new(TreeSitterProvider::new()), config)
    }

    fn health_search_status(ctx: &AppContext) -> &'static str {
        let root = std::path::Path::new("/tmp/health-warming-honesty-test");
        ctx.try_health_snapshot(root)
            .search_index
            .expect("search_index component present")
            .status
    }

    fn health_tier2_status(ctx: &AppContext) -> &'static str {
        let root = std::path::Path::new("/tmp/health-warming-honesty-test");
        ctx.try_health_snapshot(root)
            .tier2
            .expect("tier2 component present")
            .status
    }

    #[test]
    fn write_denied_search_index_reports_ready_not_building() {
        // A write-denied cold build installs an empty index that is flagged
        // build-denied and stays not-ready (so grep keeps the fallback walk).
        // Health must treat it as settled, not "building" forever.
        let config = Config {
            search_index: true,
            ..Config::default()
        };
        let ctx = ctx_with_config(config);
        let mut index = SearchIndex::new();
        index.build_denied = true;
        *ctx.search_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);

        assert_eq!(
            health_search_status(&ctx),
            "ready",
            "a build-denied index is a terminal settled state and must not report building forever"
        );
    }

    #[test]
    fn in_progress_search_index_still_reports_building() {
        // Control: a genuinely not-ready, not-denied index (a real build in
        // flight) must still report building — the build-denied carve-out must
        // not leak into ordinary in-progress builds.
        let config = Config {
            search_index: true,
            ..Config::default()
        };
        let ctx = ctx_with_config(config);
        let index = SearchIndex::new(); // ready=false, build_denied=false
        *ctx.search_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);

        assert_eq!(health_search_status(&ctx), "building");
    }

    #[test]
    fn tier2_blocked_on_callgraph_reports_ready_not_building() {
        // dead_code is suppressed (None) while the callgraph store is not ready,
        // but unused_exports/duplicates are complete and fresh. Health must not
        // report tier2 as "building" forever for a cycle that is otherwise
        // complete — the callgraph component tells the callgraph story.
        let ctx = ctx_with_config(Config::default()); // inspect.enabled defaults true
        ctx.update_status_bar_tier2(None, Some(3), Some(2), None, false);
        ctx.set_status_bar_tier2_dead_code_blocked_on_callgraph(true);

        assert_eq!(
            health_tier2_status(&ctx),
            "ready",
            "tier2 complete except dead_code-blocked-on-callgraph must not stay building"
        );
    }

    #[test]
    fn tier2_missing_dead_code_without_callgraph_block_reports_building() {
        // Control: with no callgraph block recorded, a missing dead_code count is
        // a genuine in-progress scan and must still report building.
        let ctx = ctx_with_config(Config::default());
        ctx.update_status_bar_tier2(None, Some(3), Some(2), None, false);
        ctx.set_status_bar_tier2_dead_code_blocked_on_callgraph(false);

        assert_eq!(health_tier2_status(&ctx), "building");
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
    fn changing_root_clears_project_scoped_status_counts() {
        let temp = tempfile::tempdir().expect("tempdir");
        let first_root = temp.path().join("first");
        let second_root = temp.path().join("second");
        std::fs::create_dir_all(&first_root).expect("create first root");
        std::fs::create_dir_all(&second_root).expect("create second root");
        let ctx = ctx();
        ctx.set_canonical_cache_root(first_root);
        ctx.update_status_bar_tier2(Some(5), Some(3), Some(7), Some(2), false);
        assert!(ctx.status_bar_counts().is_some());

        ctx.set_canonical_cache_root(second_root);

        assert!(
            ctx.status_bar_counts().is_none(),
            "counts from the previous root must not appear in a newly bound root"
        );
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

    #[test]
    fn status_bar_filtered_counts_ignore_environmental_flap() {
        use crate::lsp::diagnostics::{DiagnosticSeverity, StoredDiagnostic};
        use crate::lsp::registry::ServerKind;
        use crate::lsp::roots::ServerKey;

        let ctx = ctx();
        let root = if cfg!(windows) {
            std::path::PathBuf::from(r"C:\proj")
        } else {
            std::path::PathBuf::from("/proj")
        };
        ctx.set_canonical_cache_root(root.clone());
        ctx.update_status_bar_tier2(Some(0), Some(0), Some(0), Some(0), false);

        let file = root.join("aft.jsonc");
        let key = ServerKey {
            kind: ServerKind::TypeScript,
            root: root.clone(),
        };
        let env = StoredDiagnostic {
            file: file.clone(),
            line: 1,
            column: 1,
            end_line: 1,
            end_column: 2,
            severity: DiagnosticSeverity::Error,
            message: "Failed to load schema from https://example.com/schema.json".into(),
            code: None,
            source: Some("json".into()),
        };

        assert_eq!(ctx.status_bar_counts().expect("populated").errors, 0);

        {
            let mut lsp = ctx.lsp();
            lsp.diagnostics_store_mut_for_test()
                .publish(key.clone(), file.clone(), vec![env]);
        }
        assert_eq!(
            ctx.status_bar_counts().expect("populated").errors,
            0,
            "environmental publish must not change status-bar E"
        );

        {
            let mut lsp = ctx.lsp();
            lsp.diagnostics_store_mut_for_test()
                .publish(key, file, vec![]);
        }
        assert_eq!(
            ctx.status_bar_counts().expect("populated").errors,
            0,
            "environmental clear must not change status-bar E"
        );
    }
}

#[cfg(test)]
mod harness_path_tests {
    use super::*;
    use crate::harness::Harness;
    use crate::parser::TreeSitterProvider;

    fn ctx_with_storage_and_harness(storage_dir: PathBuf, harness: Harness) -> AppContext {
        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        ctx.update_config(|config| {
            config.storage_dir = Some(storage_dir);
        });
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

    #[test]
    fn callgraph_and_inspect_dirs_are_root_keyed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let storage = temp.path().join("storage");
        let root = temp.path().join("checkout");
        std::fs::create_dir_all(&root).expect("create root");
        let ctx = ctx_with_storage_and_harness(storage.clone(), Harness::Opencode);
        ctx.set_canonical_cache_root(root.clone());

        assert_eq!(
            ctx.callgraph_store_dir(),
            storage
                .join("callgraph")
                .join(crate::search_index::artifact_cache_key(&root))
        );
        assert_eq!(
            ctx.inspect_dir(),
            storage
                .join("inspect")
                .join(crate::path_identity::project_scope_key(&root))
        );
        assert!(!ctx
            .callgraph_store_dir()
            .starts_with(storage.join("opencode")));
        assert!(!ctx.inspect_dir().starts_with(storage.join("opencode")));
    }

    #[test]
    fn per_domain_capability_allows_inspect_writer_when_callgraph_read_only() {
        let storage = PathBuf::from("/tmp/cortexkit/aft");
        let ctx = ctx_with_storage_and_harness(storage, Harness::Opencode);
        ctx.set_cache_writer_capabilities(false, true);

        assert!(ctx.shared_artifacts_read_only());
        assert!(!ctx.callgraph_writer());
        assert!(ctx.inspect_writer());
    }
}

#[cfg(test)]
mod shared_db_tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn app_contexts_share_one_database_connection() {
        let storage = tempdir().expect("storage tempdir");
        let root_one = tempdir().expect("first root tempdir");
        let root_two = tempdir().expect("second root tempdir");
        let app = App::default_shared();
        let ctx_one = AppContext::from_app(
            Arc::clone(&app),
            Config {
                project_root: Some(root_one.path().to_path_buf()),
                ..Config::default()
            },
        );
        let ctx_two = AppContext::from_app(
            Arc::clone(&app),
            Config {
                project_root: Some(root_two.path().to_path_buf()),
                ..Config::default()
            },
        );
        let path = storage.path().join("aft.db");

        let first = app.open_db(&path).expect("open shared database");
        let second = app.open_db(&path).expect("reuse shared database");

        assert!(Arc::ptr_eq(&first, &second));
        assert!(Arc::ptr_eq(
            &ctx_one.db().expect("first context database"),
            &ctx_two.db().expect("second context database")
        ));
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
    /// `rebuild_gitignore` loads git's global excludes via the `ignore`
    /// crate, which discovers them from TWO places: `core.excludesfile` in
    /// `$HOME/.gitconfig` (or `$XDG_CONFIG_HOME/git/config`), and the default
    /// `$XDG_CONFIG_HOME/git/ignore` / `$HOME/.config/git/ignore` locations.
    /// A developer machine commonly has one of these, so a "no project ignore
    /// → None" assertion is only deterministic when BOTH discovery roots point
    /// at an empty directory — neutralizing only `XDG_CONFIG_HOME` still finds
    /// a `~/.gitconfig` `core.excludesfile`. Serialized on the process-wide
    /// env lock shared with every other HOME-mutating test; env is restored
    /// before the closure result is used.
    fn with_neutralized_global_gitignore<R>(f: impl FnOnce() -> R) -> R {
        let _guard = crate::test_env::process_env_lock();
        let tmp = TempDir::new().unwrap();
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        let prev_home = std::env::var_os("HOME");
        let prev_userprofile = std::env::var_os("USERPROFILE");
        // SAFETY: serialized by the process env lock; restored immediately
        // after `f`.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", tmp.path());
            std::env::set_var("HOME", tmp.path());
            std::env::set_var("USERPROFILE", tmp.path());
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        unsafe {
            match prev_xdg {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match prev_userprofile {
                Some(v) => std::env::set_var("USERPROFILE", v),
                None => std::env::remove_var("USERPROFILE"),
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

#[cfg(test)]
mod verify_memo_watcher_tests {
    use super::*;

    #[test]
    fn pending_watcher_path_invalidates_root_verify_memo() {
        let root_dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root_dir.path()).unwrap();
        let artifact = root.join("cache.bin");
        std::fs::write(&artifact, b"generation").unwrap();
        let generation = crate::cache_freshness::artifact_generation(&artifact).unwrap();
        crate::cache_freshness::record_verify_completed(
            &root,
            crate::cache_freshness::VerifyArtifact::Search,
            Some(generation),
        );
        assert_eq!(
            crate::cache_freshness::warm_verify_plan(
                &root,
                crate::cache_freshness::VerifyArtifact::Search,
                Some(generation),
            ),
            crate::cache_freshness::WarmVerifyPlan::Skip
        );

        let ctx = AppContext::from_app(
            App::default_shared(),
            Config {
                project_root: Some(root.clone()),
                ..Config::default()
            },
        );
        ctx.set_canonical_cache_root(root.clone());
        ctx.add_pending_search_index_paths([root.join("changed.rs")]);
        assert_eq!(
            crate::cache_freshness::warm_verify_plan(
                &root,
                crate::cache_freshness::VerifyArtifact::Search,
                Some(generation),
            ),
            crate::cache_freshness::WarmVerifyPlan::StatFirst
        );
    }
}

#[cfg(test)]
mod watcher_runtime_state_tests {
    use super::*;
    use crate::language::StubProvider;

    fn test_context() -> AppContext {
        AppContext::new(Box::new(StubProvider), Config::default())
    }

    #[test]
    fn finished_watcher_thread_reports_inactive_and_is_reclaimed_with_invalidation() {
        let root = tempfile::tempdir().expect("project tempdir");
        let canonical_root = std::fs::canonicalize(root.path()).expect("canonical root");
        let ctx = AppContext::new(
            Box::new(StubProvider),
            Config {
                project_root: Some(canonical_root.clone()),
                ..Config::default()
            },
        );
        ctx.set_canonical_cache_root(canonical_root.clone());
        // Suppress the physical FSEvents reinstall (parallel in-process tests
        // must not install real OS watchers); the property under test is the
        // corpse reclaim + invalidation, not the reinstall.
        struct DisableWatcherGuard;
        impl Drop for DisableWatcherGuard {
            fn drop(&mut self) {
                unsafe { std::env::remove_var("AFT_TEST_DISABLE_FILE_WATCHER") };
            }
        }
        let _env_lock = crate::test_env::process_env_lock();
        unsafe { std::env::set_var("AFT_TEST_DISABLE_FILE_WATCHER", "1") };
        let _disable_watcher = DisableWatcherGuard;
        // Warm state the corpse reclaim must invalidate: resident index +
        // warm Skip memo.
        *ctx.search_index
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(crate::search_index::SearchIndex::new());
        let artifact = canonical_root.join("artifact.bin");
        std::fs::write(&artifact, b"artifact").expect("artifact");
        let generation = crate::cache_freshness::artifact_generation(&artifact);
        crate::cache_freshness::record_verify_completed(
            &canonical_root,
            crate::cache_freshness::VerifyArtifact::Search,
            generation,
        );

        let (dispatch_tx, dispatch_rx) = crate::watcher_filter::watcher_dispatch_channel();
        let _dispatch_tx = dispatch_tx;
        // A thread that exits on its own models a backend failure while the
        // root was unbound (drains suppressed, queued error undrained).
        let join = std::thread::spawn(|| {});
        ctx.install_watcher_runtime(
            dispatch_rx,
            WatcherThreadHandle::new(Arc::new(AtomicBool::new(false)), join),
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while ctx.watcher_runtime_active() {
            assert!(
                std::time::Instant::now() < deadline,
                "a finished watcher thread must report the runtime inactive"
            );
            std::thread::yield_now();
        }

        // The production entry point: rebind restoration must reclaim the
        // corpse, invalidate the unobserved-window state, and reinstall.
        crate::commands::configure::ensure_project_watcher(&ctx);

        assert!(
            ctx.search_index
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_none(),
            "corpse reclaim must drop resident artifacts (events since the failure are lost)"
        );
        assert_eq!(
            crate::cache_freshness::warm_verify_plan(
                &canonical_root,
                crate::cache_freshness::VerifyArtifact::Search,
                generation,
            ),
            crate::cache_freshness::WarmVerifyPlan::Strict,
            "corpse reclaim must force strict re-verification"
        );
        assert!(
            !ctx.take_finished_watcher_runtime(),
            "reclaim is one-shot; the corpse is gone after ensure_project_watcher"
        );
    }

    #[test]
    fn watcher_runtime_requires_both_thread_and_dispatch_receiver() {
        let ctx = test_context();
        let (dispatch_tx, dispatch_rx) = crate::watcher_filter::watcher_dispatch_channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);
        let join = std::thread::spawn(move || {
            while !thread_shutdown.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(1));
            }
            drop(dispatch_tx);
        });
        ctx.install_watcher_runtime(
            dispatch_rx,
            WatcherThreadHandle::new(Arc::clone(&shutdown), join),
        );
        assert!(ctx.watcher_runtime_active());

        *ctx.watcher_rx.lock() = None;
        assert!(
            !ctx.watcher_runtime_active(),
            "a thread without its dispatch receiver is not a usable watcher runtime"
        );
        ctx.stop_watcher_runtime();
    }
}

#[cfg(test)]
mod semantic_probe_tests {
    use super::*;

    #[test]
    fn cleared_semantic_worker_invalidates_orphaned_probe_timer() {
        let root = tempfile::tempdir().unwrap();
        let ctx = AppContext::new(
            default_language_provider_factory(),
            Config {
                project_root: Some(root.path().to_path_buf()),
                ..Config::default()
            },
        );
        let (request_tx, _request_rx) = crossbeam_channel::unbounded();
        let (_event_tx, event_rx) = crossbeam_channel::unbounded();
        let worker_slot = Arc::new(Mutex::new(None));
        ctx.install_semantic_refresh_worker_for_build_epoch(
            request_tx,
            event_rx,
            worker_slot,
            ctx.semantic_index_rx_epoch(),
        );

        ctx.ensure_semantic_refresh_probe_scheduled(Duration::from_millis(20));
        assert!(ctx.semantic_refresh_probe_is_scheduled());
        ctx.clear_semantic_refresh_worker();
        std::thread::sleep(Duration::from_millis(50));

        assert!(!ctx.semantic_refresh_probe_ready());
        assert!(!ctx.semantic_refresh_probe_is_scheduled());
        assert!(!ctx.completion_drains_have_work());
    }
}
