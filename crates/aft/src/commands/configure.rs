use std::fs;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
#[cfg(test)]
use std::sync::{Condvar, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use crossbeam_channel::unbounded;
use notify::{RecursiveMode, Watcher};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

use crate::cache_freshness::{self, VerifyArtifact, VerifyStrategy, WarmVerifyPlan};
use crate::config::{Config, SemanticBackendConfig};
use crate::context::{
    AppContext, CallgraphStoreAccess, ConfigureMaintenanceJob, SemanticIndexEvent,
    SemanticIndexStatus, SemanticRefreshEvent, SemanticRefreshRequest, SemanticRefreshWorkerSlot,
    SubcLifecycleAdmission,
};
use crate::harness::Harness;
use crate::log_ctx;
use crate::lsp::registry::{
    resolve_lsp_binary, resolve_server_binary, servers_for_file, ServerKind,
};
use crate::parser::{detect_language, LangId, SharedSymbolCache};
use crate::protocol::{RawRequest, Response};
use crate::search_index::{
    build_path_filters, current_git_head, resolve_cache_dir_with_key,
    walk_project_files_bounded_matching, CacheLock, SearchIndex,
};
use crate::semantic_index::{is_semantic_indexed_extension, SemanticIndex, SemanticIndexLock};
use crate::watcher_filter::{self, WatcherFilterConfig, WatcherThreadHandle};
use crate::{slog_debug, slog_info, slog_warn};

static WATCHER_GENERATION: AtomicU64 = AtomicU64::new(0);

static SEMANTIC_STALE_GENERATION_DISCARDS: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static SEMANTIC_STALE_GENERATION_SIGNAL: OnceLock<(Mutex<()>, Condvar)> = OnceLock::new();
static CONFIGURE_ARTIFACT_LOAD_ATTEMPTS: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static CONFIGURE_ARTIFACT_LOAD_CANCELLATIONS: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static CONFIGURE_DEFERRED_DELAY_REACHED: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static CONFIGURE_ARTIFACT_POST_GATE_REACHED: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static CONFIGURE_ARTIFACT_POST_GATE_DELAY_MS: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
thread_local! {
    static SEMANTIC_REFRESH_RESTART_ATTEMPTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static SEMANTIC_REFRESH_RESTART_RESULT_OVERRIDE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
    static WORKSPACE_MANIFEST_FINGERPRINT_SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn note_configure_artifact_load_attempt() {
    CONFIGURE_ARTIFACT_LOAD_ATTEMPTS.fetch_add(1, Ordering::SeqCst);
}

#[doc(hidden)]
pub fn reset_configure_artifact_load_attempts_for_test() {
    CONFIGURE_ARTIFACT_LOAD_ATTEMPTS.store(0, Ordering::SeqCst);
}

#[doc(hidden)]
pub fn configure_artifact_load_attempts_for_test() -> usize {
    CONFIGURE_ARTIFACT_LOAD_ATTEMPTS.load(Ordering::SeqCst)
}

#[cfg(test)]
fn note_configure_artifact_load_cancellation_for_test() {
    CONFIGURE_ARTIFACT_LOAD_CANCELLATIONS.fetch_add(1, Ordering::SeqCst);
}

#[cfg(test)]
fn reset_configure_artifact_load_cancellations_for_test() {
    CONFIGURE_ARTIFACT_LOAD_CANCELLATIONS.store(0, Ordering::SeqCst);
}

#[cfg(test)]
fn configure_artifact_load_cancellations_for_test() -> usize {
    CONFIGURE_ARTIFACT_LOAD_CANCELLATIONS.load(Ordering::SeqCst)
}

#[cfg(test)]
fn delay_configure_artifact_load_after_gate_for_test() {
    CONFIGURE_ARTIFACT_POST_GATE_REACHED.fetch_add(1, Ordering::SeqCst);
    let delay_ms = CONFIGURE_ARTIFACT_POST_GATE_DELAY_MS.load(Ordering::SeqCst);
    if delay_ms > 0 {
        std::thread::sleep(Duration::from_millis(delay_ms));
    }
}

#[cfg(not(test))]
fn delay_configure_artifact_load_after_gate_for_test() {}

#[cfg(test)]
fn reset_configure_deferred_delay_reached_for_test() {
    CONFIGURE_DEFERRED_DELAY_REACHED.store(0, Ordering::SeqCst);
}

#[cfg(test)]
fn configure_deferred_delay_reached_for_test() -> usize {
    CONFIGURE_DEFERRED_DELAY_REACHED.load(Ordering::SeqCst)
}

#[cfg(test)]
fn set_configure_artifact_post_gate_delay_for_test(delay_ms: u64) {
    CONFIGURE_ARTIFACT_POST_GATE_REACHED.store(0, Ordering::SeqCst);
    CONFIGURE_ARTIFACT_POST_GATE_DELAY_MS.store(delay_ms, Ordering::SeqCst);
}

#[cfg(test)]
fn configure_artifact_post_gate_reached_for_test() -> usize {
    CONFIGURE_ARTIFACT_POST_GATE_REACHED.load(Ordering::SeqCst)
}

#[doc(hidden)]
pub fn reset_semantic_stale_generation_discards_for_test() {
    SEMANTIC_STALE_GENERATION_DISCARDS.store(0, Ordering::SeqCst);
}

#[doc(hidden)]
pub fn semantic_stale_generation_discards_for_test() -> usize {
    SEMANTIC_STALE_GENERATION_DISCARDS.load(Ordering::SeqCst)
}

fn note_semantic_stale_generation_discard() {
    SEMANTIC_STALE_GENERATION_DISCARDS.fetch_add(1, Ordering::SeqCst);
    #[cfg(test)]
    if let Some((_, changed)) = SEMANTIC_STALE_GENERATION_SIGNAL.get() {
        changed.notify_all();
    }
}

#[cfg(test)]
fn wait_for_semantic_stale_generation_discard_for_test(timeout: Duration) -> bool {
    if semantic_stale_generation_discards_for_test() > 0 {
        return true;
    }
    let (lock, changed) = SEMANTIC_STALE_GENERATION_SIGNAL.get_or_init(Default::default);
    let guard = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (_guard, result) = changed
        .wait_timeout_while(guard, timeout, |_| {
            semantic_stale_generation_discards_for_test() == 0
        })
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    !result.timed_out() || semantic_stale_generation_discards_for_test() > 0
}

// The watcher already coalesces filesystem events for 250 ms. Keep a short
// worker window for corpus-build replay bursts without adding a second full debounce.
// Quiet window before a semantic re-embed batch runs. Embedding costs ~1-2s
// per collect under load, and nobody semantically searches a file within
// seconds of editing it — a long window coalesces an agent's whole edit burst
// into one collect instead of one per save. Freshness is preserved where it
// matters: unembedded changed files are already excluded from results by the
// stale-file mask, and grep/read see changes instantly.
const SEMANTIC_REFRESH_QUIET_WINDOW_MS: u64 = 15_000;

/// Test seam: rigs that assert on semantic refresh timing override the quiet
/// window instead of waiting out the production window. The override is
/// clamped to the production window so a stray environment value cannot
/// silently stretch the refresh cadence (values above the default would delay
/// re-embeds indefinitely; values below it are exactly what tests need).
fn semantic_refresh_quiet_window() -> Duration {
    let from_env = std::env::var("AFT_SEMANTIC_QUIET_WINDOW_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(|ms| ms.min(SEMANTIC_REFRESH_QUIET_WINDOW_MS));
    Duration::from_millis(from_env.unwrap_or(SEMANTIC_REFRESH_QUIET_WINDOW_MS))
}
const SEMANTIC_REFRESH_MAX_BATCH_PATHS: usize = 50;
const SEMANTIC_REFRESH_LIMITER_KIND: &str = "semantic refresh";

#[derive(Clone)]
struct SemanticRefreshLimiter(Arc<crate::cold_build_limiter::ColdBuildLimiter>);

impl SemanticRefreshLimiter {
    fn acquire(
        &self,
        admitted: impl Fn() -> bool,
    ) -> Option<crate::cold_build_limiter::ColdBuildPermit> {
        crate::cold_build_limiter::acquire_blocking_while_with_limiter(
            &self.0,
            SEMANTIC_REFRESH_LIMITER_KIND,
            admitted,
        )
    }
}

#[cfg(test)]
static CONFIGURE_REPLAY_SESSION_CALLS: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
fn sleep_from_env_ms(name: &str) {
    let Some(delay) = std::env::var(name)
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|delay| *delay > 0)
    else {
        return;
    };
    thread::sleep(Duration::from_millis(delay));
}

#[cfg(not(test))]
fn sleep_from_env_ms(_name: &str) {}

fn delay_configure_deferred_walk_for_test() {
    sleep_from_env_ms("AFT_TEST_CONFIGURE_DEFERRED_WALK_DELAY_MS");
}

#[cfg(test)]
fn signal_configure_deferred_walk_start_for_test() {
    let Some(path) = std::env::var_os("AFT_TEST_CONFIGURE_DEFERRED_WALK_START_FILE") else {
        return;
    };
    fs::write(path, "started\n").expect("write deferred walk start signal");
}

#[cfg(not(test))]
fn signal_configure_deferred_walk_start_for_test() {}

#[cfg(test)]
fn run_configure_deferred_walk_synchronously_for_test() -> bool {
    std::env::var_os("AFT_TEST_CONFIGURE_FORCE_SYNCHRONOUS_DEFERRED_WALK").is_some()
}

#[cfg(not(test))]
fn run_configure_deferred_walk_synchronously_for_test() -> bool {
    false
}

fn delay_configure_deferred_maintenance_for_test() {
    #[cfg(test)]
    CONFIGURE_DEFERRED_DELAY_REACHED.fetch_add(1, Ordering::SeqCst);
    sleep_from_env_ms("AFT_TEST_CONFIGURE_DEFERRED_MAINTENANCE_DELAY_MS");
}

#[cfg(test)]
fn reset_configure_replay_session_calls_for_test() {
    CONFIGURE_REPLAY_SESSION_CALLS.store(0, Ordering::SeqCst);
}

#[cfg(test)]
fn configure_replay_session_calls_for_test() -> u64 {
    CONFIGURE_REPLAY_SESSION_CALLS.load(Ordering::SeqCst)
}

fn resolve_home_dir() -> Option<PathBuf> {
    let raw = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)?;
    Some(std::fs::canonicalize(&raw).unwrap_or(raw))
}

fn create_project_watcher(
    root_path: PathBuf,
    extra_watch_paths: Vec<PathBuf>,
    tx: mpsc::Sender<notify::Result<notify::Event>>,
) -> notify::Result<notify::RecommendedWatcher> {
    let mut watcher = notify::recommended_watcher(tx)?;
    watcher.watch(&root_path, RecursiveMode::Recursive)?;
    for path in extra_watch_paths {
        if path.exists() {
            watcher.watch(&path, RecursiveMode::NonRecursive)?;
        }
    }
    Ok(watcher)
}

fn external_ignore_watch_paths(ctx: &AppContext, root_path: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(global_ignore) = ignore::gitignore::gitconfig_excludes_path() {
        if global_ignore.is_file() {
            paths.push(global_ignore);
        }
    }
    let info_exclude = ctx
        .git_common_dir()
        .unwrap_or_else(|| root_path.join(".git"))
        .join("info")
        .join("exclude");
    if info_exclude.is_file() {
        paths.push(info_exclude);
    }
    paths.sort();
    paths.dedup();
    paths
}

fn start_project_watcher_with<W, E, F>(
    ctx: &AppContext,
    root_path: &Path,
    extra_watch_paths: Vec<PathBuf>,
    attach: F,
) where
    W: Send + 'static,
    E: std::fmt::Display + Send + 'static,
    F: FnOnce(PathBuf, Vec<PathBuf>, mpsc::Sender<notify::Result<notify::Event>>) -> Result<W, E>
        + Send
        + 'static,
{
    let generation = WATCHER_GENERATION
        .fetch_add(1, Ordering::SeqCst)
        .wrapping_add(1);
    let (dispatch_tx, dispatch_rx) = watcher_filter::watcher_dispatch_channel();
    let shutdown = Arc::new(AtomicBool::new(false));
    let thread_shutdown = Arc::clone(&shutdown);

    let root_path = root_path.to_path_buf();
    let filter_config = WatcherFilterConfig::new(root_path.clone(), ctx.git_common_dir());
    let shared_gitignore = ctx.shared_gitignore();
    let gitignore_generation = ctx.gitignore_generation();
    let session_id_for_bg = log_ctx::current_session();
    let sync_start = file_watcher_sync_start_for_test();
    let (start_tx, start_rx) = mpsc::channel::<Result<(), String>>();
    let start_tx = sync_start.then_some(start_tx);

    let join = thread::spawn(move || {
        log_ctx::with_session(session_id_for_bg, || {
            let attach_with_start =
                move |root: PathBuf,
                      extra_watch_paths: Vec<PathBuf>,
                      tx: mpsc::Sender<notify::Result<notify::Event>>| {
                    let result = attach(root, extra_watch_paths, tx);
                    if let Some(start_tx) = start_tx {
                        let _ = start_tx.send(
                            result
                                .as_ref()
                                .map(|_| ())
                                .map_err(|error| format!("watcher init failed: {error}")),
                        );
                    }
                    result
                };
            watcher_filter::run_watcher_thread(
                filter_config,
                extra_watch_paths,
                shared_gitignore,
                gitignore_generation,
                dispatch_tx,
                thread_shutdown,
                attach_with_start,
            );
        });
    });

    ctx.install_watcher_runtime(dispatch_rx, WatcherThreadHandle::new(shutdown, join));

    if sync_start {
        match start_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => slog_warn!("{error}"),
            Err(error) => slog_warn!(
                "timed out waiting for watcher startup for generation {generation}: {error}"
            ),
        }
    }
}

#[cfg(test)]
fn install_project_watcher_with<W, E, F>(
    ctx: &AppContext,
    root_path: &Path,
    extra_watch_paths: Vec<PathBuf>,
    attach: F,
) where
    W: Send + 'static,
    E: std::fmt::Display + Send + 'static,
    F: FnOnce(PathBuf, Vec<PathBuf>, mpsc::Sender<notify::Result<notify::Event>>) -> Result<W, E>
        + Send
        + 'static,
{
    // The OS watcher is owned by its runtime thread. Join it before installing
    // the replacement so stale filter threads cannot accumulate.
    ctx.stop_watcher_runtime();
    start_project_watcher_with(ctx, root_path, extra_watch_paths, attach);
}

fn file_watcher_sync_start_for_test() -> bool {
    std::env::var("AFT_TEST_SYNC_FILE_WATCHER_START").is_ok_and(|value| value == "1")
}

/// Harness-only seam: when `AFT_TEST_DISABLE_FILE_WATCHER=1`, `configure` skips
/// installing the OS file watcher entirely. The integration suite spawns ~600
/// `aft` processes; under that concurrent load the macOS FSEvents `watch()` call
/// probabilistically hangs (it never returns and never delivers events for a
/// fraction of processes), which flaked any test waiting on watcher-driven
/// invalidation. The vast majority of tests mutate files through AFT's own tools
/// (which invalidate caches directly, not via the watcher), so they need no
/// watcher at all. The test helper disables it by default; the dedicated
/// `watcher_integration` test binary (which runs alone, with no concurrent load)
/// opts back in. Never set in production.
fn file_watcher_disabled_for_test() -> bool {
    std::env::var("AFT_TEST_DISABLE_FILE_WATCHER").is_ok_and(|value| value == "1")
}

fn start_project_watcher(ctx: &AppContext, root_path: &Path) {
    if file_watcher_disabled_for_test() {
        return;
    }
    let extra_watch_paths = external_ignore_watch_paths(ctx, root_path);
    start_project_watcher_with(ctx, root_path, extra_watch_paths, create_project_watcher);
}

fn install_project_watcher(ctx: &AppContext, root_path: &Path) {
    ctx.stop_watcher_runtime();
    start_project_watcher(ctx, root_path);
}

/// Restore the watcher after an idle root released its runtime. Artifact stores
/// are lazy by design, so the first request is the natural point to reattach
/// external-change invalidation as well.
#[doc(hidden)]
pub fn ensure_project_watcher(ctx: &AppContext) {
    if ctx.watcher_runtime_active() {
        return;
    }
    // A dead-but-installed runtime means the watcher failed while drains were
    // suppressed (unbound root): events since the failure are lost, so the
    // retained warm state must not be trusted. Clean up the corpse and apply
    // the same gap invalidation a deliberate watcher stop applies. Roots
    // whose watcher was stopped deliberately (idle eviction, root deletion)
    // have no installed handle here and were already invalidated at the stop.
    if ctx.take_finished_watcher_runtime() {
        ctx.invalidate_artifacts_after_watcher_gap();
    }
    let Some(root_path) = ctx.canonical_cache_root_opt() else {
        return;
    };
    if root_path.exists() {
        install_project_watcher(ctx, &root_path);
    }
}

/// Backoff for build-level retries when the embedding backend is unreachable.
/// Ramps 15s -> 30s -> 60s then holds at 60s. Keeps the retry cadence cheap
/// (the build re-walks files each attempt) while recovering within a minute of
/// the backend returning.
fn semantic_build_retry_backoff(attempt: usize) -> Duration {
    // Test seam: shrink the schedule to a fixed small interval so recovery
    // integration tests don't wait real 15s+ windows. Not a user-facing knob.
    if let Ok(raw) = std::env::var("AFT_SEMANTIC_RETRY_BACKOFF_MS") {
        if let Ok(ms) = raw.parse::<u64>() {
            return Duration::from_millis(ms);
        }
    }
    const SCHEDULE_SECS: [u64; 3] = [15, 30, 60];
    let secs = SCHEDULE_SECS
        .get(attempt)
        .copied()
        .unwrap_or(*SCHEDULE_SECS.last().unwrap());
    Duration::from_secs(secs)
}

fn spawn_semantic_refresh_worker(
    project_root: PathBuf,
    mut index: SemanticIndex,
    mut model: crate::semantic_index::EmbeddingModel,
    max_batch_size: usize,
    max_files: usize,
    request_rx: crossbeam_channel::Receiver<SemanticRefreshRequest>,
    event_tx: crossbeam_channel::Sender<SemanticRefreshEvent>,
    lifecycle: SubcLifecycleAdmission,
    generation_flag: Arc<AtomicU64>,
    generation: u64,
    limiter: SemanticRefreshLimiter,
    session_id: Option<String>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        log_ctx::with_session(session_id, || {
            while let Ok(first_request) = request_rx.recv() {
                let mut paths = Vec::new();
                let mut corpus_requested = false;
                match first_request {
                    SemanticRefreshRequest::Files {
                        paths: request_paths,
                    } => {
                        paths.extend(request_paths);
                    }
                    SemanticRefreshRequest::Corpus => {
                        corpus_requested = true;
                    }
                }

                let mut disconnected = false;
                let quiet_window = semantic_refresh_quiet_window();
                let mut deadline = Instant::now() + quiet_window;

                while !corpus_requested && paths.len() < SEMANTIC_REFRESH_MAX_BATCH_PATHS {
                    let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                        break;
                    };
                    match request_rx.recv_timeout(remaining) {
                        Ok(SemanticRefreshRequest::Files {
                            paths: request_paths,
                        }) => {
                            paths.extend(request_paths);
                            if paths.len() >= SEMANTIC_REFRESH_MAX_BATCH_PATHS {
                                break;
                            }
                            deadline = Instant::now() + quiet_window;
                        }
                        Ok(SemanticRefreshRequest::Corpus) => {
                            paths.clear();
                            corpus_requested = true;
                            break;
                        }
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => break,
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                            disconnected = true;
                            break;
                        }
                    }
                }

                if disconnected {
                    break;
                }

                if !lifecycle.is_current(generation_flag.as_ref(), generation) {
                    return;
                }

                // Corpus catch-up and watcher batches share this worker, so both
                // use the process-wide cap. Watcher batches keep their 15-second
                // quiet window and acquire immediately when a slot is free; queueing
                // them behind another root is preferable to bursting a shared backend.
                // Interactive QueryBudget embeddings do not run on this worker and
                // therefore never wait on this maintenance limiter.
                let Some(_refresh_permit) =
                    limiter.acquire(|| lifecycle.is_current(generation_flag.as_ref(), generation))
                else {
                    return;
                };

                if corpus_requested {
                    let mut current_files = match walk_semantic_project_files_bounded(
                        &project_root,
                        max_files,
                    ) {
                        Ok(files) => files,
                        Err(observed) => {
                            let error = format!(
                                "too many files (>{}) for semantic indexing (max {})",
                                max_files, max_files
                            );
                            slog_warn!(
                                "skipping semantic corpus refresh: more than {} files exceeds limit of {}. \
                                 Raise semantic.max_files or open a specific project directory.",
                                observed.saturating_sub(1),
                                max_files
                            );
                            if event_tx
                                .send(SemanticRefreshEvent::CorpusFailed { error })
                                .is_err()
                            {
                                break;
                            }
                            continue;
                        }
                    };
                    current_files.sort();
                    current_files.dedup();
                    if current_files.len() > max_files {
                        let error = format!(
                            "too many files (>{}) for semantic indexing (max {})",
                            max_files, max_files
                        );
                        let _ = event_tx.send(SemanticRefreshEvent::CorpusFailed { error });
                        continue;
                    }
                    if event_tx
                        .send(SemanticRefreshEvent::CorpusStarted {
                            files: current_files.len(),
                        })
                        .is_err()
                    {
                        break;
                    }

                    let mut embed = |texts: Vec<String>| model.embed(texts);
                    let mut progress = |_done: usize, _total: usize| {};
                    match index.refresh_stale_files(
                        &project_root,
                        &current_files,
                        &mut embed,
                        max_batch_size,
                        &mut progress,
                    ) {
                        Ok(summary) => {
                            if !summary.is_noop() {
                                slog_info!(
                                    "semantic corpus refresh: {} changed, {} new, {} deleted, {} total processed",
                                    summary.changed,
                                    summary.added,
                                    summary.deleted,
                                    summary.total_processed,
                                );
                            }
                            if event_tx
                                .send(SemanticRefreshEvent::CorpusCompleted {
                                    index: index.clone(),
                                    changed: summary.changed,
                                    added: summary.added,
                                    deleted: summary.deleted,
                                    total_processed: summary.total_processed,
                                })
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(error) => {
                            slog_warn!("semantic corpus refresh failed: {}", error);
                            if event_tx
                                .send(SemanticRefreshEvent::CorpusFailed { error })
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                    continue;
                }

                paths.sort();
                paths.dedup();
                if paths.is_empty() {
                    continue;
                }

                if event_tx
                    .send(SemanticRefreshEvent::Started {
                        paths: paths.clone(),
                    })
                    .is_err()
                {
                    break;
                }

                let mut embed = |texts: Vec<String>| model.embed(texts);
                let mut progress = |_done: usize, _total: usize| {};
                match index.refresh_invalidated_files(
                    &project_root,
                    &paths,
                    &mut embed,
                    max_batch_size,
                    max_files,
                    &mut progress,
                ) {
                    Ok(update) => {
                        if !update.summary.is_noop() {
                            slog_info!(
                                "semantic refresh: {} changed, {} new, {} deleted, {} total processed",
                                update.summary.changed,
                                update.summary.added,
                                update.summary.deleted,
                                update.summary.total_processed,
                            );
                        }
                        if event_tx
                            .send(SemanticRefreshEvent::Completed {
                                added_entries: update.added_entries,
                                updated_metadata: update.updated_metadata,
                                completed_paths: update.completed_paths,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(error) => {
                        slog_warn!(
                            "semantic refresh failed for {} file(s): {}",
                            paths.len(),
                            error
                        );
                        if event_tx
                            .send(SemanticRefreshEvent::Failed { paths, error })
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        });
    })
}

fn normalize_absolute_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();

    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push(component.as_os_str());
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }

    normalized
}

fn validate_storage_dir(raw: &str) -> Result<PathBuf, String> {
    let storage_dir = PathBuf::from(raw);
    if !storage_dir.is_absolute() {
        return Err("configure: storage_dir must be an absolute path".to_string());
    }

    let normalized = normalize_absolute_path(&storage_dir);
    if normalized
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err("configure: storage_dir must not escape via '..' traversal".to_string());
    }

    Ok(normalized)
}

fn has_parent_component(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, Component::ParentDir))
}

fn detect_worktree_bridge(ctx: &AppContext, project_root: &Path) -> (bool, Option<PathBuf>) {
    if let Some(result) = ctx.cached_worktree_bridge(project_root) {
        return result;
    }

    // This is intentionally separate from artifact-cache-key memoization. The
    // cache key must validate the current HEAD, while worktree topology is
    // stable until the root's `.git` marker changes.
    let fail_closed_topology = || {
        let git_marker_is_file = project_root.join(".git").is_file();
        if git_marker_is_file {
            slog_warn!(
                "git worktree topology probe failed for {}; treating .git file root as borrow-only",
                project_root.display()
            );
        }
        (git_marker_is_file, None)
    };
    if configure_cancellation_requested() {
        return fail_closed_topology();
    }
    #[cfg(test)]
    ctx.record_worktree_bridge_probe_spawn_for_test();
    let output = crate::effective_path::new_command("git")
        .arg("-C")
        .arg(project_root)
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--git-dir",
            "--git-common-dir",
        ])
        .output();
    if configure_cancellation_requested() {
        return fail_closed_topology();
    }
    let Ok(output) = output else {
        return fail_closed_topology();
    };
    if !output.status.success() {
        return fail_closed_topology();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let Some(git_dir) = lines.next().map(PathBuf::from) else {
        return fail_closed_topology();
    };
    let Some(common_dir) = lines.next().map(PathBuf::from) else {
        return fail_closed_topology();
    };
    let git_dir = std::fs::canonicalize(&git_dir).unwrap_or(git_dir);
    let common_dir = std::fs::canonicalize(&common_dir).unwrap_or(common_dir);
    let is_worktree_bridge = git_dir != common_dir;
    ctx.cache_worktree_bridge(project_root, is_worktree_bridge, common_dir.clone());
    (is_worktree_bridge, Some(common_dir))
}

fn semantic_fingerprint_config_changed(
    previous: &SemanticBackendConfig,
    next: &SemanticBackendConfig,
) -> bool {
    previous.backend != next.backend
        || previous.model != next.model
        || previous.base_url != next.base_url
}

fn should_clear_failed_spawns(
    previous: &Config,
    next: &Config,
    equivalent_warm_config: bool,
) -> bool {
    !equivalent_warm_config
        || previous.lsp_paths_extra != next.lsp_paths_extra
        || previous.lsp_auto_install_binaries != next.lsp_auto_install_binaries
        || previous.lsp_inflight_installs != next.lsp_inflight_installs
}

#[cfg(test)]
fn reset_workspace_manifest_fingerprint_scans_for_test() {
    WORKSPACE_MANIFEST_FINGERPRINT_SCANS.with(|count| count.set(0));
}

#[cfg(test)]
fn workspace_manifest_fingerprint_scans_for_test() -> usize {
    WORKSPACE_MANIFEST_FINGERPRINT_SCANS.with(std::cell::Cell::get)
}

fn workspace_manifest_fingerprint(project_root: &Path) -> String {
    #[cfg(test)]
    WORKSPACE_MANIFEST_FINGERPRINT_SCANS.with(|count| count.set(count.get() + 1));
    let mut parts = Vec::new();
    push_manifest_fingerprint(&mut parts, project_root.join("package.json"));
    let packages_dir = project_root.join("packages");
    if let Ok(entries) = fs::read_dir(packages_dir) {
        let mut manifests = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path().join("package.json"))
            .collect::<Vec<_>>();
        manifests.sort();
        for manifest in manifests {
            push_manifest_fingerprint(&mut parts, manifest);
        }
    }
    parts.join("|")
}

fn push_manifest_fingerprint(parts: &mut Vec<String>, path: PathBuf) {
    if let Ok(metadata) = fs::metadata(&path) {
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|duration| (duration.as_secs(), duration.subsec_nanos()))
            .unwrap_or((0, 0));
        parts.push(format!(
            "{}:{}:{}:{}",
            path.display(),
            metadata.len(),
            modified.0,
            modified.1
        ));
    }
}

/// Parse the `lsp_paths_extra` config param: an array of absolute directory
/// paths the plugin wants AFT to search when resolving LSP binaries (used
/// for the auto-install cache, e.g.
/// `~/.cache/aft/lsp-packages/<pkg>/node_modules/.bin/`).
///
/// Rejects non-array values, non-string entries, empty strings, relative paths,
/// parent traversal, and existing paths that do not resolve to directories.
/// Non-existent paths are accepted silently — the resolver tolerates them and
/// falls through to the next candidate.
fn parse_lsp_paths_extra(value: &Value) -> Result<Vec<PathBuf>, String> {
    let array = value
        .as_array()
        .ok_or_else(|| "configure: lsp_paths_extra must be an array of strings".to_string())?;

    let mut paths = Vec::with_capacity(array.len());
    for (index, entry) in array.iter().enumerate() {
        let raw = entry
            .as_str()
            .ok_or_else(|| format!("configure: lsp_paths_extra[{index}] must be a string"))?;
        if raw.is_empty() {
            return Err(format!(
                "configure: lsp_paths_extra[{index}] must not be empty"
            ));
        }
        let path = PathBuf::from(raw);
        if !path.is_absolute() {
            return Err(format!(
                "configure: lsp_paths_extra[{index}] must be an absolute path: {raw}"
            ));
        }
        if has_parent_component(&path) {
            return Err(format!(
                "configure: lsp_paths_extra[{index}] must not contain '..' traversal: {raw}"
            ));
        }

        match std::fs::canonicalize(&path) {
            Ok(canonical) => {
                if has_parent_component(&canonical) {
                    return Err(format!(
                        "configure: lsp_paths_extra[{index}] resolved path must not contain '..' traversal: {}",
                        canonical.display()
                    ));
                }
                if !canonical.is_dir() {
                    return Err(format!(
                        "configure: lsp_paths_extra[{index}] must resolve to a directory: {}",
                        canonical.display()
                    ));
                }
                paths.push(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                paths.push(path);
            }
            Err(error) => {
                return Err(format!(
                    "configure: lsp_paths_extra[{index}] could not be resolved: {error}"
                ));
            }
        }
    }
    Ok(paths)
}

fn parse_string_set(
    value: &Value,
    field: &str,
) -> Result<std::collections::HashSet<String>, String> {
    let Some(entries) = value.as_array() else {
        return Err(format!("configure: {field} must be an array of strings"));
    };

    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            entry
                .as_str()
                .map(|value| value.to_string())
                .ok_or_else(|| format!("configure: {field}[{index}] must be a string"))
        })
        .collect()
}

fn is_custom_server(kind: &ServerKind) -> bool {
    matches!(kind, ServerKind::Custom(_))
}

fn configured_lsp_binary_resolves(
    configured: &crate::config::UserServerDef,
    files: &[PathBuf],
    config: &crate::config::Config,
) -> bool {
    let project_root = config.project_root.as_deref();
    for file in files {
        if let Some(server) = servers_for_file(file, config).into_iter().find(|server| {
            server.kind.id_str() == configured.id && server.binary == configured.binary
        }) {
            let workspace_root = if matches!(server.kind, ServerKind::Python | ServerKind::Ty) {
                server.workspace_root_for_file_with_project_root(file, project_root)
            } else {
                project_root.map(Path::to_path_buf)
            };
            return resolve_server_binary(&server, workspace_root.as_deref(), config).is_some();
        }
    }

    resolve_lsp_binary(&configured.binary, project_root, &config.lsp_paths_extra).is_some()
}

fn lsp_missing_hint(binary: &str) -> String {
    crate::format::install_hint(binary)
}

fn lang_key(lang: LangId) -> &'static str {
    match lang {
        LangId::TypeScript | LangId::JavaScript | LangId::Tsx => "typescript",
        LangId::Python => "python",
        LangId::Rust => "rust",
        LangId::Go => "go",
        LangId::C => "c",
        LangId::Cpp => "cpp",
        LangId::Zig => "zig",
        LangId::CSharp => "csharp",
        LangId::Bash => "bash",
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
        LangId::Html => "html",
        LangId::Markdown => "markdown",
        LangId::Yaml => "yaml",
        LangId::Pascal => "pascal",
        LangId::R => "r",
        LangId::Groovy => "groovy",
        LangId::ObjC => "objc",
    }
}

fn has_project_config(project_root: Option<&Path>, filenames: &[&str]) -> bool {
    let Some(root) = project_root else {
        return false;
    };
    filenames.iter().any(|file| root.join(file).exists())
}

fn has_pyproject_tool(project_root: Option<&Path>, tool_name: &str) -> bool {
    let Some(root) = project_root else {
        return false;
    };
    let pyproject = root.join("pyproject.toml");
    if !pyproject.exists() {
        return false;
    }
    std::fs::read_to_string(pyproject)
        .map(|content| content.contains(&format!("[tool.{tool_name}]")))
        .unwrap_or(false)
}

#[derive(Debug, Clone)]
struct ConfigureToolCandidate {
    tool: String,
    source: String,
    required: bool,
}

fn configure_tool_candidate(tool: &str, source: &str, required: bool) -> ConfigureToolCandidate {
    ConfigureToolCandidate {
        tool: tool.to_string(),
        source: source.to_string(),
        required,
    }
}

fn explicit_formatter_candidate(name: &str) -> Vec<ConfigureToolCandidate> {
    match name {
        "none" | "off" | "false" => Vec::new(),
        "biome" | "oxfmt" | "prettier" | "deno" | "ruff" | "black" | "rustfmt" | "goimports"
        | "gofmt" => {
            vec![configure_tool_candidate(name, "formatter config", true)]
        }
        _ => Vec::new(),
    }
}

fn explicit_checker_candidate(name: &str) -> Vec<ConfigureToolCandidate> {
    match name {
        "none" | "off" | "false" => Vec::new(),
        "tsc" | "tsgo" | "cargo" | "go" | "biome" | "pyright" | "ruff" | "staticcheck" => {
            vec![configure_tool_candidate(name, "checker config", true)]
        }
        _ => Vec::new(),
    }
}

fn formatter_candidates(
    lang: LangId,
    config: &crate::config::Config,
) -> Vec<ConfigureToolCandidate> {
    let project_root = config.project_root.as_deref();
    if let Some(preferred) = config.formatter.get(lang_key(lang)) {
        return explicit_formatter_candidate(preferred);
    }

    match lang {
        LangId::TypeScript | LangId::JavaScript | LangId::Tsx => {
            if has_project_config(project_root, &["biome.json", "biome.jsonc"]) {
                vec![configure_tool_candidate("biome", "biome.json", true)]
            } else if has_project_config(
                project_root,
                &[".oxfmtrc.json", ".oxfmtrc.jsonc", "oxfmt.config.ts"],
            ) {
                vec![configure_tool_candidate("oxfmt", "oxfmt config", true)]
            } else if has_project_config(
                project_root,
                &[
                    ".prettierrc",
                    ".prettierrc.json",
                    ".prettierrc.yml",
                    ".prettierrc.yaml",
                    ".prettierrc.js",
                    ".prettierrc.cjs",
                    ".prettierrc.mjs",
                    ".prettierrc.toml",
                    "prettier.config.js",
                    "prettier.config.cjs",
                    "prettier.config.mjs",
                ],
            ) {
                vec![configure_tool_candidate(
                    "prettier",
                    "Prettier config",
                    true,
                )]
            } else if has_project_config(project_root, &["deno.json", "deno.jsonc"]) {
                vec![configure_tool_candidate("deno", "deno.json", true)]
            } else {
                Vec::new()
            }
        }
        LangId::Python => {
            if has_project_config(project_root, &["ruff.toml", ".ruff.toml"])
                || has_pyproject_tool(project_root, "ruff")
            {
                vec![configure_tool_candidate("ruff", "ruff config", true)]
            } else if has_pyproject_tool(project_root, "black") {
                vec![configure_tool_candidate("black", "pyproject.toml", true)]
            } else {
                Vec::new()
            }
        }
        LangId::Rust => {
            if has_project_config(project_root, &["Cargo.toml"]) {
                vec![configure_tool_candidate("rustfmt", "Cargo.toml", true)]
            } else {
                Vec::new()
            }
        }
        LangId::Go => {
            if has_project_config(project_root, &["go.mod"]) {
                vec![
                    configure_tool_candidate("goimports", "go.mod", false),
                    configure_tool_candidate("gofmt", "go.mod", true),
                ]
            } else {
                Vec::new()
            }
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
        | LangId::Pascal
        | LangId::R
        | LangId::Groovy
        | LangId::ObjC => Vec::new(),
        LangId::Html | LangId::Markdown | LangId::Yaml => Vec::new(),
    }
}

fn checker_candidates(lang: LangId, config: &crate::config::Config) -> Vec<ConfigureToolCandidate> {
    let project_root = config.project_root.as_deref();
    if let Some(preferred) = config.checker.get(lang_key(lang)) {
        return explicit_checker_candidate(preferred);
    }

    match lang {
        LangId::TypeScript | LangId::JavaScript | LangId::Tsx => {
            if has_project_config(project_root, &["biome.json", "biome.jsonc"]) {
                vec![configure_tool_candidate("biome", "biome.json", true)]
            } else if has_project_config(project_root, &["tsconfig.json"]) {
                vec![configure_tool_candidate("tsc", "tsconfig.json", true)]
            } else {
                Vec::new()
            }
        }
        LangId::Python => {
            if has_project_config(project_root, &["pyrightconfig.json"])
                || has_pyproject_tool(project_root, "pyright")
            {
                vec![configure_tool_candidate("pyright", "pyright config", true)]
            } else if has_project_config(project_root, &["ruff.toml", ".ruff.toml"])
                || has_pyproject_tool(project_root, "ruff")
            {
                vec![configure_tool_candidate("ruff", "ruff config", true)]
            } else {
                Vec::new()
            }
        }
        LangId::Rust => {
            if has_project_config(project_root, &["Cargo.toml"]) {
                vec![configure_tool_candidate("cargo", "Cargo.toml", true)]
            } else {
                Vec::new()
            }
        }
        LangId::Go => {
            if has_project_config(project_root, &["go.mod"]) {
                vec![
                    configure_tool_candidate("staticcheck", "go.mod", false),
                    configure_tool_candidate("go", "go.mod", true),
                ]
            } else {
                Vec::new()
            }
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
        | LangId::Pascal
        | LangId::R
        | LangId::Groovy
        | LangId::ObjC => Vec::new(),
        LangId::Html | LangId::Markdown | LangId::Yaml => Vec::new(),
    }
}

fn resolve_tool_cached(
    tool: &str,
    project_root: Option<&Path>,
    cache: &mut HashMap<String, bool>,
) -> bool {
    if let Some(is_available) = cache.get(tool) {
        return *is_available;
    }

    let is_available = crate::format::tool_available_for_missing_warning(tool, project_root);
    cache.insert(tool.to_string(), is_available);
    is_available
}

fn should_warn_missing_formatters(config: &crate::config::Config, lang: LangId) -> bool {
    config.format_on_edit || config.formatter.contains_key(lang_key(lang))
}

fn should_warn_missing_checkers(config: &crate::config::Config, lang: LangId) -> bool {
    let mode = config.validate_on_edit.as_deref().unwrap_or("off");
    (mode == "syntax" || mode == "full") || config.checker.contains_key(lang_key(lang))
}

fn missing_tool_warning(
    kind: &str,
    language: &str,
    candidate: &ConfigureToolCandidate,
    project_root: Option<&Path>,
    tool_cache: &mut HashMap<String, bool>,
) -> Option<crate::format::MissingTool> {
    if !candidate.required || resolve_tool_cached(&candidate.tool, project_root, tool_cache) {
        return None;
    }

    Some(crate::format::MissingTool {
        kind: kind.to_string(),
        language: language.to_string(),
        tool: candidate.tool.clone(),
        // GitHub issue #47: word this so the user understands the tool may
        // be installed but missing from AFT's PATH (common with GUI-launched
        // editors that don't inherit a login shell). format.rs has the same
        // wording in `configured_tool_hint`.
        hint: format!(
            "{} is configured in {} but was not found on PATH or in common install locations. {}",
            candidate.tool,
            candidate.source,
            crate::format::install_hint(&candidate.tool)
        ),
    })
}

fn detect_missing_tools_for_languages(
    languages: &HashSet<LangId>,
    config: &crate::config::Config,
) -> Vec<crate::format::MissingTool> {
    let mut warnings = Vec::new();
    let mut seen = HashSet::new();
    let mut tool_cache = HashMap::new();

    for &lang in languages {
        let language = lang_key(lang);

        if should_warn_missing_formatters(config, lang) {
            for candidate in formatter_candidates(lang, config) {
                if let Some(warning) = missing_tool_warning(
                    "formatter_not_installed",
                    language,
                    &candidate,
                    config.project_root.as_deref(),
                    &mut tool_cache,
                ) {
                    if seen.insert((
                        warning.kind.clone(),
                        warning.language.clone(),
                        warning.tool.clone(),
                    )) {
                        warnings.push(warning);
                    }
                }
            }
        }

        if should_warn_missing_checkers(config, lang) {
            for candidate in checker_candidates(lang, config) {
                if let Some(warning) = missing_tool_warning(
                    "checker_not_installed",
                    language,
                    &candidate,
                    config.project_root.as_deref(),
                    &mut tool_cache,
                ) {
                    if seen.insert((
                        warning.kind.clone(),
                        warning.language.clone(),
                        warning.tool.clone(),
                    )) {
                        warnings.push(warning);
                    }
                }
            }
        }
    }

    warnings.sort_by(|left, right| {
        (&left.kind, &left.language, &left.tool).cmp(&(&right.kind, &right.language, &right.tool))
    });
    warnings
}

fn detect_missing_lsp_binaries(files: &[PathBuf], config: &crate::config::Config) -> Vec<Value> {
    let mut warnings = Vec::new();
    let mut seen_resolutions = HashSet::new();
    let mut file_driven_servers = HashSet::new();
    let mut seen_explicit_servers = HashSet::new();
    let mut warned_binaries = HashSet::new();

    let project_root = config.project_root.as_deref();

    for file in files {
        for server in servers_for_file(&file, config) {
            let workspace_root = if matches!(server.kind, ServerKind::Python | ServerKind::Ty) {
                server.workspace_root_for_file_with_project_root(file, project_root)
            } else {
                project_root.map(Path::to_path_buf)
            };
            let identity = (server.kind.id_str().to_string(), server.binary.clone());
            if is_custom_server(&server.kind)
                || !seen_resolutions.insert((
                    server.kind.id_str().to_string(),
                    server.binary.clone(),
                    workspace_root.clone(),
                ))
            {
                continue;
            }
            file_driven_servers.insert(identity);

            if !config.lsp_auto_install_binaries.contains(&server.binary) {
                continue;
            }

            if config.lsp_inflight_installs.contains(&server.binary) {
                continue;
            }

            if resolve_server_binary(&server, workspace_root.as_deref(), config).is_none()
                && warned_binaries.insert(server.binary.clone())
            {
                warnings.push(json!({
                    "kind": "lsp_binary_missing",
                    "server": server.binary,
                    "binary": server.binary,
                    "hint": lsp_missing_hint(&server.binary),
                }));
            }
        }
    }

    for server in &config.lsp_servers {
        // A blank binary means "partial built-in override, inherit the built-in
        // binary" — the resolvable binary is already covered by the built-in
        // pass above, so skip the missing-binary probe here (probing "" never
        // resolves and would emit a bogus warning).
        if server.binary.is_empty() {
            continue;
        }

        let identity = (server.id.clone(), server.binary.clone());
        if server.disabled
            || file_driven_servers.contains(&identity)
            || !seen_explicit_servers.insert(identity)
        {
            continue;
        }

        if config.lsp_inflight_installs.contains(&server.binary) {
            continue;
        }

        if !configured_lsp_binary_resolves(server, files, config)
            && warned_binaries.insert(server.binary.clone())
        {
            warnings.push(json!({
                "kind": "lsp_binary_missing",
                "server": server.id,
                "binary": server.binary,
                "hint": lsp_missing_hint(&server.binary),
            }));
        }
    }

    warnings.sort_by_key(|warning| warning.to_string());
    warnings
}

type SearchIndexSymbolFile = (PathBuf, SystemTime);

fn search_index_symbol_files(index: &SearchIndex) -> Vec<SearchIndexSymbolFile> {
    index
        .files
        .iter()
        .filter(|entry| !entry.path.as_os_str().is_empty())
        .map(|entry| (entry.path.clone(), entry.modified))
        .collect()
}

fn spawn_symbol_cache_prewarm(
    root: PathBuf,
    symbol_cache: SharedSymbolCache,
    symbol_storage: Option<PathBuf>,
    symbol_project_key: String,
    symbol_cache_generation: u64,
    symbol_files: Vec<SearchIndexSymbolFile>,
    is_worktree_bridge: bool,
    session_id: Option<String>,
) {
    thread::spawn(move || {
        log_ctx::with_session(session_id, || {
            prewarm_symbol_cache_from_search_files(
                root,
                symbol_cache,
                symbol_storage,
                symbol_project_key,
                symbol_cache_generation,
                symbol_files,
                is_worktree_bridge,
            );
        });
    });
}

fn prewarm_symbol_cache_from_search_files(
    root: PathBuf,
    symbol_cache: SharedSymbolCache,
    symbol_storage: Option<PathBuf>,
    symbol_project_key: String,
    symbol_cache_generation: u64,
    symbol_files: Vec<SearchIndexSymbolFile>,
    is_worktree_bridge: bool,
) {
    #[cfg(debug_assertions)]
    delay_symbol_prewarm_for_debug();

    let mut warmed_files = 0usize;
    let mut skipped_files = 0usize;
    if let Ok(mut cache) = symbol_cache.write() {
        if !cache.set_project_root_for_generation(symbol_cache_generation, root.clone()) {
            slog_info!("skipping stale symbol cache prewarm after reconfigure");
            return;
        }
        if let Some(storage_dir) = symbol_storage.as_deref() {
            let load_outcome = cache.load_from_disk_for_generation_with_outcome(
                symbol_cache_generation,
                storage_dir,
                &symbol_project_key,
                &root,
            );
            slog_info!(
                "loaded symbol cache from disk: {} files",
                load_outcome.loaded
            );
        }
    } else {
        return;
    }

    let mut parser = crate::parser::FileParser::with_symbol_cache_generation(
        symbol_cache.clone(),
        Some(symbol_cache_generation),
    );
    for (path, modified) in &symbol_files {
        let cached = symbol_cache
            .read()
            .map(|cache| cache.contains_path_with_mtime(path, *modified))
            .unwrap_or(false);
        if cached {
            skipped_files += 1;
            continue;
        }
        match parser.extract_symbols_with_cache_status(path) {
            Ok((_, true)) => warmed_files += 1,
            Ok((_, false)) => skipped_files += 1,
            Err(_) => {}
        }
    }

    let total_files = symbol_cache.read().map(|cache| cache.len()).unwrap_or(0);
    if !is_worktree_bridge {
        if let Some(storage_dir) = symbol_storage.as_deref() {
            if let Ok(cache) = symbol_cache.read() {
                if cache.generation() != symbol_cache_generation {
                    slog_info!("skipping stale symbol cache persistence after reconfigure");
                    return;
                }
                // Disk-load repairs and actual entry changes advance the cache revision;
                // a fully unchanged warm load leaves it equal to the persisted revision.
                if !cache.needs_persistence() {
                    slog_debug!("symbol cache unchanged, skipping persistence");
                } else {
                    let persistence_revision = cache.persistence_revision();
                    let persisted_files = cache.len();
                    let write_result = crate::symbol_cache_disk::write_to_disk(
                        &cache,
                        storage_dir,
                        &symbol_project_key,
                    );
                    drop(cache);
                    match write_result {
                        Ok(()) => {
                            if let Ok(mut cache) = symbol_cache.write() {
                                cache.mark_persisted_for_generation(
                                    symbol_cache_generation,
                                    persistence_revision,
                                );
                            }
                            slog_info!("persisted symbol cache: {} files", persisted_files);
                        }
                        Err(error) => {
                            slog_warn!("failed to persist symbol cache: {}", error);
                        }
                    }
                }
            }
        }
    }
    slog_info!(
        "pre-warmed symbol cache: {} new, {} cached, {} files total",
        warmed_files,
        skipped_files,
        total_files
    );
}

#[cfg(debug_assertions)]
fn delay_symbol_prewarm_for_debug() {
    let Some(delay_ms) = std::env::var("AFT_TEST_SYMBOL_PREWARM_DELAY_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
    else {
        return;
    };
    thread::sleep(Duration::from_millis(delay_ms));
}

fn walk_semantic_project_files_bounded(
    root: &Path,
    max_files: usize,
) -> Result<Vec<PathBuf>, usize> {
    let filters = build_path_filters(&[], &[]).unwrap_or_default();
    walk_project_files_bounded_matching(root, &filters, max_files, is_semantic_indexed_extension)
}

#[cfg(debug_assertions)]
fn delay_search_rebuild_publish_for_debug() {
    let Some(delay_ms) = std::env::var("AFT_TEST_SEARCH_REBUILD_PUBLISH_DELAY_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
    else {
        return;
    };
    thread::sleep(Duration::from_millis(delay_ms));
}

#[cfg(not(debug_assertions))]
fn delay_search_rebuild_publish_for_debug() {}

#[cfg(debug_assertions)]
fn mark_search_rebuild_spawn_for_debug() {
    let Some(path) = std::env::var_os("AFT_TEST_SEARCH_REBUILD_THREAD_MARKER") else {
        return;
    };
    let path = PathBuf::from(path);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, b"spawned");
}

/// Parse the optional `config: [{tier, source, doc}]` raw-tier array from
/// configure params. Returns `None` when absent or empty, so the resolver only
/// runs when tiers are actually supplied. Tiers with
/// a missing/invalid `tier` or `doc` are skipped (the `source` is optional
/// metadata). The `tier` label is trusted as stamped by the plugin (by config
/// source path) — never re-derived from `doc` content.
fn parse_config_tiers(
    params: &serde_json::Value,
) -> Option<Vec<crate::config_resolve::ConfigTier>> {
    let arr = params.get("config")?.as_array()?;
    let tiers: Vec<crate::config_resolve::ConfigTier> = arr
        .iter()
        .filter_map(|entry| {
            let tier = entry.get("tier")?.as_str()?.to_string();
            let doc = entry.get("doc")?.as_str()?.to_string();
            let source = entry
                .get("source")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            Some(crate::config_resolve::ConfigTier { tier, source, doc })
        })
        .collect();
    (!tiers.is_empty()).then_some(tiers)
}

fn parse_cortexkit_user_config_path(params: &serde_json::Value) -> Result<Option<PathBuf>, String> {
    let Some(raw) = params.get("cortexkit_user_config_path") else {
        return Ok(None);
    };
    if raw.is_null() {
        return Ok(None);
    }
    let Some(value) = raw.as_str() else {
        return Err("configure: cortexkit_user_config_path must be a string".to_string());
    };
    if value.trim().is_empty() {
        return Ok(None);
    }
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err("configure: cortexkit_user_config_path must be an absolute path".to_string());
    }
    Ok(Some(path))
}

fn find_config_tier(
    tiers: &[crate::config_resolve::ConfigTier],
    tier_name: &str,
) -> Option<crate::config_resolve::ConfigTier> {
    tiers.iter().find(|tier| tier.tier == tier_name).cloned()
}

fn resolve_config_tiers_for_configure(
    params: &serde_json::Value,
    project_root: &Path,
) -> Result<Vec<crate::config_resolve::ConfigTier>, String> {
    let wire_tiers = parse_config_tiers(params).unwrap_or_default();
    let user_config_path = parse_cortexkit_user_config_path(params)?;
    let file_tiers = crate::subc_config::read_local_cortexkit_config_tiers(
        user_config_path.as_deref(),
        project_root,
    );

    let mut tiers = Vec::new();
    for tier_name in ["user", "project"] {
        if let Some(tier) = find_config_tier(&file_tiers, tier_name) {
            tiers.push(tier);
        } else if let Some(tier) = find_config_tier(&wire_tiers, tier_name) {
            tiers.push(tier);
        }
    }
    Ok(tiers)
}

fn configure_fingerprint(
    canonical_root: &Path,
    harness: &Harness,
    session_id: &str,
    config: &Config,
) -> Value {
    let mut effective_config =
        serde_json::to_value(config).unwrap_or_else(|_| serde_json::Value::Null);
    if let Some(fields) = effective_config.as_object_mut() {
        // Root and harness have canonical, explicit fingerprint fields below.
        fields.remove("project_root");
        fields.remove("harness");
    }
    json!({
        "canonical_root": canonical_root,
        "harness": harness,
        "session_id": session_id,
        "effective_config": effective_config,
        // These process-state fields are intentionally omitted from Config's
        // serialized form, so include them explicitly in configure identity.
        "foreground_wait_window_ms": config.foreground_wait_window_ms,
        "diagnostics_on_edit": config.diagnostics_on_edit,
    })
}

fn configure_warm_key(
    canonical_root: &Path,
    config: &Config,
    home_match: bool,
    is_worktree_bridge: bool,
    shared_artifacts_read_only: bool,
    workspace_manifests: Option<&str>,
) -> String {
    format!(
        "root={:?};storage={:?};home={};worktree={};readonly={};search={}:{};semantic={}:{:?};callgraph={}:{};inspect={};manifests={}",
        canonical_root,
        config.storage_dir,
        home_match,
        is_worktree_bridge,
        shared_artifacts_read_only,
        config.search_index,
        config.search_index_max_file_size,
        config.semantic_search,
        config.semantic,
        config.callgraph_store,
        config.callgraph_chunk_size,
        config.inspect.enabled,
        workspace_manifests.unwrap_or_default(),
    )
}

/// Cooperative cancellation checkpoint for foreground configure phases.
///
/// A cancelled RouteBind (route Goodbye or bind-deadline expiry) signals the
/// configure job's [`crate::executor::JobCancellation`]; the running configure
/// observes it here at phase boundaries and returns early instead of finishing
/// a bind nobody is waiting for. Once the commit phase begins the token is
/// sealed and later cancels are ignored, so `AppContext` is never abandoned
/// half-mutated.
fn configure_cancellation_requested() -> bool {
    crate::executor::current_job_cancellation()
        .is_some_and(|token| token.cancel_requested_before_commit())
}

fn configure_cancelled(req_id: &str) -> Option<Response> {
    configure_cancellation_requested().then(|| {
        Response::error(
            req_id,
            "request_cancelled",
            "configure cancelled: the requesting route was torn down or its bind deadline expired",
        )
    })
}

fn configure_maintenance_backpressure(req_id: &str) -> Response {
    Response::error_with_data(
        req_id,
        "maintenance_backpressure",
        format!(
            "configure maintenance queue reached its per-actor capacity of {} jobs",
            crate::executor::MAINTENANCE_QUEUE_CAP
        ),
        json!({
            "retryable": true,
            "queue_cap": crate::executor::MAINTENANCE_QUEUE_CAP,
        }),
    )
}

pub(crate) const HASHLINE_DOWNGRADE_MESSAGE: &str =
    "Hashline mode was downgraded because the edit tool is not registered for this session";

pub(crate) fn hashline_downgrade_warning() -> Value {
    json!({
        "code": "hashline_downgraded",
        "reason": "edit_not_registered",
        "message": HASHLINE_DOWNGRADE_MESSAGE,
    })
}

fn register_hashline_for_configure(
    ctx: &AppContext,
    root: &Path,
    session: &str,
    configured_enabled: bool,
    edit_slot_survives: Option<bool>,
    warnings: &mut Vec<Value>,
) {
    let Some(edit_slot_survives) = edit_slot_survives else {
        if configured_enabled {
            warnings.push(hashline_downgrade_warning());
        }
        return;
    };
    let registration = ctx.hashline_bindings().register(
        root,
        session.to_string(),
        crate::hashline::integration::RegistrationRequest {
            configured_enabled,
            edit_slot_survives,
        },
    );
    if registration.downgrade.is_some() {
        warnings.push(hashline_downgrade_warning());
    }
}

/// Handle a `configure` request.
///
/// Expects `project_root` (string, required) — absolute path to the project root.
/// Sets the project root on `Config`, initializes the `CallGraph` with that root,
/// spawns a file watcher for live invalidation, and returns success with the
/// configured path.
///
/// Stderr log: `[aft] project root set: <path>`
/// Stderr log: `[aft] watcher started: <path>`
pub fn handle_configure(req: &RawRequest, ctx: &AppContext) -> Response {
    if let Some(cancelled) = configure_cancelled(&req.id) {
        return cancelled;
    }
    let params = req.params.get("params").unwrap_or(&req.params);
    let harness = match params.get("harness") {
        Some(raw) => match serde_json::from_value::<Harness>(raw.clone()) {
            Ok(harness) => harness,
            Err(_) => {
                // A malformed fed fingerprint gets its own machine code: at
                // bind time it means either a federation-module bug or
                // fingerprint-format drift between the fed spec and ours, and
                // a typed code lets the fed side detect that without parsing
                // prose.
                let is_fed_shaped = raw.as_str().is_some_and(|s| s.starts_with("fed:"));
                if is_fed_shaped {
                    return Response::error(
                        &req.id,
                        "bad_harness_fingerprint",
                        "configure payload invalid field 'harness'; fed fingerprint must be 32-64 lowercase hex characters",
                    );
                }
                return Response::error(
                    &req.id,
                    "invalid_request",
                    "configure payload invalid field 'harness'; expected 'opencode', 'pi', 'runner', 'mcp:<client>', or 'fed:<fingerprint>'",
                );
            }
        },
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "configure payload missing required field 'harness'; expected 'opencode', 'pi', 'runner', 'mcp:<client>', or 'fed:<fingerprint>'",
            );
        }
    };
    let root = match params.get("project_root").and_then(|v| v.as_str()) {
        Some(r) => r,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "configure: missing required param 'project_root'",
            );
        }
    };

    let root_path = PathBuf::from(root);
    if !root_path.is_absolute() {
        return Response::error(
            &req.id,
            "invalid_request",
            "project_root must be an absolute path",
        );
    }
    if !root_path.is_dir() {
        return Response::error(
            &req.id,
            "invalid_request",
            format!("configure: project_root is not a directory: {}", root),
        );
    }
    ctx.begin_configure_ack_phase("canonicalize");
    let canonical_cache_root =
        std::fs::canonicalize(&root_path).unwrap_or_else(|_| root_path.clone());
    debug_assert!(canonical_cache_root.is_absolute());
    ctx.begin_configure_ack_phase("worktree_probe");
    if let Some(cancelled) = configure_cancelled(&req.id) {
        return cancelled;
    }
    let (is_worktree_bridge, git_common_dir) = detect_worktree_bridge(ctx, &canonical_cache_root);
    if let Some(cancelled) = configure_cancelled(&req.id) {
        return cancelled;
    }

    let previous_config = ctx.config();
    let previous_project_root = previous_config.project_root.clone();
    let previous_canonical_cache_root = ctx.canonical_cache_root_opt();
    let project_root_changed =
        previous_canonical_cache_root.as_deref() != Some(canonical_cache_root.as_path());
    let mut next_config = previous_config.as_ref().clone();
    next_config.project_root = Some(root_path.clone());
    next_config.harness = Some(harness.clone());

    // P1 config relocation: plugins send raw config tiers
    // (`config: [{tier, source, doc}]`), and AFT-core resolves the merge +
    // trust-boundary itself (crate::config_resolve). The CortexKit
    // config files are authoritative per tier: the user file wins over the wire user tier,
    // and the project file independently wins over the wire project tier. The
    // wire tiers remain as a one-release fallback for cached old plugins and for
    // tiers that have not migrated yet.
    // ALWAYS resolve, even when no tiers are supplied. `resolve_config_onto`
    // uses reset-onto-default semantics, so an absent/empty config must still
    // run it to reset the core-domain config to defaults — otherwise a bind with
    // no tiers would keep the PREVIOUS bind's resolved core config (the
    // cross-bind escalation: a later low-trust bind inheriting an earlier
    // high-trust bind's capability by simply omitting tiers).
    ctx.begin_configure_ack_phase("config_resolve");
    if let Some(cancelled) = configure_cancelled(&req.id) {
        return cancelled;
    }
    let tiers = match resolve_config_tiers_for_configure(params, &root_path) {
        Ok(tiers) => tiers,
        Err(error) => return Response::error(&req.id, "invalid_request", error),
    };
    let config_diagnostics =
        crate::config_resolve::resolve_config_onto_with_diagnostics(&tiers, &mut next_config);
    let config_dropped_keys = config_diagnostics.dropped;
    let mut configure_warnings = config_diagnostics
        .warnings
        .into_iter()
        .map(|warning| {
            json!({
                "code": warning.code,
                "key": warning.key,
                "tier": warning.tier,
                "value": warning.value,
                "message": warning.message,
            })
        })
        .collect::<Vec<_>>();

    // NO configure-time SSRF guard on semantic.base_url — deliberate (config
    // relocation posture). The original guard existed to stop UNTRUSTED *project*
    // config from pointing the embedding backend at an internal IP (SSRF). The tier
    // resolver now drops project-tier `semantic.base_url` outright (SEMANTIC_SECRET_REASON),
    // so the only base_url that can reach `next_config` is USER-tier — the same trust
    // level as the binary itself. A user pointing AFT at their own self-hosted
    // embedding server on the LAN/WAN (homelab Ollama/LMStudio at 192.168.x / 10.x, or
    // a remote box) is legitimate and must be allowed; the SSRF threat model (attacker-
    // controlled URL) does not apply to user-trusted config. The `validate_base_url_no_ssrf`
    // primitive remains for any future less-trusted source. See semantic_test::
    // configure_accepts_user_private_base_url.

    // Parse and validate process-state configure fields into a temporary config first.
    // AppContext is mutated only after this phase succeeds, so an invalid late
    // field cannot leave the bridge half-configured. Core AftConfig fields are
    // resolved exclusively from `config: [{tier, source, doc}]` above.
    if let Some(v) = params
        .get("aft_search_registered")
        .and_then(|v| v.as_bool())
    {
        next_config.aft_search_registered = v;
    }
    let edit_slot_survives = match params.get("edit_slot_survives") {
        Some(Value::Bool(value)) => Some(*value),
        Some(_) => {
            return Response::error(
                &req.id,
                "invalid_request",
                "configure: edit_slot_survives must be a boolean",
            );
        }
        // OpenCode and Pi have a host pruning/schema layer, so absence means
        // no plugin registration. Direct harnesses have no such layer.
        None if matches!(harness, Harness::Opencode | Harness::Pi) => None,
        None => Some(true),
    };
    if let Some(v) = params.get("bash_permissions").and_then(|v| v.as_bool()) {
        next_config.bash_permissions = v;
    }
    if let Some(v) = params.get("lsp_paths_extra") {
        next_config.lsp_paths_extra = match parse_lsp_paths_extra(v) {
            Ok(paths) => paths,
            Err(error) => return Response::error(&req.id, "invalid_request", error),
        };
    }
    if let Some(v) = params.get("lsp_auto_install_binaries") {
        next_config.lsp_auto_install_binaries =
            match parse_string_set(v, "lsp_auto_install_binaries") {
                Ok(binaries) => binaries,
                Err(error) => return Response::error(&req.id, "invalid_request", error),
            };
    }
    if let Some(v) = params.get("lsp_inflight_installs") {
        next_config.lsp_inflight_installs = match parse_string_set(v, "lsp_inflight_installs") {
            Ok(binaries) => binaries,
            Err(error) => return Response::error(&req.id, "invalid_request", error),
        };
    }
    if let Some(v) = params
        .get("search_index_max_file_size")
        .and_then(|v| v.as_u64())
    {
        next_config.search_index_max_file_size = v;
    }
    if let Some(raw) = params.get("storage_dir") {
        let Some(value) = raw.as_str() else {
            return Response::error(
                &req.id,
                "invalid_request",
                "configure: storage_dir must be a string",
            );
        };
        next_config.storage_dir = match validate_storage_dir(value) {
            Ok(path) => Some(path),
            Err(error) => return Response::error(&req.id, "invalid_request", error),
        };
    }
    if next_config.storage_dir.is_none() {
        // Plugin-less consumers (the daemon-supervised module, MCP hosts)
        // never send the plugin-computed storage_dir param. Resolve the
        // shared CortexKit storage root here so every artifact lane keys off
        // one concrete path. Leaving this None made lanes that gate writes on
        // `Some(storage_dir)` (semantic persistence) silently RAM-only under
        // the daemon while lanes with their own fallback (trigram) persisted,
        // splitting the storage universe by transport.
        next_config.storage_dir = Some(crate::bash_background::storage_dir(None));
    }
    // Resolve the plugin-managed ONNX Runtime before the semantic build worker
    // thread spawns below. This sets the process-global ORT_DYLIB_PATH once at
    // startup so pre_validate_onnx_runtime finds the runtime the plugin already
    // downloaded (issue #128). Idempotent: once ORT_DYLIB_PATH is set (by us or
    // by an explicit user override) it short-circuits. Must run here, not lazily
    // from the worker thread, because env mutation races ort's own dlopen.
    if let Some(storage_dir) = next_config.storage_dir.as_deref() {
        crate::semantic_index::resolve_managed_onnx_runtime(storage_dir);
    }
    if let Some(raw) = params.get("max_background_bash_tasks") {
        let parsed = raw.as_u64().filter(|v| *v >= 1);
        match parsed.and_then(|v| usize::try_from(v).ok()) {
            Some(v) => next_config.max_background_bash_tasks = v,
            None => {
                return Response::error(
                    &req.id,
                    "invalid_request",
                    format!(
                        "max_background_bash_tasks must be a positive integer (>= 1); got {}",
                        raw
                    ),
                );
            }
        }
    }

    // Detect "this is not really a project root" scenarios before any walks
    // that traverse `project_root`.
    let mut degraded_reasons: Vec<String> = Vec::new();
    let home_match = resolve_home_dir().is_some_and(|home| home == canonical_cache_root);
    if home_match {
        degraded_reasons.push("home_root".to_string());
    }

    // `_bypass_size_limits` (set by `aft warmup --force`) lifts the semantic
    // `max_files` cap so a very large repo is fully embedded for measurement.
    // Internal benchmarking escape hatch, not a user-facing knob. (The search
    // index has no file-count cap — its disk-backed design is RAM-bounded.)
    let bypass_size_limits = params
        .get("_bypass_size_limits")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if bypass_size_limits {
        const UNCAPPED: usize = 1_000_000_000;
        next_config.semantic.max_files = next_config.semantic.max_files.max(UNCAPPED);
    }

    let search_disabled_for_home = home_match && next_config.search_index;
    let semantic_disabled_for_home = home_match && next_config.semantic_search;
    if search_disabled_for_home {
        next_config.search_index = false;
    }
    if semantic_disabled_for_home {
        next_config.semantic_search = false;
    }

    let requested_fingerprint =
        configure_fingerprint(&canonical_cache_root, &harness, req.session(), &next_config);
    let current_harness = ctx.harness_opt();
    let current_fingerprint = previous_canonical_cache_root
        .as_deref()
        .zip(current_harness.as_ref())
        .map(|(canonical_root, current_harness)| {
            configure_fingerprint(
                canonical_root,
                current_harness,
                req.session(),
                previous_config.as_ref(),
            )
        });
    let effective_configure_changed = current_fingerprint.as_ref() != Some(&requested_fingerprint);
    // Package manifests can only affect callgraph-store warming. Capture their
    // fingerprint once when that lane is enabled; disabled roots should not stat
    // every package on every equivalent bind.
    let workspace_manifests = next_config
        .callgraph_store
        .then(|| workspace_manifest_fingerprint(&canonical_cache_root));
    let preflight_warm_key = configure_warm_key(
        &canonical_cache_root,
        &next_config,
        home_match,
        is_worktree_bridge,
        ctx.shared_artifacts_read_only(),
        workspace_manifests.as_deref(),
    );
    if ctx.configure_generation() > 0
        && current_fingerprint.as_ref() == Some(&requested_fingerprint)
        && ctx.configure_warm_key_matches(&preflight_warm_key)
        && ctx.is_worktree_bridge() == is_worktree_bridge
        && ctx.git_common_dir() == git_common_dir
        && {
            let missing = missing_artifact_loads(ctx);
            !missing.search && !missing.semantic
        }
    {
        // Equivalent-configure fast path: same commit boundary as the full
        // path — session binding registration is a state mutation. The seal
        // races the canceller atomically; losing means abort without mutating.
        let needs_session_maintenance =
            !ctx.has_configure_session_binding(&canonical_cache_root, req.session());
        if needs_session_maintenance && !ctx.configure_maintenance_has_capacity() {
            return configure_maintenance_backpressure(&req.id);
        }
        if let Some(token) = crate::executor::current_job_cancellation() {
            if !token.try_seal_committed() {
                return Response::error(
                    &req.id,
                    "request_cancelled",
                    "configure cancelled: the requesting route was torn down or its bind deadline expired",
                );
            }
        }
        register_hashline_for_configure(
            ctx,
            &canonical_cache_root,
            req.session(),
            next_config.hashline_enabled,
            edit_slot_survives,
            &mut configure_warnings,
        );
        let first_session_bind = ctx.note_configure_session_binding(
            canonical_cache_root.clone(),
            req.session().to_string(),
        );
        let generation = ctx.configure_generation();
        if first_session_bind {
            let storage_root =
                crate::bash_background::storage_dir(next_config.storage_dir.as_deref());
            let enqueue_result = ctx.enqueue_configure_maintenance(ConfigureMaintenanceJob {
                generation,
                root_path: root_path.clone(),
                canonical_cache_root: canonical_cache_root.clone(),
                harness: harness.clone(),
                storage_root: storage_root.clone(),
                harness_dir: storage_root.join(harness.storage_segment()),
                session_id: req.session().to_string(),
                home_match,
                format_tool_cache_clear_needed: false,
                run_bash_replay: true,
                refresh_project_runtime: false,
                sync_bash_compress_flag: false,
                reset_filter_registry: false,
                clear_failed_spawns: false,
                warm_callgraph_store: false,
                supersede_artifact_persistence: false,
                artifact_load_starts: Vec::new(),
            });
            if enqueue_result.is_err() {
                ctx.forget_configure_session_binding(&canonical_cache_root, req.session());
                return configure_maintenance_backpressure(&req.id);
            }
            slog_debug!(
                "equivalent configure registered session {} for generation {}",
                req.session(),
                generation
            );
        } else {
            slog_debug!(
                "equivalent configure no-op for session {} at generation {}",
                req.session(),
                generation
            );
        }

        ctx.begin_configure_ack_phase("ack_ready");
        let artifact_owner_status = ctx.artifact_owner_status();
        let search_index_cache_reused = next_config.search_index
            && ctx
                .search_index()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some();
        return Response::success(
            &req.id,
            json!({
                "project_root": root_path.display().to_string(),
                "warnings": configure_warnings,
                "warnings_pending": false,
                "search_index_cache_reused": search_index_cache_reused,
                "artifact_owner": artifact_owner_status
                    .as_ref()
                    .map(|status| serde_json::to_value(status).unwrap_or(serde_json::Value::Null)),
                "config_dropped_keys": config_dropped_keys
                    .iter()
                    .map(|d| json!({ "key": d.key, "tier": d.tier, "reason": d.reason }))
                    .collect::<Vec<_>>(),
            }),
        );
    }

    if search_disabled_for_home {
        slog_warn!(
            "search_index auto-disabled: project root is the user home directory \
             ({}). Open a project subdirectory for full features.",
            canonical_cache_root.display()
        );
    }
    if semantic_disabled_for_home {
        slog_warn!(
            "semantic_search auto-disabled: project root is the user home directory \
             ({}). Open a project subdirectory for full features.",
            canonical_cache_root.display()
        );
    }

    // A new configure generation may observe a changed project-local formatter.
    // Clear its cached Ruff version before the next actual format, but never run
    // the project binary here: configure/warm paths are existence-only by design.
    let format_tool_cache_clear_needed = effective_configure_changed;

    let storage_root = crate::bash_background::storage_dir(next_config.storage_dir.as_deref());
    let artifact_key_needed = !home_match
        && (next_config.search_index || next_config.semantic_search || next_config.callgraph_store);
    ctx.begin_configure_ack_phase("cache_key_resolve");
    if let Some(cancelled) = configure_cancelled(&req.id) {
        return cancelled;
    }
    let project_key_result = artifact_key_needed.then(|| {
        ctx.memoized_artifact_cache_key_for_configure(
            &root_path,
            &canonical_cache_root,
            &storage_root,
            git_common_dir.as_deref(),
        )
    });
    if let Some(cancelled) = configure_cancelled(&req.id) {
        return cancelled;
    }
    let project_key = match project_key_result {
        Some(Ok(key)) => Some(key),
        Some(Err(error)) => {
            return Response::error_with_data(
                &req.id,
                "cache_key_probe_failed",
                error.to_string(),
                json!({
                    "retryable": true,
                    "root": error.root().display().to_string(),
                    "detail": error.detail(),
                }),
            );
        }
        None => None,
    };
    let project_scope_key = crate::path_identity::project_scope_key(&canonical_cache_root);
    ctx.begin_configure_ack_phase("artifact_owner_claim");
    if let Some(cancelled) = configure_cancelled(&req.id) {
        return cancelled;
    }
    let artifact_owner_claim = if let Some(project_key) = project_key.as_ref() {
        match crate::artifact_owner::claim_or_open_read_only(
            next_config.storage_dir.as_deref(),
            &canonical_cache_root,
            project_key,
            &project_scope_key,
            is_worktree_bridge,
            git_common_dir.as_deref(),
        ) {
            Ok(claim) => Some(claim),
            Err(error) => {
                return Response::error(
                    &req.id,
                    "artifact_owner_unavailable",
                    format!("failed to claim artifact owner manifest: {error}"),
                );
            }
        }
    } else {
        None
    };
    let project_key = project_key.unwrap_or_default();
    let artifact_owner_status = artifact_owner_claim
        .as_ref()
        .map(|claim| claim.status.clone());
    let artifact_owner_read_only = artifact_owner_status
        .as_ref()
        .is_some_and(|status| status.mode == crate::artifact_owner::ArtifactOwnerMode::ReadOnly);
    if artifact_owner_read_only {
        degraded_reasons.push("artifact_owner_read_only".to_string());
        if let Some(note) = artifact_owner_status
            .as_ref()
            .and_then(|status| status.note.as_ref())
        {
            slog_warn!("{}", note);
        }
    }
    ctx.begin_configure_ack_phase("storage_capability_probe");
    if let Some(cancelled) = configure_cancelled(&req.id) {
        return cancelled;
    }
    let root_cache_storage_ok = match crate::root_cache::storage_allows_root_keyed(&storage_root) {
        Ok(true) => true,
        Ok(false) => {
            degraded_reasons.push("root_cache_network_fs".to_string());
            slog_warn!(
                "root-keyed callgraph/inspect writers disabled: storage directory appears to be on a network filesystem ({})",
                storage_root.display()
            );
            false
        }
        Err(error) => {
            degraded_reasons.push("root_cache_fs_probe_failed".to_string());
            slog_warn!(
                "root-keyed callgraph/inspect writers disabled: failed to probe storage filesystem {}: {}",
                storage_root.display(),
                error
            );
            false
        }
    };
    let callgraph_writer_capability =
        root_cache_storage_ok && !is_worktree_bridge && !artifact_owner_read_only && !home_match;
    let inspect_writer_capability = root_cache_storage_ok && !home_match;
    let heavy_root_work_allowed =
        !home_match && !degraded_reasons.iter().any(|reason| reason == "home_root");

    // Commit seal: past this point the configure mutates AppContext and must
    // run to completion. The seal races the canceller atomically — exactly one
    // wins. Losing the race means a Goodbye/deadline cancel landed first and
    // its caller was told RunningSignalled, so abort WITHOUT mutating; winning
    // means later cancels see RunningCommitted and discard the completion.
    if !ctx.configure_maintenance_has_capacity() {
        return configure_maintenance_backpressure(&req.id);
    }
    if let Some(token) = crate::executor::current_job_cancellation() {
        if !token.try_seal_committed() {
            return Response::error(
                &req.id,
                "request_cancelled",
                "configure cancelled: the requesting route was torn down or its bind deadline expired",
            );
        }
    }
    ctx.begin_configure_ack_phase("state_commit");
    // Commit phase: no validation returns after this point.
    if semantic_fingerprint_config_changed(&previous_config.semantic, &next_config.semantic) {
        ctx.advance_semantic_fingerprint_generation();
    }
    ctx.set_config(next_config.clone());
    register_hashline_for_configure(
        ctx,
        &canonical_cache_root,
        req.session(),
        next_config.hashline_enabled,
        edit_slot_survives,
        &mut configure_warnings,
    );
    crate::logging::sync_storage_root(ctx.storage_dir());
    ctx.set_harness(harness.clone());
    {
        let mut backup = ctx.backup().lock();
        backup.set_policy(crate::backup::BackupPolicy {
            enabled: next_config.backup.enabled.unwrap_or(true),
            max_depth: next_config
                .backup
                .max_depth
                .unwrap_or(crate::backup::DEFAULT_MAX_UNDO_DEPTH),
            max_file_size: next_config.backup.max_file_size,
        });
        backup.set_db_harness(harness.clone());
    }
    ctx.set_canonical_cache_root(canonical_cache_root.clone());
    crate::root_cache::configure_artifact_access(
        &canonical_cache_root,
        &project_key,
        is_worktree_bridge,
    );
    ctx.set_cache_role(is_worktree_bridge, git_common_dir);
    let artifact_owner_lease = artifact_owner_claim.and_then(|claim| claim.lease);
    ctx.set_artifact_owner(artifact_owner_status.clone(), artifact_owner_lease);
    ctx.set_cache_writer_capabilities(callgraph_writer_capability, inspect_writer_capability);
    // Snapshot degraded-mode state once at configure time so every later
    // heavy-work entry point reads the same cheap gate instead of re-deriving
    // home-root logic independently.
    ctx.set_degraded_reasons(degraded_reasons.clone());
    ctx.set_heavy_root_work_allowed(heavy_root_work_allowed);
    let warm_key = configure_warm_key(
        &canonical_cache_root,
        &next_config,
        home_match,
        is_worktree_bridge,
        ctx.shared_artifacts_read_only(),
        workspace_manifests.as_deref(),
    );
    let (configure_generation, equivalent_warm_config) = ctx.note_configure_warm_key(warm_key);
    let first_session_bind =
        ctx.note_configure_session_binding(canonical_cache_root.clone(), req.session().to_string());
    if !equivalent_warm_config {
        ctx.reset_tier2_refresh_scheduler();
        ctx.reset_semantic_cold_seed_gate_for_configure();
        if next_config.semantic_search && !ctx.shared_artifacts_read_only() && !home_match {
            ctx.schedule_semantic_cold_seed_gate_for_configure();
        }
    }
    // Project root (and thus tsconfig resolution) may have changed; drop the
    // status-bar membership cache so the next bar count re-resolves from disk.
    // Equivalent rebinds keep this hot cache because no tsconfig input changed.
    if !equivalent_warm_config || project_root_changed {
        ctx.clear_tsconfig_membership_cache();
    }
    ctx.backup()
        .lock()
        .set_db_project_key(crate::path_identity::project_scope_key(
            &canonical_cache_root,
        ));
    ctx.begin_configure_ack_phase("index_loading_state");
    let search_index = ctx.config().search_index;
    let semantic_search = ctx.config().semantic_search;
    let mut search_index_cache_reused = false;

    // Reconfigure is still the signal that workspace package metadata may have
    // changed, even when the warm-maintenance key is otherwise equivalent.
    crate::callgraph::clear_workspace_package_cache();

    let search_build_in_progress = ctx
        .search_index_rx()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_some();
    let semantic_build_in_progress = ctx.semantic_index_rx().lock().is_some();
    let mut artifact_load_starts = Vec::new();
    if equivalent_warm_config {
        // The zero-work rebind path keeps the live index serving; report that
        // honestly instead of implying the cache was dropped.
        search_index_cache_reused = search_index
            && ctx
                .search_index()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some();
        if search_build_in_progress {
            slog_info!(
                "search index build adopted by equivalent reconfigure (generation {})",
                configure_generation
            );
        }
        if semantic_build_in_progress {
            slog_info!(
                "semantic index build adopted by equivalent reconfigure (generation {})",
                configure_generation
            );
        }
        if ctx.callgraph_store_rx().lock().is_some() {
            slog_info!(
                "callgraph store warm build adopted by equivalent reconfigure (generation {})",
                configure_generation
            );
        }
        artifact_load_starts.extend(schedule_missing_artifact_loads(
            ctx,
            search_index,
            semantic_search,
        ));
    } else {
        // We intentionally only warn on rapid reconfigure rather than joining old
        // workers. Generation gates discard their in-memory results, while
        // content/fingerprint generations, per-worker persistence epochs, and
        // artifact-specific disk locks prevent a superseded worker from overwriting a
        // newer artifact. The remaining cost is bounded duplicate CPU work.
        if search_build_in_progress {
            slog_warn!(
                "search index build cancelled (superseded by generation {})",
                configure_generation
            );
        }
        if semantic_build_in_progress {
            slog_warn!(
                "semantic index build cancelled (superseded by generation {})",
                configure_generation
            );
        }
        if ctx.callgraph_store_rx().lock().is_some() {
            slog_warn!(
                "callgraph store warm build cancelled (superseded by generation {})",
                configure_generation
            );
        }

        *ctx.search_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        ctx.retire_search_index_rx();
        *ctx.semantic_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        ctx.retire_semantic_index_rx();
        *ctx.callgraph_store()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        ctx.retire_callgraph_store_rx();
        if previous_project_root.as_ref() == Some(&root_path) {
            ctx.mark_callgraph_store_force_rebuild();
        }
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Disabled;
        ctx.clear_semantic_refresh_worker();
        *ctx.semantic_embedding_model().lock() = None;
        ctx.clear_pending_index_updates();

        artifact_load_starts.extend(schedule_missing_artifact_loads(
            ctx,
            search_index,
            semantic_search,
        ));

        // Clear the workspace package caches here because reconfigure can point AFT at a
        // different root; reset them before warming the callgraph store for the new project.
        crate::callgraph::clear_workspace_package_cache();
    }

    let refresh_project_runtime =
        !equivalent_warm_config || project_root_changed || effective_configure_changed;
    let sync_bash_compress_flag = !equivalent_warm_config
        || previous_config.experimental_bash_compress != next_config.experimental_bash_compress;
    let clear_failed_spawns =
        should_clear_failed_spawns(&previous_config, &next_config, equivalent_warm_config);
    ctx.begin_configure_ack_phase("maintenance_enqueue");
    let enqueue_result = ctx.enqueue_configure_maintenance(ConfigureMaintenanceJob {
        generation: configure_generation,
        root_path: root_path.clone(),
        canonical_cache_root: canonical_cache_root.clone(),
        harness: harness.clone(),
        storage_root: crate::bash_background::storage_dir(next_config.storage_dir.as_deref()),
        harness_dir: ctx.harness_dir(),
        session_id: req.session().to_string(),
        home_match,
        format_tool_cache_clear_needed,
        run_bash_replay: !equivalent_warm_config || first_session_bind,
        refresh_project_runtime,
        sync_bash_compress_flag,
        reset_filter_registry: !equivalent_warm_config,
        clear_failed_spawns,
        warm_callgraph_store: next_config.callgraph_store && !home_match && !equivalent_warm_config,
        supersede_artifact_persistence: !equivalent_warm_config,
        artifact_load_starts,
    });
    if enqueue_result.is_err() {
        if first_session_bind {
            ctx.forget_configure_session_binding(&canonical_cache_root, req.session());
        }
        return configure_maintenance_backpressure(&req.id);
    }

    slog_info!("project root set: {}", root_path.display());

    let config_snapshot = ctx.config().clone();

    // Defer the full source-file walk + language detection +
    // formatter/checker/LSP missing-binary detection to a background thread.
    // On a normal project this finishes in <1 s and pushes a
    // `ConfigureWarningsFrame` for the plugin to surface; on a huge directory
    // it may take seconds-to-minutes, but configure itself returns now.
    let warnings_pending = !home_match && ctx.progress_sender_handle().is_some();
    if warnings_pending {
        let warning_tx = ctx.configure_warnings_sender();
        let warning_generation = configure_generation;
        let walk_root = root_path.clone();
        let project_root_display = root_path.display().to_string();
        let config_for_bg = config_snapshot.clone();
        let session_id_for_bg = log_ctx::current_session();
        let session_id_for_frame = session_id_for_bg.clone();
        let run_deferred_walk = move || {
            log_ctx::with_session(session_id_for_bg, || {
                delay_configure_deferred_walk_for_test();
                signal_configure_deferred_walk_start_for_test();
                let source_files: Vec<PathBuf> =
                    crate::callgraph::walk_project_files(&walk_root).collect();
                let detected_languages: HashSet<LangId> = source_files
                    .iter()
                    .filter_map(|path| detect_language(path))
                    .collect();
                let mut warnings =
                    detect_missing_tools_for_languages(&detected_languages, &config_for_bg)
                        .into_iter()
                        .map(|warning| json!(warning))
                        .collect::<Vec<_>>();
                warnings.extend(detect_missing_lsp_binaries(&source_files, &config_for_bg));

                let frame = crate::protocol::ConfigureWarningsFrame::new_with_session_id(
                    session_id_for_frame,
                    project_root_display,
                    warnings,
                );
                let _ = warning_tx.send((warning_generation, frame));
            });
        };
        if run_configure_deferred_walk_synchronously_for_test() {
            run_deferred_walk();
        } else {
            thread::spawn(run_deferred_walk);
        }
    }

    // Return the success response immediately so the plugin can mark the project as
    // configured. Missing-binary warnings are sent later in a `configure_warnings`
    // push frame.
    ctx.begin_configure_ack_phase("ack_ready");
    let response = Response::success(
        &req.id,
        json!({
            "project_root": root_path.display().to_string(),
            "warnings": configure_warnings,
            "warnings_pending": warnings_pending,
            "search_index_cache_reused": search_index_cache_reused,
            "artifact_owner": artifact_owner_status
                .as_ref()
                .map(|status| serde_json::to_value(status).unwrap_or(serde_json::Value::Null)),
            "config_dropped_keys": config_dropped_keys
                .iter()
                .map(|d| json!({ "key": d.key, "tier": d.tier, "reason": d.reason }))
                .collect::<Vec<_>>(),
        }),
    );
    response
}

#[derive(Clone, Copy, Debug, Default)]
struct ArtifactLoadNeeds {
    search: bool,
    semantic: bool,
}

fn missing_artifact_loads(ctx: &AppContext) -> ArtifactLoadNeeds {
    let config = ctx.config();
    let search_enabled = config.search_index;
    let semantic_enabled = config.semantic_search;
    drop(config);

    let search_index_missing = ctx
        .search_index()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_none();
    let search_not_building = ctx
        .search_index_rx()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_none();
    let search_missing = search_enabled && search_index_missing && search_not_building;
    let semantic_not_building = !matches!(
        &*ctx
            .semantic_index_status()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        SemanticIndexStatus::Building { .. }
    );
    let semantic_index_missing = ctx
        .semantic_index()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_none();
    let semantic_receiver_missing = ctx.semantic_index_rx().lock().is_none();
    let semantic_refresh_missing = !semantic_index_missing
        && !ctx.is_worktree_bridge()
        && !ctx.shared_artifacts_read_only()
        && ctx.semantic_refresh_sender().is_none();
    let semantic_missing = semantic_enabled
        && semantic_not_building
        && semantic_receiver_missing
        && (semantic_index_missing || semantic_refresh_missing);

    ArtifactLoadNeeds {
        search: search_missing,
        semantic: semantic_missing,
    }
}

fn schedule_missing_artifact_loads(
    ctx: &AppContext,
    request_search: bool,
    request_semantic: bool,
) -> Vec<crossbeam_channel::Sender<()>> {
    let _reload_guard = ctx.artifact_reload_guard();
    let missing = missing_artifact_loads(ctx);
    let load_search = request_search && missing.search;
    let load_semantic = request_semantic && missing.semantic;
    if !load_search && !load_semantic {
        return Vec::new();
    }
    schedule_artifact_loads(ctx, load_search, load_semantic)
}

fn start_artifact_loads(starts: Vec<crossbeam_channel::Sender<()>>) -> bool {
    let started = !starts.is_empty();
    for start in starts {
        let _ = start.send(());
    }
    started
}

/// Start a trigram reload after an idle eviction. The caller keeps serving the
/// current request through the bounded fallback walk. Read-only roots reopen the
/// shared snapshot without acquiring a writer lease.
///
/// The reloaded index starts with an empty RAM delta containing only the
/// borrowed base. Subsequent watcher events repopulate the overlay when
/// `worktree.ram_overlay` is enabled. Until the next watcher apply, eviction
/// may temporarily hide in-flight local edits from search; that is the
/// intended initial behavior.
pub(crate) fn trigger_search_index_reload_if_evicted(ctx: &AppContext) -> bool {
    if ctx.canonical_cache_root_opt().is_none() {
        return false;
    }
    let generation = ctx.configure_generation();
    ctx.run_if_subc_bound_generation(generation, || {
        start_artifact_loads(schedule_missing_artifact_loads(ctx, true, false))
    })
    .unwrap_or(false)
}

/// Restart a search-index load after its build receiver disconnected without
/// delivering an index (the worker exited through a generation/admission check
/// and dropped its sender). Without this, the root is left with no in-RAM index
/// and no receiver, so health reports "building" forever — nothing reschedules
/// the load until a later search query happens to call
/// [`trigger_search_index_reload_if_evicted`]. Capped via
/// `allow_search_index_disconnect_reschedule` at one automatic replacement per
/// configure generation; after that the query-triggered reload remains the
/// recovery path so a persistently failing worker cannot loop on the drain
/// thread.
pub(crate) fn restart_search_index_after_load_disconnect(ctx: &AppContext) -> bool {
    let generation = ctx.configure_generation();
    ctx.run_if_subc_bound_generation(generation, || {
        let _reload_guard = ctx.artifact_reload_guard();
        // A replacement (or a completed build) may have raced ahead of the drain;
        // only reschedule when the index is still genuinely missing and idle.
        // `missing_artifact_loads` also enforces `config.search_index`.
        if !missing_artifact_loads(ctx).search {
            return false;
        }
        if ctx.shared_artifacts_read_only() || ctx.canonical_cache_root_opt().is_none() {
            return false;
        }
        // Consume the one-per-generation slot only now that every guard passed,
        // so a raced-ahead replacement does not waste the slot.
        if !ctx.allow_search_index_disconnect_reschedule() {
            crate::slog_info!(
                "search index load disconnected without an index; automatic replacement already used for this configure generation, leaving query-triggered reload as recovery"
            );
            return false;
        }
        let started = start_artifact_loads(schedule_artifact_loads(ctx, true, false));
        if started {
            crate::slog_info!(
                "search index load disconnected without an index; scheduled one automatic replacement load"
            );
        }
        started
    })
    .unwrap_or(false)
}

#[cfg(test)]
pub(crate) fn set_semantic_refresh_restart_result_for_test(result: Option<bool>) {
    SEMANTIC_REFRESH_RESTART_ATTEMPTS.with(|attempts| attempts.set(0));
    SEMANTIC_REFRESH_RESTART_RESULT_OVERRIDE.with(|override_result| override_result.set(result));
}

#[cfg(test)]
pub(crate) fn semantic_refresh_restart_attempts_for_test() -> usize {
    SEMANTIC_REFRESH_RESTART_ATTEMPTS.with(std::cell::Cell::get)
}

pub(crate) fn restart_semantic_artifacts_after_refresh_disconnect(
    ctx: &AppContext,
    disconnected_build_epoch: u64,
) -> bool {
    #[cfg(test)]
    SEMANTIC_REFRESH_RESTART_ATTEMPTS.with(|attempts| attempts.set(attempts.get() + 1));

    let generation = ctx.configure_generation();
    // `run_if_subc_bound_generation` holds the lifecycle admission mutex. This
    // accessor also consults that mutex, so capture it before entering admission.
    let heavy_root_work_allowed = ctx.heavy_root_work_allowed();
    ctx.run_if_subc_bound_generation(generation, || {
        let _reload_guard = ctx.artifact_reload_guard();
        if ctx.semantic_refresh_event_rx().lock().is_some() {
            return true;
        }
        let (has_build_receiver, build_epoch) = {
            let receiver = ctx.semantic_index_rx().lock();
            (receiver.is_some(), ctx.semantic_index_rx_epoch())
        };
        if has_build_receiver {
            if ctx.shared_artifacts_read_only() || build_epoch != disconnected_build_epoch {
                return true;
            }
            // This build belongs to the refresh worker that just died. If it
            // later publishes Ready, the index would be installed without a
            // worker capable of keeping it current. Re-check its epoch under
            // the receiver lock so a concurrently installed replacement wins.
            if ctx.retire_semantic_index_rx_if_epoch(build_epoch).is_none() {
                return true;
            }
        }
        let config = ctx.config();
        let semantic_enabled = config.semantic_search;
        drop(config);
        if !semantic_enabled
            || !heavy_root_work_allowed
            || ctx.shared_artifacts_read_only()
            || ctx.canonical_cache_root_opt().is_none()
        {
            *ctx.semantic_index()
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
            *ctx.semantic_index_status()
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Failed(
                "semantic refresh worker disconnected and could not be restarted".to_string(),
            );
            return false;
        }

        #[cfg(test)]
        if let Some(result) = SEMANTIC_REFRESH_RESTART_RESULT_OVERRIDE.with(std::cell::Cell::get) {
            *ctx.semantic_index()
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
            *ctx.semantic_index_status()
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = if result {
                SemanticIndexStatus::Building {
                    stage: "restarting_refresh_worker".to_string(),
                    files: None,
                    entries_done: None,
                    entries_total: None,
                }
            } else {
                SemanticIndexStatus::Failed(
                    "semantic refresh worker disconnected and could not be restarted".to_string(),
                )
            };
            return result;
        }

        *ctx.semantic_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::ready();
        let started = start_artifact_loads(schedule_artifact_loads(ctx, false, true));
        if !started {
            *ctx.semantic_index_status()
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Failed(
                "semantic refresh worker disconnected and could not be restarted".to_string(),
            );
        }
        started
    })
    .unwrap_or(false)
}

/// Start a semantic reload after an idle eviction without blocking the current
/// query. Writer-owned failed indexes retain their no-retry behavior; read-only
/// roots may retry a previously absent shared snapshot.
pub(crate) fn trigger_semantic_index_reload_if_evicted(ctx: &AppContext) -> bool {
    if ctx.canonical_cache_root_opt().is_none() {
        return false;
    }
    let reloadable = match &*ctx
        .semantic_index_status()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
    {
        SemanticIndexStatus::Ready { refreshing, .. } => refreshing.is_empty(),
        SemanticIndexStatus::Failed(_) => ctx.shared_artifacts_read_only(),
        SemanticIndexStatus::Disabled | SemanticIndexStatus::Building { .. } => false,
    };
    if !reloadable {
        return false;
    }
    let generation = ctx.configure_generation();
    ctx.run_if_subc_bound_generation(generation, || {
        start_artifact_loads(schedule_missing_artifact_loads(ctx, false, true))
    })
    .unwrap_or(false)
}

fn schedule_artifact_loads(
    ctx: &AppContext,
    load_search: bool,
    load_semantic: bool,
) -> Vec<crossbeam_channel::Sender<()>> {
    let canonical_cache_root = ctx.canonical_cache_root();
    let project_key = ctx.memoized_artifact_cache_key(&canonical_cache_root);
    let config = ctx.config();
    let storage_dir = config.storage_dir.clone();
    let search_index_max_file_size = config.search_index_max_file_size;
    let semantic_config = config.semantic.clone();
    drop(config);
    let is_worktree_bridge = ctx.is_worktree_bridge();
    let configure_generation = ctx.configure_generation();
    let configure_content_generation = ctx.configure_content_generation();
    let configure_content_generation_flag = ctx.configure_content_generation_flag();
    let subc_lifecycle = ctx.subc_lifecycle_admission();
    let semantic_cold_seed_generation = ctx.semantic_cold_seed_generation();
    let semantic_fingerprint_generation = ctx.semantic_fingerprint_generation();
    let symbol_cache_generation = if load_search {
        ctx.reset_symbol_cache()
    } else {
        0
    };
    let mut artifact_load_starts = Vec::new();

    if load_search {
        let cache_dir = resolve_cache_dir_with_key(&project_key, storage_dir.as_deref());
        let root_for_search = canonical_cache_root.clone();
        let symbol_cache = ctx.symbol_cache();
        let symbol_storage = storage_dir.clone();
        let symbol_project_key = project_key.clone();
        let is_worktree_bridge_for_search = is_worktree_bridge;
        let shared_artifacts_read_only_for_search = ctx.shared_artifacts_read_only();
        let session_id_for_bg = log_ctx::current_session();
        let search_cold_build_limiter = ctx.cold_build_limiter();
        let search_generation = configure_generation;
        let search_generation_flag = ctx.configure_generation_flag();
        let search_content_generation = configure_content_generation;
        let search_content_generation_flag = Arc::clone(&configure_content_generation_flag);
        let search_lifecycle = subc_lifecycle.clone();
        let (tx, rx) = unbounded::<SearchIndex>();
        let search_rx_epoch = ctx.install_search_index_rx(rx, search_generation);
        let search_rx_terminal_guard = ctx.search_index_rx_terminal_guard(search_rx_epoch);
        let search_persist_epoch_flag = ctx.search_persist_epoch_flag();
        let (start_tx, start_rx) = crossbeam_channel::bounded::<()>(1);
        artifact_load_starts.push(start_tx);

        #[cfg(debug_assertions)]
        mark_search_rebuild_spawn_for_debug();
        thread::spawn(move || {
            let _terminal_guard = search_rx_terminal_guard;
            if start_rx.recv().is_err() {
                #[cfg(test)]
                note_configure_artifact_load_cancellation_for_test();
                return;
            }
            delay_configure_artifact_load_after_gate_for_test();
            if !search_lifecycle.is_current(search_generation_flag.as_ref(), search_generation) {
                #[cfg(test)]
                note_configure_artifact_load_cancellation_for_test();
                return;
            }
            log_ctx::with_session(session_id_for_bg.clone(), || {
                note_configure_artifact_load_attempt();
                if shared_artifacts_read_only_for_search {
                    match crate::readonly_artifacts::open_search_index_read_only(
                        &root_for_search,
                        symbol_storage.as_deref(),
                    ) {
                        crate::readonly_artifacts::ReadOnlyArtifact::Fresh(index) => {
                            let symbol_files = search_index_symbol_files(&index);
                            let _ = search_lifecycle.run_if_current(
                                search_generation_flag.as_ref(),
                                search_generation,
                                || {
                                    let _ = tx.send(index);
                                    spawn_symbol_cache_prewarm(
                                        root_for_search,
                                        symbol_cache,
                                        symbol_storage,
                                        symbol_project_key,
                                        symbol_cache_generation,
                                        symbol_files,
                                        true,
                                        log_ctx::current_session(),
                                    );
                                },
                            );
                        }
                        crate::readonly_artifacts::ReadOnlyArtifact::Stale(stale) => {
                            let symbol_files = search_index_symbol_files(&stale.index);
                            let _ = search_lifecycle.run_if_current(
                                search_generation_flag.as_ref(),
                                search_generation,
                                || {
                                    let _ = tx.send(stale.index);
                                    spawn_symbol_cache_prewarm(
                                        root_for_search,
                                        symbol_cache,
                                        symbol_storage,
                                        symbol_project_key,
                                        symbol_cache_generation,
                                        symbol_files,
                                        true,
                                        log_ctx::current_session(),
                                    );
                                },
                            );
                        }
                        crate::readonly_artifacts::ReadOnlyArtifact::Degraded(degradation) => {
                            slog_warn!(
                                "search index is read-only but loading stopped at the interactive budget ({})",
                                degradation.reason
                            );
                        }
                        crate::readonly_artifacts::ReadOnlyArtifact::Cancelled => {
                            slog_debug!("read-only search index load was cancelled");
                        }
                        crate::readonly_artifacts::ReadOnlyArtifact::Absent => {
                            slog_warn!(
                                "search index is read-only but no shared artifact snapshot exists"
                            );
                        }
                    }
                    return;
                }

                let Some(search_persist_epoch) = search_lifecycle.run_if_current(
                    search_generation_flag.as_ref(),
                    search_generation,
                    || search_persist_epoch_flag.next(),
                ) else {
                    return;
                };
                let Some(_permit) = crate::cold_build_limiter::acquire_blocking_while_with_limiter(
                    &search_cold_build_limiter,
                    "search index post-configure load",
                    || {
                        search_lifecycle
                            .is_current(search_generation_flag.as_ref(), search_generation)
                    },
                ) else {
                    return;
                };
                if search_persist_epoch_flag.current() != search_persist_epoch {
                    return;
                }
                // HEAD freshness is deliberately probed after the bind ack. The
                // helper bounds git so a wedged repository cannot pin maintenance.
                let current_head = current_git_head(&root_for_search);
                let _cache_lock = if is_worktree_bridge_for_search {
                    None
                } else {
                    match CacheLock::acquire(&cache_dir, &root_for_search) {
                        Ok(lock) => Some(lock),
                        Err(error) => {
                            slog_warn!("failed to acquire search artifact lock: {}", error);
                            return;
                        }
                    }
                };
                let cache_path = cache_dir.join("cache.bin");
                let artifact_generation = cache_freshness::artifact_generation(&cache_path);
                let verify_ticket = cache_freshness::capture_verify_ticket(&root_for_search);
                let verify_plan = cache_freshness::warm_verify_plan(
                    &root_for_search,
                    VerifyArtifact::Search,
                    artifact_generation,
                );
                let baseline = SearchIndex::read_from_disk(&cache_dir, &root_for_search);
                let mut persist_to_disk = true;
                let mut record_completed_verify = true;
                let mut index = match baseline {
                    Some(mut index)
                        if index.stored_git_head() == current_head.as_deref()
                            && index.configured_max_file_size() == search_index_max_file_size =>
                    {
                        index.set_ready(false);
                        match verify_plan {
                            WarmVerifyPlan::Skip => {
                                index.set_ready(true);
                                persist_to_disk = false;
                                record_completed_verify = false;
                            }
                            WarmVerifyPlan::StatFirst | WarmVerifyPlan::Strict => {
                                let strategy = match verify_plan {
                                    WarmVerifyPlan::StatFirst => VerifyStrategy::StatFirst,
                                    WarmVerifyPlan::Strict => VerifyStrategy::Strict,
                                    WarmVerifyPlan::Skip => unreachable!(),
                                };
                                let disk_changed = index.verify_against_disk_with_strategy(
                                    current_head.clone(),
                                    strategy,
                                );
                                persist_to_disk = disk_changed || index.has_pending_disk_changes();
                            }
                        }
                        index
                    }
                    mut baseline => {
                        if let Some(index) = baseline.as_mut() {
                            index.set_ready(false);
                        }
                        let strategy = match verify_plan {
                            WarmVerifyPlan::Strict => VerifyStrategy::Strict,
                            WarmVerifyPlan::Skip | WarmVerifyPlan::StatFirst => {
                                VerifyStrategy::StatFirst
                            }
                        };
                        let index = SearchIndex::rebuild_or_refresh_with_strategy(
                            &root_for_search,
                            search_index_max_file_size,
                            current_head,
                            baseline,
                            Some(&cache_dir),
                            strategy,
                        );
                        delay_search_rebuild_publish_for_debug();
                        index
                    }
                };
                // Once admitted, a build may finish generation-safe disk
                // persistence after its last route closes. This avoids throwing
                // away expensive completed work and rebuilding it on rebind.
                // Lifecycle admission still gates permit acquisition, in-memory
                // publication, refresh-worker startup, and symbol prewarming.
                let generation_current = || {
                    search_content_generation_flag.load(Ordering::SeqCst)
                        == search_content_generation
                };
                let mut persistence_succeeded = !persist_to_disk;
                // Borrow-only / worktree roots never persist, overlay or not.
                // `write_to_disk` also fail-closes via artifact_write_allowed.
                if generation_current() && !is_worktree_bridge_for_search && persist_to_disk {
                    let published =
                        search_persist_epoch_flag.run_if_current(search_persist_epoch, || {
                            let head = index.stored_git_head().map(str::to_owned);
                            index.write_to_disk(&cache_dir, head.as_deref())
                        });
                    persistence_succeeded = published == Some(true);
                    if published.is_none() {
                        slog_info!(
                            "search index persistence skipped for superseded worker epoch {}",
                            search_persist_epoch
                        );
                    }
                }
                if generation_current() && record_completed_verify && persistence_succeeded {
                    let _ = cache_freshness::record_verify_completed_if_unchanged(
                        &root_for_search,
                        VerifyArtifact::Search,
                        cache_freshness::artifact_generation(&cache_path),
                        verify_ticket,
                    );
                }
                let symbol_files = search_index_symbol_files(&index);
                if search_lifecycle
                    .run_if_current(search_generation_flag.as_ref(), search_generation, || {
                        let _ = tx.send(index);
                        spawn_symbol_cache_prewarm(
                            root_for_search,
                            symbol_cache,
                            symbol_storage,
                            symbol_project_key,
                            symbol_cache_generation,
                            symbol_files,
                            false,
                            log_ctx::current_session(),
                        );
                    })
                    .is_none()
                {
                    slog_info!(
                        "search index build result discarded for stale generation {}",
                        search_generation
                    );
                }
            });
        });
    }

    if load_semantic && ctx.shared_artifacts_read_only() {
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Building {
            stage: "loading_artifacts".to_string(),
            files: None,
            entries_done: None,
            entries_total: None,
        };
        let (tx, rx) = unbounded::<SemanticIndexEvent>();
        let semantic_rx_epoch = ctx.install_semantic_index_rx(rx, configure_generation);
        let semantic_rx_terminal_guard = ctx.semantic_index_rx_terminal_guard(semantic_rx_epoch);
        let (start_tx, start_rx) = crossbeam_channel::bounded::<()>(1);
        artifact_load_starts.push(start_tx);
        let semantic_root = canonical_cache_root.clone();
        let semantic_storage = storage_dir.clone();
        let semantic_load_generation = configure_generation;
        let semantic_load_generation_flag = ctx.configure_generation_flag();
        let semantic_load_lifecycle = subc_lifecycle.clone();
        let session_id = log_ctx::current_session();
        thread::spawn(move || {
            let _terminal_guard = semantic_rx_terminal_guard;
            if start_rx.recv().is_err() {
                #[cfg(test)]
                note_configure_artifact_load_cancellation_for_test();
                return;
            }
            delay_configure_artifact_load_after_gate_for_test();
            if !semantic_load_lifecycle.is_current(
                semantic_load_generation_flag.as_ref(),
                semantic_load_generation,
            ) {
                #[cfg(test)]
                note_configure_artifact_load_cancellation_for_test();
                return;
            }
            log_ctx::with_session(session_id, || {
                note_configure_artifact_load_attempt();
                let event = match crate::readonly_artifacts::open_semantic_index_read_only(
                    &semantic_root,
                    semantic_storage.as_deref(),
                ) {
                    crate::readonly_artifacts::ReadOnlyArtifact::Fresh(index) => {
                        SemanticIndexEvent::Ready(index)
                    }
                    crate::readonly_artifacts::ReadOnlyArtifact::Stale(stale) => {
                        slog_warn!(
                            "semantic index is read-only and stale for {} file(s); serving stale snapshot without repairing shared artifacts",
                            stale.drift_count
                        );
                        SemanticIndexEvent::Ready(stale.index)
                    }
                    crate::readonly_artifacts::ReadOnlyArtifact::Degraded(degradation) => {
                        SemanticIndexEvent::Failed(format!(
                            "semantic index is read-only but loading stopped at the interactive budget ({})",
                            degradation.reason
                        ))
                    }
                    crate::readonly_artifacts::ReadOnlyArtifact::Cancelled => {
                        SemanticIndexEvent::Failed(
                            "read-only semantic index load was cancelled".to_string(),
                        )
                    }
                    crate::readonly_artifacts::ReadOnlyArtifact::Absent => {
                        SemanticIndexEvent::Failed(
                            "semantic index is read-only but no shared artifact snapshot exists"
                                .to_string(),
                        )
                    }
                };
                let _ = semantic_load_lifecycle.run_if_current(
                    semantic_load_generation_flag.as_ref(),
                    semantic_load_generation,
                    || {
                        let _ = tx.send(event);
                    },
                );
            });
        });
    } else if load_semantic {
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Building {
            stage: "loading_artifacts".to_string(),
            files: None,
            entries_done: None,
            entries_total: None,
        };
        let (tx, rx): (
            crossbeam_channel::Sender<SemanticIndexEvent>,
            crossbeam_channel::Receiver<SemanticIndexEvent>,
        ) = unbounded();
        let semantic_rx_epoch = ctx.install_semantic_index_rx(rx, configure_generation);
        let semantic_rx_terminal_guard = ctx.semantic_index_rx_terminal_guard(semantic_rx_epoch);
        let semantic_persist_epoch_flag = ctx.semantic_persist_epoch_flag();
        let semantic_persist_lock = ctx.semantic_persist_lock();
        let semantic_verify_ticket = cache_freshness::capture_verify_ticket(&canonical_cache_root);

        let (refresh_tx, refresh_rx) = unbounded::<SemanticRefreshRequest>();
        let (refresh_event_tx, refresh_event_rx) = unbounded::<SemanticRefreshEvent>();
        let refresh_worker_slot: SemanticRefreshWorkerSlot = Arc::new(Mutex::new(None));
        ctx.install_semantic_refresh_worker_for_build_epoch(
            refresh_tx,
            refresh_event_rx,
            Arc::clone(&refresh_worker_slot),
            semantic_rx_epoch,
        );

        let root_clone = canonical_cache_root.clone();
        let semantic_storage = storage_dir.clone();
        let semantic_project_key = project_key.clone();
        let semantic_config = semantic_config.clone();
        let tx_progress = tx.clone();
        let is_worktree_bridge_for_semantic = is_worktree_bridge;
        let semantic_cold_seed_active = ctx.semantic_cold_seed_active_flag();
        let semantic_cold_build_limiter = ctx.cold_build_limiter();
        let semantic_cold_seed_generation_flag = ctx.semantic_cold_seed_generation_flag();
        let semantic_cold_seed_generation_for_worker = semantic_cold_seed_generation;
        let semantic_generation = configure_generation;
        let semantic_generation_flag = ctx.configure_generation_flag();
        let semantic_lifecycle = subc_lifecycle.clone();
        let semantic_fingerprint_generation_flag = ctx.semantic_fingerprint_generation_flag();
        let session_id_for_bg2 = log_ctx::current_session();
        let (start_tx, start_rx) = crossbeam_channel::bounded::<()>(1);
        artifact_load_starts.push(start_tx);
        thread::spawn(move || {
            let _terminal_guard = semantic_rx_terminal_guard;
            if start_rx.recv().is_err() {
                #[cfg(test)]
                note_configure_artifact_load_cancellation_for_test();
                return;
            }
            delay_configure_artifact_load_after_gate_for_test();
            if !semantic_lifecycle
                .is_current(semantic_generation_flag.as_ref(), semantic_generation)
            {
                #[cfg(test)]
                note_configure_artifact_load_cancellation_for_test();
                return;
            }
            let Some(semantic_persist_epoch) = semantic_lifecycle.run_if_current(
                semantic_generation_flag.as_ref(),
                semantic_generation,
                || semantic_persist_epoch_flag.next(),
            ) else {
                #[cfg(test)]
                note_configure_artifact_load_cancellation_for_test();
                return;
            };
            note_configure_artifact_load_attempt();
            log_ctx::with_session(session_id_for_bg2, || {
                // Cap file count to bound memory on huge project roots (e.g.,
                // /home/user). The local fastembed model (~200MB) + embeddings +
                // batch buffers can exceed memory on constrained systems when
                // indexing tens of thousands of files. Configurable via
                // `semantic.max_files` (default 20k); remote backends that embed
                // server-side can raise it freely.
                let max_semantic_files = semantic_config.max_files;
                let mut semantic_retry_attempt: usize = 0;
                let set_cold_seed_active = || {
                    if semantic_cold_seed_generation_flag.load(std::sync::atomic::Ordering::SeqCst)
                        == semantic_cold_seed_generation_for_worker
                    {
                        semantic_cold_seed_active.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                };
                let clear_cold_seed_active = || {
                    if semantic_cold_seed_generation_flag.load(std::sync::atomic::Ordering::SeqCst)
                        == semantic_cold_seed_generation_for_worker
                    {
                        semantic_cold_seed_active.store(false, std::sync::atomic::Ordering::SeqCst);
                    }
                };
                let clear_cold_seed_gate_and_notify = || {
                    clear_cold_seed_active();
                    let _ = tx_progress.send(SemanticIndexEvent::ColdSeedGateCleared);
                };

                struct SemanticBuildReady {
                    index: SemanticIndex,
                    model: crate::semantic_index::EmbeddingModel,
                    persist_to_disk: bool,
                    record_verify_completion: bool,
                    verified_artifact_generation: Option<cache_freshness::ArtifactGeneration>,
                }

                let build_once = || -> Result<SemanticBuildReady, String> {
                    let _ = tx_progress.send(SemanticIndexEvent::Progress {
                        stage: "initializing_embedding_model".to_string(),
                        files: None,
                        entries_done: None,
                        entries_total: None,
                    });
                    let mut model =
                        crate::semantic_index::EmbeddingModel::from_config(&semantic_config)?;
                    let fingerprint = model.fingerprint(&semantic_config)?;
                    let fingerprint_key = fingerprint.as_string();
                    let _semantic_cache_lock = (!is_worktree_bridge_for_semantic)
                        .then(|| ())
                        .and_then(|_| semantic_storage.as_ref())
                        .and_then(|dir| {
                            match SemanticIndexLock::acquire(
                                dir,
                                &semantic_project_key,
                                &root_clone,
                            ) {
                                Ok(lock) => Some(lock),
                                Err(error) => {
                                    slog_warn!("failed to acquire semantic cache lock: {}", error);
                                    None
                                }
                            }
                        });

                    let mut verified_artifact_generation = None;
                    if let Some(ref dir) = semantic_storage {
                        let data_path = dir
                            .join("semantic")
                            .join(&semantic_project_key)
                            .join("semantic.bin");
                        verified_artifact_generation =
                            cache_freshness::artifact_generation(&data_path);
                        let verify_plan = cache_freshness::warm_verify_plan(
                            &root_clone,
                            VerifyArtifact::Semantic,
                            verified_artifact_generation,
                        );
                        if let Some(cached) = SemanticIndex::read_from_disk(
                            dir,
                            &semantic_project_key,
                            &root_clone,
                            is_worktree_bridge_for_semantic,
                            Some(&fingerprint_key),
                        ) {
                            clear_cold_seed_gate_and_notify();
                            if verify_plan == WarmVerifyPlan::Skip {
                                slog_info!(
                                    "semantic index: recently verified cache generation reused ({} entries)",
                                    cached.entry_count(),
                                );
                                return Ok(SemanticBuildReady {
                                    index: cached,
                                    model,
                                    persist_to_disk: false,
                                    record_verify_completion: false,
                                    verified_artifact_generation,
                                });
                            }
                            // Try incremental refresh: re-embed only changed/new files,
                            // drop entries for deleted files, keep everything else.
                            // This is the hot path for restart on a project with a
                            // handful of edits — avoids re-embedding 4000+ unchanged
                            // files just to pick up 10 changes.
                            let current_files = match walk_semantic_project_files_bounded(
                                &root_clone,
                                max_semantic_files,
                            ) {
                                Ok(files) => files,
                                Err(observed) => {
                                    slog_warn!(
                                        "skipping semantic index: more than {} files exceeds limit of {}. \
                                         Raise semantic.max_files or open a specific project directory.",
                                        observed.saturating_sub(1),
                                        max_semantic_files
                                    );
                                    return Err(format!(
                                        "too many files (>{}) for semantic indexing (max {})",
                                        max_semantic_files, max_semantic_files
                                    ));
                                }
                            };

                            let mut cached = cached;
                            let mut embed = |texts: Vec<String>| model.embed(texts);
                            let _ = tx_progress.send(SemanticIndexEvent::Progress {
                                stage: "refreshing_stale_files".to_string(),
                                files: None,
                                entries_done: None,
                                entries_total: None,
                            });
                            let mut progress = |done: usize, total: usize| {
                                let _ = tx_progress.send(SemanticIndexEvent::Progress {
                                    stage: "embedding_stale_symbols".to_string(),
                                    files: None,
                                    entries_done: Some(done),
                                    entries_total: Some(total),
                                });
                            };

                            let verify_strategy = match verify_plan {
                                WarmVerifyPlan::StatFirst => VerifyStrategy::StatFirst,
                                WarmVerifyPlan::Strict => VerifyStrategy::Strict,
                                WarmVerifyPlan::Skip => unreachable!(),
                            };
                            // This is the post-bind cached-index catch-up path.
                            // It runs on the deferred artifact thread, not the bind
                            // acknowledgement path, and waits only while this root
                            // remains bound so an unbound root cannot take a slot.
                            let Some(_refresh_permit) =
                                crate::cold_build_limiter::acquire_blocking_while_with_limiter(
                                    &semantic_cold_build_limiter,
                                    SEMANTIC_REFRESH_LIMITER_KIND,
                                    || {
                                        semantic_lifecycle.is_current(
                                            semantic_generation_flag.as_ref(),
                                            semantic_generation,
                                        )
                                    },
                                )
                            else {
                                return Err(
                                    "semantic post-bind refresh cancelled because root is unbound"
                                        .to_string(),
                                );
                            };
                            match cached.refresh_stale_files_with_strategy(
                                &root_clone,
                                &current_files,
                                &mut embed,
                                semantic_config.max_batch_size.max(1),
                                &mut progress,
                                verify_strategy,
                            ) {
                                Ok(summary) => {
                                    if summary.is_noop() {
                                        slog_info!(
                                            "semantic index: cached index is current ({} entries)",
                                            cached.entry_count(),
                                        );
                                    } else {
                                        slog_info!(
                                            "semantic index: refreshed incrementally — {} changed, {} new, {} deleted, {} total processed (kept {} cached)",
                                            summary.changed,
                                            summary.added,
                                            summary.deleted,
                                            summary.total_processed,
                                            cached.len(),
                                        );
                                        cached.set_fingerprint(fingerprint);
                                    }
                                    let persist_to_disk = !summary.is_noop();
                                    let _ = tx_progress.send(SemanticIndexEvent::Progress {
                                        stage: "loaded_cached_index".to_string(),
                                        files: None,
                                        entries_done: Some(cached.entry_count()),
                                        entries_total: Some(cached.entry_count()),
                                    });
                                    return Ok(SemanticBuildReady {
                                        index: cached,
                                        model,
                                        persist_to_disk,
                                        record_verify_completion: true,
                                        verified_artifact_generation,
                                    });
                                }
                                Err(error) => {
                                    if crate::semantic_index::embedding_failure_is_transient(&error)
                                    {
                                        // TRANSIENT backend error (e.g. the embedding
                                        // server is overloaded by concurrent bridges, or
                                        // briefly unreachable). Do NOT drop the cache and
                                        // full-rebuild: a full corpus re-embed against an
                                        // already-overloaded backend amplifies the overload
                                        // and cascades to other bridges AND the main
                                        // session (every bridge's incremental refresh then
                                        // fails transiently and full-rebuilds too). Keep
                                        // serving the valid cached index; the handful of
                                        // changed files re-embed on a later refresh once the
                                        // backend recovers. Mirrors the watcher-refresh
                                        // self-heal in main.rs.
                                        let clean =
                                            crate::semantic_index::strip_transient_embedding_marker(
                                                &error,
                                            );
                                        slog_warn!(
                                            "incremental refresh hit a transient backend error ({}); keeping the cached index instead of full-rebuilding",
                                            clean
                                        );
                                        return Ok(SemanticBuildReady {
                                            index: cached,
                                            model,
                                            persist_to_disk: false,
                                            record_verify_completion: false,
                                            verified_artifact_generation,
                                        });
                                    }
                                    // Permanent failure (dimension mismatch, etc.): the
                                    // cache is genuinely unusable, drop it and full-rebuild.
                                    slog_warn!(
                                        "incremental refresh failed ({}), falling back to full rebuild",
                                        error
                                    );
                                }
                            }
                        }
                    }

                    set_cold_seed_active();

                    let files = match walk_semantic_project_files_bounded(
                        &root_clone,
                        max_semantic_files,
                    ) {
                        Ok(files) => {
                            let _ = tx_progress.send(SemanticIndexEvent::Progress {
                                stage: "scanned_project_files".to_string(),
                                files: Some(files.len()),
                                entries_done: None,
                                entries_total: None,
                            });
                            files
                        }
                        Err(observed) => {
                            let _ = tx_progress.send(SemanticIndexEvent::Progress {
                                stage: "scanned_project_files".to_string(),
                                files: Some(observed),
                                entries_done: None,
                                entries_total: None,
                            });
                            slog_warn!(
                                "skipping semantic index: more than {} files exceeds limit of {}. \
                                 Raise semantic.max_files or open a specific project directory.",
                                observed.saturating_sub(1),
                                max_semantic_files
                            );
                            return Err(format!(
                                "too many files (>{}) for semantic indexing (max {})",
                                max_semantic_files, max_semantic_files
                            ));
                        }
                    };

                    let mut embed = |texts: Vec<String>| model.embed(texts);

                    let _ = tx_progress.send(SemanticIndexEvent::Progress {
                        stage: "extracting_symbols".to_string(),
                        files: Some(files.len()),
                        entries_done: None,
                        entries_total: None,
                    });
                    let mut progress = |done: usize, total: usize| {
                        let _ = tx_progress.send(SemanticIndexEvent::Progress {
                            stage: "embedding_symbols".to_string(),
                            files: Some(files.len()),
                            entries_done: Some(done),
                            entries_total: Some(total),
                        });
                    };
                    let index = SemanticIndex::build_with_progress(
                        &root_clone,
                        &files,
                        &mut embed,
                        semantic_config.max_batch_size.max(1),
                        &mut progress,
                    )?;
                    let mut index = index;
                    index.set_fingerprint(fingerprint);
                    slog_info!(
                        "built semantic index: {} files, {} entries",
                        files.len(),
                        index.len()
                    );
                    let _ = tx_progress.send(SemanticIndexEvent::Progress {
                        stage: "persisting_index".to_string(),
                        files: Some(files.len()),
                        entries_done: Some(index.len()),
                        entries_total: Some(index.len()),
                    });

                    Ok(SemanticBuildReady {
                        index,
                        model,
                        persist_to_disk: true,
                        record_verify_completion: true,
                        verified_artifact_generation,
                    })
                };

                // Build-level retry: if the embedding backend is unreachable or
                // briefly failing (connection refused, timeout, 5xx/429), riding
                // it out beats parking the index in `Failed` forever — a state
                // nothing re-triggers short of a bridge restart. We keep retrying
                // with capped backoff, surfacing an honest "waiting for backend"
                // building-state so the sidebar shows recovery-in-progress, not a
                // red failure. The moment the backend returns, the build
                // succeeds and the index goes Ready.
                //
                // Permanent errors (dimension mismatch, too-many-files, 4xx auth)
                // are NOT marked transient and fail fast with the real message.
                //
                // Supersession is automatic: a reconfigure replaces the bridge's
                // semantic receiver, so the next `tx`/`tx_progress.send` returns
                // Err (receiver dropped) and this thread exits without competing
                // with the fresh build.
                let build_result = loop {
                    if !semantic_lifecycle
                        .is_current(semantic_generation_flag.as_ref(), semantic_generation)
                    {
                        clear_cold_seed_active();
                        return;
                    }
                    let attempt_result = catch_unwind(AssertUnwindSafe(&build_once));
                    match attempt_result {
                        Ok(Err(ref error))
                            if crate::semantic_index::embedding_failure_is_transient(error) =>
                        {
                            let clean =
                                crate::semantic_index::strip_transient_embedding_marker(error);
                            let backoff = semantic_build_retry_backoff(semantic_retry_attempt);
                            semantic_retry_attempt += 1;
                            slog_warn!(
                            "semantic index build: embedding backend unavailable ({}); retrying in {}s",
                            clean,
                            backoff.as_secs(),
                        );
                            // Surface "waiting for backend" as a building stage so
                            // the sidebar shows recovery-in-progress. If the
                            // receiver is gone (reconfigure superseded us), bail.
                            clear_cold_seed_active();
                            if tx_progress
                                .send(SemanticIndexEvent::Progress {
                                    stage: format!("waiting_for_embedding_backend: {clean}"),
                                    files: None,
                                    entries_done: None,
                                    entries_total: None,
                                })
                                .is_err()
                            {
                                return;
                            }
                            if tx_progress
                                .send(SemanticIndexEvent::ColdSeedGateCleared)
                                .is_err()
                            {
                                return;
                            }
                            thread::sleep(backoff);
                            continue;
                        }
                        other => break other,
                    }
                };

                enum SemanticBuildOutcome {
                    Ready(SemanticBuildReady),
                    Failed(String),
                }

                let outcome = match build_result {
                    Ok(Ok(ready)) => SemanticBuildOutcome::Ready(ready),
                    Ok(Err(error)) => {
                        slog_warn!("failed to build semantic index: {}", error);
                        SemanticBuildOutcome::Failed(error)
                    }
                    Err(_) => {
                        let error = "semantic index build panicked".to_string();
                        slog_warn!("{}", error);
                        SemanticBuildOutcome::Failed(error)
                    }
                };

                // A worker admitted while bound may finish fingerprint-safe disk
                // persistence after unbind. The completed artifact is reusable on
                // the next bind; lifecycle admission separately prevents memory
                // publication and refresh-worker startup after route teardown.
                let persist_completed_index = |index: &SemanticIndex,
                                               reason: &str,
                                               write_artifact: bool,
                                               record_verify_completion: bool,
                                               verified_artifact_generation: Option<
                    cache_freshness::ArtifactGeneration,
                >|
                 -> bool {
                    if is_worktree_bridge_for_semantic {
                        return false;
                    }
                    let Some(dir) = semantic_storage.as_ref() else {
                        // Should be unreachable now that configure defaults
                        // storage_dir; keep it loud in case a path regresses.
                        slog_warn!(
                            "semantic index persistence skipped for {reason}: no storage_dir resolved"
                        );
                        return false;
                    };
                    let _persist_order = semantic_persist_lock.lock();
                    let Ok(_cache_lock) =
                        SemanticIndexLock::acquire(dir, &semantic_project_key, &root_clone)
                    else {
                        slog_warn!("semantic index persistence lock unavailable for {reason}");
                        return false;
                    };
                    if semantic_fingerprint_generation_flag
                        .load(std::sync::atomic::Ordering::SeqCst)
                        != semantic_fingerprint_generation
                    {
                        slog_info!(
                            "semantic index persistence skipped for {reason}: semantic fingerprint changed after generation {} started",
                            semantic_generation
                        );
                        return false;
                    }
                    let data_path = dir
                        .join("semantic")
                        .join(&semantic_project_key)
                        .join("semantic.bin");
                    if cache_freshness::artifact_generation(&data_path)
                        != verified_artifact_generation
                    {
                        slog_info!(
                            "semantic index persistence skipped for {reason}: artifact generation changed after verification"
                        );
                        return false;
                    }
                    if write_artifact {
                        let published = semantic_persist_epoch_flag
                            .run_if_current(semantic_persist_epoch, || {
                                index.write_to_disk(dir, &semantic_project_key)
                            });
                        if published != Some(true) {
                            if published.is_none() {
                                slog_info!(
                                    "semantic index persistence skipped for superseded worker epoch {}",
                                    semantic_persist_epoch
                                );
                            }
                            return false;
                        }
                    }
                    if record_verify_completion {
                        let generation = cache_freshness::artifact_generation(&data_path);
                        let _ = cache_freshness::record_verify_completed_if_unchanged(
                            &root_clone,
                            VerifyArtifact::Semantic,
                            generation,
                            semantic_verify_ticket,
                        );
                    }
                    true
                };

                if !semantic_lifecycle
                    .is_current(semantic_generation_flag.as_ref(), semantic_generation)
                {
                    if let SemanticBuildOutcome::Ready(ready) = &outcome {
                        if ready.persist_to_disk {
                            persist_completed_index(
                                &ready.index,
                                "stale generation discard",
                                true,
                                false,
                                ready.verified_artifact_generation,
                            );
                        }
                    }
                    note_semantic_stale_generation_discard();
                    slog_info!(
                        "semantic index build result discarded for stale generation {}",
                        semantic_generation
                    );
                    clear_cold_seed_active();
                    return;
                }

                let event = match outcome {
                    SemanticBuildOutcome::Ready(ready) => {
                        let SemanticBuildReady {
                            index,
                            model,
                            persist_to_disk,
                            record_verify_completion,
                            verified_artifact_generation,
                        } = ready;
                        if persist_to_disk || record_verify_completion {
                            let _ = persist_completed_index(
                                &index,
                                "completed build",
                                persist_to_disk,
                                record_verify_completion,
                                verified_artifact_generation,
                            );
                        }
                        semantic_lifecycle.run_if_current(
                            semantic_generation_flag.as_ref(),
                            semantic_generation,
                            || {
                                let worker_index = index.clone();
                                let worker_handle = spawn_semantic_refresh_worker(
                                    root_clone.clone(),
                                    worker_index,
                                    model,
                                    semantic_config.max_batch_size.max(1),
                                    semantic_config.max_files,
                                    refresh_rx,
                                    refresh_event_tx,
                                    semantic_lifecycle.clone(),
                                    Arc::clone(&semantic_generation_flag),
                                    semantic_generation,
                                    SemanticRefreshLimiter(Arc::clone(
                                        &semantic_cold_build_limiter,
                                    )),
                                    log_ctx::current_session(),
                                );
                                if let Ok(mut slot) = refresh_worker_slot.lock() {
                                    *slot = Some(worker_handle);
                                }
                                SemanticIndexEvent::Ready(index)
                            },
                        )
                    }
                    SemanticBuildOutcome::Failed(error) => semantic_lifecycle.run_if_current(
                        semantic_generation_flag.as_ref(),
                        semantic_generation,
                        || SemanticIndexEvent::Failed(error),
                    ),
                };

                if event.is_none_or(|event| tx.send(event).is_err()) {
                    clear_cold_seed_active();
                }
            });
        });
    }

    artifact_load_starts
}

fn replay_configure_session(ctx: &AppContext, job: &ConfigureMaintenanceJob) {
    crate::bash_background::repair_legacy_root_tasks(&job.storage_root, job.harness.clone());
    #[cfg(test)]
    CONFIGURE_REPLAY_SESSION_CALLS.fetch_add(1, Ordering::SeqCst);
    if let Err(error) = ctx.bash_background().replay_session_for_project(
        &job.harness_dir,
        &job.session_id,
        &job.root_path,
    ) {
        slog_warn!("failed to replay background bash tasks: {error}");
    }
}

fn forget_configure_job_binding(ctx: &AppContext, job: &ConfigureMaintenanceJob) {
    if job.run_bash_replay {
        ctx.forget_configure_session_binding(&job.canonical_cache_root, &job.session_id);
    }
}

fn cancel_unbound_configure_jobs(
    ctx: &AppContext,
    jobs: impl IntoIterator<Item = ConfigureMaintenanceJob>,
) {
    let mut cancelled_any = false;
    for job in jobs {
        cancelled_any = true;
        forget_configure_job_binding(ctx, &job);
    }
    if cancelled_any {
        ctx.invalidate_configure_warm_state();
    }
    ctx.cancel_unbound_artifact_work();
}

pub(crate) fn cancel_deferred_configure_maintenance(ctx: &AppContext) -> usize {
    let jobs = ctx.drain_configure_maintenance();
    let cancelled = jobs.len();
    cancel_unbound_configure_jobs(ctx, jobs);
    cancelled
}

#[doc(hidden)]
pub fn drain_deferred_configure_maintenance(ctx: &AppContext) {
    let mut jobs = ctx.drain_configure_maintenance().into_iter();
    while let Some(job) = jobs.next() {
        if ctx.subc_unbound_quiesced() {
            cancel_unbound_configure_jobs(ctx, std::iter::once(job).chain(jobs));
            return;
        }

        if ctx.configure_generation() != job.generation {
            slog_info!(
                "dropping stale configure maintenance for generation {} (current {})",
                job.generation,
                ctx.configure_generation()
            );
            // The superseding configure re-runs everything root-scoped, but
            // bash replay is per-(root, session) and gated on the first bind
            // of that session; forget the binding so the session's next bind
            // replays its tasks instead of losing them to the dropped job.
            forget_configure_job_binding(ctx, &job);
            continue;
        }

        let session_only = job.run_bash_replay
            && !job.format_tool_cache_clear_needed
            && !job.refresh_project_runtime
            && !job.sync_bash_compress_flag
            && !job.reset_filter_registry
            && !job.clear_failed_spawns
            && !job.warm_callgraph_store
            && job.artifact_load_starts.is_empty();
        if session_only {
            replay_configure_session(ctx, &job);
            continue;
        }

        delay_configure_deferred_maintenance_for_test();
        // Artifact workers are created in a blocked state during configure so
        // no deserialization can race ahead of the bind acknowledgement. The
        // lifecycle gate makes the bound check and gate release one admission:
        // final-route teardown either precedes all starts or follows all starts.
        if ctx
            .run_if_subc_bound_generation(job.generation, || {
                if job.supersede_artifact_persistence {
                    ctx.next_search_persist_epoch();
                    ctx.next_semantic_persist_epoch();
                    ctx.next_callgraph_persist_epoch();
                }
                for start in &job.artifact_load_starts {
                    let _ = start.send(());
                }
            })
            .is_none()
        {
            if ctx.subc_unbound_quiesced() {
                cancel_unbound_configure_jobs(ctx, std::iter::once(job).chain(jobs));
                return;
            }
            forget_configure_job_binding(ctx, &job);
            continue;
        }

        if job.format_tool_cache_clear_needed {
            crate::format::clear_tool_cache_for_root(Some(&job.root_path));
        }

        ctx.backup()
            .lock()
            .set_db_project_key(crate::path_identity::project_scope_key(
                &job.canonical_cache_root,
            ));

        if let Some(storage_dir) = ctx.config().storage_dir.clone() {
            // Ensure the storage root exists for persistence subsystems. This is
            // maintenance work: the configure ack only needs the accepted config
            // snapshot, while disk-backed stores can converge immediately after.
            if let Err(err) = fs::create_dir_all(&storage_dir) {
                slog_warn!(
                    "failed to create storage directory {}: {}",
                    storage_dir.display(),
                    err
                );
            }
            ctx.backup().lock().set_storage_dir_for_harness(
                storage_dir,
                job.harness.clone(),
                ctx.config().checkpoint_ttl_hours,
            );
        }

        if job.refresh_project_runtime {
            // Rebuild gitignore matcher used by the watcher event filter to honor
            // the user's `.gitignore` files instead of a hardcoded directory list.
            // Skipped entirely for home roots because that walk would traverse
            // `$HOME`.
            if !job.home_match {
                ctx.rebuild_gitignore();
            } else {
                ctx.clear_gitignore();
            }
        }

        match crate::url_fetch::cleanup_url_cache(&job.storage_root) {
            Ok(0) => {}
            Ok(n) => slog_info!("URL cache cleanup: removed {} stale entries", n),
            Err(err) => slog_warn!("URL cache cleanup failed: {}", err),
        }

        let db_path = job.storage_root.join("aft.db");
        match ctx.app().open_db(&db_path) {
            Ok(shared) => {
                ctx.backup().lock().set_db_pool(shared.clone());
                ctx.bash_background().set_db_pool(shared);
            }
            Err(err) => {
                // Do not clear the process-shared handle if another root is
                // already using it. A failed root configure must not close that
                // root's SQLite connection and WAL descriptors.
                ctx.app().clear_db_for_path(&db_path);
                ctx.backup().lock().clear_db_pool();
                ctx.bash_background().clear_db_pool();
                slog_warn!(
                    "failed to open aft.db at {}: {} — running with JSON-only persistence",
                    db_path.display(),
                    err
                );
            }
        }

        match crate::migrate_storage::cleanup_staging_dirs(&job.storage_root, job.harness.clone()) {
            Ok(0) => {}
            Ok(n) => slog_info!(
                "swept {} staging directory orphans from prior migrations",
                n
            ),
            Err(err) => slog_warn!(
                "staging cleanup failed: {} (will retry next configure)",
                err
            ),
        }

        let config = ctx.config();
        ctx.bash_background().configure_long_running_reminders(
            config.bash_long_running_reminder_enabled,
            config.bash_long_running_reminder_interval_ms,
        );
        drop(config);

        if job.run_bash_replay {
            replay_configure_session(ctx, &job);
        }

        if job.refresh_project_runtime {
            if ctx
                .run_if_subc_bound_generation(job.generation, || ())
                .is_none()
            {
                if ctx.subc_unbound_quiesced() {
                    cancel_unbound_configure_jobs(ctx, std::iter::once(job).chain(jobs));
                    return;
                }
                forget_configure_job_binding(ctx, &job);
                continue;
            }

            // Joining a prior FSEvents thread can block in the OS. Do that
            // outside lifecycle admission, then reacquire admission only for
            // the atomic watcher start. An unbind or superseding configure that
            // wins during the join prevents the replacement from being started.
            ctx.stop_watcher_runtime();
            if ctx
                .run_if_subc_bound_generation(job.generation, || {
                    if !job.home_match {
                        start_project_watcher(ctx, &job.canonical_cache_root);
                    }
                })
                .is_none()
            {
                if ctx.subc_unbound_quiesced() {
                    cancel_unbound_configure_jobs(ctx, std::iter::once(job).chain(jobs));
                    return;
                }
                forget_configure_job_binding(ctx, &job);
                continue;
            }
        }

        if job.sync_bash_compress_flag {
            ctx.sync_bash_compress_flag();
        }
        if job.reset_filter_registry {
            ctx.reset_filter_registry();
        }

        if job.clear_failed_spawns {
            // Forget cached LSP spawn FAILURES when configure inputs changed. A
            // pure equivalent rebind keeps this cache hot; a real config/root
            // change lets the next file event retry previously missing servers.
            let cleared = ctx.lsp().clear_failed_spawns();
            if cleared > 0 {
                slog_debug!(
                    "configure: cleared {} cached LSP spawn failure(s) for retry",
                    cleared
                );
            }
        }

        if job.warm_callgraph_store && callgraph_configure_warm_allowed(ctx) {
            if ctx.semantic_cold_seed_active() {
                ctx.defer_callgraph_store_warm_for_semantic_cold_seed();
                slog_info!(
                    "callgraph store warm deferred until semantic cold seed gate clears or completes"
                );
            } else {
                match ctx.schedule_callgraph_store_warm() {
                    CallgraphStoreAccess::Ready(_) => {
                        slog_debug!("callgraph store ready at configure maintenance");
                    }
                    CallgraphStoreAccess::Building => {
                        slog_info!("callgraph store warm build scheduled by configure maintenance");
                    }
                    CallgraphStoreAccess::Suspended(suspension) => {
                        slog_warn!(
                            "callgraph store warm suspended for {} after {} deaths; run doctor reset-build-breaker",
                            suspension.domain.as_str(),
                            suspension.death_count
                        );
                    }
                    CallgraphStoreAccess::Unavailable => {
                        slog_info!(
                            "callgraph store unavailable at configure maintenance; dead_code will retry later"
                        );
                    }
                    CallgraphStoreAccess::Error(error) => {
                        slog_warn!("callgraph store configure warm failed: {}", error);
                    }
                }
            }
            if ctx.subc_unbound_quiesced() {
                cancel_unbound_configure_jobs(ctx, std::iter::once(job).chain(jobs));
                return;
            }
            if ctx.configure_generation() != job.generation {
                forget_configure_job_binding(ctx, &job);
                continue;
            }
        } else if job.warm_callgraph_store {
            // Non-git roots still build semantic and search indexes eagerly. Their
            // callgraph is intentionally on-demand, so configure cannot trigger a
            // corpus-scale cold build while the user is idle.
            slog_debug!("callgraph configure warm deferred for non-git root");
        }

        ctx.status_emitter().signal(ctx.build_status_snapshot());
    }
}

fn callgraph_configure_warm_allowed(ctx: &AppContext) -> bool {
    // Integration fixtures disable the watcher and historically expect their
    // tiny non-repository callgraphs to be ready before the first request.
    if std::env::var_os("AFT_TEST_DISABLE_FILE_WATCHER").is_some() {
        return true;
    }
    // Allow callgraph warming unless the Git-root probe confirms this is not a
    // repository. Transient probe failures, including process-spawn errors, keep
    // warming enabled so a temporary Git failure cannot disable a real repo.
    ctx.is_worktree_bridge()
        || ctx
            .callgraph_project_root()
            .is_some_and(|root| !crate::search_index::git_root_probe_confirms_non_repo(&root))
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};
    use std::ffi::OsString;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc, Barrier, Condvar, Mutex, OnceLock, RwLock};
    use std::time::{Duration, Instant};

    use super::{
        configure_artifact_load_attempts_for_test, configure_artifact_load_cancellations_for_test,
        configure_artifact_post_gate_reached_for_test, configure_deferred_delay_reached_for_test,
        external_ignore_watch_paths, handle_configure, install_project_watcher_with,
        parse_lsp_paths_extra, reset_configure_artifact_load_attempts_for_test,
        reset_configure_artifact_load_cancellations_for_test,
        reset_configure_deferred_delay_reached_for_test, semantic_build_retry_backoff,
        set_configure_artifact_post_gate_delay_for_test, should_clear_failed_spawns,
        validate_storage_dir,
    };
    use crate::cache_freshness::{self, VerifyArtifact, WarmVerifyPlan};
    use crate::config::{Config, SemanticBackend, SemanticBackendConfig};
    use crate::context::{AppContext, SemanticRefreshEvent, SemanticRefreshRequest};
    use crate::parser::{FileParser, SymbolCache, TreeSitterProvider};
    use crate::protocol::{ConfigureWarningsFrame, PushFrame, RawRequest, Response};
    use crate::search_index::CacheLock;
    use crate::semantic_index::SemanticIndex;
    use std::process::Command;

    struct TestContext {
        context: AppContext,
        _storage: tempfile::TempDir,
    }

    impl std::ops::Deref for TestContext {
        type Target = AppContext;

        fn deref(&self) -> &Self::Target {
            &self.context
        }
    }

    fn test_context() -> TestContext {
        let storage = tempfile::tempdir().expect("create configure test storage");
        let context = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                storage_dir: Some(storage.path().to_path_buf()),
                ..Config::default()
            },
        );
        TestContext {
            context,
            _storage: storage,
        }
    }

    fn git_command(root: &std::path::Path) -> Command {
        let mut command = Command::new("git");
        crate::test_env::apply_hermetic_git_env(command.current_dir(root));
        command
    }

    fn handle_configure_for_test(req: &RawRequest, ctx: &AppContext) -> Response {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        super::handle_configure(req, ctx)
    }

    fn symbol_cache_prewarm_test_mutex() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn write_symbol_cache_source(
        project: &std::path::Path,
        relative: &str,
        content: &str,
    ) -> PathBuf {
        let path = project.join(relative);
        std::fs::create_dir_all(path.parent().expect("source parent"))
            .expect("create source parent");
        std::fs::write(&path, content).expect("write source");
        path
    }

    fn persist_symbol_cache_fixture(
        project: &std::path::Path,
        storage: &std::path::Path,
        project_key: &str,
        sources: &[PathBuf],
    ) {
        crate::root_cache::configure_artifact_access(project, project_key, false);
        let mut parser = FileParser::new();
        for source in sources {
            parser.extract_symbols(source).expect("extract symbols");
        }
        let shared = parser.symbol_cache();
        let mut cache = shared.read().expect("read symbol cache").clone();
        cache.set_project_root(project.to_path_buf());
        crate::symbol_cache_disk::write_to_disk(&cache, storage, project_key)
            .expect("persist symbol cache fixture");
    }

    fn symbol_file(path: &std::path::Path) -> super::SearchIndexSymbolFile {
        let modified = std::fs::metadata(path)
            .expect("stat source")
            .modified()
            .expect("source mtime");
        (path.to_path_buf(), modified)
    }

    #[test]
    fn configure_symbol_cache_unchanged_prewarm_skips_persistence() {
        let _guard = symbol_cache_prewarm_test_mutex()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let project = tempfile::tempdir().expect("create project dir");
        let storage = tempfile::tempdir().expect("create storage dir");
        let project_key = "unchanged-prewarm";
        let source = write_symbol_cache_source(
            project.path(),
            "src/lib.rs",
            "pub fn unchanged() -> bool { true }\n",
        );
        persist_symbol_cache_fixture(
            project.path(),
            storage.path(),
            project_key,
            std::slice::from_ref(&source),
        );
        crate::symbol_cache_disk::watch_cache_writes(crate::symbol_cache_disk::cache_path(
            storage.path(),
            project_key,
        ));

        super::prewarm_symbol_cache_from_search_files(
            project.path().to_path_buf(),
            Arc::new(RwLock::new(SymbolCache::new())),
            Some(storage.path().to_path_buf()),
            project_key.to_string(),
            0,
            vec![symbol_file(&source)],
            false,
        );

        assert_eq!(crate::symbol_cache_disk::watched_cache_write_count(), 0);
    }

    #[test]
    fn configure_symbol_cache_new_file_persists_prewarm_mutation() {
        let _guard = symbol_cache_prewarm_test_mutex()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let project = tempfile::tempdir().expect("create project dir");
        let storage = tempfile::tempdir().expect("create storage dir");
        let project_key = "new-file-prewarm";
        let existing =
            write_symbol_cache_source(project.path(), "src/existing.rs", "pub fn existing() {}\n");
        let added =
            write_symbol_cache_source(project.path(), "src/added.rs", "pub fn added() {}\n");
        persist_symbol_cache_fixture(
            project.path(),
            storage.path(),
            project_key,
            std::slice::from_ref(&existing),
        );
        crate::symbol_cache_disk::watch_cache_writes(crate::symbol_cache_disk::cache_path(
            storage.path(),
            project_key,
        ));

        super::prewarm_symbol_cache_from_search_files(
            project.path().to_path_buf(),
            Arc::new(RwLock::new(SymbolCache::new())),
            Some(storage.path().to_path_buf()),
            project_key.to_string(),
            0,
            vec![symbol_file(&existing), symbol_file(&added)],
            false,
        );

        assert_eq!(crate::symbol_cache_disk::watched_cache_write_count(), 1);
        let disk = crate::symbol_cache_disk::read_from_disk(storage.path(), project_key)
            .expect("read persisted cache");
        assert_eq!(disk.len(), 2);
    }

    #[test]
    fn configure_symbol_cache_stale_disk_drop_is_persisted() {
        let _guard = symbol_cache_prewarm_test_mutex()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let project = tempfile::tempdir().expect("create project dir");
        let storage = tempfile::tempdir().expect("create storage dir");
        let project_key = "stale-drop-prewarm";
        let deleted =
            write_symbol_cache_source(project.path(), "src/deleted.rs", "pub fn deleted() {}\n");
        persist_symbol_cache_fixture(
            project.path(),
            storage.path(),
            project_key,
            std::slice::from_ref(&deleted),
        );
        std::fs::remove_file(&deleted).expect("delete stale source");
        crate::symbol_cache_disk::watch_cache_writes(crate::symbol_cache_disk::cache_path(
            storage.path(),
            project_key,
        ));

        super::prewarm_symbol_cache_from_search_files(
            project.path().to_path_buf(),
            Arc::new(RwLock::new(SymbolCache::new())),
            Some(storage.path().to_path_buf()),
            project_key.to_string(),
            0,
            Vec::new(),
            false,
        );

        assert_eq!(crate::symbol_cache_disk::watched_cache_write_count(), 1);
        let disk = crate::symbol_cache_disk::read_from_disk(storage.path(), project_key)
            .expect("read compacted cache");
        assert!(disk.is_empty());
    }

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }

        fn remove(key: &'static str) -> Self {
            let previous = std::env::var_os(key);
            unsafe { std::env::remove_var(key) };
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(previous) = self.previous.take() {
                unsafe { std::env::set_var(self.key, previous) };
            } else {
                unsafe { std::env::remove_var(self.key) };
            }
        }
    }

    fn wait_for_configure_warnings(
        ctx: &AppContext,
        generation: u64,
        timeout: Duration,
    ) -> ConfigureWarningsFrame {
        let deadline = Instant::now() + timeout;
        loop {
            for (frame_generation, frame) in ctx.drain_configure_warnings() {
                if frame_generation == generation {
                    return frame;
                }
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for configure warnings frame"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn semantic_build_retry_backoff_ramps_then_holds() {
        assert_eq!(semantic_build_retry_backoff(0), Duration::from_secs(15));
        assert_eq!(semantic_build_retry_backoff(1), Duration::from_secs(30));
        assert_eq!(semantic_build_retry_backoff(2), Duration::from_secs(60));
        // Holds at the cap for all later attempts.
        assert_eq!(semantic_build_retry_backoff(3), Duration::from_secs(60));
        assert_eq!(semantic_build_retry_backoff(99), Duration::from_secs(60));
    }

    fn configure_request(project_root: serde_json::Value) -> RawRequest {
        RawRequest {
            id: "cfg".to_string(),
            command: "configure".to_string(),
            lsp_hints: None,
            session_id: None,
            params: json!({ "project_root": project_root, "harness": "opencode" }),
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

    fn configure_request_with_session(params: serde_json::Value, session_id: &str) -> RawRequest {
        RawRequest {
            id: "cfg".to_string(),
            command: "configure".to_string(),
            lsp_hints: None,
            session_id: Some(session_id.to_string()),
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

    fn project_tier(doc: serde_json::Value) -> serde_json::Value {
        json!({
            "tier": "project",
            "source": "/p/.opencode/aft.jsonc",
            "doc": doc.to_string(),
        })
    }

    fn write_config(path: &std::path::Path, doc: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, doc).unwrap();
    }

    #[test]
    fn configure_registers_effective_hashline_and_reports_edit_slot_downgrade() {
        let project = tempfile::tempdir().unwrap();
        let canonical_root = std::fs::canonicalize(project.path()).unwrap();
        let ctx = test_context();
        let base_params = json!({
            "project_root": canonical_root,
            "harness": "opencode",
            "config": [project_tier(json!({
                "edit_mode": "hashline",
                "search_index": false,
                "semantic_search": false
            }))]
        });
        let mut downgraded_params = base_params.clone();
        downgraded_params["edit_slot_survives"] = Value::Bool(false);
        let downgraded = handle_configure(
            &configure_request_with_session(downgraded_params, "hashline-session"),
            &ctx,
        );
        assert!(downgraded.success, "{}", downgraded.data);
        assert!(downgraded.data["warnings"]
            .as_array()
            .is_some_and(|warnings| {
                warnings.iter().any(|warning| {
                    warning["code"] == "hashline_downgraded"
                        && warning["reason"] == "edit_not_registered"
                })
            }));
        assert!(!ctx
            .hashline_bindings()
            .peek(&canonical_root, "hashline-session")
            .unwrap()
            .effective());

        let mut enabled_params = base_params;
        enabled_params["edit_slot_survives"] = Value::Bool(true);
        let enabled = handle_configure(
            &configure_request_with_session(enabled_params, "hashline-session"),
            &ctx,
        );
        assert!(enabled.success, "{}", enabled.data);
        assert!(ctx
            .hashline_bindings()
            .peek(&canonical_root, "hashline-session")
            .unwrap()
            .effective());
    }

    fn init_git_fixture(root: &std::path::Path) {
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(
            root.join("tracked.rs"),
            format!("// fixture:{}\nfn tracked() {{}}\n", root.display()),
        )
        .unwrap();
        assert!(git_command(root)
            .args(["init", "--quiet"])
            .status()
            .unwrap()
            .success());
        assert!(git_command(root)
            .args(["add", "."])
            .status()
            .unwrap()
            .success());
        assert!(git_command(root)
            .args([
                "-c",
                "user.name=AFT Tests",
                "-c",
                "user.email=aft-tests@example.com",
                "commit",
                "--quiet",
                "-m",
                "initial",
            ])
            .status()
            .unwrap()
            .success());
    }

    #[test]
    fn configure_without_storage_dir_defaults_to_shared_storage_root() {
        let _env_guard = home_env_mutex();
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _disable_watcher = EnvVarGuard::set("AFT_TEST_DISABLE_FILE_WATCHER", "1");
        let temp = tempfile::tempdir().unwrap();
        init_git_fixture(temp.path());

        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        let req = configure_request_with_params(json!({
            "project_root": temp.path(),
            "harness": "opencode",
            "config": [user_tier(json!({
                "search_index": false,
                "semantic_search": false,
                "callgraph_store": false
            }))],
        }));
        let response = handle_configure_for_test(&req, &ctx);
        assert!(response.success);

        // Plugin-less consumers never send storage_dir; the resolved config
        // must still carry a concrete storage root so artifact lanes that
        // gate persistence on `Some(storage_dir)` (semantic index) write to
        // the same universe as lanes with their own fallback (trigram).
        let resolved = ctx.config().storage_dir.clone();
        assert_eq!(
            resolved,
            Some(crate::bash_background::storage_dir(None)),
            "configure must default storage_dir to the shared storage root"
        );
    }

    #[test]
    fn configure_writes_cache_key_memo_to_explicit_storage_only() {
        let _env_guard = home_env_mutex();
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _aft_cache_dir = EnvVarGuard::remove("AFT_CACHE_DIR");
        let temp = tempfile::tempdir().expect("create fixture root");
        let default_data_root = temp.path().join("default-data");
        let _xdg_data_home = EnvVarGuard::set(
            "XDG_DATA_HOME",
            default_data_root.to_str().expect("temporary path is UTF-8"),
        );
        let default_storage = crate::bash_background::storage_dir(None);
        let project = temp.path().join("project");
        let storage = temp.path().join("configured-storage");
        init_git_fixture(&project);

        let ctx = test_context();
        let response = handle_configure_for_test(&configure_with_storage(&project, &storage), &ctx);

        assert!(response.success, "configure failed: {response:?}");
        assert!(
            storage.join("cache-keys.json").is_file(),
            "explicit storage_dir must receive the cache-key memo"
        );
        assert!(
            !default_storage.join("cache-keys.json").exists(),
            "explicit storage_dir must not write the default storage memo"
        );
    }

    fn configure_with_storage(root: &std::path::Path, storage: &std::path::Path) -> RawRequest {
        configure_request_with_params(json!({
            "project_root": root,
            "harness": "opencode",
            "storage_dir": storage,
            "config": [user_tier(json!({ "search_index": true, "semantic_search": false }))],
        }))
    }

    fn configure_search_with_max_file_size(
        root: &std::path::Path,
        storage: &std::path::Path,
        max_file_size: u64,
    ) -> RawRequest {
        configure_request_with_params(json!({
            "project_root": root,
            "harness": "opencode",
            "storage_dir": storage,
            "search_index_max_file_size": max_file_size,
            "config": [user_tier(json!({
                "search_index": true,
                "semantic_search": false,
                "callgraph_store": false
            }))],
        }))
    }

    fn configure_semantic_with_storage(
        root: &std::path::Path,
        storage: &std::path::Path,
        base_url: &str,
        semantic_search: bool,
    ) -> RawRequest {
        configure_request_with_params(json!({
            "project_root": root,
            "harness": "opencode",
            "storage_dir": storage,
            "config": [user_tier(json!({
                "search_index": false,
                "semantic_search": semantic_search,
                "callgraph_store": false,
                "semantic": {
                    "backend": "openai_compatible",
                    "model": "counting-test-embedding",
                    "base_url": base_url,
                    "timeout_ms": 5_000,
                    "max_batch_size": 64,
                    "max_files": 1_000
                }
            }))],
        }))
    }

    fn owner_manifest_from_response(
        response: &Response,
    ) -> crate::artifact_owner::ArtifactOwnerManifest {
        let manifest_path = response.data["artifact_owner"]["manifest_path"]
            .as_str()
            .expect("artifact owner manifest path");
        let bytes = std::fs::read(manifest_path).expect("read owner manifest");
        serde_json::from_slice(&bytes).expect("parse owner manifest")
    }

    fn semantic_cache_file(
        storage: &std::path::Path,
        root: &std::path::Path,
    ) -> std::path::PathBuf {
        let project_key = crate::search_index::artifact_cache_key(root);
        storage
            .join("semantic")
            .join(project_key)
            .join("semantic.bin")
    }

    fn semantic_refresh_test_config(base_url: &str) -> SemanticBackendConfig {
        SemanticBackendConfig {
            backend: SemanticBackend::OpenAiCompatible,
            model: "semantic-refresh-test".to_string(),
            base_url: Some(base_url.to_string()),
            api_key_env: None,
            timeout_ms: 5_000,
            query_timeout_ms: 5_000,
            max_batch_size: 64,
            max_files: 1_000,
        }
    }

    fn spawn_semantic_corpus_refresh_worker_for_test(
        project_root: PathBuf,
        config: &SemanticBackendConfig,
        limiter: super::SemanticRefreshLimiter,
    ) -> (
        TestContext,
        crossbeam_channel::Sender<SemanticRefreshRequest>,
        crossbeam_channel::Receiver<SemanticRefreshEvent>,
        std::thread::JoinHandle<()>,
    ) {
        let ctx = test_context();
        let generation = ctx.configure_generation();
        let (request_tx, request_rx) = crossbeam_channel::unbounded();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let worker = super::spawn_semantic_refresh_worker(
            project_root.clone(),
            SemanticIndex::new(project_root, 3),
            crate::semantic_index::EmbeddingModel::from_config(config)
                .expect("construct semantic refresh model"),
            config.max_batch_size,
            config.max_files,
            request_rx,
            event_tx,
            ctx.subc_lifecycle_admission(),
            ctx.configure_generation_flag(),
            generation,
            limiter,
            None,
        );
        (ctx, request_tx, event_rx, worker)
    }

    fn count_non_probe_inputs(requests: &[Vec<String>]) -> usize {
        requests
            .iter()
            .flatten()
            .filter(|text| text.as_str() != "semantic index fingerprint probe")
            .count()
    }

    struct CountingEmbeddingServer {
        base_url: String,
        stop: Arc<AtomicBool>,
        requests: Arc<Mutex<Vec<Vec<String>>>>,
        request_changed: Arc<Condvar>,
        response_release: Arc<(Mutex<bool>, Condvar)>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl CountingEmbeddingServer {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind embedding mock");
            let addr = listener.local_addr().expect("embedding mock addr");
            listener
                .set_nonblocking(true)
                .expect("embedding mock nonblocking");
            let stop = Arc::new(AtomicBool::new(false));
            let requests = Arc::new(Mutex::new(Vec::new()));
            let request_changed = Arc::new(Condvar::new());
            let response_release = Arc::new((Mutex::new(false), Condvar::new()));
            let stop_for_thread = Arc::clone(&stop);
            let requests_for_thread = Arc::clone(&requests);
            let request_changed_for_thread = Arc::clone(&request_changed);
            let response_release_for_thread = Arc::clone(&response_release);
            let handle = std::thread::spawn(move || {
                while !stop_for_thread.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let requests = Arc::clone(&requests_for_thread);
                            let request_changed = Arc::clone(&request_changed_for_thread);
                            let response_release = Arc::clone(&response_release_for_thread);
                            std::thread::spawn(move || {
                                handle_counting_embedding_request(
                                    stream,
                                    requests,
                                    request_changed,
                                    response_release,
                                )
                            });
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("embedding mock accept failed: {error}"),
                    }
                }
            });
            Self {
                base_url: format!("http://{addr}"),
                stop,
                requests,
                request_changed,
                response_release,
                handle: Some(handle),
            }
        }

        fn non_probe_input_count(&self) -> usize {
            count_non_probe_inputs(
                &self
                    .requests
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            )
        }

        fn non_probe_request_count(&self) -> usize {
            self.requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .filter(|inputs| {
                    inputs
                        .iter()
                        .any(|text| text != "semantic index fingerprint probe")
                })
                .count()
        }

        fn wait_for_non_probe_input(&self, timeout: Duration) -> bool {
            self.wait_for_non_probe_input_count(1, timeout)
        }

        fn wait_for_non_probe_input_count(&self, expected: usize, timeout: Duration) -> bool {
            let requests = self
                .requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let (requests, result) = self
                .request_changed
                .wait_timeout_while(requests, timeout, |requests| {
                    count_non_probe_inputs(requests) < expected
                })
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            !result.timed_out() || count_non_probe_inputs(&requests) >= expected
        }

        fn wait_for_non_probe_request_count(&self, expected: usize, timeout: Duration) -> bool {
            let requests = self
                .requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let (requests, result) = self
                .request_changed
                .wait_timeout_while(requests, timeout, |requests| {
                    requests
                        .iter()
                        .filter(|inputs| {
                            inputs
                                .iter()
                                .any(|text| text != "semantic index fingerprint probe")
                        })
                        .count()
                        < expected
                })
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            !result.timed_out()
                || requests
                    .iter()
                    .filter(|inputs| {
                        inputs
                            .iter()
                            .any(|text| text != "semantic index fingerprint probe")
                    })
                    .count()
                    >= expected
        }

        fn release_responses(&self) {
            let (released, changed) = &*self.response_release;
            *released
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
            changed.notify_all();
        }
    }

    impl Drop for CountingEmbeddingServer {
        fn drop(&mut self) {
            self.release_responses();
            self.stop.store(true, Ordering::Relaxed);
            let _ = std::net::TcpStream::connect(self.base_url.trim_start_matches("http://"));
            if let Some(handle) = self.handle.take() {
                handle.join().expect("embedding mock joins");
            }
        }
    }

    fn handle_counting_embedding_request(
        mut stream: std::net::TcpStream,
        requests: Arc<Mutex<Vec<Vec<String>>>>,
        request_changed: Arc<Condvar>,
        response_release: Arc<(Mutex<bool>, Condvar)>,
    ) {
        stream
            .set_nonblocking(false)
            .expect("embedding request stream blocking mode");
        let mut reader = BufReader::new(stream.try_clone().expect("clone embedding stream"));
        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            let read = reader.read_line(&mut line).expect("read embedding header");
            if read == 0 || line == "\r\n" {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    content_length = value.trim().parse().unwrap_or(0);
                }
            }
        }
        let mut body = vec![0; content_length];
        reader.read_exact(&mut body).expect("read embedding body");
        let request_body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let inputs = match request_body.get("input") {
            Some(serde_json::Value::Array(values)) => values
                .iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect::<Vec<_>>(),
            Some(serde_json::Value::String(value)) => vec![value.clone()],
            _ => vec![String::new()],
        };
        requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(inputs.clone());
        request_changed.notify_all();
        if inputs
            .iter()
            .any(|text| text != "semantic index fingerprint probe")
        {
            let (released, changed) = &*response_release;
            let guard = released
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            drop(
                changed
                    .wait_while(guard, |released| !*released)
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
        }
        let data = inputs
            .iter()
            .enumerate()
            .map(|(index, _)| {
                let base = index as f64 + 1.0;
                json!({
                    "embedding": [base, base + 0.1, base + 0.2],
                    "index": index,
                })
            })
            .collect::<Vec<_>>();
        let response_body = json!({ "data": data }).to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response_body.len(),
            response_body
        );
        stream
            .write_all(response.as_bytes())
            .expect("write embedding response");
    }

    fn wait_for_semantic_build_ready(ctx: &AppContext, timeout: Duration) {
        super::drain_deferred_configure_maintenance(ctx);
        let deadline = Instant::now() + timeout;
        loop {
            crate::runtime_drain::drain_build_completions(ctx);
            let ready = ctx
                .semantic_index()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some();
            if ready && ctx.semantic_index_rx().lock().is_none() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for semantic build"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_for_search_index_ready(ctx: &AppContext, timeout: Duration) {
        super::drain_deferred_configure_maintenance(ctx);
        let deadline = Instant::now() + timeout;
        loop {
            crate::runtime_drain::drain_build_completions(ctx);
            let ready = ctx
                .search_index()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .is_some_and(|index| index.ready);
            if ready
                && ctx
                    .search_index_rx()
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .is_none()
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for search index build"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn semantic_search_request(query: &str) -> RawRequest {
        serde_json::from_value(json!({
            "id": "semantic-reload-query",
            "command": "semantic_search",
            "query": query,
            "top_k": 5
        }))
        .expect("semantic search request")
    }

    fn grep_request(pattern: &str) -> RawRequest {
        serde_json::from_value(json!({
            "id": "grep-reload-query",
            "command": "grep",
            "pattern": pattern,
            "max_results": 10
        }))
        .expect("grep request")
    }

    #[test]
    fn configure_user_file_wins_project_wire_falls_back_per_tier() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_context();
        let user_path = temp.path().join("xdg/cortexkit/aft.jsonc");
        write_config(&user_path, r#"{ "format_on_edit": false }"#);
        let req = configure_request_with_params(json!({
            "project_root": temp.path(),
            "harness": "opencode",
            "cortexkit_user_config_path": user_path,
            "config": [
                user_tier(json!({ "format_on_edit": true, "url_fetch_allow_private": true })),
                project_tier(json!({ "callgraph_chunk_size": 3 }))
            ]
        }));

        let response = handle_configure_for_test(&req, &ctx);
        assert!(response.success, "configure failed: {:?}", response.data);
        assert!(!ctx.config().format_on_edit);
        assert!(!ctx.config().url_fetch_allow_private);
        assert_eq!(ctx.config().callgraph_chunk_size, 3);
    }

    #[test]
    fn configure_project_file_wins_user_wire_falls_back_per_tier() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_context();
        let user_path = temp.path().join("xdg/cortexkit/aft.jsonc");
        let project_path = temp.path().join(".cortexkit/aft.jsonc");
        write_config(&project_path, r#"{ "callgraph_chunk_size": 7 }"#);
        let req = configure_request_with_params(json!({
            "project_root": temp.path(),
            "harness": "opencode",
            "cortexkit_user_config_path": user_path,
            "config": [
                user_tier(json!({ "url_fetch_allow_private": true })),
                project_tier(json!({ "callgraph_chunk_size": 4 }))
            ]
        }));

        let response = handle_configure_for_test(&req, &ctx);
        assert!(response.success, "configure failed: {:?}", response.data);
        assert!(ctx.config().url_fetch_allow_private);
        assert_eq!(ctx.config().callgraph_chunk_size, 7);
    }

    #[test]
    fn configure_both_files_ignore_wire_tiers() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_context();
        let user_path = temp.path().join("xdg/cortexkit/aft.jsonc");
        let project_path = temp.path().join(".cortexkit/aft.jsonc");
        write_config(
            &user_path,
            r#"{ "format_on_edit": false, "url_fetch_allow_private": false }"#,
        );
        write_config(&project_path, r#"{ "callgraph_chunk_size": 9 }"#);
        let req = configure_request_with_params(json!({
            "project_root": temp.path(),
            "harness": "opencode",
            "cortexkit_user_config_path": user_path,
            "config": [
                user_tier(json!({ "format_on_edit": true, "url_fetch_allow_private": true })),
                project_tier(json!({ "callgraph_chunk_size": 2 }))
            ]
        }));

        let response = handle_configure_for_test(&req, &ctx);
        assert!(response.success, "configure failed: {:?}", response.data);
        assert!(!ctx.config().format_on_edit);
        assert!(!ctx.config().url_fetch_allow_private);
        assert_eq!(ctx.config().callgraph_chunk_size, 9);
    }

    #[test]
    fn configure_neither_file_uses_wire_tiers_for_old_plugins() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_context();
        let user_path = temp.path().join("xdg/cortexkit/aft.jsonc");
        let req = configure_request_with_params(json!({
            "project_root": temp.path(),
            "harness": "opencode",
            "cortexkit_user_config_path": user_path,
            "config": [
                user_tier(json!({ "format_on_edit": false, "url_fetch_allow_private": true })),
                project_tier(json!({ "callgraph_chunk_size": 11 }))
            ]
        }));

        let response = handle_configure_for_test(&req, &ctx);
        assert!(response.success, "configure failed: {:?}", response.data);
        assert!(!ctx.config().format_on_edit);
        assert!(ctx.config().url_fetch_allow_private);
        assert_eq!(ctx.config().callgraph_chunk_size, 11);
    }

    #[test]
    fn configure_accepts_old_plugin_wire_without_cortexkit_user_path() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_context();
        let req = configure_request_with_params(json!({
            "project_root": temp.path(),
            "harness": "opencode",
            "config": [
                user_tier(json!({ "format_on_edit": false })),
                project_tier(json!({ "callgraph_chunk_size": 13 }))
            ]
        }));

        let response = handle_configure_for_test(&req, &ctx);
        assert!(response.success, "configure failed: {:?}", response.data);
        assert!(!ctx.config().format_on_edit);
        assert_eq!(ctx.config().callgraph_chunk_size, 13);
    }

    #[test]
    fn configure_resolves_config_tiers_and_surfaces_dropped_keys() {
        // P1 receiver half: configure accepts raw config tiers, resolves them in
        // core (merge + trust boundary), and surfaces project-tier drops.
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_context();
        let req = configure_request_with_params(json!({
            "project_root": temp.path(),
            "harness": "opencode",
            "config": [
                { "tier": "user", "source": "/u/aft.jsonc",
                  "doc": "{ \"restrict_to_project_root\": true, \"search_index\": true, \"backup\": { \"enabled\": false, \"max_depth\": 7 }, \"disabled_tools\": [\"aft_safety\"] }" },
                { "tier": "project", "source": "/p/.opencode/aft.jsonc",
                  "doc": "{ \"restrict_to_project_root\": false, \"semantic\": { \"api_key_env\": \"EVIL\" }, \"backup\": { \"enabled\": true, \"max_depth\": 1 }, \"disabled_tools\": [\"aft_safety\"] }" }
            ]
        }));

        let response = handle_configure_for_test(&req, &ctx);
        assert!(response.success, "configure failed: {:?}", response.data);

        // Core-resolved field applied: user search_index=true survived.
        assert!(ctx.config().search_index);
        // Trust boundary: project tried restrict=false over user restrict=true →
        // user value wins.
        assert!(ctx.config().restrict_to_project_root);
        // Project semantic.api_key_env never reached Config.
        assert!(ctx.config().semantic.api_key_env.is_none());
        assert_eq!(ctx.config().backup.enabled, Some(false));
        assert_eq!(ctx.config().backup.max_depth, Some(7));

        // Drops surfaced for the warning path.
        let dropped = response.data["config_dropped_keys"].as_array().unwrap();
        let keys: Vec<&str> = dropped.iter().filter_map(|d| d["key"].as_str()).collect();
        assert!(keys.contains(&"restrict_to_project_root"), "keys: {keys:?}");
        assert!(keys.contains(&"semantic.api_key_env"), "keys: {keys:?}");
        assert!(keys.contains(&"backup"), "keys: {keys:?}");
        assert!(
            keys.contains(&"disabled_tools.aft_safety"),
            "keys: {keys:?}"
        );
    }

    #[test]
    fn configure_without_harness_returns_invalid_request() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_context();
        let req = configure_request_with_params(json!({ "project_root": temp.path() }));

        let response = handle_configure_for_test(&req, &ctx);

        assert!(!response.success);
        assert_eq!(response.data["code"], "invalid_request");
        assert_eq!(
            response.data["message"],
            "configure payload missing required field 'harness'; expected 'opencode', 'pi', 'runner', 'mcp:<client>', or 'fed:<fingerprint>'"
        );
    }

    #[test]
    fn configure_with_invalid_harness_returns_invalid_request() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_context();
        let req = configure_request_with_params(json!({
            "project_root": temp.path(),
            "harness": "claude_code"
        }));

        let response = handle_configure_for_test(&req, &ctx);

        assert!(!response.success);
        assert_eq!(response.data["code"], "invalid_request");
    }

    #[test]
    fn harness_set_on_appcontext_after_configure() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_context();
        let req = configure_request_with_params(json!({
            "project_root": temp.path(),
            "harness": "pi"
        }));

        let response = handle_configure_for_test(&req, &ctx);

        assert!(response.success);
        assert_eq!(ctx.harness(), crate::harness::Harness::Pi);
        assert_eq!(ctx.config().harness, Some(crate::harness::Harness::Pi));
    }

    #[test]
    fn handle_configure_rejects_relative_project_root() {
        let ctx = test_context();
        let req = configure_request(json!("relative/path"));

        let response = handle_configure_for_test(&req, &ctx);

        assert!(!response.success);
        assert_eq!(response.data["code"], "invalid_request");
    }

    #[test]
    fn handle_configure_populates_canonical_cache_root() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_context();
        let req = configure_request(json!(temp.path()));

        let response = handle_configure_for_test(&req, &ctx);

        assert!(response.success);
        assert_eq!(
            ctx.canonical_cache_root(),
            std::fs::canonicalize(temp.path()).unwrap()
        );
        assert_eq!(ctx.cache_role(), "main");
    }

    #[test]
    fn reconfigure_invalidates_path_restriction_root_memo() {
        let workspace = tempfile::tempdir().unwrap();
        let first_root = workspace.path().join("first-root");
        let second_root = workspace.path().join("second-root");
        std::fs::create_dir_all(&first_root).unwrap();
        std::fs::create_dir_all(&second_root).unwrap();
        let first_file = first_root.join("first.txt");
        let second_file = second_root.join("second.txt");
        std::fs::write(&first_file, "first").unwrap();
        std::fs::write(&second_file, "second").unwrap();
        let ctx = test_context();
        let request = |root: &std::path::Path| {
            configure_request_with_params(json!({
                "project_root": root,
                "harness": "opencode",
                "config": [user_tier(json!({
                    "restrict_to_project_root": true,
                    "search_index": false,
                    "semantic_search": false,
                    "callgraph_store": false
                }))]
            }))
        };

        assert!(handle_configure_for_test(&request(&first_root), &ctx).success);
        assert_eq!(
            ctx.validate_path("first-root", &first_file)
                .expect("first root validates"),
            std::fs::canonicalize(&first_file).unwrap()
        );
        assert!(!ctx.path_restriction_root_memo_is_empty_for_test());

        assert!(handle_configure_for_test(&request(&second_root), &ctx).success);
        assert!(
            ctx.path_restriction_root_memo_is_empty_for_test(),
            "committing a different configured project root must clear the old root memo"
        );
        assert_eq!(
            ctx.validate_path("second-root", &second_file)
                .expect("second root validates after reconfigure"),
            std::fs::canonicalize(&second_file).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn reconfigure_recanonicalizes_configured_root_after_target_changes() {
        let workspace = tempfile::tempdir().unwrap();
        let first_target = workspace.path().join("first-target");
        let second_target = workspace.path().join("second-target");
        let intermediate_root = workspace.path().join("intermediate-root");
        let configured_root = workspace.path().join("configured-root");
        std::fs::create_dir_all(&first_target).unwrap();
        std::fs::create_dir_all(&second_target).unwrap();
        std::fs::create_dir_all(&intermediate_root).unwrap();
        std::os::unix::fs::symlink(&first_target, &configured_root).unwrap();
        std::fs::write(first_target.join("inside.txt"), "first").unwrap();
        std::fs::write(second_target.join("inside.txt"), "second").unwrap();
        let ctx = test_context();
        let request = |root: &std::path::Path| {
            configure_request_with_params(json!({
                "project_root": root,
                "harness": "opencode",
                "config": [user_tier(json!({
                    "restrict_to_project_root": true,
                    "search_index": false,
                    "semantic_search": false,
                    "callgraph_store": false
                }))]
            }))
        };

        assert!(handle_configure_for_test(&request(&configured_root), &ctx).success);
        assert_eq!(
            ctx.validate_path("first-target", std::path::Path::new("inside.txt"))
                .expect("first target validates"),
            std::fs::canonicalize(first_target.join("inside.txt")).unwrap()
        );
        assert!(handle_configure_for_test(&request(&intermediate_root), &ctx).success);

        // The first target deliberately stays present, so only reconfigure
        // invalidation—not the missing-target refresh—can discard its memo entry.
        std::fs::remove_file(&configured_root).unwrap();
        std::os::unix::fs::symlink(&second_target, &configured_root).unwrap();
        assert!(handle_configure_for_test(&request(&configured_root), &ctx).success);

        assert_eq!(
            ctx.validate_path("second-target", std::path::Path::new("inside.txt"))
                .expect("new target validates after reconfigure"),
            std::fs::canonicalize(second_target.join("inside.txt")).unwrap()
        );
    }

    #[test]
    fn configure_reuses_cached_worktree_probe_until_forced_to_reprobe() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        let storage = temp.path().join("storage");
        init_git_fixture(&root);
        let ctx = test_context();
        let request = || {
            configure_request_with_params(json!({
                "project_root": root.clone(),
                "harness": "opencode",
                "storage_dir": storage.clone(),
                "config": [user_tier(json!({
                    "search_index": false,
                    "semantic_search": false,
                    "callgraph_store": false,
                }))],
            }))
        };

        assert!(handle_configure_for_test(&request(), &ctx).success);
        assert_eq!(ctx.worktree_bridge_probe_spawns_for_test(), 1);
        assert!(handle_configure_for_test(&request(), &ctx).success);
        assert_eq!(
            ctx.worktree_bridge_probe_spawns_for_test(),
            1,
            "an equivalent configure must reuse the successful git topology probe"
        );

        filetime::set_file_mtime(
            root.join(".git"),
            filetime::FileTime::from_system_time(
                std::time::SystemTime::now() + Duration::from_secs(5),
            ),
        )
        .expect("advance root git marker mtime");
        assert!(handle_configure_for_test(&request(), &ctx).success);
        assert_eq!(
            ctx.worktree_bridge_probe_spawns_for_test(),
            2,
            "a changed root .git marker must invalidate the cached topology"
        );

        ctx.force_worktree_bridge_reprobe_for_test(true);
        assert!(handle_configure_for_test(&request(), &ctx).success);
        assert_eq!(ctx.worktree_bridge_probe_spawns_for_test(), 3);
        ctx.force_worktree_bridge_reprobe_for_test(false);
    }

    #[test]
    fn configure_cancellation_aborts_slow_git_retry_cluster_within_bound() {
        let _probe_lock = crate::search_index::git_root_commit_probe_override_lock_for_test();
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        let storage = temp.path().join("storage");
        init_git_fixture(&root);
        let canonical_root = std::fs::canonicalize(&root).unwrap();
        let probe_started = Arc::new(AtomicBool::new(false));
        let _probe_guard =
            crate::search_index::force_git_root_commit_probe_slow_transient_for_paths_for_test(
                vec![canonical_root.clone()],
                Duration::from_millis(150),
                Arc::clone(&probe_started),
            );

        let ctx = Arc::new(AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                storage_dir: Some(storage.clone()),
                ..Config::default()
            },
        ));
        let executor = crate::executor::Executor::new();
        let root_id = crate::path_identity::ProjectRootId::from_path(&canonical_root)
            .expect("canonical project root id");
        assert!(executor.register_actor(root_id.clone(), Arc::clone(&ctx)));
        let request = configure_request_with_params(json!({
            "project_root": root,
            "harness": "opencode",
            "storage_dir": storage,
            "config": [user_tier(json!({
                "search_index": true,
                "semantic_search": false,
                "callgraph_store": false,
            }))],
        }));
        let request_id = request.id.clone();
        let (response_rx, cancellation) = executor.submit_cancellable_async(
            root_id.clone(),
            crate::executor::Lane::Mutating,
            request_id,
            Box::new(move |ctx| super::handle_configure(&request, ctx)),
        );

        let probe_deadline = Instant::now() + Duration::from_secs(2);
        while !probe_started.load(Ordering::SeqCst) {
            assert!(
                Instant::now() < probe_deadline,
                "stubbed git probe did not start"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let cancelled_at = Instant::now();
        assert_eq!(
            executor.cancel_job(&root_id, &cancellation),
            crate::executor::JobCancelOutcome::RunningSignalled
        );

        let response = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("build cancellation test runtime")
            .block_on(async {
                tokio::time::timeout(Duration::from_millis(400), response_rx)
                    .await
                    .expect("configure cancellation must stop before the retry ladder finishes")
                    .expect("configure completion sender")
            });
        assert!(!response.success);
        assert_eq!(response.data["code"], "request_cancelled");
        assert!(
            cancelled_at.elapsed() < Duration::from_millis(400),
            "cancelled configure exceeded the bounded git-probe exit time"
        );
    }

    #[test]
    fn handle_configure_rejects_git_like_root_when_cache_key_probe_fails_without_memo() {
        let _probe_lock = crate::search_index::git_root_commit_probe_override_lock_for_test();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        std::fs::create_dir_all(root.join(".git")).expect("create git marker");
        let storage = temp.path().join("storage");
        let canonical_root = std::fs::canonicalize(&root).expect("canonical root");
        let _override =
            crate::search_index::force_git_root_commit_probe_transient_for_paths_for_test(
                vec![root.clone(), canonical_root],
                "spawn failed: Too many open files (os error 24)",
            );
        let ctx = test_context();
        let req = configure_request_with_params(json!({
            "project_root": root.clone(),
            "harness": "opencode",
            "storage_dir": storage.clone(),
            "config": [user_tier(json!({
                "search_index": true,
                "semantic_search": false,
                "callgraph_store": false,
            }))],
        }));

        let response = handle_configure_for_test(&req, &ctx);

        assert!(
            !response.success,
            "configure must reject ambiguous git identity"
        );
        assert_eq!(response.data["code"], "cache_key_probe_failed");
        assert_eq!(response.data["retryable"], true);
        let path_key = crate::search_index::artifact_path_identity_key_for_test(&root);
        assert!(!storage.join("index").join(&path_key).exists());
        assert!(!storage.join("semantic").join(&path_key).exists());
        assert!(!storage.join("callgraph").join(&path_key).exists());
    }

    #[test]
    fn sibling_clone_same_artifact_key_opens_shared_artifacts_read_only() {
        let _artifact_guard = artifact_owner_test_mutex().lock().unwrap();
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("storage");
        let owner = temp.path().join("owner");
        init_git_fixture(&owner);
        let sibling = temp.path().join("sibling");
        let mut clone_command = Command::new("git");
        assert!(crate::test_env::apply_hermetic_git_env(&mut clone_command)
            .args(["clone", "--quiet"])
            .arg(&owner)
            .arg(&sibling)
            .status()
            .unwrap()
            .success());

        let owner_ctx = test_context();
        let owner_response =
            handle_configure_for_test(&configure_with_storage(&owner, &storage), &owner_ctx);
        assert!(owner_response.success);
        assert_eq!(owner_ctx.cache_role(), "main");

        let sibling_ctx = test_context();
        let sibling_response =
            handle_configure_for_test(&configure_with_storage(&sibling, &storage), &sibling_ctx);

        assert!(sibling_response.success);
        assert_eq!(sibling_ctx.cache_role(), "read_only");
        assert!(sibling_ctx.shared_artifacts_read_only());
        assert_eq!(
            sibling_response.data["artifact_owner"]["mode"],
            json!("read_only")
        );
        assert!(sibling_response.data["artifact_owner"]["note"]
            .as_str()
            .unwrap()
            .contains("sharing the repo index family"));
        assert!(
            sibling_ctx.search_index_rx().read().unwrap().is_some(),
            "read-only sibling search artifact must remain gated until configure maintenance"
        );
    }

    #[test]
    fn detect_worktree_bridge_returns_common_dir_for_main_and_linked_worktree() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let temp = tempfile::tempdir().unwrap();
        let main = temp.path().join("main");
        init_git_fixture(&main);
        let worktree = temp.path().join("worktree");
        let mut worktree_command = Command::new("git");
        assert!(
            crate::test_env::apply_hermetic_git_env(worktree_command.arg("-C").arg(&main))
                .args(["worktree", "add", "--detach", "--quiet"])
                .arg(&worktree)
                .arg("HEAD")
                .status()
                .unwrap()
                .success()
        );

        let canonical_main = std::fs::canonicalize(&main).unwrap();
        let canonical_worktree = std::fs::canonicalize(&worktree).unwrap();
        let ctx = test_context();
        let (main_is_worktree, main_common) = super::detect_worktree_bridge(&ctx, &canonical_main);
        let (linked_is_worktree, linked_common) =
            super::detect_worktree_bridge(&ctx, &canonical_worktree);

        assert!(!main_is_worktree);
        assert!(linked_is_worktree);
        let expected_common = canonical_main.join(".git");
        assert_eq!(main_common.as_deref(), Some(expected_common.as_path()));
        assert_eq!(linked_common, main_common);
        assert_eq!(ctx.worktree_bridge_probe_spawns_for_test(), 2);

        let repeated_main = super::detect_worktree_bridge(&ctx, &canonical_main);
        let repeated_worktree = super::detect_worktree_bridge(&ctx, &canonical_worktree);
        assert_eq!(repeated_main, (main_is_worktree, main_common));
        assert_eq!(repeated_worktree, (linked_is_worktree, linked_common));
        assert_eq!(
            ctx.worktree_bridge_probe_spawns_for_test(),
            2,
            "main and linked-worktree roots must each retain their own cached result"
        );
    }

    #[test]
    fn broken_git_file_topology_fails_closed_as_linked_worktree() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("broken-worktree");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".git"), "gitdir: /definitely/missing/worktree\n").unwrap();
        let canonical_root = std::fs::canonicalize(&root).unwrap();
        let ctx = test_context();

        let topology = super::detect_worktree_bridge(&ctx, &canonical_root);

        assert_eq!(topology, (true, None));
    }

    #[test]
    fn worktree_then_main_claim_sequence_ends_with_main_owner() {
        let _artifact_guard = artifact_owner_test_mutex().lock().unwrap();
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("storage");
        let main = temp.path().join("main");
        init_git_fixture(&main);
        let worktree = temp.path().join("worktree");
        let mut worktree_command = Command::new("git");
        assert!(
            crate::test_env::apply_hermetic_git_env(worktree_command.arg("-C").arg(&main))
                .args(["worktree", "add", "--detach", "--quiet"])
                .arg(&worktree)
                .arg("HEAD")
                .status()
                .unwrap()
                .success()
        );

        let worktree_ctx = test_context();
        let worktree_response =
            handle_configure_for_test(&configure_with_storage(&worktree, &storage), &worktree_ctx);
        assert!(worktree_response.success);
        assert_eq!(worktree_ctx.cache_role(), "worktree");
        assert!(worktree_ctx.shared_artifacts_read_only());
        let borrowed_manifest_path = worktree_response.data["artifact_owner"]["manifest_path"]
            .as_str()
            .unwrap();
        assert!(
            !std::path::Path::new(borrowed_manifest_path).exists(),
            "linked worktree must not create the family owner manifest"
        );

        let main_ctx = test_context();
        let main_response =
            handle_configure_for_test(&configure_with_storage(&main, &storage), &main_ctx);
        assert!(main_response.success);
        assert_eq!(main_ctx.cache_role(), "main");
        assert!(!main_ctx.shared_artifacts_read_only());
        assert_eq!(main_response.data["artifact_owner"]["mode"], json!("owner"));
        let manifest = owner_manifest_from_response(&main_response);
        assert_eq!(
            manifest.checkout_path,
            std::fs::canonicalize(&main).unwrap().display().to_string()
        );
    }

    #[test]
    fn artifact_replacement_after_locked_memo_record_is_not_marked_fresh() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let cache_dir = temp.path().join("search-cache");
        crate::root_cache::configure_artifact_access(&root, "search-cache", false);
        std::fs::create_dir_all(&cache_dir).unwrap();
        let artifact = cache_dir.join("cache.bin");
        std::fs::write(&artifact, b"generation-a").unwrap();
        let generation_a = cache_freshness::artifact_generation(&artifact);
        let ticket = cache_freshness::capture_verify_ticket(&root);
        let lock = CacheLock::acquire(&cache_dir, &root).unwrap();
        let (attempted_tx, attempted_rx) = crossbeam_channel::bounded(1);
        let (acquired_tx, acquired_rx) = crossbeam_channel::bounded(1);
        let writer_cache = cache_dir.clone();
        let writer_root = root.clone();
        let writer_artifact = artifact.clone();
        let writer = std::thread::spawn(move || {
            attempted_tx.send(()).unwrap();
            let _lock = CacheLock::acquire(&writer_cache, &writer_root).unwrap();
            acquired_tx.send(()).unwrap();
            std::fs::write(writer_artifact, b"generation-b-longer").unwrap();
        });
        attempted_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(acquired_rx.recv_timeout(Duration::from_millis(50)).is_err());
        assert!(cache_freshness::record_verify_completed_if_unchanged(
            &root,
            VerifyArtifact::Search,
            generation_a,
            ticket,
        ));
        drop(lock);
        acquired_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        writer.join().unwrap();
        let generation_b = cache_freshness::artifact_generation(&artifact);
        assert_ne!(generation_b, generation_a);
        assert_ne!(
            cache_freshness::warm_verify_plan(&root, VerifyArtifact::Search, generation_b),
            WarmVerifyPlan::Skip,
            "a later atomic replacement must not inherit the prior generation's memo"
        );
    }

    #[test]
    fn persisted_search_cache_reconfigures_max_file_size_on_same_head() {
        let _artifact_guard = artifact_owner_test_mutex().lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let storage = temp.path().join("storage");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("large.rs"), "x".repeat(256)).unwrap();
        let ctx = test_context();

        let initial = handle_configure_for_test(
            &configure_search_with_max_file_size(&project, &storage, 512),
            &ctx,
        );
        assert!(initial.success);
        wait_for_search_index_ready(&ctx, Duration::from_secs(10));
        assert_eq!(
            ctx.search_index()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .unwrap()
                .configured_max_file_size(),
            512
        );

        let lowered = handle_configure_for_test(
            &configure_search_with_max_file_size(&project, &storage, 64),
            &ctx,
        );
        assert!(lowered.success);
        wait_for_search_index_ready(&ctx, Duration::from_secs(10));
        assert_eq!(
            ctx.search_index()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .unwrap()
                .configured_max_file_size(),
            64,
            "the persisted same-HEAD cache must honor a lowered size limit"
        );

        let raised = handle_configure_for_test(
            &configure_search_with_max_file_size(&project, &storage, 512),
            &ctx,
        );
        assert!(raised.success);
        wait_for_search_index_ready(&ctx, Duration::from_secs(10));
        assert_eq!(
            ctx.search_index()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .unwrap()
                .configured_max_file_size(),
            512,
            "the persisted same-HEAD cache must honor a raised size limit"
        );
    }

    #[test]
    fn main_then_worktree_keeps_main_owner_and_worktree_borrows_read_only() {
        let _artifact_guard = artifact_owner_test_mutex().lock().unwrap();
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("storage");
        let main = temp.path().join("main");
        init_git_fixture(&main);
        let worktree = temp.path().join("worktree");
        let mut worktree_command = Command::new("git");
        assert!(
            crate::test_env::apply_hermetic_git_env(worktree_command.arg("-C").arg(&main))
                .args(["worktree", "add", "--detach", "--quiet"])
                .arg(&worktree)
                .arg("HEAD")
                .status()
                .unwrap()
                .success()
        );

        let main_ctx = test_context();
        let main_response =
            handle_configure_for_test(&configure_with_storage(&main, &storage), &main_ctx);
        assert!(main_response.success);
        assert_eq!(main_response.data["artifact_owner"]["mode"], json!("owner"));
        let main_manifest = owner_manifest_from_response(&main_response);

        let worktree_ctx = test_context();
        let worktree_response =
            handle_configure_for_test(&configure_with_storage(&worktree, &storage), &worktree_ctx);
        assert!(worktree_response.success);
        assert_eq!(worktree_ctx.cache_role(), "worktree");
        assert!(worktree_ctx.shared_artifacts_read_only());
        assert_eq!(
            worktree_response.data["artifact_owner"]["mode"],
            json!("read_only")
        );
        let manifest_after_worktree = owner_manifest_from_response(&main_response);
        assert_eq!(
            manifest_after_worktree.project_scope_key,
            main_manifest.project_scope_key
        );
        assert_eq!(
            manifest_after_worktree.checkout_path,
            main_manifest.checkout_path
        );
    }

    #[test]
    fn main_bind_self_heals_live_worktree_owner_manifest() {
        let _artifact_guard = artifact_owner_test_mutex().lock().unwrap();
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("storage");
        let main = temp.path().join("main");
        init_git_fixture(&main);
        let worktree = temp.path().join("worktree");
        let mut worktree_command = Command::new("git");
        assert!(
            crate::test_env::apply_hermetic_git_env(worktree_command.arg("-C").arg(&main))
                .args(["worktree", "add", "--detach", "--quiet"])
                .arg(&worktree)
                .arg("HEAD")
                .status()
                .unwrap()
                .success()
        );
        let canonical_main = std::fs::canonicalize(&main).unwrap();
        let canonical_worktree = std::fs::canonicalize(&worktree).unwrap();
        let project_key = crate::search_index::artifact_cache_key(&canonical_main);
        let worktree_scope = crate::path_identity::project_scope_key(&canonical_worktree);
        let worktree_probe_ctx = test_context();
        let (_, common_dir) =
            super::detect_worktree_bridge(&worktree_probe_ctx, &canonical_worktree);
        let common_dir = common_dir.expect("linked worktree common dir");
        crate::artifact_owner::write_synthetic_manifest_with_git_common_dir_for_test(
            &storage,
            &canonical_worktree,
            &project_key,
            &worktree_scope,
            std::process::id(),
            0,
            Some(&common_dir),
        );

        let main_ctx = test_context();
        let main_response =
            handle_configure_for_test(&configure_with_storage(&main, &storage), &main_ctx);
        assert!(main_response.success);
        assert_eq!(main_response.data["artifact_owner"]["mode"], json!("owner"));
        assert_eq!(main_ctx.cache_role(), "main");
        let manifest = owner_manifest_from_response(&main_response);
        assert_eq!(manifest.checkout_path, canonical_main.display().to_string());
        assert_eq!(
            manifest.project_scope_key,
            crate::path_identity::project_scope_key(&canonical_main)
        );
        let common_dir_string = common_dir.display().to_string();
        assert_eq!(
            manifest.git_common_dir.as_deref(),
            Some(common_dir_string.as_str())
        );
    }

    #[test]
    fn stale_generation_semantic_build_persists_and_followup_refreshes_incrementally() {
        let _artifact_guard = artifact_owner_test_mutex().lock().unwrap();
        let _env_lock = home_env_mutex();
        let _disable_watcher = EnvVarGuard::set("AFT_TEST_DISABLE_FILE_WATCHER", "1");
        super::reset_semantic_stale_generation_discards_for_test();
        let server = CountingEmbeddingServer::start();
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let storage = temp.path().join("storage");
        std::fs::create_dir_all(project.join("src")).unwrap();
        for name in ["alpha", "beta", "gamma", "delta"] {
            std::fs::write(
                project.join("src").join(format!("{name}.rs")),
                format!("pub fn {name}_symbol() -> usize {{ 1 }}\n"),
            )
            .unwrap();
        }
        let semantic_file = semantic_cache_file(&storage, &project);

        let first_ctx = test_context();
        let first_response = handle_configure_for_test(
            &configure_semantic_with_storage(&project, &storage, &server.base_url, true),
            &first_ctx,
        );
        assert!(
            first_response.success,
            "configure failed: {:?}",
            first_response.data
        );
        super::drain_deferred_configure_maintenance(&first_ctx);
        // Opening the maintenance gate only makes the worker runnable. Hold its
        // first corpus response so the next configure deterministically fences
        // an in-flight build rather than racing thread scheduling.
        assert!(
            server.wait_for_non_probe_input(Duration::from_secs(5)),
            "post-ack semantic loader did not begin its corpus request"
        );
        let disabled_response = handle_configure_for_test(
            &configure_semantic_with_storage(&project, &storage, &server.base_url, false),
            &first_ctx,
        );
        assert!(
            disabled_response.success,
            "disable configure failed: {:?}",
            disabled_response.data
        );
        server.release_responses();

        assert!(
            super::wait_for_semantic_stale_generation_discard_for_test(Duration::from_secs(5)),
            "timed out waiting for stale semantic build to persist"
        );
        assert!(
            semantic_file.is_file(),
            "stale semantic build was not persisted"
        );
        let initial_non_probe_inputs = server.non_probe_input_count();
        assert!(
            initial_non_probe_inputs >= 4,
            "initial build should embed the corpus, saw {initial_non_probe_inputs} inputs"
        );

        std::fs::write(
            project.join("src").join("epsilon.rs"),
            "pub fn epsilon_symbol() -> usize { 5 }\n",
        )
        .unwrap();

        let before_followup_inputs = server.non_probe_input_count();
        let second_ctx = test_context();
        let second_response = handle_configure_for_test(
            &configure_semantic_with_storage(&project, &storage, &server.base_url, true),
            &second_ctx,
        );
        assert!(
            second_response.success,
            "second configure failed: {:?}",
            second_response.data
        );
        wait_for_semantic_build_ready(&second_ctx, Duration::from_secs(5));
        let followup_inputs = server
            .non_probe_input_count()
            .saturating_sub(before_followup_inputs);
        assert!(
            followup_inputs < initial_non_probe_inputs,
            "follow-up build should use the persisted index and embed only the delta; initial={initial_non_probe_inputs}, followup={followup_inputs}"
        );
        assert!(
            followup_inputs <= 2,
            "expected only the new file's semantic chunks to be embedded, got {followup_inputs}"
        );
        super::reset_semantic_stale_generation_discards_for_test();
    }

    #[test]
    fn semantic_catchup_refreshes_never_embed_more_than_two_roots_concurrently() {
        let _artifact_guard = artifact_owner_test_mutex().lock().unwrap();
        let server = CountingEmbeddingServer::start();
        let temp = tempfile::tempdir().unwrap();
        let config = semantic_refresh_test_config(&server.base_url);
        let limiter = crate::cold_build_limiter::test_limiter(2);
        let mut workers = Vec::new();

        for root_number in 0..3 {
            let root = temp.path().join(format!("root-{root_number}"));
            std::fs::create_dir_all(&root).expect("create refresh test root");
            std::fs::write(
                root.join("lib.rs"),
                format!("pub fn refresh_root_{root_number}() {{}}\n"),
            )
            .expect("write refresh test source");
            let (ctx, request_tx, event_rx, worker) = spawn_semantic_corpus_refresh_worker_for_test(
                root,
                &config,
                super::SemanticRefreshLimiter(Arc::clone(&limiter)),
            );
            request_tx
                .send(SemanticRefreshRequest::Corpus)
                .expect("queue corpus refresh");
            drop(request_tx);
            workers.push((ctx, event_rx, worker));
        }

        assert!(
            server.wait_for_non_probe_request_count(2, Duration::from_secs(5)),
            "two semantic refreshes did not reach the embedding server"
        );
        std::thread::sleep(Duration::from_millis(250));
        assert_eq!(
            server.non_probe_request_count(),
            2,
            "the third root must wait for a process-wide semantic refresh slot"
        );

        server.release_responses();
        assert!(
            server.wait_for_non_probe_request_count(3, Duration::from_secs(5)),
            "queued semantic refresh did not start after a slot was released"
        );
        for (_ctx, _event_rx, worker) in workers {
            worker.join().expect("semantic refresh worker joins");
        }
    }

    #[test]
    fn unbound_semantic_refresh_worker_never_takes_a_queued_slot() {
        let _artifact_guard = artifact_owner_test_mutex().lock().unwrap();
        let server = CountingEmbeddingServer::start();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        std::fs::create_dir_all(&root).expect("create refresh test root");
        std::fs::write(root.join("lib.rs"), "pub fn queued_refresh() {}\n")
            .expect("write refresh test source");
        let config = semantic_refresh_test_config(&server.base_url);
        let limiter = crate::cold_build_limiter::test_limiter(1);
        let held = crate::cold_build_limiter::acquire_blocking_while_with_test_limiter(
            &limiter,
            "test queued semantic refresh",
            || true,
        )
        .expect("hold test refresh slot");
        let (ctx, request_tx, event_rx, worker) = spawn_semantic_corpus_refresh_worker_for_test(
            root,
            &config,
            super::SemanticRefreshLimiter(Arc::clone(&limiter)),
        );
        request_tx
            .send(SemanticRefreshRequest::Corpus)
            .expect("queue corpus refresh");
        drop(request_tx);

        std::thread::sleep(Duration::from_millis(150));
        ctx.mark_subc_unbound();
        drop(held);
        server.release_responses();
        worker.join().expect("queued refresh worker joins");

        assert_eq!(
            server.non_probe_request_count(),
            0,
            "an unbound root must leave the limiter queue without embedding"
        );
        assert!(
            matches!(
                event_rx.try_recv(),
                Err(crossbeam_channel::TryRecvError::Disconnected)
            ),
            "an unbound worker must not publish a refresh event after its slot wait"
        );
    }

    #[test]
    fn equivalent_reconfigure_reloads_evicted_semantic_index_from_disk() {
        let _artifact_guard = artifact_owner_test_mutex().lock().unwrap();
        let _env_lock = home_env_mutex();
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _disable_watcher = EnvVarGuard::set("AFT_TEST_DISABLE_FILE_WATCHER", "1");
        let _quiet_window = EnvVarGuard::set("AFT_SEMANTIC_QUIET_WINDOW_MS", "1");
        let server = CountingEmbeddingServer::start();
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let storage = temp.path().join("storage");
        std::fs::create_dir_all(project.join("src")).unwrap();
        std::fs::write(
            project.join("src/lib.rs"),
            "pub fn reloadable_semantic_symbol() -> bool { true }\n",
        )
        .unwrap();
        init_git_fixture(&project);
        let request = configure_semantic_with_storage(&project, &storage, &server.base_url, true);
        let ctx = test_context();

        let response = handle_configure_for_test(&request, &ctx);
        assert!(response.success, "configure failed: {:?}", response.data);
        super::drain_deferred_configure_maintenance(&ctx);
        assert!(
            server.wait_for_non_probe_input(Duration::from_secs(5)),
            "initial semantic build did not reach the embedding server"
        );
        server.release_responses();
        wait_for_semantic_build_ready(&ctx, Duration::from_secs(10));
        assert!(semantic_cache_file(&storage, &project).is_file());
        let embedded_before_reload = server.non_probe_input_count();

        assert!(ctx.evict_idle_artifacts(), "semantic index should be idle");
        assert!(ctx.semantic_index().read().unwrap().is_none());
        reset_configure_artifact_load_attempts_for_test();
        let response = handle_configure_for_test(&request, &ctx);
        assert!(
            response.success,
            "equivalent configure failed: {:?}",
            response.data
        );
        assert_eq!(
            response.data["search_index_cache_reused"],
            json!(false),
            "an evicted artifact must not be reported as resident reuse"
        );
        assert_eq!(
            configure_artifact_load_attempts_for_test(),
            0,
            "equivalent rebind must keep disk loading behind the acknowledgement"
        );
        assert!(
            ctx.semantic_index_rx().lock().is_some(),
            "equivalent rebind did not schedule the missing semantic artifact"
        );

        wait_for_semantic_build_ready(&ctx, Duration::from_secs(10));
        assert_eq!(
            configure_artifact_load_attempts_for_test(),
            1,
            "equivalent rebind should start one semantic loader"
        );
        assert_eq!(
            server.non_probe_input_count(),
            embedded_before_reload,
            "semantic reload re-embedded corpus content instead of reading semantic.bin"
        );

        ctx.mark_subc_unbound();
        ctx.cancel_unbound_artifact_work();
        assert!(ctx.semantic_index().read().unwrap().is_some());
        assert!(ctx.semantic_refresh_sender().is_none());

        ctx.mark_subc_bound();
        let response = handle_configure_for_test(&request, &ctx);
        assert!(
            response.success,
            "equivalent rebind failed: {:?}",
            response.data
        );
        super::drain_deferred_configure_maintenance(&ctx);
        wait_for_semantic_build_ready(&ctx, Duration::from_secs(10));
        let refresh_sender = ctx
            .semantic_refresh_sender()
            .expect("writer rebind must restore semantic refresh worker");

        let source = project.join("src/lib.rs");
        std::fs::write(
            &source,
            "pub fn semantic_symbol_after_rebind() -> bool { true }\n",
        )
        .unwrap();
        {
            let mut index = ctx.semantic_index().write().unwrap();
            index
                .as_mut()
                .expect("semantic index")
                .invalidate_file(&source);
        }
        ctx.semantic_index_status()
            .write()
            .unwrap()
            .start_refreshing_file(source.clone());
        let inputs_before_refresh = server.non_probe_input_count();
        refresh_sender
            .send(SemanticRefreshRequest::Files {
                paths: vec![source],
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            crate::runtime_drain::drain_semantic_refresh_events(&ctx);
            if server.non_probe_input_count() > inputs_before_refresh
                && ctx
                    .semantic_index_status()
                    .read()
                    .unwrap()
                    .refreshing_count()
                    == 0
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!(
            "watcher-style semantic refresh did not complete after equivalent rebind: inputs {} -> {}, status {:?}, receiver_present={}",
            inputs_before_refresh,
            server.non_probe_input_count(),
            *ctx.semantic_index_status().read().unwrap(),
            ctx.semantic_refresh_event_rx().lock().is_some(),
        );
    }

    #[test]
    fn semantic_query_reloads_evicted_index_once_without_reconfigure() {
        let _artifact_guard = artifact_owner_test_mutex().lock().unwrap();
        let _env_lock = home_env_mutex();
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _disable_watcher = EnvVarGuard::set("AFT_TEST_DISABLE_FILE_WATCHER", "1");
        let server = CountingEmbeddingServer::start();
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let storage = temp.path().join("storage");
        std::fs::create_dir_all(project.join("src")).unwrap();
        std::fs::write(
            project.join("src/lib.rs"),
            "pub fn query_reload_symbol() -> bool { true }\n",
        )
        .unwrap();
        init_git_fixture(&project);
        let ctx = test_context();
        let configure = handle_configure_for_test(
            &configure_semantic_with_storage(&project, &storage, &server.base_url, true),
            &ctx,
        );
        assert!(configure.success, "configure failed: {:?}", configure.data);
        super::drain_deferred_configure_maintenance(&ctx);
        assert!(
            server.wait_for_non_probe_input(Duration::from_secs(5)),
            "initial semantic build did not reach the embedding server"
        );
        server.release_responses();
        wait_for_semantic_build_ready(&ctx, Duration::from_secs(10));
        let embedded_before_reload = server.non_probe_input_count();

        assert!(ctx.evict_idle_artifacts(), "semantic index should be idle");
        reset_configure_artifact_load_attempts_for_test();
        let first = crate::commands::semantic_search::handle_semantic_search(
            &semantic_search_request("how does the query reload semantic state"),
            &ctx,
        );
        assert!(
            first.success,
            "degraded search should succeed: {:?}",
            first.data
        );
        assert!(
            first.data["text"]
                .as_str()
                .is_some_and(|text| text.contains("Semantic index is reloading; retry shortly.")),
            "first query did not disclose the scheduled reload: {:?}",
            first.data
        );
        assert!(
            ctx.semantic_index_rx().lock().is_some(),
            "first query did not reserve the semantic reload slot"
        );

        let second = crate::commands::semantic_search::handle_semantic_search(
            &semantic_search_request("how does the query reload semantic state"),
            &ctx,
        );
        assert!(
            second.success,
            "building fallback should succeed: {:?}",
            second.data
        );
        wait_for_semantic_build_ready(&ctx, Duration::from_secs(10));
        assert_eq!(
            configure_artifact_load_attempts_for_test(),
            1,
            "repeated queries started duplicate semantic reload workers"
        );
        assert_eq!(
            server.non_probe_input_count(),
            embedded_before_reload,
            "query-path reload re-embedded corpus content instead of reading semantic.bin"
        );
    }

    #[test]
    fn grep_query_reloads_evicted_trigram_index_from_cache() {
        let _artifact_guard = artifact_owner_test_mutex().lock().unwrap();
        let _env_lock = home_env_mutex();
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _disable_watcher = EnvVarGuard::set("AFT_TEST_DISABLE_FILE_WATCHER", "1");
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let storage = temp.path().join("storage");
        std::fs::create_dir_all(project.join("src")).unwrap();
        std::fs::write(
            project.join("src/lib.rs"),
            "pub fn TrigramReloadNeedle() -> bool { true }\n",
        )
        .unwrap();
        init_git_fixture(&project);
        let ctx = test_context();
        let configure = handle_configure_for_test(
            &configure_request_with_params(json!({
                "project_root": project,
                "harness": "opencode",
                "storage_dir": storage,
                "config": [user_tier(json!({
                    "search_index": true,
                    "semantic_search": false,
                    "callgraph_store": false
                }))]
            })),
            &ctx,
        );
        assert!(configure.success, "configure failed: {:?}", configure.data);
        wait_for_search_index_ready(&ctx, Duration::from_secs(10));
        ctx.flush_search_index_on_graceful_shutdown();
        let canonical_project = std::fs::canonicalize(&project).unwrap();
        let cache_dir = crate::search_index::resolve_cache_dir_with_key(
            &ctx.memoized_artifact_cache_key(&canonical_project),
            Some(&storage),
        );
        let cache_file = cache_dir.join("cache.bin");
        let cache_modified_before = std::fs::metadata(&cache_file)
            .and_then(|metadata| metadata.modified())
            .expect("search cache modification time");

        assert!(ctx.evict_idle_artifacts(), "trigram index should be idle");
        reset_configure_artifact_load_attempts_for_test();
        let first = crate::commands::grep::handle_grep(&grep_request("TrigramReloadNeedle"), &ctx);
        assert!(first.success, "fallback grep failed: {:?}", first.data);
        assert_eq!(first.data["index_status"], json!("Building"));
        assert_eq!(first.data["total_matches"], json!(1));
        assert!(
            ctx.search_index_rx().read().unwrap().is_some(),
            "fallback grep did not reserve the trigram reload slot"
        );

        wait_for_search_index_ready(&ctx, Duration::from_secs(10));
        assert_eq!(
            configure_artifact_load_attempts_for_test(),
            1,
            "fallback queries started duplicate trigram reload workers"
        );
        assert_eq!(
            std::fs::metadata(&cache_file)
                .and_then(|metadata| metadata.modified())
                .expect("reloaded search cache modification time"),
            cache_modified_before,
            "trigram reload rewrote cache.bin instead of loading the verified cache"
        );
        let indexed =
            crate::commands::grep::handle_grep(&grep_request("TrigramReloadNeedle"), &ctx);
        assert!(indexed.success, "indexed grep failed: {:?}", indexed.data);
        assert_eq!(indexed.data["index_status"], json!("Ready"));
        assert_eq!(indexed.data["total_matches"], json!(1));
    }

    #[test]
    fn linked_worktree_configure_defers_read_only_artifact_loads_without_cold_builds() {
        let _artifact_guard = artifact_owner_test_mutex().lock().unwrap();
        let _env_guard = home_env_mutex();
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _disable_watcher = EnvVarGuard::set("AFT_TEST_DISABLE_FILE_WATCHER", "1");
        let temp = tempfile::tempdir().unwrap();
        let main = temp.path().join("main");
        init_git_fixture(&main);
        let worktree = temp.path().join("worktree");
        let mut worktree_command = Command::new("git");
        assert!(
            crate::test_env::apply_hermetic_git_env(worktree_command.arg("-C").arg(&main))
                .args(["worktree", "add", "--detach", "--quiet"])
                .arg(&worktree)
                .arg("HEAD")
                .status()
                .unwrap()
                .success()
        );

        let ctx = test_context();
        let req = configure_request_with_params(json!({
            "project_root": worktree,
            "harness": "opencode",
            "config": [user_tier(json!({
                "search_index": true,
                "semantic_search": true,
                "callgraph_store": true
            }))]
        }));

        let response = handle_configure_for_test(&req, &ctx);

        assert!(response.success);
        assert_eq!(ctx.cache_role(), "worktree");
        assert!(
            ctx.search_index_rx().read().unwrap().is_some(),
            "read-only search artifact load should wait for configure maintenance"
        );
        assert!(
            ctx.semantic_index_rx().lock().is_some(),
            "read-only semantic artifact load should wait for configure maintenance"
        );
        assert!(
            ctx.callgraph_store_rx().lock().is_none(),
            "linked worktrees must not schedule a callgraph cold build"
        );

        super::drain_deferred_configure_maintenance(&ctx);
        let deadline = Instant::now() + Duration::from_secs(5);
        while ctx.search_index_rx().read().unwrap().is_some()
            || ctx.semantic_index_rx().lock().is_some()
        {
            crate::runtime_drain::drain_build_completions(&ctx);
            assert!(
                Instant::now() < deadline,
                "read-only artifact opens did not settle"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            ctx.evict_idle_artifacts(),
            "read-only artifacts should be idle"
        );
        reset_configure_artifact_load_attempts_for_test();
        let owner_before = ctx.artifact_owner_status();
        let grep = crate::commands::grep::handle_grep(&grep_request("tracked"), &ctx);
        assert!(
            grep.success,
            "read-only fallback grep failed: {:?}",
            grep.data
        );
        assert!(super::trigger_semantic_index_reload_if_evicted(&ctx));
        assert!(
            ctx.search_index_rx().read().unwrap().is_some(),
            "read-only grep should reopen its evicted shared search snapshot"
        );
        assert!(
            ctx.semantic_index_rx().lock().is_some(),
            "read-only semantic query should reopen its evicted shared snapshot"
        );
        assert!(ctx.shared_artifacts_read_only());
        assert_eq!(
            ctx.artifact_owner_status().map(|status| status.mode),
            owner_before.map(|status| status.mode),
            "read-only fallback must not acquire an owner lease"
        );
        ctx.mark_subc_unbound();
        ctx.cancel_unbound_artifact_work();
    }

    #[test]
    fn read_only_absent_search_loader_wakes_completion_drain() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let ctx = test_context();
        ctx.update_config(|config| {
            config.project_root = Some(root.path().to_path_buf());
            config.storage_dir = Some(storage.path().to_path_buf());
            config.search_index = true;
        });
        ctx.set_canonical_cache_root(root.path().to_path_buf());
        ctx.set_cache_writer_capabilities(false, true);

        let starts = super::schedule_artifact_loads(&ctx, true, false);
        assert_eq!(starts.len(), 1);
        assert!(super::start_artifact_loads(starts));
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if ctx.completion_drains_have_work() {
                crate::runtime_drain::drain_search_index_events(&ctx);
            }
            if ctx
                .search_index_rx()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_none()
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "terminal empty loader receiver did not wake and retire through maintenance"
            );
            std::thread::yield_now();
        }

        assert!(
            super::trigger_search_index_reload_if_evicted(&ctx),
            "a later query must be able to retry the read-only snapshot"
        );
        assert!(ctx.search_index_rx().read().unwrap().is_some());
        ctx.mark_subc_unbound();
        ctx.cancel_unbound_artifact_work();
    }

    #[test]
    fn semantic_refresh_disconnect_restart_installs_replacement_loaders() {
        let _artifact_guard = artifact_owner_test_mutex().lock().unwrap();
        let _env_guard = home_env_mutex();
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let ctx = test_context();
        ctx.update_config(|config| {
            config.project_root = Some(root.path().to_path_buf());
            config.storage_dir = Some(storage.path().to_path_buf());
            config.semantic_search = true;
        });
        ctx.set_canonical_cache_root(root.path().to_path_buf());
        set_configure_artifact_post_gate_delay_for_test(500);
        struct DelayReset;
        impl Drop for DelayReset {
            fn drop(&mut self) {
                set_configure_artifact_post_gate_delay_for_test(0);
            }
        }
        let _delay_reset = DelayReset;

        assert!(super::restart_semantic_artifacts_after_refresh_disconnect(
            &ctx,
            ctx.semantic_index_rx_epoch(),
        ));
        assert!(ctx.semantic_index_rx().lock().is_some());
        assert!(ctx.semantic_refresh_event_rx().lock().is_some());

        ctx.mark_subc_unbound();
        ctx.cancel_unbound_artifact_work();
        let deadline = Instant::now() + Duration::from_secs(2);
        while configure_artifact_load_cancellations_for_test() == 0 {
            assert!(
                Instant::now() < deadline,
                "replacement semantic loader did not observe lifecycle cancellation"
            );
            std::thread::yield_now();
        }
    }

    #[test]
    fn configure_defers_large_tree_file_walk_until_after_ack() {
        let _env_guard = home_env_mutex();
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _disable_watcher = EnvVarGuard::set("AFT_TEST_DISABLE_FILE_WATCHER", "1");
        let _delay_walk = EnvVarGuard::set("AFT_TEST_CONFIGURE_DEFERRED_WALK_DELAY_MS", "2000");
        let temp = tempfile::tempdir().unwrap();
        let walk_start_file = temp.path().join("deferred-walk-start");
        let _walk_start_signal = EnvVarGuard::set(
            "AFT_TEST_CONFIGURE_DEFERRED_WALK_START_FILE",
            walk_start_file.to_str().unwrap(),
        );
        init_git_fixture(temp.path());
        for dir in 0..10 {
            let dir_path = temp.path().join(format!("bulk-{dir}"));
            std::fs::create_dir_all(&dir_path).unwrap();
            for file in 0..40 {
                std::fs::write(dir_path.join(format!("file-{file}.rs")), "fn main() {}\n").unwrap();
            }
        }

        let ctx = test_context();
        ctx.set_progress_sender(Some(Arc::new(Box::new(|_frame: PushFrame| {}))));
        let req = configure_request_with_params(json!({
            "project_root": temp.path(),
            "harness": "opencode",
            "config": [user_tier(json!({
                "search_index": false,
                "semantic_search": false,
                "callgraph_store": false
            }))]
        }));

        let start = Instant::now();
        let response = handle_configure_for_test(&req, &ctx);
        let elapsed = start.elapsed();

        assert!(response.success);
        assert!(
            elapsed < Duration::from_secs(5),
            "configure acknowledgement exceeded its generous sanity ceiling: {elapsed:?}"
        );
        assert!(
            !walk_start_file.exists(),
            "configure acknowledgement was not observed before the deferred file walk started"
        );
        assert!(response.data.get("source_file_count").is_none());
        assert!(ctx.drain_configure_warnings().is_empty());

        let walk_start_deadline = Instant::now() + Duration::from_secs(5);
        while !walk_start_file.exists() {
            assert!(
                Instant::now() < walk_start_deadline,
                "timed out waiting for the deferred file walk to start"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        // The deferred walk still completes and publishes its frame (for
        // missing-binary warnings); only the count field is gone.
        let frame =
            wait_for_configure_warnings(&ctx, ctx.configure_generation(), Duration::from_secs(5));
        assert_eq!(frame.frame_type, "configure_warnings");
    }

    #[test]
    fn configure_artifact_loads_start_only_from_post_ack_maintenance() {
        let _artifact_guard = artifact_owner_test_mutex().lock().unwrap();
        let _env_guard = home_env_mutex();
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _disable_watcher = EnvVarGuard::set("AFT_TEST_DISABLE_FILE_WATCHER", "1");
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        init_git_fixture(root.path());
        let ctx = test_context();
        let req = configure_request_with_params(json!({
            "project_root": root.path(),
            "harness": "opencode",
            "storage_dir": storage.path(),
            "config": [user_tier(json!({
                "search_index": true,
                "semantic_search": false,
                "callgraph_store": false
            }))]
        }));
        reset_configure_artifact_load_attempts_for_test();

        let response = handle_configure_for_test(&req, &ctx);
        assert!(response.success);
        assert_eq!(
            configure_artifact_load_attempts_for_test(),
            0,
            "artifact deserialization started before configure returned its acknowledgement"
        );
        assert!(ctx.search_index_rx().read().unwrap().is_some());

        super::drain_deferred_configure_maintenance(&ctx);
        let deadline = Instant::now() + Duration::from_secs(2);
        while configure_artifact_load_attempts_for_test() == 0 {
            assert!(
                Instant::now() < deadline,
                "post-ack maintenance did not release the artifact loader"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let completion_deadline = Instant::now() + Duration::from_secs(5);
        while ctx.search_index_rx().read().unwrap().is_some() {
            crate::runtime_drain::drain_search_index_events(&ctx);
            assert!(
                Instant::now() < completion_deadline,
                "post-ack artifact loader did not complete"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn configure_maintenance_does_not_inline_wait_for_callgraph_cold_build() {
        // Query ops honor AFT_CALLGRAPH_BUILD_WAIT_MS so a tiny fixture can
        // resolve Ready on the first call. Configure maintenance runs on the
        // transport loop; if it inherited that wait, stdin EOF would not be
        // observed until the cold build finished or the window expired.
        let _wait_guard = crate::context::override_callgraph_build_wait_ms_for_test(2_000);
        let _artifact_guard = artifact_owner_test_mutex().lock().unwrap();
        let _env_guard = home_env_mutex();
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _disable_watcher = EnvVarGuard::set("AFT_TEST_DISABLE_FILE_WATCHER", "1");
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        init_git_fixture(root.path());
        std::fs::write(root.path().join("lib.rs"), "pub fn marker() {}\n").unwrap();
        let ctx = test_context();
        ctx.isolate_cold_build_limiter_for_test(1);
        let req = configure_request_with_params(json!({
            "project_root": root.path(),
            "harness": "opencode",
            "storage_dir": storage.path(),
            "config": [user_tier(json!({
                "search_index": false,
                "semantic_search": false,
                "callgraph_store": true
            }))]
        }));
        let response = handle_configure_for_test(&req, &ctx);
        assert!(response.success, "configure failed: {response:?}");

        let canonical_root = ctx.canonical_cache_root();
        let (reached, release) =
            crate::context::install_callgraph_build_start_gate_for_test(canonical_root);

        let started = Instant::now();
        super::drain_deferred_configure_maintenance(&ctx);
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(500),
            "configure maintenance waited {elapsed:?} for a callgraph cold build; \
             the transport loop must return immediately so stdin EOF stays observable"
        );

        reached
            .recv_timeout(Duration::from_secs(2))
            .expect("callgraph worker must start so this test actually covers the wait skip");
        release.send(()).unwrap();
    }

    #[test]
    fn non_git_configure_defers_callgraph_until_navigation_and_reports_trip_terminally() {
        let _artifact_guard = artifact_owner_test_mutex().lock().unwrap();
        let _env_guard = home_env_mutex();
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let source = root.path().join("lib.rs");
        std::fs::write(&source, "pub fn marker() {}\n").unwrap();
        let ctx = test_context();
        ctx.isolate_cold_build_limiter_for_test(1);
        let request = configure_request_with_params(json!({
            "project_root": root.path(),
            "harness": "opencode",
            "storage_dir": storage.path(),
            "config": [user_tier(json!({
                "search_index": false,
                "semantic_search": false,
                "callgraph_store": true
            }))]
        }));
        assert!(handle_configure_for_test(&request, &ctx).success);
        super::drain_deferred_configure_maintenance(&ctx);

        let limiter = ctx.cold_build_limiter();
        assert!(
            limiter.admission_events().is_empty(),
            "non-git configure must not admit a callgraph cold-build job"
        );
        assert!(ctx.callgraph_store_rx().lock().is_none());
        assert!(
            !ctx.callgraph_store_dir()
                .join("build-breaker.sqlite")
                .exists(),
            "configure must not reach breaker attempt admission"
        );

        let canonical_root = ctx.callgraph_project_root().unwrap();
        let (reached, release) =
            crate::context::install_callgraph_build_start_gate_for_test(canonical_root);
        let navigation = RawRequest {
            id: "navigation".to_string(),
            command: "callers".to_string(),
            lsp_hints: None,
            session_id: None,
            params: json!({"file": source, "symbol": "marker"}),
        };
        let first = crate::commands::callers::handle_callers(&navigation, &ctx);
        assert!(!first.success);
        assert_eq!(first.data["code"], json!("callgraph_building"));
        reached
            .recv_timeout(Duration::from_secs(2))
            .expect("the first navigation query must enter the background build path");
        assert_eq!(limiter.admission_events().len(), 1);
        let second = crate::commands::callers::handle_callers(&navigation, &ctx);
        assert_eq!(second.data["code"], json!("callgraph_building"));
        assert_eq!(
            limiter.admission_events().len(),
            1,
            "the retry must join the existing single-flight build"
        );
        release.send(()).unwrap();
        let completion_deadline = Instant::now() + Duration::from_secs(10);
        while ctx.callgraph_store_rx().lock().is_some() {
            crate::runtime_drain::drain_callgraph_store_events(&ctx);
            assert!(
                Instant::now() < completion_deadline,
                "navigation-started callgraph build did not settle"
            );
            std::thread::sleep(Duration::from_millis(5));
        }

        let suspended_root = tempfile::tempdir().unwrap();
        let suspended_storage = tempfile::tempdir().unwrap();
        let suspended_source = suspended_root.path().join("lib.rs");
        std::fs::write(&suspended_source, "pub fn suspended_marker() {}\n").unwrap();
        let suspended_ctx = test_context();
        suspended_ctx.isolate_cold_build_limiter_for_test(1);
        let suspended_request = configure_request_with_params(json!({
            "project_root": suspended_root.path(),
            "harness": "opencode",
            "storage_dir": suspended_storage.path(),
            "config": [user_tier(json!({
                "search_index": false,
                "semantic_search": false,
                "callgraph_store": true
            }))]
        }));
        assert!(handle_configure_for_test(&suspended_request, &suspended_ctx).success);
        super::drain_deferred_configure_maintenance(&suspended_ctx);
        let project_root = suspended_ctx.callgraph_project_root().unwrap();
        let breaker_key = crate::build_breaker::BreakerKey::new(
            project_root.display().to_string(),
            crate::build_breaker::BuildDomain::CallgraphCold,
            crate::callgraph_store::callgraph_corpus_fingerprint_for_test(&project_root, &[])
                .unwrap(),
        );
        let breaker = crate::build_breaker::BuildDeathBreaker::open(
            suspended_ctx
                .callgraph_store_dir()
                .join("build-breaker.sqlite"),
        )
        .unwrap();
        let now = crate::callgraph_store::unix_millis_now();
        for offset in 0..3 {
            let crate::build_breaker::BreakerAdmission::Admitted(attempt) =
                breaker.admit_at(&breaker_key, 0, now + offset).unwrap()
            else {
                panic!("breaker tripped before the third zero-credit death");
            };
            breaker
                .record_attributed_death_at(&breaker_key, &attempt.attempt_id, 0, 0, now + offset)
                .unwrap();
        }

        let suspended_navigation = RawRequest {
            id: "suspended-navigation".to_string(),
            command: "callers".to_string(),
            lsp_hints: None,
            session_id: None,
            params: json!({"file": suspended_source, "symbol": "suspended_marker"}),
        };
        let suspended =
            crate::commands::callers::handle_callers(&suspended_navigation, &suspended_ctx);
        assert!(!suspended.success);
        assert_eq!(suspended.data["code"], json!("build_suspended"));
        let message = suspended.data["message"].as_str().unwrap();
        assert!(
            message.starts_with("callers: build_suspended domain=callgraph_cold deaths=3 age_ms=")
        );
        assert!(message.ends_with(
            " reason=zero_credit_death_limit; run doctor reset-build-breaker to resume"
        ));
        assert!(
            suspended_ctx
                .cold_build_limiter()
                .admission_events()
                .is_empty(),
            "a terminal suspension must refuse before limiter admission"
        );
    }

    #[test]
    fn unbound_configure_cancellation_clears_gated_loaders_and_rebind_can_retry() {
        let _artifact_guard = artifact_owner_test_mutex().lock().unwrap();
        let _env_guard = home_env_mutex();
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _disable_watcher = EnvVarGuard::set("AFT_TEST_DISABLE_FILE_WATCHER", "1");
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        init_git_fixture(root.path());
        let ctx = test_context();
        let req = configure_request_with_params(json!({
            "project_root": root.path(),
            "harness": "opencode",
            "storage_dir": storage.path(),
            "config": [user_tier(json!({
                "search_index": true,
                "semantic_search": false,
                "callgraph_store": false
            }))]
        }));
        reset_configure_artifact_load_attempts_for_test();
        reset_configure_artifact_load_cancellations_for_test();

        assert!(handle_configure_for_test(&req, &ctx).success);
        assert!(ctx.search_index_rx().read().unwrap().is_some());
        ctx.mark_subc_unbound();
        assert!(super::cancel_deferred_configure_maintenance(&ctx) > 0);
        assert!(ctx.search_index_rx().read().unwrap().is_none());
        assert!(!ctx.configure_tail_has_work());
        let cancel_deadline = Instant::now() + Duration::from_secs(2);
        while configure_artifact_load_cancellations_for_test() == 0 {
            assert!(
                Instant::now() < cancel_deadline,
                "cancelled artifact loader did not observe its disconnected gate"
            );
            std::thread::yield_now();
        }
        assert_eq!(
            configure_artifact_load_attempts_for_test(),
            0,
            "cancelling a gated configure must not start its artifact loader"
        );

        assert!(handle_configure_for_test(&req, &ctx).success);
        assert!(ctx.search_index_rx().read().unwrap().is_some());
        ctx.mark_subc_bound();
        super::drain_deferred_configure_maintenance(&ctx);
        let deadline = Instant::now() + Duration::from_secs(2);
        while configure_artifact_load_attempts_for_test() == 0 {
            assert!(
                Instant::now() < deadline,
                "rebind did not replace the cancelled artifact loader"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn artifact_worker_rechecks_lifecycle_after_its_start_gate() {
        let _artifact_guard = artifact_owner_test_mutex().lock().unwrap();
        let _env_guard = home_env_mutex();
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _disable_watcher = EnvVarGuard::set("AFT_TEST_DISABLE_FILE_WATCHER", "1");
        struct ResetPostGateDelay;
        impl Drop for ResetPostGateDelay {
            fn drop(&mut self) {
                set_configure_artifact_post_gate_delay_for_test(0);
            }
        }
        let _reset = ResetPostGateDelay;

        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        init_git_fixture(root.path());
        let req = configure_request_with_params(json!({
            "project_root": root.path(),
            "harness": "opencode",
            "storage_dir": storage.path(),
            "config": [user_tier(json!({
                "search_index": true,
                "semantic_search": false,
                "callgraph_store": false
            }))]
        }));
        let ctx = Arc::new(test_context());
        reset_configure_artifact_load_attempts_for_test();
        reset_configure_artifact_load_cancellations_for_test();
        set_configure_artifact_post_gate_delay_for_test(500);

        assert!(handle_configure_for_test(&req, &ctx).success);
        ctx.mark_subc_bound();
        let drain_ctx = Arc::clone(&ctx);
        let drain = std::thread::spawn(move || {
            super::drain_deferred_configure_maintenance(&drain_ctx);
        });

        let reached_deadline = Instant::now() + Duration::from_secs(2);
        while configure_artifact_post_gate_reached_for_test() == 0 {
            assert!(
                Instant::now() < reached_deadline,
                "artifact worker did not cross its start gate"
            );
            std::thread::yield_now();
        }
        ctx.mark_subc_unbound();
        super::cancel_deferred_configure_maintenance(&ctx);
        drain.join().unwrap();

        let cancel_deadline = Instant::now() + Duration::from_secs(2);
        while configure_artifact_load_cancellations_for_test() == 0 {
            assert!(
                Instant::now() < cancel_deadline,
                "worker did not reject the now-unbound lifecycle"
            );
            std::thread::yield_now();
        }
        assert_eq!(configure_artifact_load_attempts_for_test(), 0);
        assert!(ctx.search_index_rx().read().unwrap().is_none());
    }

    #[test]
    fn superseded_configure_tail_does_not_cancel_current_artifact_receiver() {
        let _artifact_guard = artifact_owner_test_mutex().lock().unwrap();
        let _env_guard = home_env_mutex();
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _disable_watcher = EnvVarGuard::set("AFT_TEST_DISABLE_FILE_WATCHER", "1");
        let _delay = EnvVarGuard::set("AFT_TEST_CONFIGURE_DEFERRED_MAINTENANCE_DELAY_MS", "500");
        reset_configure_deferred_delay_reached_for_test();

        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        init_git_fixture(root.path());
        let req = configure_request_with_params(json!({
            "project_root": root.path(),
            "harness": "opencode",
            "storage_dir": storage.path(),
            "config": [user_tier(json!({
                "search_index": true,
                "semantic_search": false,
                "callgraph_store": false
            }))]
        }));
        let ctx = Arc::new(test_context());
        assert!(handle_configure_for_test(&req, &ctx).success);
        ctx.mark_subc_bound();

        let drain_ctx = Arc::clone(&ctx);
        let drain = std::thread::spawn(move || {
            super::drain_deferred_configure_maintenance(&drain_ctx);
        });
        let reached_deadline = Instant::now() + Duration::from_secs(2);
        while configure_deferred_delay_reached_for_test() == 0 {
            assert!(
                Instant::now() < reached_deadline,
                "configure tail did not reach the controlled delay"
            );
            std::thread::yield_now();
        }

        let current_generation = ctx.advance_configure_generation();
        let current_persist_epoch = ctx.next_search_persist_epoch();
        let (current_tx, current_rx) = crossbeam_channel::unbounded();
        ctx.install_search_index_rx(current_rx, current_generation);
        drain.join().unwrap();

        assert!(
            ctx.search_index_rx().read().unwrap().is_some(),
            "a superseded configure tail must not clear the current generation's receiver"
        );
        assert_eq!(
            ctx.search_persist_epoch_flag().current(),
            current_persist_epoch,
            "a stale configure tail must not invalidate a newer worker's persistence epoch"
        );
        drop(current_tx);
        ctx.search_index_rx().write().unwrap().take();
    }

    #[test]
    fn unbind_during_configure_tail_delay_does_not_release_artifact_loader() {
        let _artifact_guard = artifact_owner_test_mutex().lock().unwrap();
        let _env_guard = home_env_mutex();
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _disable_watcher = EnvVarGuard::set("AFT_TEST_DISABLE_FILE_WATCHER", "1");
        let _delay = EnvVarGuard::set("AFT_TEST_CONFIGURE_DEFERRED_MAINTENANCE_DELAY_MS", "500");
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        init_git_fixture(root.path());
        let ctx = Arc::new(test_context());
        let req = configure_request_with_params(json!({
            "project_root": root.path(),
            "harness": "opencode",
            "storage_dir": storage.path(),
            "config": [user_tier(json!({
                "search_index": true,
                "semantic_search": false,
                "callgraph_store": false
            }))]
        }));
        reset_configure_artifact_load_attempts_for_test();
        reset_configure_artifact_load_cancellations_for_test();

        assert!(handle_configure_for_test(&req, &ctx).success);
        ctx.mark_subc_bound();
        let drain_ctx = Arc::clone(&ctx);
        let drain = std::thread::spawn(move || {
            super::drain_deferred_configure_maintenance(&drain_ctx);
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while ctx.configure_maintenance_job_count_for_test() != 0 {
            assert!(
                Instant::now() < deadline,
                "configure tail did not leave the queue"
            );
            std::thread::yield_now();
        }

        ctx.mark_subc_unbound();
        assert_eq!(super::cancel_deferred_configure_maintenance(&ctx), 0);
        drain.join().unwrap();
        let cancel_deadline = Instant::now() + Duration::from_secs(2);
        while configure_artifact_load_cancellations_for_test() == 0 {
            assert!(
                Instant::now() < cancel_deadline,
                "in-flight configure tail did not cancel its gated artifact loader"
            );
            std::thread::yield_now();
        }
        assert_eq!(configure_artifact_load_attempts_for_test(), 0);
        assert!(ctx.search_index_rx().read().unwrap().is_none());
    }

    #[test]
    fn non_equivalent_reconfigure_retires_superseded_callgraph_receiver() {
        let _env_guard = home_env_mutex();
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _disable_watcher = EnvVarGuard::set("AFT_TEST_DISABLE_FILE_WATCHER", "1");
        let first_root = tempfile::tempdir().unwrap();
        let second_root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        init_git_fixture(first_root.path());
        init_git_fixture(second_root.path());
        let ctx = test_context();
        let config = [user_tier(json!({
            "search_index": false,
            "semantic_search": false,
            "callgraph_store": false
        }))];
        let first = configure_request_with_params(json!({
            "project_root": first_root.path(),
            "harness": "opencode",
            "storage_dir": storage.path(),
            "config": config.clone()
        }));
        assert!(handle_configure_for_test(&first, &ctx).success);

        let generation = ctx.configure_generation();
        let (_old_tx, old_rx) = crossbeam_channel::unbounded();
        ctx.note_callgraph_store_rx_generation(generation);
        let old_epoch = ctx.next_callgraph_store_rx_epoch();
        let old_persist_epoch = ctx.next_callgraph_persist_epoch();
        *ctx.callgraph_store_rx().lock() = Some(old_rx);

        let second = configure_request_with_params(json!({
            "project_root": second_root.path(),
            "harness": "opencode",
            "storage_dir": storage.path(),
            "config": config
        }));
        assert!(handle_configure_for_test(&second, &ctx).success);
        super::drain_deferred_configure_maintenance(&ctx);

        assert!(
            ctx.callgraph_store_rx().lock().is_none(),
            "a non-equivalent configure must retire the superseded receiver"
        );
        assert!(
            ctx.callgraph_store_rx_epoch() > old_epoch,
            "receiver retirement must invalidate a dequeued stale completion"
        );
        assert!(
            ctx.callgraph_persist_epoch_flag().current() > old_persist_epoch,
            "receiver retirement must also invalidate stale disk publication"
        );
    }

    #[test]
    fn workspace_manifest_fingerprint_is_lazy_and_reused_per_configure() {
        let _env_guard = home_env_mutex();
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        init_git_fixture(root.path());
        std::fs::create_dir_all(root.path().join("packages/pkg-a")).unwrap();
        std::fs::write(root.path().join("packages/pkg-a/package.json"), "{}").unwrap();
        let ctx = test_context();
        super::reset_workspace_manifest_fingerprint_scans_for_test();

        let disabled = configure_request_with_params(json!({
            "project_root": root.path(),
            "harness": "opencode",
            "storage_dir": storage.path(),
            "config": [user_tier(json!({
                "search_index": false,
                "semantic_search": false,
                "callgraph_store": false
            }))]
        }));
        assert!(handle_configure_for_test(&disabled, &ctx).success);
        let disabled_generation = ctx.configure_generation();
        assert_eq!(super::workspace_manifest_fingerprint_scans_for_test(), 0);
        std::fs::write(
            root.path().join("packages/pkg-a/package.json"),
            "{\"disabled\":true}",
        )
        .unwrap();
        assert!(handle_configure_for_test(&disabled, &ctx).success);
        assert_eq!(ctx.configure_generation(), disabled_generation);
        assert_eq!(super::workspace_manifest_fingerprint_scans_for_test(), 0);

        let enabled = configure_request_with_params(json!({
            "project_root": root.path(),
            "harness": "opencode",
            "storage_dir": storage.path(),
            "config": [user_tier(json!({
                "search_index": false,
                "semantic_search": false,
                "callgraph_store": true
            }))]
        }));
        assert!(handle_configure_for_test(&enabled, &ctx).success);
        let enabled_generation = ctx.configure_generation();
        assert_eq!(
            super::workspace_manifest_fingerprint_scans_for_test(),
            1,
            "a full configure must reuse its preflight manifest fingerprint"
        );
        std::fs::write(
            root.path().join("packages/pkg-a/package.json"),
            "{\"enabled\":true,\"changed\":true}",
        )
        .unwrap();
        assert!(handle_configure_for_test(&enabled, &ctx).success);
        assert_eq!(ctx.configure_generation(), enabled_generation + 1);
        assert_eq!(super::workspace_manifest_fingerprint_scans_for_test(), 2);
        assert!(handle_configure_for_test(&enabled, &ctx).success);
        assert_eq!(ctx.configure_generation(), enabled_generation + 1);
        assert_eq!(super::workspace_manifest_fingerprint_scans_for_test(), 3);
    }

    #[test]
    fn equivalent_reconfigure_keeps_warm_work_adopted_and_idempotent() {
        let _env_guard = home_env_mutex();
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _disable_watcher = EnvVarGuard::set("AFT_TEST_DISABLE_FILE_WATCHER", "1");
        let temp = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        init_git_fixture(temp.path());
        let ctx = test_context();
        let req = configure_request_with_params(json!({
            "project_root": temp.path(),
            "harness": "opencode",
            "storage_dir": storage.path(),
            "config": [user_tier(json!({
                "search_index": false,
                "semantic_search": false,
                "callgraph_store": false
            }))]
        }));

        let first = handle_configure_for_test(&req, &ctx);
        assert!(first.success);
        super::drain_deferred_configure_maintenance(&ctx);

        let generation_after_first = ctx.configure_generation();
        assert_eq!(
            ctx.backup().lock().disk_io_count_for_tests(),
            0,
            "initial configure maintenance must not inspect backup directories"
        );
        let tsconfig_clear_generation_after_first =
            ctx.tsconfig_membership_clear_generation_for_test();
        let filter_rebuilds_after_first = ctx.filter_registry_rebuild_count_for_test();
        let artifact_derivations_after_first = ctx.artifact_cache_key_derivation_count_for_test();
        assert_eq!(
            artifact_derivations_after_first, 0,
            "configure should not derive an unused artifact key when artifact-backed features are disabled"
        );
        for _ in 0..5 {
            let response = handle_configure_for_test(&req, &ctx);
            assert!(response.success);
            super::drain_deferred_configure_maintenance(&ctx);
        }

        assert_eq!(
            ctx.backup().lock().disk_io_count_for_tests(),
            0,
            "equivalent configures must not inspect backup directories"
        );
        assert!(ctx.search_index_rx().read().unwrap().is_none());
        assert!(ctx.semantic_index_rx().lock().is_none());
        assert!(ctx.callgraph_store_rx().lock().is_none());
        assert_eq!(
            ctx.tsconfig_membership_clear_generation_for_test(),
            tsconfig_clear_generation_after_first,
            "equivalent rebind must keep the tsconfig-membership cache hot"
        );
        assert_eq!(
            ctx.filter_registry_rebuild_count_for_test(),
            filter_rebuilds_after_first,
            "equivalent rebind must not rebuild the TOML filter registry"
        );
        assert_eq!(
            ctx.artifact_cache_key_derivation_count_for_test(),
            artifact_derivations_after_first,
            "equivalent rebind must reuse the artifact cache key"
        );
        // Load-bearing: in-flight build workers publish only while the
        // generation flag equals their spawn generation. If equivalent
        // rebinds advanced it, every rebind during a long build would
        // silently discard the build's result at completion.
        assert_eq!(ctx.configure_generation(), generation_after_first);

        // A genuinely different warm config must still advance (this is what
        // cancels superseded in-flight builds).
        let changed = configure_request_with_params(json!({
            "project_root": temp.path(),
            "harness": "opencode",
            "config": [user_tier(json!({
                "search_index": true,
                "semantic_search": false,
                "callgraph_store": false
            }))]
        }));
        let response = handle_configure_for_test(&changed, &ctx);
        assert!(response.success);
        super::drain_deferred_configure_maintenance(&ctx);
        assert_eq!(ctx.configure_generation(), generation_after_first + 1);
        assert!(ctx.config().search_index, "changed config must apply fully");
        assert_eq!(
            ctx.tsconfig_membership_clear_generation_for_test(),
            tsconfig_clear_generation_after_first + 1
        );
        assert_eq!(
            ctx.filter_registry_rebuild_count_for_test(),
            filter_rebuilds_after_first + 1
        );
    }

    #[test]
    fn equivalent_reconfigure_replays_new_sessions_but_not_same_session_rebinds() {
        let _env_guard = home_env_mutex();
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _disable_watcher = EnvVarGuard::set("AFT_TEST_DISABLE_FILE_WATCHER", "1");
        super::reset_configure_replay_session_calls_for_test();
        let temp = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        init_git_fixture(temp.path());
        let ctx = test_context();
        let params = json!({
            "project_root": temp.path(),
            "harness": "opencode",
            "storage_dir": storage.path(),
            "config": [user_tier(json!({
                "search_index": false,
                "semantic_search": false,
                "callgraph_store": false
            }))]
        });

        let session_a = configure_request_with_session(params.clone(), "session-a");
        let response = handle_configure_for_test(&session_a, &ctx);
        assert!(response.success);
        super::drain_deferred_configure_maintenance(&ctx);
        assert_eq!(super::configure_replay_session_calls_for_test(), 1);
        assert_eq!(ctx.backup().lock().disk_io_count_for_tests(), 0);

        let response = handle_configure_for_test(&session_a, &ctx);
        assert!(response.success);
        super::drain_deferred_configure_maintenance(&ctx);
        assert_eq!(
            super::configure_replay_session_calls_for_test(),
            1,
            "equivalent rebind for an already-bound session should skip replay"
        );

        let session_b = configure_request_with_session(params, "session-b");
        let response = handle_configure_for_test(&session_b, &ctx);
        assert!(response.success);
        super::drain_deferred_configure_maintenance(&ctx);
        assert_eq!(
            super::configure_replay_session_calls_for_test(),
            2,
            "a new session on an equivalent warm root still needs session replay"
        );
        assert_eq!(
            ctx.backup().lock().disk_io_count_for_tests(),
            0,
            "a fresh-session bind must not inspect backup directories"
        );
    }

    #[test]
    fn dead_artifact_owner_manifest_is_taken_over_on_configure() {
        let _artifact_guard = artifact_owner_test_mutex().lock().unwrap();
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("storage");
        let owner = temp.path().join("owner");
        init_git_fixture(&owner);
        let sibling = temp.path().join("sibling");
        let mut clone_command = Command::new("git");
        assert!(crate::test_env::apply_hermetic_git_env(&mut clone_command)
            .args(["clone", "--quiet"])
            .arg(&owner)
            .arg(&sibling)
            .status()
            .unwrap()
            .success());
        let key = crate::search_index::artifact_cache_key(&owner);
        crate::artifact_owner::write_synthetic_manifest_for_test(
            &storage,
            &owner,
            &key,
            "dead-owner",
            0,
            0,
        );

        let sibling_ctx = test_context();
        let sibling_response =
            handle_configure_for_test(&configure_with_storage(&sibling, &storage), &sibling_ctx);

        assert!(sibling_response.success);
        assert_eq!(sibling_ctx.cache_role(), "main");
        assert_eq!(
            sibling_response.data["artifact_owner"]["mode"],
            json!("owner")
        );
    }

    #[test]
    fn semantic_file_cap_counts_only_semantic_extensions() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("src")).unwrap();
        std::fs::write(temp.path().join("src/lib.rs"), "pub fn one() {}\n").unwrap();
        for index in 0..5 {
            std::fs::write(
                temp.path().join(format!("asset-{index}.bin")),
                format!("asset {index}"),
            )
            .unwrap();
        }

        let files = super::walk_semantic_project_files_bounded(temp.path(), 1)
            .expect("one semantic file should be within cap");
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("src/lib.rs"));

        std::fs::write(temp.path().join("src/second.rs"), "pub fn two() {}\n").unwrap();
        assert!(super::walk_semantic_project_files_bounded(temp.path(), 1).is_err());
    }

    #[test]
    fn configure_missing_tools_warns_for_explicit_oxfmt_formatter() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = Config {
            project_root: Some(temp.path().to_path_buf()),
            ..Config::default()
        };
        config
            .formatter
            .insert("typescript".to_string(), "oxfmt".to_string());
        let candidates = super::formatter_candidates(crate::parser::LangId::TypeScript, &config);
        assert_eq!(candidates.len(), 1);
        let mut tool_cache = std::collections::HashMap::from([("oxfmt".to_string(), false)]);
        let warning = super::missing_tool_warning(
            "formatter_not_installed",
            "typescript",
            &candidates[0],
            config.project_root.as_deref(),
            &mut tool_cache,
        )
        .expect("expected missing oxfmt warning");

        assert_eq!(warning.kind, "formatter_not_installed");
        assert_eq!(warning.language, "typescript");
        assert_eq!(warning.tool, "oxfmt");
    }

    #[test]
    fn detect_missing_tools_skips_formatters_when_format_on_edit_disabled() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("biome.json"), "{}\n").unwrap();
        let config = Config {
            project_root: Some(temp.path().to_path_buf()),
            format_on_edit: false,
            ..Config::default()
        };
        let languages = std::collections::HashSet::from([crate::parser::LangId::TypeScript]);
        let warnings = super::detect_missing_tools_for_languages(&languages, &config);
        assert!(
            warnings.is_empty(),
            "format_on_edit:false should suppress derived formatter warnings: {warnings:?}"
        );
    }

    #[test]
    fn detect_missing_tools_still_warns_explicit_formatter_when_format_on_edit_disabled() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = Config {
            project_root: Some(temp.path().to_path_buf()),
            format_on_edit: false,
            ..Config::default()
        };
        config
            .formatter
            .insert("typescript".to_string(), "biome".to_string());
        let languages = std::collections::HashSet::from([crate::parser::LangId::TypeScript]);
        let warnings = super::detect_missing_tools_for_languages(&languages, &config);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].tool, "biome");
    }

    #[test]
    fn configure_missing_tools_warns_for_oxfmt_project_config() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join(".oxfmtrc.json"), "{}\n").unwrap();
        let config = Config {
            project_root: Some(temp.path().to_path_buf()),
            ..Config::default()
        };

        let candidates = super::formatter_candidates(crate::parser::LangId::TypeScript, &config);
        assert_eq!(candidates.len(), 1);
        let mut tool_cache = std::collections::HashMap::from([("oxfmt".to_string(), false)]);
        let warning = super::missing_tool_warning(
            "formatter_not_installed",
            "typescript",
            &candidates[0],
            config.project_root.as_deref(),
            &mut tool_cache,
        )
        .expect("expected missing oxfmt warning");

        assert_eq!(warning.kind, "formatter_not_installed");
        assert_eq!(warning.language, "typescript");
        assert_eq!(warning.tool, "oxfmt");
    }

    #[cfg(unix)]
    #[test]
    fn configure_missing_tools_uses_shared_go_tool_resolution() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("go.mod"), "module example.test\ngo 1.21\n").unwrap();
        let bin_dir = temp.path().join("node_modules/.bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        use std::os::unix::fs::PermissionsExt;

        let go = bin_dir.join("go");
        std::fs::write(
            &go,
            "#!/bin/sh\nif [ \"$1\" = \"version\" ]; then exit 0; fi\nif [ \"$1\" = \"--version\" ]; then exit 2; fi\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&go, std::fs::Permissions::from_mode(0o755)).unwrap();

        let gofmt = bin_dir.join("gofmt");
        std::fs::write(
            &gofmt,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then exit 2; fi\ncat >/dev/null\nexit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(&gofmt, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut languages = std::collections::HashSet::new();
        languages.insert(crate::parser::LangId::Go);
        let config = Config {
            project_root: Some(temp.path().to_path_buf()),
            ..Config::default()
        };
        let warnings = super::detect_missing_tools_for_languages(&languages, &config);

        assert!(
            warnings.is_empty(),
            "expected shared Go resolver to avoid false missing-tool warnings, got {warnings:?}"
        );
    }

    /// Serialize the home-root tests below on the process-wide env lock.
    /// They mutate process-global `HOME` / `USERPROFILE` env vars, and `cargo
    /// test` runs unit tests concurrently within the same process — without
    /// serialization a parallel `set_var("HOME", X)` in another env-mutating
    /// test (e.g. the gitignore-neutralizing tests in `context.rs`) can race
    /// `resolve_home_dir()` here and produce flaky failures. A module-local
    /// mutex is not enough: the lock must be shared with every other test
    /// that touches these variables.
    fn home_env_mutex() -> crate::test_env::ProcessEnvLockGuard {
        crate::test_env::process_env_lock()
    }

    /// Shared mutex serializing the watcher tests below. They install watcher
    /// runtimes on an `AppContext`, and each test must stop its runtime before
    /// the next one starts.
    fn watcher_test_mutex() -> &'static std::sync::Mutex<()> {
        static M: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        M.get_or_init(|| std::sync::Mutex::new(()))
    }

    fn artifact_owner_test_mutex() -> &'static std::sync::Mutex<()> {
        static M: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        M.get_or_init(|| std::sync::Mutex::new(()))
    }

    #[test]
    fn handle_configure_enters_degraded_mode_when_project_root_is_home() {
        let _guard = home_env_mutex();
        // Simulate the Desktop-launches-from-`~` case by pointing `HOME` at a
        // tempdir and using that same tempdir as `project_root`. The
        // canonical-equality check inside `handle_configure` is the same
        // mechanism that catches real `$HOME` regardless of HOME mutation.
        let temp = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(temp.path()).unwrap();

        // Save + restore HOME so we don't pollute other tests in the
        // same process (Rust runs tests in parallel by default but env
        // mutation is process-global). For Windows, USERPROFILE is the
        // var `resolve_home_dir` checks after HOME.
        let prev_home = std::env::var_os("HOME");
        let prev_userprofile = std::env::var_os("USERPROFILE");
        // SAFETY: env mutation is sound here — this is single-threaded test
        // setup. The matching restore at the bottom runs even on assertion
        // failure because Rust's panic path executes drop order, but we make
        // the env writes explicit for clarity.
        unsafe {
            std::env::set_var("HOME", &canonical);
            std::env::set_var("USERPROFILE", &canonical);
        }

        let ctx = test_context();
        // Supply search_index/semantic_search as a user config TIER (how real
        // configures carry core fields under reset resolution). The point of this
        // test is that the HOME-root degraded gate force-disables them EVEN WHEN
        // user config enables them — so they must actually be enabled by the tier
        // first, then asserted off below.
        let req = configure_request_with_params(json!({
            "project_root": temp.path(),
            "harness": "opencode",
            "config": [user_tier(json!({ "search_index": true, "semantic_search": true }))],
        }));
        let response = handle_configure_for_test(&req, &ctx);

        // Restore env immediately so a later assertion failure doesn't leak.
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match prev_userprofile {
                Some(v) => std::env::set_var("USERPROFILE", v),
                None => std::env::remove_var("USERPROFILE"),
            }
        }
        drop(_guard);

        assert!(response.success);
        assert!(ctx.is_degraded(), "expected degraded mode for HOME root");
        assert!(
            !ctx.heavy_root_work_allowed(),
            "HOME root configure must close the heavy-root-work gate"
        );
        assert!(
            ctx.degraded_reasons().contains(&"home_root".to_string()),
            "expected `home_root` reason, got {:?}",
            ctx.degraded_reasons()
        );
        // Heavy subsystems must have been force-disabled regardless of user config.
        assert!(
            !ctx.config().search_index,
            "search_index must be auto-disabled at HOME root"
        );
        assert!(
            !ctx.config().semantic_search,
            "semantic_search must be auto-disabled at HOME root"
        );
    }

    #[test]
    fn handle_configure_stays_full_featured_for_subdirectory_of_home() {
        let _guard = home_env_mutex();
        // A real subdirectory of `$HOME` (the legitimate case: most projects
        // live under `~/Work`, `~/Documents`, etc.) must NOT trip the
        // degraded gate. We point HOME at a tempdir and configure against a
        // nested subdir to confirm subdirs pass through.
        let temp = tempfile::tempdir().unwrap();
        let subdir = temp.path().join("project");
        std::fs::create_dir(&subdir).unwrap();
        let canonical_home = std::fs::canonicalize(temp.path()).unwrap();

        let prev_home = std::env::var_os("HOME");
        let prev_userprofile = std::env::var_os("USERPROFILE");
        unsafe {
            std::env::set_var("HOME", &canonical_home);
            std::env::set_var("USERPROFILE", &canonical_home);
        }

        let ctx = test_context();
        // search_index is a core-domain field that arrives via a config TIER on
        // every real configure (P1 relocation). With reset-onto-default resolution
        // it must be supplied as a tier, not seeded via update_config — a tier-less
        // configure correctly resets core fields to default.
        let req = configure_request_with_params(json!({
            "project_root": subdir,
            "harness": "opencode",
            "storage_dir": temp.path().join("storage"),
            "config": [user_tier(json!({
                "search_index": true,
                "semantic_search": false,
                "callgraph_store": false,
            }))],
        }));
        let response = handle_configure_for_test(&req, &ctx);

        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match prev_userprofile {
                Some(v) => std::env::set_var("USERPROFILE", v),
                None => std::env::remove_var("USERPROFILE"),
            }
        }
        drop(_guard);

        assert!(response.success);
        assert!(
            !ctx.is_degraded(),
            "subdirectories of $HOME must not enter degraded mode"
        );
        assert!(
            ctx.heavy_root_work_allowed(),
            "subdirectories of $HOME must keep heavy root work enabled"
        );
        assert!(
            ctx.degraded_reasons().is_empty(),
            "expected no degraded reasons, got {:?}",
            ctx.degraded_reasons()
        );
        // User config preserved.
        assert!(ctx.config().search_index);
    }

    #[cfg(unix)]
    fn create_dir_symlink(src: &std::path::Path, dst: &std::path::Path) {
        std::os::unix::fs::symlink(src, dst).unwrap();
    }

    #[cfg(windows)]
    fn create_dir_symlink(src: &std::path::Path, dst: &std::path::Path) {
        std::os::windows::fs::symlink_dir(src, dst).unwrap();
    }

    #[cfg(unix)]
    fn create_file_symlink(src: &std::path::Path, dst: &std::path::Path) {
        std::os::unix::fs::symlink(src, dst).unwrap();
    }

    #[cfg(windows)]
    fn create_file_symlink(src: &std::path::Path, dst: &std::path::Path) {
        std::os::windows::fs::symlink_file(src, dst).unwrap();
    }

    #[test]
    fn validate_storage_dir_requires_absolute_paths() {
        assert!(validate_storage_dir("relative/cache").is_err());
    }

    #[test]
    fn validate_storage_dir_normalizes_safe_parents() {
        let base = std::env::temp_dir();
        let path = base.join("aft-config-test").join("..").join("cache");
        assert_eq!(
            validate_storage_dir(path.to_str().unwrap()).unwrap(),
            base.join("cache")
        );
    }

    #[test]
    fn validate_storage_dir_rejects_relative_with_dotdot() {
        // Relative paths with .. are rejected (not absolute)
        assert!(validate_storage_dir("../../../etc/passwd").is_err());
    }

    // Unix-only: on Windows, `\..\..\cache` isn't an absolute path (no
    // drive letter), so the dotdot-normalization-of-absolute-path
    // semantics this test asserts don't apply.
    #[cfg(unix)]
    #[test]
    fn validate_storage_dir_accepts_absolute_with_dotdot_that_normalizes() {
        // /../../cache normalizes to /cache which is a valid absolute path
        let mut path = PathBuf::from(std::path::MAIN_SEPARATOR.to_string());
        path.push("..");
        path.push("..");
        path.push("cache");
        assert!(validate_storage_dir(path.to_str().unwrap()).is_ok());
    }

    #[test]
    fn parse_lsp_paths_extra_accepts_existing_directory_after_canonicalize() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("cache").join("node_modules").join(".bin");
        std::fs::create_dir_all(&dir).unwrap();

        let paths = parse_lsp_paths_extra(&json!([dir])).unwrap();

        assert_eq!(paths, vec![std::fs::canonicalize(&dir).unwrap()]);
    }

    #[test]
    fn parse_lsp_paths_extra_accepts_nonexistent_directory_for_later_install() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("pending").join("node_modules").join(".bin");

        let paths = parse_lsp_paths_extra(&json!([missing])).unwrap();

        assert_eq!(paths, vec![missing]);
    }

    #[test]
    fn parse_lsp_paths_extra_rejects_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("not-a-dir");
        std::fs::write(&file, "not a directory").unwrap();

        let error = parse_lsp_paths_extra(&json!([file])).unwrap_err();

        assert!(error.contains("must resolve to a directory"));
    }

    #[test]
    fn parse_lsp_paths_extra_rejects_parent_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let traversing = tmp.path().join("project").join("..").join("outside");

        let error = parse_lsp_paths_extra(&json!([traversing])).unwrap_err();

        assert!(error.contains("must not contain '..' traversal"));
    }

    #[test]
    fn parse_lsp_paths_extra_accepts_symlink_to_directory_as_target() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target-dir");
        let link = tmp.path().join("linked-dir");
        std::fs::create_dir_all(&target).unwrap();
        create_dir_symlink(&target, &link);

        let paths = parse_lsp_paths_extra(&json!([link])).unwrap();

        assert_eq!(paths, vec![std::fs::canonicalize(&target).unwrap()]);
    }

    #[test]
    fn parse_lsp_paths_extra_rejects_symlink_to_file() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target-file");
        let link = tmp.path().join("linked-file");
        std::fs::write(&target, "not a directory").unwrap();
        create_file_symlink(&target, &link);

        let error = parse_lsp_paths_extra(&json!([link])).unwrap_err();

        assert!(error.contains("must resolve to a directory"));
    }

    #[test]
    fn watcher_attach_runs_off_configure_foreground_when_slow() {
        let _guard = watcher_test_mutex()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let root = tempfile::tempdir().unwrap();
        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        let attach_started = Arc::new(Barrier::new(2));
        let attach_started_for_thread = Arc::clone(&attach_started);

        let started = Instant::now();
        install_project_watcher_with(
            &ctx,
            root.path(),
            Vec::new(),
            move |_root, _extra_watch_paths, _tx| {
                attach_started_for_thread.wait();
                std::thread::sleep(Duration::from_millis(250));
                Ok::<(), &'static str>(())
            },
        );

        assert!(
            started.elapsed() < Duration::from_millis(100),
            "watcher installation should not wait for slow attach"
        );
        assert!(ctx.watcher_rx().lock().is_some());
        assert!(ctx.watcher().lock().is_none());

        attach_started.wait();
        ctx.stop_watcher_runtime();
    }

    #[test]
    fn watcher_attach_failure_reports_error_on_receiver() {
        let _guard = watcher_test_mutex()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let root = tempfile::tempdir().unwrap();
        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());

        install_project_watcher_with(
            &ctx,
            root.path(),
            Vec::new(),
            |_root, _extra_watch_paths, _tx| Err::<(), _>("no watcher backend"),
        );

        let event = ctx
            .watcher_rx()
            .lock()
            .as_ref()
            .expect("watcher receiver installed")
            .recv_timeout(Duration::from_secs(2))
            .expect("watcher error event");
        match event {
            crate::watcher_filter::WatcherDispatchEvent::Error(error) => {
                assert!(error.contains("no watcher backend"));
            }
            other => panic!("unexpected watcher event: {other:?}"),
        }
        ctx.stop_watcher_runtime();
    }

    #[test]
    fn watcher_reconfigure_does_not_leak_filter_threads() {
        let _guard = watcher_test_mutex()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        struct FakeWatcher {
            _tx: mpsc::Sender<notify::Result<notify::Event>>,
            drops: Arc<std::sync::atomic::AtomicUsize>,
        }

        impl Drop for FakeWatcher {
            fn drop(&mut self) {
                self.drops.fetch_add(1, Ordering::SeqCst);
            }
        }

        // Watcher teardown happens on a detached reaper thread, so drops land
        // asynchronously; the count must still reach the exact expected value.
        fn wait_for_drop_count(
            drops: &Arc<std::sync::atomic::AtomicUsize>,
            expected: usize,
            what: &str,
        ) {
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            loop {
                let observed = drops.load(Ordering::SeqCst);
                if observed == expected {
                    return;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "{what}: expected={expected}, observed={observed}"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        let root1 = tempfile::tempdir().unwrap();
        let root2 = tempfile::tempdir().unwrap();
        let root3 = tempfile::tempdir().unwrap();
        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let drops_for_watcher = Arc::clone(&drops);
        install_project_watcher_with(
            &ctx,
            root1.path(),
            Vec::new(),
            move |_root, _extra_watch_paths, tx| {
                Ok::<_, &'static str>(FakeWatcher {
                    _tx: tx,
                    drops: drops_for_watcher,
                })
            },
        );
        assert_eq!(drops.load(Ordering::SeqCst), 0);

        let drops_for_watcher = Arc::clone(&drops);
        install_project_watcher_with(
            &ctx,
            root2.path(),
            Vec::new(),
            move |_root, _extra_watch_paths, tx| {
                Ok::<_, &'static str>(FakeWatcher {
                    _tx: tx,
                    drops: drops_for_watcher,
                })
            },
        );
        wait_for_drop_count(&drops, 1, "first watcher should be dropped on reconfigure");

        let drops_for_watcher = Arc::clone(&drops);
        install_project_watcher_with(
            &ctx,
            root3.path(),
            Vec::new(),
            move |_root, _extra_watch_paths, tx| {
                Ok::<_, &'static str>(FakeWatcher {
                    _tx: tx,
                    drops: drops_for_watcher,
                })
            },
        );
        wait_for_drop_count(&drops, 2, "second watcher should be dropped on reconfigure");

        ctx.stop_watcher_runtime();
        wait_for_drop_count(
            &drops,
            3,
            "final watcher should be dropped on explicit shutdown",
        );
    }

    #[test]
    fn external_ignore_watch_paths_includes_git_common_info_exclude() {
        let root = tempfile::tempdir().unwrap();
        let common = tempfile::tempdir().unwrap();
        let info = common.path().join("info");
        std::fs::create_dir_all(&info).unwrap();
        let exclude = info.join("exclude");
        std::fs::write(
            &exclude,
            "ignored/
",
        )
        .unwrap();

        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        ctx.set_cache_role(false, Some(common.path().to_path_buf()));

        let paths = external_ignore_watch_paths(&ctx, root.path());

        assert!(paths.contains(&exclude));
    }

    #[test]
    fn invalid_late_configure_field_does_not_mutate_existing_context() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let ctx = test_context();
        let first_req = configure_request_with_params(json!({
            "project_root": first.path(),
            "harness": "opencode",
            "config": [user_tier(json!({ "format_on_edit": true }))]
        }));
        let first_response = handle_configure_for_test(&first_req, &ctx);
        assert!(first_response.success);
        let canonical_before = ctx.canonical_cache_root();

        let invalid_req = configure_request_with_params(json!({
            "project_root": second.path(),
            "harness": "pi",
            "max_background_bash_tasks": 0
        }));
        let invalid_response = handle_configure_for_test(&invalid_req, &ctx);

        assert!(!invalid_response.success);
        assert_eq!(invalid_response.data["code"], "invalid_request");
        assert_eq!(ctx.harness_opt(), Some(crate::harness::Harness::Opencode));
        assert_eq!(ctx.canonical_cache_root(), canonical_before);
        let config = ctx.config();
        assert_eq!(config.project_root.as_deref(), Some(first.path()));
        assert_eq!(config.harness, Some(crate::harness::Harness::Opencode));
        assert!(config.format_on_edit);
    }

    #[test]
    fn configure_replaces_formatter_and_checker_maps_when_present() {
        let root = tempfile::tempdir().unwrap();
        let ctx = test_context();
        let first_req = configure_request_with_params(json!({
            "project_root": root.path(),
            "harness": "opencode",
            "config": [user_tier(json!({
                "formatter": { "typescript": "biome", "python": "ruff" },
                "checker": { "typescript": "tsc" }
            }))]
        }));
        assert!(handle_configure_for_test(&first_req, &ctx).success);

        let second_req = configure_request_with_params(json!({
            "project_root": root.path(),
            "harness": "opencode",
            "config": [user_tier(json!({
                "formatter": { "rust": "rustfmt" },
                "checker": { "go": "go" }
            }))]
        }));
        assert!(handle_configure_for_test(&second_req, &ctx).success);

        let config = ctx.config();
        assert_eq!(
            config.formatter.get("rust").map(String::as_str),
            Some("rustfmt")
        );
        assert!(!config.formatter.contains_key("typescript"));
        assert!(!config.formatter.contains_key("python"));
        assert_eq!(config.checker.get("go").map(String::as_str), Some("go"));
        assert!(!config.checker.contains_key("typescript"));
    }

    #[test]
    fn configure_rejects_invalid_process_state_without_mutation() {
        let root = tempfile::tempdir().unwrap();
        let ctx = test_context();
        let req = configure_request_with_params(json!({
            "project_root": root.path(),
            "harness": "opencode",
            "max_background_bash_tasks": 0
        }));

        let response = handle_configure_for_test(&req, &ctx);

        assert!(!response.success);
        assert_eq!(response.data["code"], "invalid_request");
        assert!(ctx.config().project_root.is_none());
        assert!(ctx.harness_opt().is_none());
    }

    #[test]
    fn configure_generation_advances_only_after_successful_configure() {
        let root = tempfile::tempdir().unwrap();
        let ctx = test_context();
        let invalid_req = configure_request_with_params(json!({
            "project_root": root.path(),
            "harness": "opencode",
            "max_background_bash_tasks": 0
        }));
        assert!(!handle_configure_for_test(&invalid_req, &ctx).success);
        assert_eq!(ctx.configure_generation(), 0);

        let valid_req = configure_request(json!(root.path()));
        assert!(handle_configure_for_test(&valid_req, &ctx).success);
        assert_eq!(ctx.configure_generation(), 1);
    }

    #[test]
    fn semantic_max_files_defaults_to_20k() {
        assert_eq!(SemanticBackendConfig::default().max_files, 20_000);
    }

    #[test]
    fn lsp_paths_extra_change_clears_failed_spawns_for_retry() {
        let previous = Config::default();
        let mut next = previous.clone();
        next.lsp_paths_extra.push(PathBuf::from("/cache/lsp/.bin"));

        assert!(should_clear_failed_spawns(&previous, &next, true));
        assert!(!should_clear_failed_spawns(&previous, &previous, true));
    }

    /// Manual bind-path benchmark with a 5,120-source-file, 64-package repository.
    /// The legacy samples explicitly replay the manifest scans removed from the
    /// handler, keeping before/after timings paired in the same process.
    /// Run with:
    /// `cargo test --release -p agent-file-tools configure_bind_path_measurement -- --ignored --nocapture`
    #[test]
    #[ignore = "manual configure bind-path measurement"]
    fn configure_bind_path_measurement() {
        const RUNS: usize = 21;
        const PACKAGES: usize = 64;
        const FILES_PER_PACKAGE: usize = 80;

        fn median(samples: &mut [Duration]) -> Duration {
            samples.sort_unstable();
            samples[samples.len() / 2]
        }

        let _env_guard = home_env_mutex();
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _disable_watcher = EnvVarGuard::set("AFT_TEST_DISABLE_FILE_WATCHER", "1");
        let fixture = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        init_git_fixture(fixture.path());
        std::fs::create_dir_all(fixture.path().join(".cortexkit")).unwrap();
        std::fs::write(
            fixture.path().join(".cortexkit/aft.jsonc"),
            r#"{
                // A representative project config is reparsed by every bind.
                "format_on_edit": false,
                "validate_on_edit": "syntax",
                "formatter": {"typescript": "biome", "rust": "rustfmt"},
                "checker": {"typescript": "biome", "rust": "cargo"},
                "search_index": false,
                "semantic_search": false,
                "callgraph_store": false,
                "inspect": {
                    "enabled": false,
                    "duplicates": {
                        "expected_mirrors": [
                            ["packages/opencode-plugin/**", "packages/pi-plugin/**"],
                            ["packages/npm/darwin-*/**", "packages/npm/linux-*/**"]
                        ]
                    }
                },
                "backup": {"enabled": true, "max_depth": 20, "max_file_size": 1048576}
            }"#,
        )
        .unwrap();
        for package in 0..PACKAGES {
            let package_root = fixture
                .path()
                .join("packages")
                .join(format!("pkg-{package:02}"));
            let source_root = package_root.join("src");
            std::fs::create_dir_all(&source_root).unwrap();
            std::fs::write(
                package_root.join("package.json"),
                format!(r#"{{"name":"@fixture/pkg-{package:02}","version":"1.0.0"}}"#),
            )
            .unwrap();
            for file in 0..FILES_PER_PACKAGE {
                std::fs::write(
                    source_root.join(format!("module-{file:03}.ts")),
                    format!("export const value{file} = {file};\n"),
                )
                .unwrap();
            }
        }

        let params = json!({
            "project_root": fixture.path(),
            "harness": "opencode",
            "storage_dir": storage.path(),
        });
        let measure_cold_bind = |session: &str, emulate_legacy_scans: bool| {
            let ctx = test_context();
            let req = configure_request_with_session(params.clone(), session);
            let started = Instant::now();
            if emulate_legacy_scans {
                std::hint::black_box(super::workspace_manifest_fingerprint(fixture.path()));
                std::hint::black_box(super::workspace_manifest_fingerprint(fixture.path()));
            }
            let response = handle_configure_for_test(&req, &ctx);
            let pre_ack = started.elapsed();
            assert!(response.success, "cold configure failed: {response:?}");
            ctx.mark_subc_bound();
            let started = Instant::now();
            super::drain_deferred_configure_maintenance(&ctx);
            (pre_ack, started.elapsed())
        };
        let mut legacy_cold_pre_ack = Vec::with_capacity(RUNS);
        let mut legacy_cold_post_ack = Vec::with_capacity(RUNS);
        let mut optimized_cold_pre_ack = Vec::with_capacity(RUNS);
        let mut optimized_cold_post_ack = Vec::with_capacity(RUNS);
        for run in 0..RUNS {
            let legacy_session = format!("legacy-cold-{run}");
            let optimized_session = format!("optimized-cold-{run}");
            let (first_session, first_legacy, second_session, second_legacy) = if run % 2 == 0 {
                (&optimized_session, false, &legacy_session, true)
            } else {
                (&legacy_session, true, &optimized_session, false)
            };
            for (session, legacy) in [
                (first_session.as_str(), first_legacy),
                (second_session.as_str(), second_legacy),
            ] {
                let (pre_ack, post_ack) = measure_cold_bind(session, legacy);
                if legacy {
                    legacy_cold_pre_ack.push(pre_ack);
                    legacy_cold_post_ack.push(post_ack);
                } else {
                    optimized_cold_pre_ack.push(pre_ack);
                    optimized_cold_post_ack.push(post_ack);
                }
            }
        }

        let ctx = test_context();
        let initial = configure_request_with_session(params.clone(), "warm-initial");
        assert!(handle_configure_for_test(&initial, &ctx).success);
        ctx.mark_subc_bound();
        super::drain_deferred_configure_maintenance(&ctx);
        let measure_warm_bind = |req: &RawRequest, emulate_legacy_scan: bool| {
            let started = Instant::now();
            if emulate_legacy_scan {
                std::hint::black_box(super::workspace_manifest_fingerprint(fixture.path()));
            }
            let response = handle_configure_for_test(req, &ctx);
            let pre_ack = started.elapsed();
            assert!(response.success, "warm configure failed: {response:?}");
            let started = Instant::now();
            super::drain_deferred_configure_maintenance(&ctx);
            (pre_ack, started.elapsed())
        };
        let mut legacy_warm_pre_ack = Vec::with_capacity(RUNS);
        let mut legacy_warm_post_ack = Vec::with_capacity(RUNS);
        let mut optimized_warm_pre_ack = Vec::with_capacity(RUNS);
        let mut optimized_warm_post_ack = Vec::with_capacity(RUNS);
        for run in 0..RUNS {
            let legacy =
                configure_request_with_session(params.clone(), &format!("legacy-warm-{run}"));
            let optimized =
                configure_request_with_session(params.clone(), &format!("optimized-warm-{run}"));
            let (first, first_legacy, second, second_legacy) = if run % 2 == 0 {
                (&optimized, false, &legacy, true)
            } else {
                (&legacy, true, &optimized, false)
            };
            for (req, legacy) in [(first, first_legacy), (second, second_legacy)] {
                let (pre_ack, post_ack) = measure_warm_bind(req, legacy);
                if legacy {
                    legacy_warm_pre_ack.push(pre_ack);
                    legacy_warm_post_ack.push(post_ack);
                } else {
                    optimized_warm_pre_ack.push(pre_ack);
                    optimized_warm_post_ack.push(post_ack);
                }
            }
        }

        eprintln!(
            "configure bind path: files={} packages={PACKAGES} runs={RUNS} legacy_cold_pre_ack_us={} optimized_cold_pre_ack_us={} legacy_cold_post_ack_us={} optimized_cold_post_ack_us={} legacy_warm_pre_ack_us={} optimized_warm_pre_ack_us={} legacy_warm_post_ack_us={} optimized_warm_post_ack_us={}",
            PACKAGES * FILES_PER_PACKAGE,
            median(&mut legacy_cold_pre_ack).as_micros(),
            median(&mut optimized_cold_pre_ack).as_micros(),
            median(&mut legacy_cold_post_ack).as_micros(),
            median(&mut optimized_cold_post_ack).as_micros(),
            median(&mut legacy_warm_pre_ack).as_micros(),
            median(&mut optimized_warm_pre_ack).as_micros(),
            median(&mut legacy_warm_post_ack).as_micros(),
            median(&mut optimized_warm_post_ack).as_micros(),
        );
    }
}
