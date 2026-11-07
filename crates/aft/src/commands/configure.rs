use std::fs;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
#[cfg(test)]
use std::sync::{Condvar, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::unbounded;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

use crate::cache_freshness::{self, VerifyArtifact, VerifyStrategy, WarmVerifyPlan};
use crate::config::{Config, SemanticBackendConfig};
use crate::context::{
    AppContext, CallgraphStoreAccess, ConfigureMaintenanceJob, SemanticBuildProgress,
    SemanticIndexEvent, SemanticIndexStatus, SemanticRefreshEvent, SemanticRefreshRequest,
    SemanticRefreshWorkerSlot, SubcLifecycleAdmission,
};
use crate::harness::Harness;
use crate::log_ctx;
use crate::lsp::registry::{
    resolve_lsp_binary, resolve_server_binary, servers_for_file_in_pass, BiomeSelectionMemo,
    ServerKind,
};
use crate::parser::{detect_language, LangId, SharedSymbolCache};
use crate::protocol::{RawRequest, Response};
use crate::search_index::{
    build_path_filters, current_git_head, resolve_cache_dir_with_key, CacheLock, SearchIndex,
};
use crate::semantic_index::{is_semantic_indexed_extension, SemanticIndex, SemanticIndexLock};
use crate::watcher_filter::{self, WatcherFilterConfig, WatcherThreadHandle};
use crate::{slog_debug, slog_info, slog_warn};

static WATCHER_GENERATION: AtomicU64 = AtomicU64::new(0);

static SEMANTIC_STALE_GENERATION_DISCARDS: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static SEMANTIC_STALE_GENERATION_SIGNAL: OnceLock<(Mutex<()>, Condvar)> = OnceLock::new();
#[cfg(test)]
static CONFIGURE_ARTIFACT_LOAD_CANCELLATIONS: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static CONFIGURE_DEFERRED_DELAY_REACHED: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static CONFIGURE_ARTIFACT_POST_GATE_REACHED: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static CONFIGURE_ARTIFACT_POST_GATE_DELAY_MS: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
static CONFIGURE_DEFERRED_MAINTENANCE_GATE: OnceLock<
    Mutex<Option<ConfigureDeferredMaintenanceGate>>,
> = OnceLock::new();
#[cfg(test)]
static CONFIGURE_SEMANTIC_SNAPSHOT_GATE: OnceLock<Mutex<Option<ConfigureSemanticSnapshotGate>>> =
    OnceLock::new();

#[cfg(test)]
#[derive(Clone)]
struct ConfigureDeferredMaintenanceGate {
    root: PathBuf,
    reached: crossbeam_channel::Sender<()>,
    release: crossbeam_channel::Receiver<()>,
}

#[cfg(test)]
pub(crate) struct ConfigureDeferredMaintenanceGateGuard {
    root: PathBuf,
}

#[cfg(test)]
impl Drop for ConfigureDeferredMaintenanceGateGuard {
    fn drop(&mut self) {
        let mut slot = CONFIGURE_DEFERRED_MAINTENANCE_GATE
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.as_ref().is_some_and(|gate| gate.root == self.root) {
            *slot = None;
        }
    }
}

#[cfg(test)]
pub(crate) fn gate_configure_deferred_maintenance_for_test(
    root: PathBuf,
) -> (
    ConfigureDeferredMaintenanceGateGuard,
    crossbeam_channel::Receiver<()>,
    crossbeam_channel::Sender<()>,
) {
    let (reached_tx, reached_rx) = crossbeam_channel::bounded(1);
    let (release_tx, release_rx) = crossbeam_channel::bounded(1);
    let gate = ConfigureDeferredMaintenanceGate {
        root: root.clone(),
        reached: reached_tx,
        release: release_rx,
    };
    let mut slot = CONFIGURE_DEFERRED_MAINTENANCE_GATE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(slot.is_none(), "configure gate already installed");
    *slot = Some(gate);
    (
        ConfigureDeferredMaintenanceGateGuard { root },
        reached_rx,
        release_tx,
    )
}

#[cfg(test)]
#[derive(Clone)]
struct ConfigureSemanticSnapshotGate {
    request_id: String,
    reached: crossbeam_channel::Sender<()>,
    release: crossbeam_channel::Receiver<()>,
}

#[cfg(test)]
struct ConfigureSemanticSnapshotGateGuard {
    request_id: String,
}

#[cfg(test)]
impl Drop for ConfigureSemanticSnapshotGateGuard {
    fn drop(&mut self) {
        let mut slot = CONFIGURE_SEMANTIC_SNAPSHOT_GATE
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot
            .as_ref()
            .is_some_and(|gate| gate.request_id == self.request_id)
        {
            *slot = None;
        }
    }
}

#[cfg(test)]
fn gate_configure_after_semantic_snapshot_for_test(
    request_id: String,
) -> (
    ConfigureSemanticSnapshotGateGuard,
    crossbeam_channel::Receiver<()>,
    crossbeam_channel::Sender<()>,
) {
    let (reached_tx, reached_rx) = crossbeam_channel::bounded(1);
    let (release_tx, release_rx) = crossbeam_channel::bounded(1);
    let gate = ConfigureSemanticSnapshotGate {
        request_id: request_id.clone(),
        reached: reached_tx,
        release: release_rx,
    };
    let mut slot = CONFIGURE_SEMANTIC_SNAPSHOT_GATE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(
        slot.is_none(),
        "configure semantic snapshot gate already installed"
    );
    *slot = Some(gate);
    (
        ConfigureSemanticSnapshotGateGuard { request_id },
        reached_rx,
        release_tx,
    )
}

#[cfg(test)]
fn wait_on_configure_semantic_snapshot_gate_for_test(request_id: &str) {
    let gate = {
        let slot = CONFIGURE_SEMANTIC_SNAPSHOT_GATE
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        slot.as_ref()
            .filter(|gate| gate.request_id == request_id)
            .cloned()
    };
    if let Some(gate) = gate {
        gate.reached
            .send(())
            .expect("signal configure semantic snapshot gate");
        gate.release
            .recv_timeout(Duration::from_secs(12))
            .expect("release configure semantic snapshot gate");
    }
}

#[cfg(not(test))]
fn wait_on_configure_semantic_snapshot_gate_for_test(_request_id: &str) {}

/// Parks a configure of one root at a fixed point, so a test can hold it
/// there. `ConfigPublished` is right after the new configuration is
/// published and before the cache root, artifact access and degraded reasons
/// follow it: the window in which a reader admitted beside the configure
/// would see a half-published root. `WriterAdmitted` is right after the
/// configure is admitted as the root's exclusive writer, before any change
/// and before the commit seal: where a bind that loses a race with route
/// teardown must still stop.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConfigureGateStage {
    WriterAdmitted,
    ConfigPublished,
}

#[cfg(test)]
struct ConfigureCommitGate {
    root: PathBuf,
    stage: ConfigureGateStage,
    reached: crossbeam_channel::Sender<()>,
    release: crossbeam_channel::Receiver<()>,
}

#[cfg(test)]
static CONFIGURE_COMMIT_GATE: std::sync::Mutex<Vec<ConfigureCommitGate>> =
    std::sync::Mutex::new(Vec::new());

/// Arm a one-shot gate at `ConfigPublished` for the next configure of `root`.
/// Returns a receiver that fires when that configure parks and a sender that
/// releases it. Gates for other roots, used by tests running in parallel, are
/// independent.
#[cfg(test)]
pub(crate) fn gate_configure_commit_for_test(
    root: PathBuf,
) -> (
    crossbeam_channel::Receiver<()>,
    crossbeam_channel::Sender<()>,
) {
    gate_configure_stage_for_test(root, ConfigureGateStage::ConfigPublished)
}

/// Arm a one-shot gate at `stage` for the next configure of `root` that
/// reaches it.
#[cfg(test)]
pub(crate) fn gate_configure_stage_for_test(
    root: PathBuf,
    stage: ConfigureGateStage,
) -> (
    crossbeam_channel::Receiver<()>,
    crossbeam_channel::Sender<()>,
) {
    let (reached_tx, reached_rx) = crossbeam_channel::bounded(1);
    let (release_tx, release_rx) = crossbeam_channel::bounded(1);
    CONFIGURE_COMMIT_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(ConfigureCommitGate {
            root,
            stage,
            reached: reached_tx,
            release: release_rx,
        });
    (reached_rx, release_tx)
}

#[cfg(test)]
fn wait_on_configure_gate_for_test(root: &Path, stage: ConfigureGateStage) {
    let gate = {
        let mut gates = CONFIGURE_COMMIT_GATE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        gates
            .iter()
            .position(|gate| gate.root == root && gate.stage == stage)
            .map(|index| gates.swap_remove(index))
    };
    if let Some(gate) = gate {
        let _ = gate.reached.send(());
        let _ = gate.release.recv_timeout(Duration::from_secs(60));
    }
}

#[cfg(test)]
fn wait_on_configure_commit_gate_for_test(root: &Path) {
    wait_on_configure_gate_for_test(root, ConfigureGateStage::ConfigPublished);
}

#[cfg(test)]
fn wait_on_configure_writer_gate_for_test(root: &Path) {
    wait_on_configure_gate_for_test(root, ConfigureGateStage::WriterAdmitted);
}

#[cfg(not(test))]
fn wait_on_configure_commit_gate_for_test(_root: &Path) {}

#[cfg(not(test))]
fn wait_on_configure_writer_gate_for_test(_root: &Path) {}

#[cfg(test)]
thread_local! {
    static SEMANTIC_REFRESH_RESTART_ATTEMPTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static SEMANTIC_REFRESH_RESTART_RESULT_OVERRIDE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
    static WORKSPACE_MANIFEST_FINGERPRINT_SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

// Attempts are also recorded per project root: the process-wide total is
// observed by every configure in a parallel libtest binary, so a test that
// asserts "no load started yet" must read its own root's count or a neighbour's
// background load races it.

// Keys are canonical so a test that configured `/var/...` matches the load
// recorded under `/private/var/...`.

#[cfg(test)]
type ReadOnlyQueryWarmupHook =
    Box<dyn FnOnce() -> Result<crate::semantic_index::EmbeddingModel, String> + Send>;

#[cfg(test)]
static READ_ONLY_QUERY_WARMUP_HOOKS: Mutex<Option<HashMap<PathBuf, ReadOnlyQueryWarmupHook>>> =
    Mutex::new(None);

// Test seam between the worker's start gate and its lifecycle recheck. A
// non-zero "delay" HOLDS the worker here until the test releases it (bounded so
// a test that forgets cannot hang the suite) rather than sleeping a fixed time:
// the tests using it change the lifecycle in that window, and on a loaded
// Windows runner a 500 ms sleep elapsed before the test thread got scheduled,
// so the worker rechecked a still-bound lifecycle and the test read one load
// attempt where it expected none (train 126).

#[cfg(test)]
fn reset_configure_deferred_delay_reached_for_test() {
    CONFIGURE_DEFERRED_DELAY_REACHED.store(0, Ordering::SeqCst);
}

#[cfg(test)]
fn configure_deferred_delay_reached_for_test() -> usize {
    CONFIGURE_DEFERRED_DELAY_REACHED.load(Ordering::SeqCst)
}

/// Non-zero arms the post-gate hold; zero releases it (and is what the tests'
/// drop guards call so a panicking test still frees the worker).
#[cfg(test)]
fn set_configure_artifact_post_gate_delay_for_test(delay_ms: u64) {
    if delay_ms > 0 {
        CONFIGURE_ARTIFACT_POST_GATE_REACHED.store(0, Ordering::SeqCst);
    }
    CONFIGURE_ARTIFACT_POST_GATE_DELAY_MS.store(delay_ms, Ordering::SeqCst);
}

#[cfg(test)]
fn release_configure_artifact_post_gate_for_test() {
    CONFIGURE_ARTIFACT_POST_GATE_DELAY_MS.store(0, Ordering::SeqCst);
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

// Quiet window before a semantic re-embed batch runs. Embedding costs ~1-2s
// per collect under load, and nobody semantically searches a file within
// seconds of editing it — a long window coalesces an agent's whole edit burst
// into one collect instead of one per save. Freshness is preserved where it
// matters: unembedded changed files are already excluded from results by the
// stale-file mask, and grep/read see changes instantly. A batch is admitted
// after at most four quiet windows even if requests keep arriving.

#[cfg(test)]
static INDEX_ORDER_GRACE_MS: AtomicU64 = AtomicU64::new(30_000);
#[cfg(test)]
static INDEX_ORDER_TIMEOUT_LOGS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
#[cfg(test)]
static INDEX_ORDER_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[derive(Clone)]
struct SemanticRefreshLimiter(Arc<crate::cold_build_limiter::ColdBuildLimiter>);

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

fn delay_configure_deferred_maintenance_for_test(_root: &Path) {
    #[cfg(test)]
    {
        CONFIGURE_DEFERRED_DELAY_REACHED.fetch_add(1, Ordering::SeqCst);
        let gate = CONFIGURE_DEFERRED_MAINTENANCE_GATE
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .filter(|gate| gate.root == _root)
            .cloned();
        if let Some(gate) = gate {
            let _ = gate.reached.send(());
            let _ = gate.release.recv();
        }
    }
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
    let raw = crate::environment::non_empty_os_var("HOME")
        .or_else(|| crate::environment::non_empty_os_var("USERPROFILE"))
        .map(PathBuf::from)?;
    Some(std::fs::canonicalize(&raw).unwrap_or(raw))
}

pub(crate) fn resolve_account_home_dir() -> Option<PathBuf> {
    #[cfg(unix)]
    let home = crate::production_storage::account_home().ok()?;
    #[cfg(windows)]
    let home = resolve_windows_profile_dir().ok()?;

    let canonical = std::fs::canonicalize(home).ok()?;
    canonical.is_dir().then_some(canonical)
}

pub(crate) fn project_root_is_too_broad(
    canonical_root: &Path,
    canonicalization_failed: bool,
    account_home: Option<&Path>,
) -> bool {
    let root_identity = crate::access_policy::policy_path(canonical_root);
    let account_home = account_home.filter(|home| home.is_dir());
    canonicalization_failed
        || crate::path_identity::project_root_volume(canonical_root, account_home)
            .map(|volume| crate::access_policy::policy_path(&volume))
            .is_none_or(|volume| {
                let home_identity = account_home
                    .map(crate::access_policy::policy_path)
                    .unwrap_or_else(|| volume.clone());
                crate::path_identity::project_root_is_narrow(
                    &root_identity,
                    &home_identity,
                    &volume,
                ) != Some(true)
            })
}

#[cfg(windows)]
fn resolve_windows_profile_dir() -> std::io::Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{
        FOLDERID_Profile, SHGetKnownFolderPath, KF_FLAG_DONT_VERIFY,
    };

    struct FolderPath(*mut u16);
    impl Drop for FolderPath {
        fn drop(&mut self) {
            // SAFETY: SHGetKnownFolderPath allocates its result with the COM allocator.
            unsafe { CoTaskMemFree(self.0.cast()) };
        }
    }

    let mut raw = std::ptr::null_mut();
    // SAFETY: the folder identifier is valid and raw is writable.
    let status = unsafe {
        SHGetKnownFolderPath(
            &FOLDERID_Profile,
            KF_FLAG_DONT_VERIFY as u32,
            std::ptr::null_mut(),
            &mut raw,
        )
    };
    let path = FolderPath(raw);
    if status < 0 || path.0.is_null() {
        return Err(std::io::Error::other(format!(
            "SHGetKnownFolderPath(FOLDERID_Profile) lookup failed (HRESULT 0x{:08x})",
            status as u32
        )));
    }
    let mut len = 0;
    // SAFETY: a successful lookup returns an allocated NUL-terminated path.
    while unsafe { *path.0.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: the scan above found the end of the allocated path.
    let units = unsafe { std::slice::from_raw_parts(path.0, len) };
    Ok(PathBuf::from(std::ffi::OsString::from_wide(units)))
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
    if let Ok(metadata) =
        crate::git_head::capture_git_head_metadata(root_path, ctx.git_common_dir().as_deref())
    {
        paths.extend(metadata.watch_paths().map(Path::to_path_buf));
        if let Some(git_common_dir) = ctx.git_common_dir() {
            let refs = git_common_dir.join("refs");
            if refs.is_dir() {
                paths.push(refs);
            }
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

/// Load a root's ignore rules if nothing has loaded them yet.
///
/// The OS backend derives its kernel exclusion set (`target/`, `node_modules/`,
/// and the other ecosystem directories) from the gitignore matcher when it is
/// created, and re-derives whenever the matcher generation moves. The
/// configure tail builds the matcher in its `ProjectRuntime` stage before its
/// `Watcher` stage, so a watcher it starts is always covered. A watcher can
/// also start without that tail: when a root is quiesced (its queued configure
/// maintenance cancelled before `ProjectRuntime` ran) and then rebound,
/// `ensure_project_watcher` restarts the watcher directly, and the next
/// configure sees an active watcher and skips the project-runtime refresh.
/// Nothing then ever builds the matcher, and the backend keeps the only plan
/// it can make without one: `.git` alone.
///
/// `ensure_project_watcher` runs on the subc frame loop, which must never walk
/// a project tree, so it does not call this. Its callers queue this as a
/// maintenance job on the root's actor instead (see
/// `ignore_rules_load_pending`); publishing the matcher bumps the generation
/// and the running backend re-derives from it.
///
/// Returns whether ignore rules are now published. A root that is unbound
/// again by the time this runs is left unloaded on purpose: its next rebind
/// restores the watcher and queues this again. Degraded roots (the home
/// directory among them) get the same explicit empty matcher the watcher drain
/// installs for them, because walking them for ignore files is too costly.
pub(crate) fn load_ignore_rules_if_unpublished(ctx: &AppContext) -> bool {
    if ctx.gitignore_published() {
        return true;
    }
    if ctx.subc_unbound_quiesced() {
        return false;
    }
    if ctx.heavy_root_work_allowed() {
        ctx.rebuild_gitignore();
    } else {
        ctx.clear_gitignore();
    }
    true
}

/// Whether a watcher is running for this root with ignore rules that were
/// never loaded, so the caller must queue [`load_ignore_rules_if_unpublished`]
/// off its own thread.
pub(crate) fn ignore_rules_load_pending(ctx: &AppContext) -> bool {
    ctx.watcher_runtime_active() && !ctx.gitignore_published()
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
    start_project_watcher_with_spawn(ctx, root_path, extra_watch_paths, attach, |builder, job| {
        builder.spawn(job)
    });
}

fn start_project_watcher_with_spawn<W, E, F, S>(
    ctx: &AppContext,
    root_path: &Path,
    extra_watch_paths: Vec<PathBuf>,
    attach: F,
    spawn: S,
) where
    W: Send + 'static,
    E: std::fmt::Display + Send + 'static,
    F: FnOnce(PathBuf, Vec<PathBuf>, mpsc::Sender<notify::Result<notify::Event>>) -> Result<W, E>
        + Send
        + 'static,
    S: FnOnce(thread::Builder, Box<dyn FnOnce() + Send>) -> std::io::Result<thread::JoinHandle<()>>,
{
    let generation = WATCHER_GENERATION
        .fetch_add(1, Ordering::SeqCst)
        .wrapping_add(1);
    let (dispatch_tx, dispatch_rx) = watcher_filter::watcher_dispatch_channel();
    let shutdown = Arc::new(AtomicBool::new(false));
    let thread_shutdown = Arc::clone(&shutdown);

    let root_path = root_path.to_path_buf();
    let watcher_counters = crate::context::watcher_counters_for_root(&root_path);
    let app = ctx.app();
    let db = app.db();
    watcher_filter::load_watcher_observations(&root_path, &watcher_counters, db.as_ref());
    let filter_config = WatcherFilterConfig::new(root_path.clone(), ctx.git_common_dir())
        .with_config_reload_signal(ctx.config_live().signal());
    let shared_gitignore = ctx.shared_gitignore();
    let gitignore_generation = ctx.gitignore_generation();
    let session_id_for_bg = log_ctx::current_session();
    let sync_start = file_watcher_sync_start_for_test();
    let (start_tx, start_rx) = mpsc::channel::<Result<(), String>>();
    let start_tx = sync_start.then_some(start_tx);

    let join = spawn(
        thread::Builder::new().name("aft-watcher-filter".to_string()),
        Box::new(move || {
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
        }),
    );
    let join = match join {
        Ok(join) => join,
        Err(error) => {
            slog_warn!(
                "watcher filter thread could not start for generation {generation}: {error}"
            );
            return;
        }
    };

    let watcher_thread_id = join.thread().id();
    ctx.install_watcher_runtime_with_thread_id(
        dispatch_rx,
        WatcherThreadHandle::new(shutdown, join),
        watcher_thread_id,
    );

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
    if ctx.root_too_broad() || file_watcher_disabled_for_test() {
        return;
    }
    let mut extra_watch_paths = external_ignore_watch_paths(ctx, root_path);
    // Watch `<root>/.cortexkit/` directly as well, so an edit to the project
    // config file is seen even when the project ignores `.cortexkit` (the
    // recursive watch may then exclude it). Without the directory, the root's
    // own config file watch covers it until the next watcher start.
    let config_dir = root_path.join(".cortexkit");
    let sees_config = config_dir.is_dir();
    if sees_config {
        extra_watch_paths.push(config_dir);
    }
    ctx.config_live()
        .set_project_watcher_sees_config(sees_config);
    let matcher = ctx.shared_gitignore();
    let matcher_generation = ctx.gitignore_generation();
    start_project_watcher_with(
        ctx,
        root_path,
        extra_watch_paths,
        move |root, extra_watch_paths, tx| {
            crate::watcher_backend::create_project_watcher(
                root,
                extra_watch_paths,
                tx,
                matcher,
                matcher_generation,
            )
        },
    );
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
    if ctx.root_too_broad() || ctx.subc_unbound_quiesced() {
        ctx.stop_watcher_runtime();
        return;
    }
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
        // With the watcher attached again, edits from here on are observed, so
        // this is the point to compare the callgraph store with the disk for
        // the edits made while nothing was watching.
        ctx.start_pending_callgraph_reconcile();
        // Config edits made while nothing watched the files are picked up
        // now; the reload compares file contents, so it is a no-op otherwise.
        crate::config_live::sync_config_watches(ctx);
        ctx.config_live().signal().request_now();
    }
}

#[cfg(test)]
static SEMANTIC_BLOB_OPEN_OBSERVERS: std::sync::OnceLock<
    Mutex<std::collections::HashMap<String, crossbeam_channel::Sender<(PathBuf, bool)>>>,
> = std::sync::OnceLock::new();

/// Tracks one worker's private index without retaining the index in the census.
/// Each worker owns its contribution: a retiring generation cannot erase a
/// replacement worker's bytes. Shared bases contribute zero here and are counted
/// once by the process-wide semantic base registry. Estimates are refreshed at
/// completed mutation boundaries; transient embedding/refresh buffers are excluded.
struct SemanticWorkerMemory {
    total: Arc<AtomicU64>,
    bytes: u64,
}

impl SemanticWorkerMemory {}

impl Drop for SemanticWorkerMemory {
    fn drop(&mut self) {
        self.total.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

#[cfg(any(test, feature = "test-timing-hooks"))]
fn semantic_quiet_gate_path() -> Option<PathBuf> {
    std::env::var_os("AFT_TEST_SEMANTIC_QUIET_GATE").map(PathBuf::from)
}

#[cfg(any(test, feature = "test-timing-hooks"))]
fn semantic_quiet_gate_held() -> bool {
    let Some(path) = semantic_quiet_gate_path()
        .filter(|path| path.exists() && !path.with_extension("release").exists())
    else {
        return false;
    };
    std::fs::write(path.with_extension("waiting"), b"waiting").expect("quiet gate observation");
    true
}

struct SemanticRefreshDebounce {
    quiet_window: Duration,
    quiet_deadline: Instant,
    batch_deadline: Instant,
}

impl SemanticRefreshDebounce {}

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
    // Without runnable git this is the same answer a failed probe gives.
    if configure_cancellation_requested() || !crate::developer_tools::git_usable() {
        return fail_closed_topology();
    }
    #[cfg(test)]
    ctx.record_worktree_bridge_probe_spawn_for_test();
    let output = crate::effective_path::new_command("git")
        .build()
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
        || previous.subc_connection_file != next.subc_connection_file
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

fn configs_equal_including_runtime_only_fields(previous: &Config, next: &Config) -> bool {
    if previous.disabled_lsp != next.disabled_lsp {
        return false;
    }
    let mut normalized_next = next.clone();
    normalized_next
        .disabled_lsp
        .clone_from(&previous.disabled_lsp);
    let serialized_equal = match (
        serde_json::to_value(previous),
        serde_json::to_value(&normalized_next),
    ) {
        (Ok(previous), Ok(next)) => previous == next,
        _ => false,
    };
    serialized_equal
        && previous.foreground_wait_window_ms == next.foreground_wait_window_ms
        && previous.diagnostics_on_edit == next.diagnostics_on_edit
        && previous.semantic.subc_connection_file == next.semantic.subc_connection_file
        && previous.semantic.route_project_root == next.semantic.route_project_root
        && previous.semantic.route_harness == next.semantic.route_harness
}

/// The plugin owns these three process-state fields: binary search directories,
/// auto-installable binary names, and installs currently in flight. None changes
/// project artifacts, watcher inputs, or the active server registry.
fn only_lsp_process_state_changed(previous: &Config, next: &Config) -> bool {
    let changed = previous.lsp_paths_extra != next.lsp_paths_extra
        || previous.lsp_auto_install_binaries != next.lsp_auto_install_binaries
        || previous.lsp_inflight_installs != next.lsp_inflight_installs;
    if !changed {
        return false;
    }

    let mut without_lsp_process_state = next.clone();
    without_lsp_process_state
        .lsp_paths_extra
        .clone_from(&previous.lsp_paths_extra);
    without_lsp_process_state
        .lsp_auto_install_binaries
        .clone_from(&previous.lsp_auto_install_binaries);
    without_lsp_process_state
        .lsp_inflight_installs
        .clone_from(&previous.lsp_inflight_installs);
    configs_equal_including_runtime_only_fields(previous, &without_lsp_process_state)
}

/// An equivalent bind may skip topology probes only while the topology memo and
/// watcher remain live. Missing trigram/semantic artifacts do not change the
/// root configuration: the bind admits their reload and queues its start after
/// acknowledgement, without requiring exclusive use beside existing readers.
fn fast_path_admissible(ctx: &AppContext, canonical_root: &Path, config: &Config) -> bool {
    // A provisional identity is finalized only by a full bind.
    if ctx.subc_unbound_quiesced()
        || ctx.database_runtime_failed()
        || ctx.artifact_identity_provisional()
    {
        return false;
    }
    if ctx.cached_worktree_bridge(canonical_root).is_none() {
        return false;
    }
    // The full path re-reads the workspace manifest fingerprint because a bind
    // may follow a window nobody observed; while the watcher is live every
    // manifest edit reaches the callgraph through it, so the scan is redundant
    // only then. A root whose watcher is down takes the full path.
    if !ctx.watcher_runtime_active() {
        return false;
    }
    let callgraph_ready = !config.indexes.callgraph
        || ctx
            .callgraph_store()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
        || ctx.callgraph_store_rx().lock().is_some();
    callgraph_ready
}

#[cfg(test)]
fn reset_workspace_manifest_fingerprint_scans_for_test() {
    WORKSPACE_MANIFEST_FINGERPRINT_SCANS.with(|count| count.set(0));
}

#[cfg(test)]
fn workspace_manifest_fingerprint_scans_for_test() -> usize {
    WORKSPACE_MANIFEST_FINGERPRINT_SCANS.with(std::cell::Cell::get)
}

/// Resolution fields of the workspace manifests configure watches, keyed by
/// path; None marks a watched file that does not exist.
type WorkspaceResolutionManifests = std::collections::BTreeMap<PathBuf, Option<String>>;

/// Read what module resolution uses from the workspace manifests (see
/// `callgraph_store::resolution_config`). A `version` bump or a new dependency
/// leaves this unchanged; an `exports`, `main`, `name` or `workspaces` change
/// does not.
fn workspace_manifest_fingerprint(project_root: &Path) -> WorkspaceResolutionManifests {
    #[cfg(test)]
    WORKSPACE_MANIFEST_FINGERPRINT_SCANS.with(|count| count.set(count.get() + 1));
    crate::callgraph_store::resolution_config::workspace_resolution_manifest_paths(project_root)
        .into_iter()
        .map(|path| {
            let fields = crate::callgraph_store::resolution_config::resolution_fields(&path);
            (path, fields)
        })
        .collect()
}

/// Hand workspace manifests whose resolution fields changed since this root's
/// previous configure to the incremental callgraph refresh. The refresh
/// compares each with the fields the store recorded and re-resolves only the
/// files importing through it, so a manifest change never rebuilds the store.
fn refresh_changed_workspace_manifests(
    ctx: &AppContext,
    canonical_root: &Path,
    manifests: Option<WorkspaceResolutionManifests>,
) {
    let Some(manifests) = manifests else {
        return;
    };
    let changed = ctx.note_workspace_resolution_manifests(canonical_root, manifests);
    if changed.is_empty() || !ctx.callgraph_writer() {
        return;
    }
    slog_info!(
        "workspace manifest resolution fields changed in {} file(s); refreshing their importers in the callgraph store",
        changed.len()
    );
    ctx.enqueue_callgraph_store_refresh_before_queries(changed);
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
    memo: &mut BiomeSelectionMemo,
) -> bool {
    let project_root = config.project_root.as_deref();
    for file in files {
        if let Some(server) = servers_for_file_in_pass(file, config, memo)
            .into_iter()
            .find(|server| {
                server.kind.id_str() == configured.id && server.binary == configured.binary
            })
        {
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
        LangId::Cuda => "cuda",
        LangId::Metal => "metal",
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
        LangId::Toml => "toml",
    }
}

fn has_project_config(project_root: Option<&Path>, filenames: &[&str]) -> bool {
    let Some(root) = project_root else {
        return false;
    };
    filenames.iter().any(|file| root.join(file).exists())
}

fn read_pyproject(project_root: Option<&Path>) -> Option<toml::Value> {
    let content = std::fs::read_to_string(project_root?.join("pyproject.toml")).ok()?;
    toml::from_str(&content).ok()
}

fn pyproject_has_tool(pyproject: Option<&toml::Value>, tool_name: &str) -> bool {
    pyproject
        .and_then(|value| value.get("tool"))
        .and_then(|tools| tools.get(tool_name))
        .is_some_and(toml::Value::is_table)
}

#[cfg(test)]
fn has_pyproject_tool(project_root: Option<&Path>, tool_name: &str) -> bool {
    pyproject_has_tool(read_pyproject(project_root).as_ref(), tool_name)
}

#[cfg(test)]
#[test]
fn audit01_pyproject_configuration_not_raw_markers() {
    let root = tempfile::tempdir().unwrap();
    for (text, expected) in [
        ("[tool.ruff]\n", true),
        ("[tool.ruff.lint]\nselect = ['E']\n", true),
        ("[tool.\"ruff\"]\n", true),
        ("# [tool.ruff]\n", false),
        ("description = '[tool.ruff]'\n", false),
        ("[tool.ruff]\ninvalid = [\n", false),
    ] {
        std::fs::write(root.path().join("pyproject.toml"), text).unwrap();
        assert_eq!(
            has_pyproject_tool(Some(root.path()), "ruff"),
            expected,
            "{text}"
        );
    }
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

#[cfg(test)]
fn formatter_candidates(
    lang: LangId,
    config: &crate::config::Config,
) -> Vec<ConfigureToolCandidate> {
    formatter_candidates_with_pyproject(
        lang,
        config,
        read_pyproject(config.project_root.as_deref()).as_ref(),
    )
}

fn formatter_candidates_with_pyproject(
    lang: LangId,
    config: &crate::config::Config,
    pyproject: Option<&toml::Value>,
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
                || pyproject_has_tool(pyproject, "ruff")
            {
                vec![configure_tool_candidate("ruff", "ruff config", true)]
            } else if pyproject_has_tool(pyproject, "black") {
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
        | LangId::Pascal
        | LangId::R
        | LangId::Groovy
        | LangId::ObjC
        | LangId::Toml => Vec::new(),
        LangId::Html | LangId::Markdown | LangId::Yaml => Vec::new(),
    }
}

fn checker_candidates(
    lang: LangId,
    config: &crate::config::Config,
    pyproject: Option<&toml::Value>,
) -> Vec<ConfigureToolCandidate> {
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
                || pyproject_has_tool(pyproject, "pyright")
            {
                vec![configure_tool_candidate("pyright", "pyright config", true)]
            } else if has_project_config(project_root, &["ruff.toml", ".ruff.toml"])
                || pyproject_has_tool(pyproject, "ruff")
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
        | LangId::Pascal
        | LangId::R
        | LangId::Groovy
        | LangId::ObjC
        | LangId::Toml => Vec::new(),
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
    detect_missing_tools_with_cache(languages, config, HashMap::new())
}

/// `tool_cache` maps a tool name to whether it is installed. Production starts
/// it empty; tests pre-seed it so the result does not depend on which
/// formatters the machine running the tests happens to have.
fn detect_missing_tools_with_cache(
    languages: &HashSet<LangId>,
    config: &crate::config::Config,
    tool_cache: HashMap<String, bool>,
) -> Vec<crate::format::MissingTool> {
    detect_missing_tools_with_loader(languages, config, tool_cache, read_pyproject)
}

fn detect_missing_tools_with_loader(
    languages: &HashSet<LangId>,
    config: &crate::config::Config,
    mut tool_cache: HashMap<String, bool>,
    load: impl FnOnce(Option<&Path>) -> Option<toml::Value>,
) -> Vec<crate::format::MissingTool> {
    let mut warnings = Vec::new();
    let mut seen = HashSet::new();
    // Formatter and checker selection must agree on one parsed Python snapshot.
    // Invalid pyproject.toml contributes no candidates; separate tool config files
    // and explicit formatter/checker settings are still considered.
    let pyproject = if languages.contains(&LangId::Python) {
        load(config.project_root.as_deref())
    } else {
        None
    };

    for &lang in languages {
        let language = lang_key(lang);

        if should_warn_missing_formatters(config, lang) {
            for candidate in formatter_candidates_with_pyproject(lang, config, pyproject.as_ref()) {
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
            for candidate in checker_candidates(lang, config, pyproject.as_ref()) {
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

#[cfg(test)]
#[test]
fn audit01_python_warning_collection_uses_one_snapshot() {
    let root = tempfile::tempdir().unwrap();
    let config = Config {
        project_root: Some(root.path().to_path_buf()),
        format_on_edit: true,
        validate_on_edit: Some("syntax".to_string()),
        ..Default::default()
    };
    let mut calls = 0;
    let warnings = detect_missing_tools_with_loader(
        &HashSet::from([LangId::Python]),
        &config,
        HashMap::from([("ruff".to_string(), false), ("pyright".to_string(), false)]),
        |_| {
            calls += 1;
            Some(toml::from_str("[tool.ruff.lint]\nselect = ['E']\n[tool.pyright]\ntypeCheckingMode = 'strict'\n").unwrap())
        },
    );
    assert_eq!(calls, 1);
    assert_eq!(warnings.len(), 2);
    assert!(warnings.iter().any(|warning| warning.tool == "ruff"));
    assert!(warnings.iter().any(|warning| warning.tool == "pyright"));
    detect_missing_tools_with_loader(
        &HashSet::from([LangId::TypeScript]),
        &config,
        HashMap::new(),
        |_| panic!("non-Python discovery must not load pyproject"),
    );
}

fn detect_missing_lsp_binaries(files: &[PathBuf], config: &crate::config::Config) -> Vec<Value> {
    let mut warnings = Vec::new();
    let mut seen_resolutions = HashSet::new();
    let mut file_driven_servers = HashSet::new();
    let mut seen_explicit_servers = HashSet::new();
    let mut warned_binaries = HashSet::new();

    let project_root = config.project_root.as_deref();
    // One memo for the whole pass: files that share folders share their Biome
    // root and configuration lookups. A fresh one each pass sees config edits.
    let mut biome_memo = BiomeSelectionMemo::default();

    for file in files {
        for server in servers_for_file_in_pass(file, config, &mut biome_memo) {
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

        if !configured_lsp_binary_resolves(server, files, config, &mut biome_memo)
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

#[cfg(test)]
#[test]
fn lsp_warnings_probe_each_biome_folder_once_per_pass() {
    let temp = tempfile::tempdir().unwrap();
    let root = crate::inspect::job::canonicalize_normalized(temp.path());
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(root.join("biome.json"), "{}").unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    let files: Vec<PathBuf> = (0..25)
        .map(|index| {
            let file = root.join(format!("src/file{index}.ts"));
            std::fs::write(&file, "").unwrap();
            file
        })
        .collect();
    let config = Config {
        project_root: Some(root.clone()),
        ..Default::default()
    };
    let walks = || crate::lsp::registry::BIOME_ROOT_WALKS.with(std::cell::Cell::get);
    let probes = || crate::lsp::registry::BIOME_CONFIG_DIRECTORY_PROBES.with(std::cell::Cell::get);
    let (walks_before, probes_before) = (walks(), probes());
    detect_missing_lsp_binaries(&files, &config);
    assert_eq!(
        walks() - walks_before,
        1,
        "src resolved its Biome root once"
    );
    assert_eq!(
        probes() - probes_before,
        1,
        "the root was searched for a configuration once"
    );
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WarningBoundary {
    Discovery,
    Walking,
    Tools,
    ParsingTools,
    Lsp,
    ResolvingLsp,
    Publication,
}

struct ConfigureWarningWork {
    root: PathBuf,
    config: Arc<Config>,
    generation_flag: Arc<AtomicU64>,
    generation: u64,
    tx: crossbeam_channel::Sender<(u64, crate::protocol::ConfigureWarningsFrame)>,
    session_id: Option<String>,
}

impl ConfigureWarningWork {
    // Per-call observers let tests supersede a worker at real work boundaries;
    // production supplies a no-op observer rather than a process-wide delay.
    fn run(self, mut boundary: impl FnMut(WarningBoundary)) {
        let current = || self.generation_flag.load(Ordering::SeqCst) == self.generation;
        boundary(WarningBoundary::Discovery);
        if !current() {
            return;
        }
        boundary(WarningBoundary::Walking);
        signal_configure_deferred_walk_start_for_test();
        let flag = self.generation_flag.clone();
        let generation = self.generation;
        let source_files: Vec<PathBuf> =
            crate::callgraph::walk_project_files_while(&self.root, move || {
                flag.load(Ordering::SeqCst) == generation
            })
            .collect();
        boundary(WarningBoundary::Tools);
        if !current() {
            return;
        }
        boundary(WarningBoundary::ParsingTools);
        let languages: HashSet<LangId> = source_files
            .iter()
            .filter_map(|path| detect_language(path))
            .collect();
        let mut warnings = detect_missing_tools_for_languages(&languages, &self.config)
            .into_iter()
            .map(|warning| json!(warning))
            .collect::<Vec<_>>();
        boundary(WarningBoundary::Lsp);
        if !current() {
            return;
        }
        boundary(WarningBoundary::ResolvingLsp);
        warnings.extend(detect_missing_lsp_binaries(&source_files, &self.config));
        boundary(WarningBoundary::Publication);
        // The consumer still filters the check/send race; obsolete discovery
        // must not knowingly queue a frame.
        if !current() {
            return;
        }
        let frame = crate::protocol::ConfigureWarningsFrame::new_with_session_id(
            self.session_id,
            self.root.display().to_string(),
            warnings,
        );
        let _ = self.tx.send((self.generation, frame));
    }
}

#[cfg(test)]
fn assert_warning_superseded_at(phase: WarningBoundary) {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("one.rs"), "fn fixture() {}\n").unwrap();
    let flag = Arc::new(AtomicU64::new(1));
    let (tx, rx) = crossbeam_channel::unbounded();
    let mut observed = Vec::new();
    ConfigureWarningWork {
        root: root.path().to_path_buf(),
        config: Arc::new(Config::default()),
        generation_flag: flag.clone(),
        generation: 1,
        tx,
        session_id: None,
    }
    .run(|at| {
        observed.push(at);
        if at == phase {
            flag.store(2, Ordering::SeqCst);
        }
    });
    assert_eq!(
        observed.contains(&WarningBoundary::Walking),
        phase != WarningBoundary::Discovery
    );
    assert_eq!(
        observed.contains(&WarningBoundary::ParsingTools),
        matches!(phase, WarningBoundary::Lsp | WarningBoundary::Publication)
    );
    assert_eq!(
        observed.contains(&WarningBoundary::ResolvingLsp),
        phase == WarningBoundary::Publication
    );
    assert!(
        rx.try_recv().is_err(),
        "superseded worker queued a stale raw frame"
    );
}

#[cfg(test)]
#[test]
fn audit01_warning_discovery_guard() {
    assert_warning_superseded_at(WarningBoundary::Discovery);
}
#[cfg(test)]
#[test]
fn audit01_warning_walk_guard() {
    assert_warning_superseded_at(WarningBoundary::Walking);
}
#[cfg(test)]
#[test]
fn audit01_warning_tool_guard() {
    assert_warning_superseded_at(WarningBoundary::Tools);
}
#[cfg(test)]
#[test]
fn audit01_warning_lsp_guard() {
    assert_warning_superseded_at(WarningBoundary::Lsp);
}
#[cfg(test)]
#[test]
fn audit01_warning_publication_guard() {
    assert_warning_superseded_at(WarningBoundary::Publication);
}

#[cfg(test)]
#[test]
fn audit01_warning_current_generation_publishes() {
    let root = tempfile::tempdir().unwrap();
    let (tx, rx) = crossbeam_channel::unbounded();
    ConfigureWarningWork {
        root: root.path().to_path_buf(),
        config: Arc::new(Config::default()),
        generation_flag: Arc::new(AtomicU64::new(2)),
        generation: 2,
        tx,
        session_id: None,
    }
    .run(|_| {});
    assert_eq!(rx.try_recv().unwrap().0, 2);
}

/// When a symbol-cache prewarm should give up before visiting every file.
/// The prewarm can run for a long time on a large root, so it stops between
/// files when its configuration is replaced or its root has been unbound past
/// the abandon grace. Idle eviction resets the symbol cache to a new
/// generation, which the prewarm checks on every file as well.
#[derive(Clone)]
struct SymbolPrewarmStop {
    lifecycle: crate::context::SubcLifecycleAdmission,
    content_generation_flag: Arc<AtomicU64>,
    content_generation: u64,
}

impl SymbolPrewarmStop {
    #[cfg(test)]
    fn never() -> Self {
        Self {
            lifecycle: crate::context::SubcLifecycleAdmission::default(),
            content_generation_flag: Arc::new(AtomicU64::new(0)),
            content_generation: 0,
        }
    }
}

#[cfg(test)]
thread_local! {
    /// Called with the number of files visited so far, after each prewarm file.
    static SYMBOL_PREWARM_AFTER_FILE: std::cell::RefCell<Option<Box<dyn FnMut(usize)>>> =
        std::cell::RefCell::new(None);
}

/// What one views-routed symbol prewarm did with the files it was given.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SymbolViewPrewarm {
    /// Already in the symbol cache for the file's current modification time.
    cached: u64,
    /// Served from rows the family's v3 symbol plane already held.
    reused: u64,
    /// Parsed here and stored into the v3 symbol plane.
    stored: u64,
    /// Left for an ordinary on-demand parse: unreadable, changed since the
    /// lexical view verified it, or not parseable.
    skipped: u64,
}

#[cfg(test)]
mod nested_worktree_semantic_tests {
    use super::*;
}

#[cfg(not(debug_assertions))]
fn delay_search_rebuild_publish_for_debug() {}

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

/// The tiers for one configure, plus what the automatic migration of the user
/// file did while reading it (reported back as a configure warning).
fn resolve_config_tiers_for_configure(
    params: &serde_json::Value,
    project_root: &Path,
    harness: Option<&Harness>,
) -> Result<
    (
        Vec<crate::config_resolve::ConfigTier>,
        Option<crate::config_fix::UserConfigMigration>,
    ),
    String,
> {
    let wire_tiers = parse_config_tiers(params).unwrap_or_default();
    let user_config_path = parse_cortexkit_user_config_path(params)?;
    let (file_tiers, migration) = crate::subc_config::read_local_cortexkit_config_tiers_for_harness(
        user_config_path.as_deref(),
        project_root,
        harness,
    );

    let mut tiers = Vec::new();
    for tier_name in ["user", "project"] {
        if let Some(tier) = find_config_tier(&file_tiers, tier_name) {
            tiers.push(tier);
        } else if let Some(tier) = find_config_tier(&wire_tiers, tier_name) {
            tiers.push(tier);
        }
    }
    Ok((tiers, migration))
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
    root_too_broad: bool,
    is_worktree_bridge: bool,
    shared_artifacts_read_only: bool,
) -> String {
    // Keys a live config reload may change are left out: they select no
    // artifact, so a connect after a live edit must not reload artifacts
    // because of them. `callgraph_chunk_size` is read at the next cold build,
    // the semantic query fields per query, and `inspect.enabled` per call.
    let mut semantic = config.semantic.clone();
    semantic.query_timeout_ms = 0;
    semantic.query_instruction.clear();
    format!(
        "root={:?};storage={:?};home={};worktree={};readonly={};search={}:{};semantic={}:{:?};views={};callgraph={}",
        canonical_root,
        (config.cache_storage_dir(), config.cache_epoch),
        root_too_broad,
        is_worktree_bridge,
        shared_artifacts_read_only,
        config.indexes.trigram,
        config.search_index_max_file_size,
        config.indexes.semantic,
        semantic,
        false,
        config.indexes.callgraph,
    )
}

/// What selects the callgraph store itself. Workspace manifests are not part
/// of it: a manifest change moves only the imports that resolve through it,
/// which `refresh_changed_workspace_manifests` refreshes incrementally.
fn configure_callgraph_build_key(
    canonical_root: &Path,
    config: &Config,
    root_too_broad: bool,
    is_worktree_bridge: bool,
    shared_artifacts_read_only: bool,
) -> String {
    format!(
        "root={:?};storage={:?};home={};worktree={};readonly={};enabled={}",
        canonical_root,
        (config.cache_storage_dir(), config.cache_epoch),
        root_too_broad,
        is_worktree_bridge,
        shared_artifacts_read_only,
        config.indexes.callgraph,
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

pub(crate) const HASHLINE_READ_DOWNGRADE_MESSAGE: &str =
    "Hashline mode was downgraded because the tagged read tool is not registered for this session, so nothing could mint the tags a patch addresses";

/// Agent/user-visible text for one downgrade reason.
pub(crate) fn hashline_downgrade_message(reason: &str) -> &'static str {
    if reason == crate::hashline::integration::DowngradeWarning::TAGGED_READ_UNAVAILABLE.reason {
        HASHLINE_READ_DOWNGRADE_MESSAGE
    } else {
        HASHLINE_DOWNGRADE_MESSAGE
    }
}

pub(crate) fn hashline_downgrade_warning_for(
    warning: &crate::hashline::integration::DowngradeWarning,
) -> Value {
    json!({
        "code": warning.code,
        "reason": warning.reason,
        "message": hashline_downgrade_message(warning.reason),
    })
}

pub(crate) fn hashline_downgrade_warning() -> Value {
    hashline_downgrade_warning_for(
        &crate::hashline::integration::DowngradeWarning::EDIT_NOT_REGISTERED,
    )
}

fn slow_configure_prefix_line(total: Duration, phases: &str) -> String {
    format!(
        "configure prefix slow: total={}ms {phases}",
        total.as_millis()
    )
}

/// Route binds may start beside a running configure tail with only a shared
/// hold on the actor (see `crate::executor::current_bind_gate_is_shared`).
/// The equivalent-rebind fast paths need nothing more: they compare the bind
/// against the configuration already published, which the tail never changes,
/// and record the session in state behind its own locks. Any path that changes
/// the root calls this first, before any change: under a shared hold it asks
/// the executor to run the bind again as an exclusive writer and returns the
/// response the caller must return at once (the executor discards it). The
/// worker is handed back rather than waiting for the tail.
fn defer_to_exclusive_configure(req_id: &str) -> Option<Response> {
    if !crate::executor::request_exclusive_rerun() {
        return None;
    }
    Some(Response::error(
        req_id,
        "configure_needs_exclusive",
        "configure must run with exclusive use of the root; queued again",
    ))
}

fn log_slow_configure_prefix(ctx: &AppContext, started_at: Instant) {
    const SLOW_PREFIX: Duration = Duration::from_secs(1);
    let total = started_at.elapsed();
    if total >= SLOW_PREFIX {
        slog_info!(
            "{}",
            slow_configure_prefix_line(total, &ctx.configure_ack_phase_snapshot())
        );
    }
}

struct CacheKeyResolveProfile<'a> {
    root: &'a Path,
    started: Instant,
    key: crate::search_index::ArtifactCacheKeyTimings,
    retention_bind: Duration,
    format_preflight: Duration,
}

impl<'a> CacheKeyResolveProfile<'a> {
    fn new(root: &'a Path) -> Self {
        Self {
            root,
            started: Instant::now(),
            key: Default::default(),
            retention_bind: Duration::ZERO,
            format_preflight: Duration::ZERO,
        }
    }

    fn slow_line(&self, total: Duration) -> Option<String> {
        if total < Duration::from_secs(1) {
            return None;
        }
        // These are wall times, not CPU times. Hold includes memo load/prune/
        // write; rev-list includes process startup and waiting for child exit.
        Some(format!(
            "slow configure cache_key_resolve root={} total_ms={} context_cache_hit={} git_spawns={} git_usable_ms={} rev_list_wall_ms={} retry_wait_ms={} memo_wait_ms={} memo_hold_ms={} memo_load_ms={} memo_prune_ms={} memo_write_ms={} context_memo_wait_ms={} retention_bind_ms={} format_preflight_ms={} legacy_memo_ms={}",
            self.root.display(), total.as_millis(), self.key.context_cache_hit,
            self.key.git_spawns, self.key.git_usable.as_millis(),
            self.key.rev_list.as_millis(), self.key.retry_wait.as_millis(),
            self.key.memo_wait.as_millis(), self.key.memo_hold.as_millis(),
            self.key.memo_load.as_millis(), self.key.memo_prune.as_millis(),
            self.key.memo_write.as_millis(), self.key.context_memo_wait.as_millis(), self.retention_bind.as_millis(),
            self.format_preflight.as_millis(),
            self.key.legacy_memo.as_millis(),
        ))
    }
}

impl Drop for CacheKeyResolveProfile<'_> {
    fn drop(&mut self) {
        if let Some(line) = self.slow_line(self.started.elapsed()) {
            slog_info!("{line}");
        }
    }
}

fn register_hashline_for_configure(
    ctx: &AppContext,
    root: &Path,
    session: &str,
    configured_enabled: bool,
    read_slot_survives: bool,
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
            read_slot_survives,
        },
    );
    if let Some(warning) = &registration.downgrade {
        warnings.push(hashline_downgrade_warning_for(warning));
    }
}

/// Handle a `configure` request.
///
/// Expects `project_root` (string, required) — absolute path to the project root.
/// Sets the project root on `Config`, warms the persistent callgraph store,
/// spawns a file watcher for live invalidation, and returns success with the
/// configured path.
///
/// Stderr log: `[aft] project root set: <path>`
/// Stderr log: `[aft] watcher started: <path>`
pub fn handle_configure(req: &RawRequest, ctx: &AppContext) -> Response {
    handle_configure_prepared(req, ctx, None)
}

fn prepared_cache_matches(prepared: &Config, candidate: &Config) -> bool {
    prepared.cache_dir == candidate.cache_dir
        && prepared.storage_dir == candidate.storage_dir
        && prepared.cache_config_unknown == candidate.cache_config_unknown
        && prepared.cache_dir_opt_out == candidate.cache_dir_opt_out
}

fn prepared_cache_warning(
    prepared: &Config,
    storage: &Path,
) -> Option<crate::config_resolve::ConfigWarning> {
    prepared.cache_unavailable.as_ref().map(|reason| {
        crate::root_cache::unavailable_warning(
            prepared.cache_dir.as_deref().unwrap_or(storage),
            reason,
        )
    })
}

/// Re-run the session's retained connect request after the standing probe saw
/// its cache volume change state, with `prepared` carrying the fresh volume
/// check. Unlike a connect it re-resolves no config file: the session keeps
/// the configuration it is running with (see `handle_configure_inner`).
/// Re-run the connect request `req` to recheck the cache location, unless a
/// connect started after `req` was retained (`connect_generation` changed).
/// The newer connect's settings stand, and the next probe checks them; an old
/// request re-run over them would restore its storage root, harness and other
/// process settings.
pub(crate) fn handle_cache_recovery(
    req: &RawRequest,
    connect_generation: u64,
    ctx: &AppContext,
    prepared: &Config,
) -> Response {
    if ctx.connect_generation() != connect_generation {
        return Response::success(
            &req.id,
            json!({"cache_recovery": "dropped", "reason": "a newer connect replaced the request"}),
        );
    }
    handle_configure_prepared(req, ctx, Some(prepared))
}

fn handle_configure_prepared(
    req: &RawRequest,
    ctx: &AppContext,
    prepared: Option<&Config>,
) -> Response {
    let response = configure_prepared(req, ctx, prepared);
    if response.success {
        // The rebind's rescan runs in the configure tail, after this reply;
        // marking it now keeps a query sent at once from answering without it.
        crate::data_layer::runtime::rebind_configured(ctx);
    }
    response
}

fn configure_prepared(req: &RawRequest, ctx: &AppContext, prepared: Option<&Config>) -> Response {
    // The configuration active before a connect, kept only to recognise a
    // refused connect that left it in place.
    let active_before = prepared.is_none().then(|| ctx.config_unpinned());
    if prepared.is_none() {
        ctx.config_live().clear_applied_bind_snapshot();
        ctx.note_connect_started();
    }
    let _sources_scope = crate::config_live::configure_sources_scope();
    // Configure can change the project root, search paths or server
    // overrides, and follows installs the file watcher may not report, so the
    // memoized TypeScript server choices are recomputed.
    crate::lsp::typescript_project::clear_typescript_selection();
    let response = handle_configure_inner(req, ctx, prepared);
    crate::provisional_identity_retry::note_configure_outcome(ctx, req, response.success);
    // A successful configure applied the files it read; record them so a live
    // config reload can tell later edits apart and knows what is deferred.
    ctx.begin_configure_ack_step("finish_configure");
    crate::config_live::finish_configure(ctx, response.success);
    if response.success {
        ctx.retain_cache_recovery_request(req);
    } else if active_before.is_some_and(|before| Arc::ptr_eq(&before, &ctx.config_unpinned())) {
        // The refused connect published nothing, so the earlier connect's
        // request still describes the active configuration. Without a new
        // stamp every later cache recovery would be dropped as stale and
        // caches would not restart when their volume returns.
        ctx.restamp_cache_recovery_request();
    }
    response
}

fn handle_configure_inner(
    req: &RawRequest,
    ctx: &AppContext,
    prepared: Option<&Config>,
) -> Response {
    let prefix_started_at = Instant::now();
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
    let previous_config = ctx.config();
    let previous_project_root = previous_config.project_root.clone();
    let previous_canonical_cache_root = ctx.canonical_cache_root_opt();
    wait_on_configure_semantic_snapshot_gate_for_test(&req.id);
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
    // A cache recovery (`prepared` is set) re-runs this configure only to
    // re-check the cache volume and restart what depends on it. It keeps the
    // configuration the session is running with, including keys live reload
    // applied since, and reads no config file: an edit live reload reported as
    // deferred until the next connect must stay deferred. The running config
    // already holds this request's resolved tiers, so nothing is re-resolved
    // and the recorded config sources are left as they are.
    let recovery = prepared.is_some();
    let bind_config = if recovery {
        None
    } else {
        crate::subc_config::current_prepared_bind_config()
    };
    if bind_config
        .as_ref()
        .is_some_and(|snapshot| !snapshot.matches(&root_path, &harness))
    {
        return Response::error(
            &req.id,
            "config_unknown",
            "prepared local configuration does not match this bind's root and harness",
        );
    }
    ctx.begin_configure_ack_step("tier_file_reads");
    let (tiers, user_config_migration) = if recovery {
        (Vec::new(), None)
    } else if let Some(snapshot) = bind_config.as_ref() {
        (snapshot.tiers().to_vec(), snapshot.migration())
    } else {
        match resolve_config_tiers_for_configure(params, &root_path, Some(&harness)) {
            Ok(read) => read,
            Err(error) => return Response::error(&req.id, "invalid_request", error),
        }
    };
    ctx.begin_configure_ack_step("tier_merge");
    let config_diagnostics = if recovery {
        crate::config_resolve::ResolveDiagnostics::default()
    } else {
        crate::config_resolve::resolve_config_onto_with_diagnostics_for_harness(
            &tiers,
            Some(&harness),
            &mut next_config,
        )
    };
    let config_dropped_keys = config_diagnostics.dropped;
    if !config_diagnostics.errors.is_empty() {
        // The whole candidate is rejected before anything is published or any
        // index work starts; the context keeps its last-good configuration.
        return Response::error(
            &req.id,
            "config_rejected",
            format!(
                "configure: configuration rejected: {}",
                config_diagnostics.errors.join(", ")
            ),
        );
    }
    ctx.begin_configure_ack_step("source_tracking");
    if !recovery {
        let sources = if let Some(snapshot) = bind_config.as_ref() {
            snapshot.sources(&next_config)
        } else {
            crate::config_live::ConfigSources::from_configure(
                parse_cortexkit_user_config_path(params).ok().flatten(),
                &root_path,
                &tiers,
                &next_config,
            )
        };
        crate::config_live::stage_configure_sources(sources);
    }
    ctx.begin_configure_ack_step("validate_params");
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
    // The automatic rewrite of the user file is reported once, by the
    // configure that performed (or failed) it. `kind`/`hint` is the shape the
    // host plugins deliver to the user.
    if let Some(migration) = &user_config_migration {
        configure_warnings.push(json!({
            "kind": "config_migrated",
            "code": match migration {
                crate::config_fix::UserConfigMigration::Migrated { .. } => "user_config_migrated",
                crate::config_fix::UserConfigMigration::NotMigrated { .. } => "user_config_not_migrated",
            },
            "tier": "user",
            "hint": migration.notice(),
            "message": migration.notice(),
        }));
    }
    let legacy_notice = if let Some(snapshot) = bind_config.as_ref() {
        snapshot.legacy_notice()
    } else {
        find_config_tier(&tiers, "project").and_then(|project| {
            crate::subc_config::legacy_project_config_location_notice(
                &root_path,
                Path::new(&project.source),
            )
            .filter(|_| {
                crate::subc_config::project_config_read_path(&root_path, Some(&harness))
                    == Path::new(&project.source)
            })
        })
    };
    if let Some(notice) = legacy_notice {
        configure_warnings.push(json!({
            "code": "legacy_project_config_location",
            "tier": "project",
            "hint": notice,
            "message": notice,
        }));
    }

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
    // `aft_search_registered` is derived by the resolver from the resolved
    // disabled list (registration only). The plugin-sent flag of the same name
    // is still accepted for older plugins but no longer trusted.
    let _legacy_aft_search_registered = params.get("aft_search_registered");
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
    // Resolve both plugin-injected and plugin-less paths through the shared
    // storage resolver. This keeps AFT_STORAGE_DIR ahead of a wire value and
    // gives every artifact lane one concrete absolute root.
    ctx.begin_configure_ack_step("storage_root");
    let resolved_storage_dir =
        crate::bash_background::storage_dir(next_config.storage_dir.as_deref());
    if resolved_storage_dir.exists() {
        crate::reader_floor::prepare(&resolved_storage_dir);
    }
    next_config.storage_dir = Some(resolved_storage_dir.clone());
    if let Err(error) = crate::root_cache::ensure_config_storage_lease(&mut next_config) {
        return Response::error(&req.id, "storage_in_use", error.to_string());
    }
    let warning = if let Some(prepared) =
        prepared.filter(|prepared| prepared_cache_matches(prepared, &next_config))
    {
        next_config.cache_unavailable = prepared.cache_unavailable.clone();
        next_config.cache_epoch = prepared.cache_epoch;
        next_config.cache_lease = prepared.cache_lease.clone();
        next_config.semantic.local_cache_dir = prepared.semantic.local_cache_dir.clone();
        prepared_cache_warning(prepared, &resolved_storage_dir)
    } else {
        crate::root_cache::prepare_cache_root(&mut next_config)
    };
    if let Some(warning) = warning {
        if next_config
            .cache_dir
            .as_ref()
            .is_some_and(|parent| !parent.exists() && resolved_storage_dir.starts_with(parent))
        {
            return Response::error(
                &req.id,
                crate::root_cache::CACHE_DIR_UNAVAILABLE,
                warning.message,
            );
        }
        configure_warnings.push(json!({"code": warning.code, "key": warning.key, "tier": warning.tier, "value": warning.value, "message": warning.message}));
    }
    if let Err(error) = crate::private_storage::open_root(&resolved_storage_dir) {
        log::warn!(
            "could not open private storage {}: {error}",
            resolved_storage_dir.display()
        );
    }
    // Check the reader floor (and write today's formats as its baseline)
    // before this configure reads or writes anything under the storage root,
    // so a build below the floor refuses the affected components by name.
    ctx.begin_configure_ack_step("reader_floor");
    crate::reader_floor::prepare(&resolved_storage_dir);
    // Owed runner cancels of saved paused tasks go out now, on their own
    // thread, whether or not their projects ever bind.
    crate::bash_background::note_storage_root(&resolved_storage_dir);
    ctx.begin_configure_ack_step("validate_runtime_params");
    next_config.storage_dir = Some(resolved_storage_dir);
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

    ctx.begin_configure_ack_phase("canonicalize");
    let canonical_root = std::fs::canonicalize(&root_path);
    let canonicalization_failed = canonical_root.is_err();
    let canonical_cache_root = canonical_root.unwrap_or_else(|_| root_path.clone());
    debug_assert!(canonical_cache_root.is_absolute());
    let mut degraded_reasons: Vec<String> = Vec::new();
    let home = resolve_home_dir();
    let home_root = home
        .as_ref()
        .is_some_and(|home| *home == canonical_cache_root);
    if home_root {
        degraded_reasons.push("home_root".to_string());
    }
    let account_home = resolve_account_home_dir();
    let root_too_broad = home_root
        || project_root_is_too_broad(
            &canonical_cache_root,
            canonicalization_failed,
            account_home.as_deref(),
        );
    if root_too_broad {
        degraded_reasons.push("root_too_broad".to_string());
        let warning_reason = if home_root {
            "home_root"
        } else if account_home.is_some() {
            "root_too_broad"
        } else {
            "account_home_unknown"
        };
        let warning_message = if account_home.is_some() || home_root {
            "Watching and indexing are off for this broad root; explicit-path tools remain available."
        } else {
            "The account home could not be determined, so only the filesystem or volume root is protected by the home rule. Watching and indexing are off for this broad root; explicit-path tools remain available."
        };
        configure_warnings.push(json!({
            "code": "root_too_broad",
            "reason": warning_reason,
            "message": warning_message,
        }));
    }

    // Equivalent live binds can reuse the existing topology and artifacts, but
    // only after canonicalization and the broad-root safety check above.
    let active_canonical_root = previous_canonical_cache_root
        .as_ref()
        .filter(|canonical_root| {
            ctx.configure_generation() > 0
                && !root_too_broad
                && ctx.harness_opt().as_ref() == Some(&harness)
                && (previous_project_root.as_deref() == Some(root_path.as_path())
                    || canonical_root.as_path() == root_path)
                && fast_path_admissible(ctx, canonical_root, &next_config)
        });
    if let Some(canonical_root) = active_canonical_root {
        next_config.semantic.route_project_root = Some(canonical_root.clone());
        next_config.semantic.route_harness = Some(harness.wire_label());
        let session_already_bound =
            ctx.has_configure_session_binding(canonical_root, req.session());
        if configs_equal_including_runtime_only_fields(&previous_config, &next_config) {
            let repair_artifacts = false;
            if (!session_already_bound || repair_artifacts)
                && !ctx.configure_maintenance_has_capacity()
            {
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
                canonical_root,
                req.session(),
                next_config.hashline_enabled,
                next_config.read_slot_survives(),
                edit_slot_survives,
                &mut configure_warnings,
            );
            if !session_already_bound || repair_artifacts {
                let first_session_bind = !session_already_bound
                    && ctx.note_configure_session_binding(
                        canonical_root.clone(),
                        req.session().to_string(),
                    );
                let storage_root =
                    crate::bash_background::storage_dir(next_config.storage_dir.as_deref());
                let enqueue_result = ctx.enqueue_configure_maintenance(ConfigureMaintenanceJob {
                    generation: ctx.configure_generation(),
                    root_path: root_path.clone(),
                    canonical_cache_root: canonical_root.clone(),
                    project_key: ctx
                        .cached_artifact_cache_key(canonical_root)
                        .unwrap_or_default(),
                    harness: harness.clone(),
                    storage_root: storage_root.clone(),
                    artifact_root: crate::root_cache::cache_root(&next_config),
                    harness_dir: storage_root.join(harness.storage_segment()),
                    session_id: req.session().to_string(),
                    root_too_broad: ctx.root_too_broad(),
                    format_tool_cache_clear_needed: false,
                    run_bash_replay: first_session_bind,
                    configure_database_runtime: false,
                    refresh_project_runtime: false,
                    sync_bash_compress_flag: false,
                    reset_filter_registry: false,
                    clear_failed_spawns: false,
                    warm_callgraph_store: false,
                    supersede_search_artifact_persistence: false,
                    supersede_callgraph_artifact_persistence: false,
                    supersede_semantic_artifact_persistence: false,
                });
                if enqueue_result.is_err() {
                    if first_session_bind {
                        ctx.forget_configure_session_binding(canonical_root, req.session());
                    }
                    return configure_maintenance_backpressure(&req.id);
                }
                slog_debug!(
                    "equivalent configure queued session {} replay={} artifact_reload={} for generation {}",
                    req.session(), first_session_bind, repair_artifacts,
                    ctx.configure_generation()
                );
            }
            ctx.begin_configure_ack_phase("ack_ready");
            log_slow_configure_prefix(ctx, prefix_started_at);
            return Response::success(
                &req.id,
                json!({
                    "project_root": root_path.display().to_string(),
                    "warnings": configure_warnings,
                    "warnings_pending": false,
                    "config_dropped_keys": config_dropped_keys
                        .iter()
                        .map(|d| json!({ "key": d.key, "tier": d.tier, "reason": d.reason }))
                        .collect::<Vec<_>>(),
                }),
            );
        }
        if session_already_bound && only_lsp_process_state_changed(&previous_config, &next_config) {
            // Publishes a new config, so readers must not overlap it.
            if let Some(deferred) = defer_to_exclusive_configure(&req.id) {
                return deferred;
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
            let path_count = next_config.lsp_paths_extra.len();
            {
                let mut lsp = ctx.lsp();
                lsp.set_search_paths(next_config.lsp_paths_extra.clone());
                lsp.clear_failed_spawns();
            }
            ctx.set_config(next_config.clone());
            ctx.begin_configure_ack_phase("ack_ready");
            log_slow_configure_prefix(ctx, prefix_started_at);
            slog_info!(
                "configure: lsp paths updated in place ({} dirs), no reconfigure",
                path_count
            );
            return Response::success(
                &req.id,
                json!({
                    "project_root": root_path.display().to_string(),
                    "warnings": configure_warnings,
                    "warnings_pending": false,
                    "config_dropped_keys": config_dropped_keys
                        .iter()
                        .map(|d| json!({ "key": d.key, "tier": d.tier, "reason": d.reason }))
                        .collect::<Vec<_>>(),
                }),
            );
        }
    }

    let project_root_changed =
        previous_canonical_cache_root.as_deref() != Some(canonical_cache_root.as_path());
    next_config.semantic.route_project_root = Some(canonical_cache_root.clone());
    next_config.semantic.route_harness = Some(harness.wire_label());
    ctx.begin_configure_ack_phase("worktree_probe");
    if let Some(cancelled) = configure_cancelled(&req.id) {
        return cancelled;
    }
    let (is_worktree_bridge, git_common_dir) = detect_worktree_bridge(ctx, &canonical_cache_root);
    if let Some(cancelled) = configure_cancelled(&req.id) {
        return cancelled;
    }
    let watcher_topology_changed =
        ctx.is_worktree_bridge() != is_worktree_bridge || ctx.git_common_dir() != git_common_dir;

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

    // Reject filesystem/volume roots, home and its ancestors before discovering
    // repositories or scheduling indexes. Direct children of a volume can be
    // ordinary projects; retain this project-specific admission for later work.
    if root_too_broad {
        next_config.indexes.trigram = false;
        next_config.indexes.semantic = false;
        next_config.indexes.callgraph = false;
    }
    // A context that binds another root stops holding the parent folder
    // session it held before; a session no context holds stops.
    crate::views::parent::release_context_except(ctx, &canonical_cache_root);
    // With views on, a plain folder of repositories is a parent folder: it is
    // served from its child repositories' own indexes and, like HOME, builds
    // none of its own. The index types the user enabled are passed to the
    // parent session, which serves those same types from the children.
    // The decision is a probe bounded by a short wall-clock budget; the full
    // walk for every child runs on the parent session's worker.
    let parent_folder = !root_too_broad
        && crate::views::parent::prepare(
            &canonical_cache_root,
            crate::views::parent::RequestedPlanes {
                trigram: next_config.indexes.trigram,
                semantic: next_config.indexes.semantic,
                callgraph: next_config.indexes.callgraph,
            },
        );
    let parent_semantic = next_config.semantic.clone();
    if parent_folder {
        next_config.indexes.trigram = false;
        next_config.indexes.semantic = false;
        next_config.indexes.callgraph = false;
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
    // Workspace manifests only matter to the callgraph store. Read them once
    // when that lane is enabled; disabled roots should not read every package
    // manifest on every equivalent bind.
    let workspace_manifests = next_config
        .indexes
        .callgraph
        .then(|| workspace_manifest_fingerprint(&canonical_cache_root));
    let preflight_warm_key = configure_warm_key(
        &canonical_cache_root,
        &next_config,
        root_too_broad,
        is_worktree_bridge,
        ctx.shared_artifacts_read_only(),
    );
    if ctx.configure_generation() > 0
        && !root_too_broad
        && (!parent_folder || crate::views::parent::held_by(ctx, &canonical_cache_root))
        && !ctx.database_runtime_failed()
        && !ctx.artifact_identity_provisional()
        && current_fingerprint.as_ref() == Some(&requested_fingerprint)
        && ctx.configure_warm_key_matches(&preflight_warm_key)
        && ctx.is_worktree_bridge() == is_worktree_bridge
        && ctx.git_common_dir() == git_common_dir
    {
        // Equivalent-configure fast path: same commit boundary as the full
        // path — session binding registration is a state mutation. The seal
        // races the canceller atomically; losing means abort without mutating.
        //
        // A root whose routes all detached is quiesced until this bind's
        // completion marks it bound again. A configure tail that was already
        // running at quiesce time cancels, at its next unit, every configure
        // job it finds queued, forgetting their session bindings. Beside such
        // a tail, the session-only replay job this path queues would be
        // cancelled after the bind acked success, losing that session's bash
        // replay. So a quiesced root takes this path only with exclusive use:
        // then no tail runs (queued maintenance was cancelled at quiesce, and
        // new maintenance for the root waits while its bind is pending), and
        // the root cannot quiesce again while the bind is pending, so the
        // check cannot go stale before the seal below.
        if ctx.subc_unbound_quiesced() {
            if let Some(deferred) = defer_to_exclusive_configure(&req.id) {
                return deferred;
            }
        }
        let needs_session_maintenance =
            !ctx.has_configure_session_binding(&canonical_cache_root, req.session());
        let repair_artifacts = false;
        if (needs_session_maintenance || repair_artifacts)
            && !ctx.configure_maintenance_has_capacity()
        {
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
        refresh_changed_workspace_manifests(ctx, &canonical_cache_root, workspace_manifests);
        register_hashline_for_configure(
            ctx,
            &canonical_cache_root,
            req.session(),
            next_config.hashline_enabled,
            next_config.read_slot_survives(),
            edit_slot_survives,
            &mut configure_warnings,
        );
        let first_session_bind = ctx.note_configure_session_binding(
            canonical_cache_root.clone(),
            req.session().to_string(),
        );
        debug_assert!(ctx.has_configure_session_binding(&canonical_cache_root, req.session()));
        let generation = ctx.configure_generation();
        if first_session_bind || repair_artifacts {
            let storage_root =
                crate::bash_background::storage_dir(next_config.storage_dir.as_deref());
            let enqueue_result = ctx.enqueue_configure_maintenance(ConfigureMaintenanceJob {
                generation,
                root_path: root_path.clone(),
                canonical_cache_root: canonical_cache_root.clone(),
                project_key: ctx
                    .cached_artifact_cache_key(&canonical_cache_root)
                    .unwrap_or_default(),
                harness: harness.clone(),
                storage_root: storage_root.clone(),
                artifact_root: crate::root_cache::cache_root(&next_config),
                harness_dir: storage_root.join(harness.storage_segment()),
                session_id: req.session().to_string(),
                root_too_broad,
                format_tool_cache_clear_needed: false,
                run_bash_replay: first_session_bind,
                configure_database_runtime: false,
                refresh_project_runtime: false,
                sync_bash_compress_flag: false,
                reset_filter_registry: false,
                clear_failed_spawns: false,
                warm_callgraph_store: false,
                supersede_search_artifact_persistence: false,
                supersede_callgraph_artifact_persistence: false,
                supersede_semantic_artifact_persistence: false,
            });
            if enqueue_result.is_err() {
                if first_session_bind {
                    ctx.forget_configure_session_binding(&canonical_cache_root, req.session());
                }
                return configure_maintenance_backpressure(&req.id);
            }
            slog_debug!(
                "equivalent configure queued session {} replay={} artifact_reload={} for generation {}",
                req.session(), first_session_bind, repair_artifacts,
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
        log_slow_configure_prefix(ctx, prefix_started_at);
        return Response::success(
            &req.id,
            json!({
                "project_root": root_path.display().to_string(),
                "warnings": configure_warnings,
                "warnings_pending": false,
                "config_dropped_keys": config_dropped_keys
                    .iter()
                    .map(|d| json!({ "key": d.key, "tier": d.tier, "reason": d.reason }))
                    .collect::<Vec<_>>(),
            }),
        );
    }

    // Everything below re-establishes the root (a new generation, config,
    // runtime, and artifact loads), so it runs with exclusive use of the actor.
    if let Some(deferred) = defer_to_exclusive_configure(&req.id) {
        return deferred;
    }
    wait_on_configure_writer_gate_for_test(&canonical_cache_root);
    if let Some(cancelled) = configure_cancelled(&req.id) {
        return cancelled;
    }

    // Both calls below act on the candidate configuration outside the actor:
    // `maintain` installs or removes child-process hooks and the gh shim, and
    // the ONNX runtime lookup sets a process-wide environment variable. The
    // equivalent fast paths above never need them (the configuration they
    // accept is the one already applied), so they run only here, with
    // exclusive use of the root, and still before any build worker starts.
    let child_storage_root =
        crate::bash_background::storage_dir(next_config.storage_dir.as_deref());
    if let Err(error) = crate::agent_child_env::maintain(&next_config, &child_storage_root) {
        return Response::error(&req.id, "child_environment_unavailable", error);
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

    if root_too_broad {
        slog_warn!(
            "watching and indexing auto-disabled: root_too_broad ({}). Open a project subdirectory for full features.",
            canonical_cache_root.display()
        );
    }

    // A new configure generation may observe a changed project-local formatter.
    // Clear its cached Ruff version before the next actual format, but never run
    // the project binary here: configure/warm paths are existence-only by design.
    let format_tool_cache_clear_needed = effective_configure_changed;

    let storage_root = crate::root_cache::cache_root(&next_config);
    let durable_root = crate::bash_background::storage_dir(next_config.storage_dir.as_deref());
    let cache_reason = crate::root_cache::cache_unavailable(&next_config);
    let cache_available = cache_reason.is_none();
    if let Some(reason) = cache_reason {
        degraded_reasons.push(crate::root_cache::cache_status_code(&reason).into());
    }
    // Choose a durable identity at the first bind that needs an artifact key.
    // With indexes and views off, keep the no-key, no-Git-probe fast path.
    let artifact_key_needed = cache_available
        && !root_too_broad
        && (next_config.indexes.trigram
            || next_config.indexes.semantic
            || next_config.indexes.callgraph);
    ctx.begin_configure_ack_phase("cache_key_resolve");
    ctx.begin_configure_ack_step("artifact_key");
    let mut cache_key_profile = CacheKeyResolveProfile::new(&canonical_cache_root);
    if let Some(cancelled) = configure_cancelled(&req.id) {
        return cancelled;
    }
    let project_key_result = artifact_key_needed.then(|| {
        ctx.memoized_artifact_cache_key_for_configure_profiled(
            &root_path,
            &canonical_cache_root,
            &durable_root,
            git_common_dir.as_deref(),
            &mut cache_key_profile.key,
        )
    });
    if let Some(cancelled) = configure_cancelled(&req.id) {
        return cancelled;
    }
    let mut identity_provisional = false;
    let project_key = match project_key_result {
        Some(Ok(Some(key))) => Some(key),
        Some(Ok(None)) => {
            identity_provisional = true;
            degraded_reasons.push(crate::search_index::PROVISIONAL_IDENTITY_REASON.to_string());
            None
        }
        Some(Err(error)) => {
            if let Some(reason) = error.provisional_git_reason() {
                identity_provisional = true;
                degraded_reasons.push(reason);
                None
            } else {
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
        }
        None => None,
    };
    if identity_provisional {
        // An unreadable legacy identity pin in aft.db or a failed Git probe
        // leaves the identity unsettled. Serve tools that do not need indexes,
        // but write no artifacts, ownership or retention records under a key
        // the next bind may replace. A busy aft.db is retried in the
        // background (`crate::provisional_identity_retry`); a failed Git probe
        // waits for the next bind.
        next_config.indexes.trigram = false;
        next_config.indexes.semantic = false;
        next_config.indexes.callgraph = false;
    }
    let project_scope_key = crate::path_identity::project_scope_key(&canonical_cache_root);
    drop(cache_key_profile);
    let project_key = project_key.unwrap_or_default();
    ctx.begin_configure_ack_phase("storage_capability_probe");
    if let Some(cancelled) = configure_cancelled(&req.id) {
        return cancelled;
    }
    let root_cache_storage_ok = cache_available
        && match crate::root_cache::storage_allows_root_keyed(&storage_root) {
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
        root_cache_storage_ok && !is_worktree_bridge && !root_too_broad && !identity_provisional;
    // A parent folder, like HOME, runs no project-wide work over the folder
    // itself; its children are served from their own sessions' results.
    let inspect_writer_capability =
        root_cache_storage_ok && !root_too_broad && !parent_folder && !identity_provisional;
    let heavy_root_work_allowed = cache_available
        && !root_too_broad
        && !parent_folder
        && !identity_provisional
        && !degraded_reasons.iter().any(|reason| reason == "home_root");

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
    // Each step below is timed separately inside `state_commit`, so a slow
    // commit names the step that took the time in the timing line.
    ctx.begin_configure_ack_step("database_runtime");
    ctx.begin_database_runtime(
        canonical_cache_root.clone(),
        crate::bash_background::storage_dir(next_config.storage_dir.as_deref()),
        req.session().to_string(),
    );
    // Commit phase: no validation returns after this point.
    if semantic_fingerprint_config_changed(&previous_config.semantic, &next_config.semantic) {
        ctx.advance_semantic_fingerprint_generation();
    }
    ctx.begin_configure_ack_step("lsp_search_paths");
    ctx.lsp()
        .set_search_paths(next_config.lsp_paths_extra.clone());
    ctx.begin_configure_ack_step("config_publish");
    ctx.set_config(next_config.clone());
    wait_on_configure_commit_gate_for_test(&canonical_cache_root);
    ctx.begin_configure_ack_step("hashline");
    register_hashline_for_configure(
        ctx,
        &canonical_cache_root,
        req.session(),
        next_config.hashline_enabled,
        next_config.read_slot_survives(),
        edit_slot_survives,
        &mut configure_warnings,
    );
    ctx.begin_configure_ack_step("log_root");
    crate::logging::sync_storage_root(ctx.storage_dir());
    ctx.set_harness(harness.clone());
    ctx.begin_configure_ack_step("backup_policy");
    {
        let mut backup = ctx.backup().lock();
        backup.set_policy(crate::backup::BackupPolicy {
            enabled: next_config.backup.enabled.unwrap_or(true),
            max_depth: next_config
                .backup
                .max_depth
                .unwrap_or(crate::backup::DEFAULT_MAX_UNDO_DEPTH),
            max_file_size: Some(
                next_config
                    .backup
                    .max_file_size
                    .unwrap_or(crate::backup::DEFAULT_MAX_BACKUP_FILE_SIZE),
            ),
        });
        backup.set_db_harness(harness.clone());
        // Select the undo store's disk namespace here, before the configure
        // reply, rather than in the deferred maintenance job. The database can
        // be open (so mutating tools and undo pass the persistence gate) before
        // that job runs; a store without a storage directory then keeps
        // snapshots in memory only, and after a restart undo cannot find
        // them. Selecting the directory does no disk I/O.
        if let Some(storage_dir) = next_config.storage_dir.clone() {
            backup.set_storage_dir_for_harness(
                storage_dir,
                harness.clone(),
                next_config.checkpoint_ttl_hours,
            );
        }
    }
    // Named checkpoints need the same treatment. Before configure, the
    // checkpoint store writes under `<storage>/unbound/checkpoints`; the
    // harness namespace is `<storage>/<harness>/checkpoints`. A subc route
    // accepts tool calls as soon as this reply is sent, before the deferred
    // maintenance job runs, so a checkpoint could land in the unbound
    // directory. When that job later switched directories, an older
    // checkpoint of the same name in the harness directory (written by
    // another project root using the same storage directory and session)
    // shadowed it, and restore wrote that older snapshot.
    //
    // This runs before the bind reply, so it only records the choice: it
    // takes neither the store's mutex nor its file lock and does no I/O. The
    // store switches directories at the start of its next operation, and
    // moving checkpoints saved before configure stays with that operation or
    // the deferred maintenance job, whichever takes the file lock first.
    ctx.begin_configure_ack_step("artifact_access");
    if let Some(storage_dir) = next_config.storage_dir.clone() {
        ctx.request_checkpoint_namespace(storage_dir, harness.clone());
    }
    ctx.set_canonical_cache_root(canonical_cache_root.clone());
    crate::root_cache::configure_artifact_access(
        &canonical_cache_root,
        &project_key,
        is_worktree_bridge,
    );
    ctx.set_cache_role(is_worktree_bridge, git_common_dir);
    ctx.set_cache_writer_capabilities(callgraph_writer_capability, inspect_writer_capability);
    // Snapshot degraded-mode state once at configure time so every later
    // heavy-work entry point reads the same cheap gate instead of re-deriving
    // directory-scope rules independently.
    ctx.set_degraded_reasons(degraded_reasons.clone());
    if identity_provisional {
        // This bind published no key. A reader on another thread may have
        // cached a Git-derived key for the root before the reasons above
        // said provisional; the settled identity can key differently.
        ctx.forget_unsettled_artifact_cache_key(&canonical_cache_root, &root_path);
    }
    ctx.set_heavy_root_work_allowed(heavy_root_work_allowed);
    if root_too_broad {
        ctx.stop_watcher_runtime();
        crate::views::parent::deactivate(&canonical_cache_root);
    }
    // Past the commit seal: start (or join) the parent session now, so the
    // first request after this bind is already answered by it. Starting only
    // spawns its worker.
    ctx.begin_configure_ack_step("views_parent");
    if cache_available && !root_too_broad && !identity_provisional {
        crate::views::parent::activate(ctx, &canonical_cache_root, &storage_root, &parent_semantic);
    }
    ctx.begin_configure_ack_step("warm_key");
    let warm_key = configure_warm_key(
        &canonical_cache_root,
        &next_config,
        root_too_broad,
        is_worktree_bridge,
        ctx.shared_artifacts_read_only(),
    );
    let semantic_build_inputs_changed = project_root_changed
        || previous_config.indexes.semantic != next_config.indexes.semantic
        || semantic_fingerprint_config_changed(&previous_config.semantic, &next_config.semantic)
        || previous_config.semantic.max_files != next_config.semantic.max_files
        || previous_config.cache_epoch != next_config.cache_epoch;
    let (configure_generation, equivalent_warm_config) =
        ctx.note_configure_warm_key(warm_key, semantic_build_inputs_changed);
    release_callgraph_start_waiters_for_generation_change(
        previous_canonical_cache_root.as_deref(),
        &canonical_cache_root,
        equivalent_warm_config,
    );
    let callgraph_build_key = configure_callgraph_build_key(
        &canonical_cache_root,
        &next_config,
        root_too_broad,
        is_worktree_bridge,
        ctx.shared_artifacts_read_only(),
    );
    let equivalent_callgraph_build = ctx.note_callgraph_build_key(callgraph_build_key);
    ctx.begin_configure_ack_step("build_state");
    let callgraph_build_in_progress = ctx.callgraph_store_rx().lock().is_some();
    let semantic_build_in_progress = ctx.semantic_index_rx().lock().is_some();
    let semantic_build_adopted = cache_available
        && !equivalent_warm_config
        && semantic_build_in_progress
        && !semantic_build_inputs_changed;
    let first_session_bind =
        ctx.note_configure_session_binding(canonical_cache_root.clone(), req.session().to_string());
    // An interactive session binding this root opens it for semantic indexing
    // before the artifact scheduling below consults that (see
    // `semantic_admission`). Route teardown closes it again.
    if crate::semantic_admission::opens_root(&harness) {
        crate::semantic_admission::note_opened(req.session(), &canonical_cache_root);
    }
    let semantic_admitted =
        crate::semantic_admission::admission(&canonical_cache_root, &next_config.index.roots)
            .admits();
    ctx.begin_configure_ack_step("tier2_reset");
    if !equivalent_warm_config {
        ctx.reset_tier2_refresh_scheduler();
        if !semantic_build_adopted {
            ctx.reset_semantic_cold_seed_gate_for_configure();
            // The cold-seed gate holds Tier-2 work until a semantic build
            // finishes; a root that will not build must not hold it.
            if cache_available
                && next_config.indexes.semantic
                && !ctx.shared_artifacts_read_only()
                && !root_too_broad
                && semantic_admitted
            {
                ctx.schedule_semantic_cold_seed_gate_for_configure();
            }
        }
    } else if previous_config.inspect.enabled != next_config.inspect.enabled
        || previous_config.inspect.categories != next_config.inspect.categories
    {
        // `inspect.enabled` is not part of the warm key (it selects no
        // artifact), but turning it on or off still restarts Tier-2 timing.
        ctx.reset_tier2_refresh_scheduler();
    }
    // Project root (and thus tsconfig resolution) may have changed; drop the
    // status-bar membership cache so the next bar count re-resolves from disk.
    // Equivalent rebinds keep this hot cache because no tsconfig input changed.
    if !equivalent_warm_config || project_root_changed {
        ctx.clear_tsconfig_membership_cache();
    }
    ctx.begin_configure_ack_step("backup_project_key");
    ctx.backup()
        .lock()
        .set_db_project_key(crate::path_identity::project_scope_key(
            &canonical_cache_root,
        ));
    ctx.begin_configure_ack_phase("index_loading_state");
    // The data layer's text plane serves search; today's index is not built.
    let search_index = false;
    let semantic_search = ctx.config().indexes.semantic;

    // Reconfigure is still the signal that this root's workspace package
    // metadata may have changed, even when the warm-maintenance key is
    // otherwise equivalent. Scoped to the root: the caches are process-wide
    // and other roots' binds must not empty them.
    if let Some(root) = ctx.config().project_root.as_deref() {
        crate::callgraph::clear_workspace_package_cache_under(root);
    }

    if equivalent_warm_config {
        if semantic_build_in_progress {
            slog_info!(
                "semantic index build adopted by equivalent reconfigure (generation {})",
                configure_generation
            );
        }
        if callgraph_build_in_progress {
            slog_info!(
                "callgraph store warm build adopted by equivalent reconfigure (generation {})",
                configure_generation
            );
        }
    } else {
        // Semantic and callgraph workers keep their receiver and dedicated
        // build epoch when that lane's inputs are unchanged. Changed lane inputs
        // invalidate the old worker at its next bounded batch or slice.
        if semantic_build_in_progress {
            if semantic_build_adopted {
                slog_info!(
                    "semantic index build adopted by matching semantic configuration (generation {})",
                    configure_generation
                );
            } else {
                slog_warn!(
                    "semantic index build cancelled (semantic build inputs changed at generation {})",
                    configure_generation
                );
            }
        }
        if callgraph_build_in_progress {
            if equivalent_callgraph_build {
                slog_info!(
                    "callgraph store warm build adopted by matching root and corpus configuration (generation {})",
                    configure_generation
                );
            } else {
                slog_warn!(
                    "callgraph store warm build cancelled (callgraph inputs changed at generation {})",
                    configure_generation
                );
            }
        }

        let retain_semantic = cache_available
            && !semantic_build_inputs_changed
            && previous_config.storage_dir == next_config.storage_dir
            && previous_config.cache_dir == next_config.cache_dir
            && previous_config.cache_epoch == next_config.cache_epoch;
        if semantic_build_adopted {
            let adopted = ctx.adopt_semantic_index_rx_generation(configure_generation);
            debug_assert!(
                adopted,
                "semantic build receiver disappeared before adoption"
            );
        } else {
            if !retain_semantic {
                *ctx.semantic_index()
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
            }
            ctx.retire_semantic_index_rx(
                "a reconfigure with different semantic settings superseded it",
            );
            ctx.set_semantic_build_progress(None);
        }
        if equivalent_callgraph_build {
            if callgraph_build_in_progress {
                let adopted = ctx.adopt_callgraph_store_rx_generation(configure_generation);
                debug_assert!(
                    adopted,
                    "callgraph build receiver disappeared before adoption"
                );
            }
        } else {
            *ctx.callgraph_store()
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
            ctx.retire_callgraph_store_rx();
            // Only a callgraph writer can fulfil a force token. A read-only
            // root (linked worktree, borrowed index family) follows the
            // owner's published store; a token here would never be fulfilled
            // and would keep every later callgraph query Unavailable.
            if previous_project_root.as_ref() == Some(&root_path) && ctx.callgraph_writer() {
                ctx.mark_callgraph_store_force_rebuild(
                    "callgraph build inputs changed (storage, enablement, or worktree topology)",
                );
            }
        }
        if !semantic_build_adopted {
            if !retain_semantic {
                *ctx.semantic_index_status()
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    SemanticIndexStatus::Disabled;
            }
            ctx.clear_semantic_refresh_worker();
            *ctx.semantic_embedding_model().lock() = None;
        }
        if equivalent_callgraph_build {
            ctx.clear_pending_index_updates_preserving_callgraph();
        } else {
            ctx.clear_pending_index_updates();
        }

        // Reconfigure can point AFT at a different root; reset that root's
        // entries before warming the callgraph store for it.
        if let Some(root) = ctx.config().project_root.as_deref() {
            crate::callgraph::clear_workspace_package_cache_under(root);
        }
    }
    refresh_changed_workspace_manifests(ctx, &canonical_cache_root, workspace_manifests);

    let refresh_project_runtime =
        project_root_changed || watcher_topology_changed || !ctx.watcher_runtime_active();
    let sync_bash_compress_flag = !equivalent_warm_config
        || previous_config.experimental_bash_compress != next_config.experimental_bash_compress;
    // Bash can finish before deferred maintenance reaches ProcessFlags. Publish
    // the cheap mirror before acknowledging configure, so completion rendering
    // cannot cache raw output while requests already see compression enabled.
    if sync_bash_compress_flag {
        ctx.sync_bash_compress_flag();
    }
    let clear_failed_spawns =
        should_clear_failed_spawns(&previous_config, &next_config, equivalent_warm_config);
    let storage_root = crate::bash_background::storage_dir(next_config.storage_dir.as_deref());
    ctx.begin_configure_ack_phase("maintenance_enqueue");
    let enqueue_result = ctx.enqueue_configure_maintenance(ConfigureMaintenanceJob {
        generation: configure_generation,
        root_path: root_path.clone(),
        canonical_cache_root: canonical_cache_root.clone(),
        project_key: project_key.clone(),
        harness: harness.clone(),
        storage_root: storage_root.clone(),
        artifact_root: crate::root_cache::cache_root(&next_config),
        harness_dir: storage_root.join(harness.storage_segment()),
        session_id: req.session().to_string(),
        root_too_broad,
        format_tool_cache_clear_needed,
        run_bash_replay: !equivalent_warm_config || first_session_bind,
        configure_database_runtime: true,
        refresh_project_runtime,
        sync_bash_compress_flag,
        reset_filter_registry: !equivalent_warm_config,
        clear_failed_spawns,
        warm_callgraph_store: next_config.indexes.callgraph
            && !root_too_broad
            && (!equivalent_warm_config || ctx.configure_callgraph_upgrade_pending())
            && (!equivalent_callgraph_build || !callgraph_build_in_progress),
        supersede_search_artifact_persistence: !equivalent_warm_config,
        supersede_callgraph_artifact_persistence: !equivalent_callgraph_build,
        supersede_semantic_artifact_persistence: !equivalent_warm_config && !semantic_build_adopted,
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
    let warnings_pending = !root_too_broad && ctx.progress_sender_handle().is_some();
    if warnings_pending {
        let warning_tx = ctx.configure_warnings_sender();
        let session_id_for_bg = log_ctx::current_session();
        let work = ConfigureWarningWork {
            root: root_path.clone(),
            config: config_snapshot.clone(),
            generation_flag: ctx.configure_generation_flag(),
            generation: configure_generation,
            tx: warning_tx,
            session_id: session_id_for_bg.clone(),
        };
        let run_deferred_walk = move || {
            log_ctx::with_session(session_id_for_bg, || {
                delay_configure_deferred_walk_for_test();
                work.run(|_| {});
            });
        };
        if run_configure_deferred_walk_synchronously_for_test() {
            run_deferred_walk();
        } else {
            thread::Builder::new()
                .name("aft-configure-warnings".to_string())
                .spawn(run_deferred_walk)
                .expect("failed to spawn thread");
        }
    }

    // Return the success response immediately so the plugin can mark the project as
    // configured. Missing-binary warnings are sent later in a `configure_warnings`
    // push frame.
    ctx.begin_configure_ack_phase("ack_ready");
    log_slow_configure_prefix(ctx, prefix_started_at);
    let response = Response::success(
        &req.id,
        json!({
            "project_root": root_path.display().to_string(),
            "warnings": configure_warnings,
            "warnings_pending": warnings_pending,
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

fn release_callgraph_start_waiters_for_generation_change(
    previous_root: Option<&Path>,
    configured_root: &Path,
    equivalent_generation: bool,
) {
    if equivalent_generation {
        return;
    }
    if let Some(previous_root) = previous_root {
        crate::logging::release_index_build_start_waiters(
            crate::logging::IndexPlane::Callgraph,
            previous_root,
        );
    }
    if previous_root != Some(configured_root) {
        crate::logging::release_index_build_start_waiters(
            crate::logging::IndexPlane::Callgraph,
            configured_root,
        );
    }
}

// Fleet restarts can reopen dozens of owner caches at once. Four warm loads
// bound disk parsing and verification without sharing slots with cold builds.

#[cfg(test)]
thread_local! {
    static WARM_SEARCH_RELOAD_LIMITER_FOR_TEST: std::cell::RefCell<Option<Arc<crate::cold_build_limiter::ColdBuildLimiter>>> = const { std::cell::RefCell::new(None) };
}

/// Open the root's database under the newest open epoch (the configure that
/// committed last). Used by the per-call retry after a busy open; the deferred
/// open after a bind goes through `crate::database_open`.
pub(crate) fn configure_database_runtime_with_mode(
    ctx: &AppContext,
    canonical_cache_root: &Path,
    storage_root: &Path,
    mode: crate::db::OpenMode,
) {
    let epoch = ctx.database_open_epoch();
    let _ = open_database_runtime(ctx, canonical_cache_root, storage_root, mode, epoch);
}

/// Open `aft.db` under `storage_root`, install it for backups and bash, and
/// publish the root's persistence readiness, unless a configure newer than
/// `epoch` committed meanwhile (then the newer configure's open publishes).
/// Returns how long the floor check and the open took, for the readiness log.
pub(crate) fn open_database_runtime(
    ctx: &AppContext,
    canonical_cache_root: &Path,
    storage_root: &Path,
    mode: crate::db::OpenMode,
    epoch: u64,
) -> crate::database_open::DatabaseOpenReport {
    use crate::database_open::{DatabaseOpenOutcome, DatabaseOpenReport};

    #[cfg(test)]
    ctx.note_database_open_attempt_for_test();

    wait_on_configure_tail_stage_gate_for_test(
        canonical_cache_root,
        ConfigureMaintenanceStage::DatabaseRuntime,
    );
    ctx.backup()
        .lock()
        .set_db_project_key(crate::path_identity::project_scope_key(
            canonical_cache_root,
        ));

    let db_path = storage_root.join("aft.db");
    // Hold deferred migration in conformance tests until the caller releases it.
    // The marker proves the bind replied while persistence was still opening.
    #[cfg(debug_assertions)]
    if mode == crate::db::OpenMode::Deferred {
        if let Some(gate) = std::env::var_os("AFT_TEST_DATABASE_OPEN_GATE").map(PathBuf::from) {
            if gate.exists() {
                std::fs::write(gate.with_extension("entered"), b"opening").unwrap();
                let deadline = Instant::now() + Duration::from_secs(30);
                while gate.exists() {
                    assert!(
                        Instant::now() < deadline,
                        "database open test gate was not released: {}",
                        gate.display()
                    );
                    thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }
    // In debug builds, optionally delay opening the database to exercise readiness handling.
    #[cfg(debug_assertions)]
    if let Some(delay_ms) = std::env::var("AFT_TEST_DATABASE_OPEN_DELAY_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|delay| *delay > 0 && mode == crate::db::OpenMode::Deferred)
    {
        slog_info!(
            "delaying database open by {}ms at {}",
            delay_ms.min(60_000),
            db_path.display()
        );
        thread::sleep(Duration::from_millis(delay_ms.min(60_000)));
    }
    // A database whose schema a newer build wrote (or any database while the
    // storage root's reader floor is above this build) is refused by name
    // before a read-write connection, PRAGMA or migration touches it.
    let floor_started = Instant::now();
    let schema = ctx.app().database_schema_version(&db_path, mode);
    // A busy resident handle is not evidence that its schema became readable.
    // Retain any recorded refusal until a successful version read revalidates
    // it; in particular never peek through a second fd to bypass contention.
    let gate = match &schema {
        Ok((version, _)) => crate::persisted_format::gate(
            crate::persisted_format::PersistedStore::AftDb,
            &db_path,
            &db_path,
            version.map(u64::from),
        ),
        Err(_) => Ok(()),
    };
    let floor_check = floor_started.elapsed();
    if let Err(refusal) = gate {
        let published = ctx.publish_database_open(epoch, |slot| {
            slot.ready_for = None;
            // Keep any process-shared handle: other roots may still own its
            // pools, and evicting it would let a retry open a second descriptor.
            ctx.backup().lock().clear_db_pool();
            ctx.bash_background().clear_db_pool();
            ctx.finish_database_runtime_error(refusal.to_string(), false);
        });
        if published {
            slog_warn!("aft.db not opened: {refusal} — tools refused with database_unavailable");
        }
        return DatabaseOpenReport {
            floor_check,
            open: Duration::ZERO,
            reused_handle: false,
            outcome: if published {
                DatabaseOpenOutcome::Refused(refusal.to_string())
            } else {
                DatabaseOpenOutcome::Superseded
            },
        };
    }
    let reused_handle = schema.as_ref().is_ok_and(|(_, reused)| *reused);
    let open_started = Instant::now();
    let opened = schema.and_then(|_| ctx.app().open_db_with_mode(&db_path, mode));
    let open = open_started.elapsed();
    let (published, outcome) = match opened {
        Ok(shared) => (
            ctx.publish_database_open(epoch, |slot| {
                slot.ready_for = Some((
                    canonical_cache_root.to_path_buf(),
                    storage_root.to_path_buf(),
                ));
                ctx.backup().lock().set_db_pool(shared.clone());
                ctx.bash_background().set_db_pool(shared);
                ctx.finish_database_runtime(Ok(()));
            }),
            DatabaseOpenOutcome::Ready,
        ),
        Err(err) => {
            let message = format!("{}: {err}", db_path.display());
            // Do not clear the process-shared handle if another root is already
            // using it. A failed root configure must not close that root's SQLite
            // connection and WAL descriptors.
            let published = ctx.publish_database_open(epoch, |slot| {
                slot.ready_for = None;
                ctx.backup().lock().clear_db_pool();
                ctx.bash_background().clear_db_pool();
                ctx.finish_database_runtime_error(message.clone(), err.is_busy());
            });
            if published {
                slog_warn!(
                    "failed to open aft.db at {}: {} — tools refused with database_unavailable",
                    db_path.display(),
                    err
                );
            }
            (published, DatabaseOpenOutcome::Failed(message))
        }
    };
    DatabaseOpenReport {
        floor_check,
        open,
        reused_handle,
        outcome: if published {
            outcome
        } else {
            DatabaseOpenOutcome::Superseded
        },
    }
}

fn claim_configure_replay_batch<'a>(
    ctx: &'a AppContext,
    job: &ConfigureMaintenanceJob,
) -> Result<crate::bash_background::registry::ProjectReplayBatch<'a>, ConfigureMaintenanceUnitResult>
{
    wait_on_configure_replay_gate_for_test(&job.canonical_cache_root, "replay_claim");
    // Admit exactly one replay call, not the remaining cursor. Disk scans,
    // legacy repair and recovery must never hold up the unbind transition.
    match ctx.run_if_subc_bound_generation(job.generation, || {
        ctx.bash_background().claim_project_replay_batch()
    }) {
        Some(Some(batch)) => {
            wait_on_configure_replay_gate_for_test(&job.canonical_cache_root, "replay_claimed");
            Ok(batch)
        }
        Some(None) => Err(ConfigureMaintenanceUnitResult::Continue),
        None if ctx.subc_unbound_quiesced() => Err(ConfigureMaintenanceUnitResult::CancelAll),
        None => {
            forget_configure_job_binding(ctx, job);
            Err(ConfigureMaintenanceUnitResult::Complete)
        }
    }
}

fn replay_configure_session(
    ctx: &AppContext,
    job: &ConfigureMaintenanceJob,
) -> Result<(), ConfigureMaintenanceUnitResult> {
    let batch = claim_configure_replay_batch(ctx, job)?;
    replay_configure_session_parts(
        ctx,
        batch,
        &job.storage_root,
        job.harness.clone(),
        &job.harness_dir,
        &job.session_id,
        &job.root_path,
    );
    Ok(())
}

fn replay_configure_session_parts(
    ctx: &AppContext,
    batch: crate::bash_background::registry::ProjectReplayBatch<'_>,
    storage_root: &Path,
    harness: Harness,
    harness_dir: &Path,
    session_id: &str,
    root_path: &Path,
) {
    ctx.bash_background()
        .repair_legacy_root_tasks(storage_root, harness.clone());
    // A replayed task a worker was waiting on is granted one worker wait
    // window from now, so the registry needs the configured window first.
    ctx.bash_background()
        .set_worker_wait_window(std::time::Duration::from_millis(
            crate::commands::bash_orchestrate::worker_wait_max_ms(ctx),
        ));
    #[cfg(test)]
    CONFIGURE_REPLAY_SESSION_CALLS.fetch_add(1, Ordering::SeqCst);
    if let Err(error) =
        batch.replay_session_for_project(harness_dir, &harness, session_id, root_path)
    {
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
    // An unbound or quiesced root stops both identity and cache recovery.
    crate::provisional_identity_retry::cancel(ctx);
    let parked = ctx
        .take_parked_configure_tail()
        .map(|state| {
            state
                .jobs
                .into_iter()
                .map(|pending| pending.job)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let jobs = parked
        .into_iter()
        .chain(ctx.drain_configure_maintenance())
        .collect::<Vec<_>>();
    let cancelled = jobs.len();
    cancel_unbound_configure_jobs(ctx, jobs);
    cancelled
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ConfigureMaintenanceStage {
    #[default]
    Admission,
    DatabaseRuntime,
    SessionReplay,
    BashRuntime,
    ProjectRuntime,
    Watcher,
    ProcessFlags,
    Status,
}

impl ConfigureMaintenanceStage {
    fn label(self) -> &'static str {
        match self {
            Self::Admission => "admission",
            Self::DatabaseRuntime => "database_runtime",
            Self::SessionReplay => "session_replay",
            Self::BashRuntime => "bash_runtime",
            Self::ProjectRuntime => "project_runtime",
            Self::Watcher => "watcher",
            Self::ProcessFlags => "process_flags",
            Self::Status => "status",
        }
    }

    /// Initial work that runs before the first request after configure: bash
    /// replay restores the DB tasks and one bounded disk batch, so an immediate
    /// `bash_drain_completions` sees those results. Disk-only tasks beyond that
    /// batch arrive through later maintenance and completion delivery rather
    /// than delaying requests for a full disk scan. The prefix also prepares the
    /// project runtime (a queued `outline`/`glob` must see the rebuilt gitignore
    /// matcher), and the watcher start. The watcher only observes changes made
    /// after it subscribes, so a file written between the configure ack and the
    /// first request is lost for good if the start waits for a later yield: on a
    /// small tree the index is ready after one request, an ignore-rule change
    /// written right after it was never seen, and the ignored file stayed in
    /// grep results. Everything after these is housekeeping and yields to
    /// requests.
    fn is_non_yielding_prefix(self) -> bool {
        matches!(
            self,
            Self::Admission
                | Self::DatabaseRuntime
                | Self::SessionReplay
                | Self::BashRuntime
                | Self::ProjectRuntime
                | Self::Watcher
        )
    }
}

#[derive(Debug)]
struct ConfigureMaintenanceContinuation {
    job: ConfigureMaintenanceJob,
    stage: ConfigureMaintenanceStage,
    replay_retry_at: Option<Instant>,
    lexical_bind_retry_at: Option<Instant>,
    lexical_bind_retry_delay: Duration,
    callgraph_upgrade_retry_at: Option<Instant>,
    replay_error_logged: bool,
}

impl ConfigureMaintenanceContinuation {
    fn new(job: ConfigureMaintenanceJob) -> Self {
        Self {
            job,
            stage: ConfigureMaintenanceStage::Admission,
            replay_retry_at: None,
            lexical_bind_retry_at: None,
            lexical_bind_retry_delay: Duration::from_secs(1),
            callgraph_upgrade_retry_at: None,
            replay_error_logged: false,
        }
    }

    fn retry_at(&self) -> Option<Instant> {
        self.callgraph_upgrade_retry_at
            .or(self.lexical_bind_retry_at)
            .or(self.replay_retry_at)
    }
}

#[derive(Debug, Default)]
pub(crate) struct ConfigureMaintenanceState {
    jobs: VecDeque<ConfigureMaintenanceContinuation>,
    // Freeze retry eligibility across one full drain so a deferred unit cannot
    // become due again while other units run and monopolize the same drain.
    replay_retry_cutoff: Option<Instant>,
}

impl ConfigureMaintenanceState {
    pub(crate) fn callgraph_upgrade_pending(&self) -> bool {
        self.jobs
            .iter()
            .any(|continuation| continuation.callgraph_upgrade_retry_at.is_some())
    }

    pub(crate) fn jobs_is_empty(&self) -> bool {
        self.jobs.is_empty()
    }

    pub(crate) fn has_ready_job(&self) -> bool {
        let now = self.replay_retry_cutoff.unwrap_or_else(Instant::now);
        self.jobs.iter().any(|continuation| {
            continuation
                .retry_at()
                .is_none_or(|deadline| deadline <= now)
        })
    }

    fn select_ready_job(&mut self, ctx: &AppContext) -> bool {
        let now = self.replay_retry_cutoff.unwrap_or_else(Instant::now);
        let Some(index) = self.jobs.iter().position(|continuation| {
            ctx.subc_unbound_quiesced()
                || continuation
                    .retry_at()
                    .is_none_or(|deadline| deadline <= now)
        }) else {
            return false;
        };
        self.jobs.rotate_left(index);
        true
    }

    fn absorb_enqueued(&mut self, ctx: &AppContext) {
        self.jobs.extend(
            ctx.drain_configure_maintenance()
                .into_iter()
                .map(ConfigureMaintenanceContinuation::new),
        );
    }

    /// True when the next unit to run starts a job or continues one past its
    /// request-critical prefix, so stopping here leaves no prefix half done.
    ///
    /// Why the remaining stops are safe: stopping before a job's Admission
    /// leaves the root exactly as it is between a bind ack and the tail's
    /// first submission, which every bind already passes through. Past the
    /// prefix, the standalone loop already lets requests (configures and edits
    /// included) run between units. A writer that runs meanwhile either keeps
    /// the configure generation (an equivalent rebind, an LSP-path update or
    /// an edit, none of which touch what the remaining stages read) or
    /// advances it, in which case `drop_superseded_parked_continuations`
    /// discards the rest and the newer configure's own tail repeats every
    /// root-scoped stage.
    fn can_step_aside(&self) -> bool {
        self.jobs.front().is_none_or(|continuation| {
            continuation.stage == ConfigureMaintenanceStage::Admission
                || !continuation.stage.is_non_yielding_prefix()
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConfigureMaintenanceUnitResult {
    Continue,
    Complete,
    CancelAll,
}

pub(crate) fn standalone_configure_maintenance_pending(
    ctx: &AppContext,
    state: &mut ConfigureMaintenanceState,
) -> bool {
    state.absorb_enqueued(ctx);
    state.has_ready_job() || (state.jobs.is_empty() && ctx.provisional_identity_retry().is_due())
}

/// Finish request-critical configure work before standalone dispatch resumes.
/// Cooperative suffixes retain their relative queue order while each pending
/// prefix advances through bash replay and reminder configuration.
pub(crate) fn drain_standalone_configure_prefix(
    ctx: &AppContext,
    state: &mut ConfigureMaintenanceState,
) -> bool {
    state.absorb_enqueued(ctx);
    let jobs_to_visit = state.jobs.len();
    let mut ran_prefix = false;

    for _ in 0..jobs_to_visit {
        let Some(mut continuation) = state.jobs.pop_front() else {
            break;
        };
        if !continuation.stage.is_non_yielding_prefix() {
            state.jobs.push_back(continuation);
            continue;
        }
        ran_prefix = true;

        loop {
            if ctx.subc_unbound_quiesced() {
                cancel_unbound_configure_jobs(
                    ctx,
                    std::iter::once(continuation.job)
                        .chain(state.jobs.drain(..).map(|pending| pending.job))
                        .chain(ctx.drain_configure_maintenance()),
                );
                return ran_prefix;
            }

            match run_configure_maintenance_unit(ctx, &mut continuation) {
                ConfigureMaintenanceUnitResult::Continue
                    if continuation.stage.is_non_yielding_prefix() => {}
                ConfigureMaintenanceUnitResult::Continue => {
                    state.jobs.push_back(continuation);
                    break;
                }
                ConfigureMaintenanceUnitResult::Complete => break,
                ConfigureMaintenanceUnitResult::CancelAll => {
                    cancel_unbound_configure_jobs(
                        ctx,
                        std::iter::once(continuation.job)
                            .chain(state.jobs.drain(..).map(|pending| pending.job))
                            .chain(ctx.drain_configure_maintenance()),
                    );
                    return ran_prefix;
                }
            }
        }
    }

    ran_prefix
}

/// Run one unit of deferred configure work after sending the acknowledgement.
/// In standalone mode, returning after each unit lets newly queued stdin requests
/// run before more maintenance work.
pub(crate) fn drain_deferred_configure_maintenance_unit(
    ctx: &AppContext,
    state: &mut ConfigureMaintenanceState,
) -> bool {
    state.absorb_enqueued(ctx);
    if !state.select_ready_job(ctx) {
        return false;
    }
    let Some(mut continuation) = state.jobs.pop_front() else {
        return false;
    };

    if ctx.subc_unbound_quiesced() {
        cancel_unbound_configure_jobs(
            ctx,
            std::iter::once(continuation.job)
                .chain(state.jobs.drain(..).map(|pending| pending.job))
                .chain(ctx.drain_configure_maintenance()),
        );
        return false;
    }

    let result = run_configure_maintenance_unit(ctx, &mut continuation);
    match result {
        ConfigureMaintenanceUnitResult::Continue => {
            // Keep one delayed continuation per stage/root/harness. Replay and
            // callgraph admission retries must not discard each other's work.
            let redundant_retry = continuation.retry_at().is_some()
                && state.jobs.iter().any(|pending| {
                    pending.retry_at().is_some()
                        && pending.stage == continuation.stage
                        && pending.job.harness_dir == continuation.job.harness_dir
                        && pending.job.root_path == continuation.job.root_path
                        && pending.job.generation >= continuation.job.generation
                });
            if !redundant_retry {
                if continuation.retry_at().is_some() {
                    state.jobs.retain(|pending| {
                        pending.retry_at().is_none()
                            || pending.stage != continuation.stage
                            || pending.job.harness_dir != continuation.job.harness_dir
                            || pending.job.root_path != continuation.job.root_path
                    });
                }
                state.jobs.push_front(continuation);
            }
        }
        ConfigureMaintenanceUnitResult::Complete => {}
        ConfigureMaintenanceUnitResult::CancelAll => {
            cancel_unbound_configure_jobs(
                ctx,
                std::iter::once(continuation.job)
                    .chain(state.jobs.drain(..).map(|pending| pending.job))
                    .chain(ctx.drain_configure_maintenance()),
            );
            return false;
        }
    }

    state.has_ready_job()
}

/// View store opens, manifest scans and legacy migration can take minutes on
/// slow storage. Only install their completed snapshot on the request thread;
/// an obsolete configure's worker never gets to replace the active snapshot.
pub(crate) fn drain_standalone_configure_unit(
    ctx: &Arc<AppContext>,
    state: &mut ConfigureMaintenanceState,
) -> bool {
    state.absorb_enqueued(ctx);
    drop_superseded_parked_continuations(ctx, state);
    if state.jobs.is_empty() && crate::provisional_identity_retry::drain_inline(ctx) {
        // The retry replayed the bind on this request thread, where the
        // standalone loop runs every configure request. Its tail is queued on
        // the context and runs from the next unit, as after a bind request.
        return true;
    }
    if !state.select_ready_job(ctx) {
        return false;
    }
    drain_deferred_configure_maintenance_unit(ctx, state)
}

/// Drain deferred configure work on the calling thread. For callers that
/// also run configure requests on this thread (in-process embedders, the
/// warmup command, tests): a bind the identity retry replays here runs on
/// that same thread, serialized with their other configure requests. The
/// attached daemon uses [`drain_deferred_configure_maintenance_yielding`]
/// instead, which never replays a bind; the daemon submits the replay
/// through the executor as a route bind.
pub fn drain_deferred_configure_maintenance(ctx: &AppContext) {
    if ctx.configure_tail_idle() {
        crate::provisional_identity_retry::drain_inline(ctx);
    }
    drain_configure_tail(ctx, false);
}

/// Drain deferred configure work, stepping aside when an interactive writer
/// (a route-bind configure or a tool edit) queues on this root's actor.
///
/// The drain runs as one maintenance job holding the actor's epoch read gate,
/// and a queued writer cannot start until that gate is free. Across a daemon
/// restart one root's tail (artifact loads, storage sweeps, watcher start) ran
/// for over ten seconds while the next session's bind for the same root
/// waited behind it, past the daemon's relay deadline. Between units the drain
/// now checks for a waiting writer and, at a point where no request-critical
/// stage is half done, parks the remaining work on the context and returns
/// `true`. The caller requeues the drain; it resumes where it stopped once the
/// writer has run.
///
/// Off an executor worker no writer can be observed, so this drains fully.
pub(crate) fn drain_deferred_configure_maintenance_yielding(ctx: &AppContext) -> bool {
    drain_configure_tail(ctx, true)
}

fn drain_configure_tail(ctx: &AppContext, allow_step_aside: bool) -> bool {
    let mut state = ctx.take_parked_configure_tail().unwrap_or_default();
    drop_superseded_parked_continuations(ctx, &mut state);
    state.replay_retry_cutoff = Some(Instant::now());
    while drain_deferred_configure_maintenance_unit(ctx, &mut state) {
        if allow_step_aside
            && crate::executor::current_actor_writer_waiting()
            && state.can_step_aside()
        {
            state.replay_retry_cutoff = None;
            ctx.park_configure_tail(state);
            return true;
        }
    }
    if !state.jobs.is_empty() {
        // Backoff is pending, not runnable work. Keep the cursor's continuation
        // for a later scheduler tick instead of immediately re-enqueuing it.
        state.replay_retry_cutoff = None;
        ctx.park_configure_tail(state);
    }
    false
}

/// A configure that ran while a tail was parked may have replaced the
/// generation the parked work belongs to. That configure queued its own tail,
/// which repeats every root-scoped stage, so a started continuation from the
/// older generation has nothing left to do. Its request-critical prefix (bash
/// replay included) already ran before it parked, so the session binding
/// stays. Unstarted continuations keep their normal Admission-stage handling.
fn drop_superseded_parked_continuations(ctx: &AppContext, state: &mut ConfigureMaintenanceState) {
    let current = ctx.configure_generation();
    state.jobs.retain(|continuation| {
        continuation.stage == ConfigureMaintenanceStage::Admission
            || continuation.job.generation == current
    });
}

fn run_configure_maintenance_unit(
    ctx: &AppContext,
    continuation: &mut ConfigureMaintenanceContinuation,
) -> ConfigureMaintenanceUnitResult {
    let stage = continuation.stage;
    ctx.note_configure_tail_stage(Some(stage.label()));
    if !matches!(stage, ConfigureMaintenanceStage::DatabaseRuntime) {
        wait_on_configure_tail_stage_gate_for_test(&continuation.job.canonical_cache_root, stage);
    }
    // Maintenance units run on whatever executor thread drains the tail, and
    // that thread's log session is left over from its last request. Tag every
    // line this unit logs with the session whose configure queued the job, so
    // a callgraph rebuild is never attributed to an unrelated session.
    let session = Some(continuation.job.session_id.clone());
    let result = log_ctx::with_session(session, || {
        run_configure_maintenance_unit_inner(ctx, continuation)
    });
    ctx.note_configure_tail_stage(None);
    record_configure_unit_for_test(&continuation.job, stage);
    result
}

/// A test hook that holds one root's configure tail at the start of one stage,
/// standing in for a stage that runs long on a large root.
#[cfg(test)]
struct ConfigureTailStageGate {
    root: PathBuf,
    stage: &'static str,
    reached: crossbeam_channel::Sender<()>,
    release: crossbeam_channel::Receiver<()>,
    worker: Option<(thread::JoinHandle<()>, crossbeam_channel::Receiver<()>)>,
}

#[cfg(test)]
fn configure_tail_stage_gates() -> &'static std::sync::Mutex<Vec<ConfigureTailStageGate>> {
    static GATES: std::sync::OnceLock<std::sync::Mutex<Vec<ConfigureTailStageGate>>> =
        std::sync::OnceLock::new();
    GATES.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// Removes its root's stage gate when dropped.
#[cfg(test)]
pub(crate) struct ConfigureTailStageGateGuard {
    root: PathBuf,
}

#[cfg(test)]
impl ConfigureTailStageGateGuard {
    pub(crate) fn join_storage_sweep_worker(&self) {
        let worker = configure_tail_stage_gates()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter_mut()
            .find(|gate| gate.root == self.root)
            .and_then(|gate| gate.worker.take());
        if let Some((worker, finished)) = worker {
            // The storage stage outlives the executor's tail response. Reap its
            // worker before the test removes the root and storage directories.
            let completed = finished.recv_timeout(Duration::from_secs(30));
            if completed.is_ok() {
                let result = worker.join();
                if !thread::panicking() {
                    result.expect("storage sweep worker panicked");
                }
            } else if !thread::panicking() {
                panic!("storage sweep worker did not finish: {completed:?}");
            }
        }
    }
}

#[cfg(test)]
impl Drop for ConfigureTailStageGateGuard {
    fn drop(&mut self) {
        self.join_storage_sweep_worker();
        configure_tail_stage_gates()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|gate| gate.root != self.root);
    }
}

/// Hold the configure tail of `root` (its canonical path) when it reaches
/// `stage`, a [`ConfigureMaintenanceStage`] label such as `"view_load"`, or
/// `"view_payload_check"` for the payload check inside the view load. The
/// first receiver fires once the tail is held; a send on the returned sender
/// lets it continue.
#[cfg(test)]
pub(crate) fn gate_configure_tail_stage_for_test(
    root: PathBuf,
    stage: &'static str,
) -> (
    ConfigureTailStageGateGuard,
    crossbeam_channel::Receiver<()>,
    crossbeam_channel::Sender<()>,
) {
    let (reached_tx, reached_rx) = crossbeam_channel::bounded(1);
    let (release_tx, release_rx) = crossbeam_channel::bounded(1);
    configure_tail_stage_gates()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(ConfigureTailStageGate {
            root: root.clone(),
            stage,
            reached: reached_tx,
            release: release_rx,
            worker: None,
        });
    (ConfigureTailStageGateGuard { root }, reached_rx, release_tx)
}

#[cfg(test)]
fn wait_on_configure_tail_stage_gate_for_test(root: &Path, stage: ConfigureMaintenanceStage) {
    wait_on_configure_tail_gate_for_test(root, stage.label(), false);
}

/// Holds the caller while a test gate for `root` and `label` is installed:
/// signals that the gate was reached, then waits for the release. With
/// `test_release_only`, only the test may release it; otherwise a 60 s timer
/// also does, so a failed test never leaves a worker parked for good.
#[cfg(test)]
fn wait_on_configure_tail_gate_for_test(root: &Path, label: &str, test_release_only: bool) {
    let gate = configure_tail_stage_gates()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .find(|gate| gate.root == root && gate.stage == label)
        .map(|gate| (gate.reached.clone(), gate.release.clone()));
    if let Some((reached, release)) = gate {
        let _ = reached.try_send(());
        if test_release_only {
            // Only the test may release the sweep: a timer must not let an
            // admission reply pass after the supposedly held worker resumes.
            let _ = release.recv();
        } else {
            let _ = release.recv_timeout(Duration::from_secs(60));
        }
    }
}

#[cfg(not(test))]
fn wait_on_configure_tail_stage_gate_for_test(_root: &Path, _stage: ConfigureMaintenanceStage) {}

#[cfg(test)]
fn wait_on_configure_replay_gate_for_test(root: &Path, label: &str) {
    wait_on_configure_tail_gate_for_test(root, label, false);
}

#[cfg(not(test))]
fn wait_on_configure_replay_gate_for_test(_root: &Path, _label: &str) {}

/// One finished configure-tail unit, as recorded for tests: the job's root and
/// generation, the stage that ran, and when it finished.
#[cfg(test)]
#[derive(Clone, Debug)]
pub(crate) struct ConfigureUnitRecord {
    pub(crate) root: PathBuf,
    pub(crate) generation: u64,
    pub(crate) stage: &'static str,
    pub(crate) finished_at: std::time::Instant,
}

#[cfg(test)]
fn configure_unit_records() -> &'static std::sync::Mutex<Vec<ConfigureUnitRecord>> {
    static RECORDS: std::sync::OnceLock<std::sync::Mutex<Vec<ConfigureUnitRecord>>> =
        std::sync::OnceLock::new();
    RECORDS.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

#[cfg(test)]
fn record_configure_unit_for_test(job: &ConfigureMaintenanceJob, stage: ConfigureMaintenanceStage) {
    configure_unit_records()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(ConfigureUnitRecord {
            root: job.canonical_cache_root.clone(),
            generation: job.generation,
            stage: stage.label(),
            finished_at: std::time::Instant::now(),
        });
}

#[cfg(not(test))]
fn record_configure_unit_for_test(
    _job: &ConfigureMaintenanceJob,
    _stage: ConfigureMaintenanceStage,
) {
}

/// Configure-tail units that ran for `root`, in order. Tests run in parallel,
/// so every test reads only its own roots.
#[cfg(test)]
pub(crate) fn configure_unit_records_for_root(root: &Path) -> Vec<ConfigureUnitRecord> {
    configure_unit_records()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter(|record| record.root == root)
        .cloned()
        .collect()
}

fn run_configure_maintenance_unit_inner(
    ctx: &AppContext,
    continuation: &mut ConfigureMaintenanceContinuation,
) -> ConfigureMaintenanceUnitResult {
    let job = &continuation.job;
    match continuation.stage {
        ConfigureMaintenanceStage::Admission => {
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
                forget_configure_job_binding(ctx, job);
                return ConfigureMaintenanceUnitResult::Complete;
            }

            let session_only = job.run_bash_replay
                && !job.configure_database_runtime
                && !job.format_tool_cache_clear_needed
                && !job.refresh_project_runtime
                && !job.sync_bash_compress_flag
                && !job.reset_filter_registry
                && !job.clear_failed_spawns
                && !job.warm_callgraph_store;
            if session_only {
                if ctx
                    .database_runtime_refusal("configure-session-replay", "bash_drain_completions")
                    .is_some()
                {
                    forget_configure_job_binding(ctx, job);
                    return ConfigureMaintenanceUnitResult::Complete;
                }
                if let Err(result) = replay_configure_session(ctx, job) {
                    return result;
                }
                if ctx
                    .bash_background()
                    .project_replay_pending(&job.harness_dir, &job.root_path)
                {
                    continuation.stage = ConfigureMaintenanceStage::Status;
                } else {
                    return ConfigureMaintenanceUnitResult::Complete;
                }
                return ConfigureMaintenanceUnitResult::Continue;
            }

            delay_configure_deferred_maintenance_for_test(&job.root_path);
            // Start artifact workers only after sending the configure acknowledgement,
            // and only if this job's configure generation is still active.
            if ctx
                .run_if_subc_bound_generation(job.generation, || {
                    if job.supersede_search_artifact_persistence
                        && job.supersede_semantic_artifact_persistence
                    {
                        ctx.next_semantic_persist_epoch();
                    }
                    if job.supersede_callgraph_artifact_persistence {
                        ctx.next_callgraph_persist_epoch();
                    }
                })
                .is_none()
            {
                if ctx.subc_unbound_quiesced() {
                    return ConfigureMaintenanceUnitResult::CancelAll;
                }
                forget_configure_job_binding(ctx, job);
                return ConfigureMaintenanceUnitResult::Complete;
            }

            if job.format_tool_cache_clear_needed {
                crate::format::clear_tool_cache_for_root(Some(&job.root_path));
            }
            if let Some(storage_dir) = ctx.config().storage_dir.clone() {
                if let Err(err) = crate::private_storage::open_root(&storage_dir) {
                    slog_warn!(
                        "failed to create storage directory {}: {}",
                        storage_dir.display(),
                        err
                    );
                }
                ctx.backup().lock().set_storage_dir_for_harness(
                    storage_dir.clone(),
                    job.harness.clone(),
                    ctx.config().checkpoint_ttl_hours,
                );
                ctx.checkpoint()
                    .lock()
                    .set_storage_dir_for_harness(storage_dir, job.harness.clone());
            }
            continuation.stage = ConfigureMaintenanceStage::DatabaseRuntime;
        }
        ConfigureMaintenanceStage::DatabaseRuntime => {
            // Opening SQLite can wait on another root's open or on a database
            // writer, and can run migrations. Bind readiness does not depend on
            // persistence, but session replay must use the selected database.
            // Under subc the dedicated database-open thread usually opened it
            // right after the bind reply; this runs the open only if nothing
            // did, and otherwise waits for the running open to publish.
            if job.configure_database_runtime {
                crate::database_open::run_staged_open(
                    ctx,
                    crate::database_open::DatabaseOpenRunner::ConfigureTail,
                    true,
                );
                if ctx.database_runtime_failed() {
                    forget_configure_job_binding(ctx, job);
                    return ConfigureMaintenanceUnitResult::Complete;
                }
            }
            continuation.stage = ConfigureMaintenanceStage::SessionReplay;
        }
        ConfigureMaintenanceStage::SessionReplay => {
            if job.run_bash_replay {
                if let Err(result) = replay_configure_session(ctx, job) {
                    return result;
                }
            }
            continuation.stage = ConfigureMaintenanceStage::BashRuntime;
        }
        ConfigureMaintenanceStage::BashRuntime => {
            let config = ctx.config();
            ctx.bash_background().configure_long_running_reminders(
                config.bash_long_running_reminder_enabled,
                config.bash_long_running_reminder_interval_ms,
            );
            drop(config);

            continuation.stage = ConfigureMaintenanceStage::ProjectRuntime;
        }
        ConfigureMaintenanceStage::ProjectRuntime => {
            if job.refresh_project_runtime {
                // Rebuild the watcher matcher outside the acknowledgement path. Keep
                // this project walk separate so queued requests can run before artifact
                // loading and callgraph work.
                if !job.root_too_broad {
                    ctx.rebuild_gitignore();
                } else {
                    ctx.clear_gitignore();
                }
            }
            continuation.stage = ConfigureMaintenanceStage::Watcher;
        }
        ConfigureMaintenanceStage::Watcher => {
            if job.refresh_project_runtime {
                let gitignore_generation = ctx.gitignore_generation().load(Ordering::SeqCst);
                if !ctx.watcher_runtime_matches(&job.canonical_cache_root, gitignore_generation) {
                    if ctx
                        .run_if_subc_bound_generation(job.generation, || ())
                        .is_none()
                    {
                        if ctx.subc_unbound_quiesced() {
                            return ConfigureMaintenanceUnitResult::CancelAll;
                        }
                        forget_configure_job_binding(ctx, job);
                        return ConfigureMaintenanceUnitResult::Complete;
                    }

                    // Joining a prior FSEvents thread can block in the OS. Do that
                    // outside lifecycle admission, then reacquire admission only for
                    // the atomic watcher start. An unbind or superseding configure that
                    // wins during the join prevents the replacement from being started.
                    ctx.stop_watcher_runtime();
                    if ctx
                        .run_if_subc_bound_generation(job.generation, || {
                            if !job.root_too_broad {
                                start_project_watcher(ctx, &job.canonical_cache_root);
                            }
                        })
                        .is_none()
                    {
                        if ctx.subc_unbound_quiesced() {
                            return ConfigureMaintenanceUnitResult::CancelAll;
                        }
                        forget_configure_job_binding(ctx, job);
                        return ConfigureMaintenanceUnitResult::Complete;
                    }
                }
            }
            // The project watcher may or may not cover the project config file
            // now; start or stop the root's own config file watch to match.
            crate::config_live::sync_config_watches(ctx);
            continuation.stage = ConfigureMaintenanceStage::ProcessFlags;
        }
        ConfigureMaintenanceStage::ProcessFlags => {
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

            continuation.stage = ConfigureMaintenanceStage::Status;
        }
        ConfigureMaintenanceStage::Status => {
            // The cursor belongs to the root, not one configure generation.
            // A newer tail also finishes a scan started by a superseded tail.
            let batch = match claim_configure_replay_batch(ctx, job) {
                Ok(batch) => batch,
                Err(result) => return result,
            };
            let retry_due_failures = continuation.replay_retry_at.take().is_some();
            match batch.continue_project_replay(
                &job.harness_dir,
                &job.harness,
                &job.session_id,
                &job.root_path,
                retry_due_failures,
            ) {
                Ok(true) => {
                    continuation.replay_error_logged = false;
                    continuation.replay_retry_at = ctx.bash_background().project_replay_retry_at();
                    ctx.signal_status_changed();
                    return ConfigureMaintenanceUnitResult::Continue;
                }
                Ok(false) => {}
                Err(error) => {
                    if !continuation.replay_error_logged {
                        slog_warn!("failed to continue background bash replay: {error}");
                        continuation.replay_error_logged = true;
                    }
                    continuation.replay_retry_at = Some(Instant::now() + Duration::from_secs(5));
                    return ConfigureMaintenanceUnitResult::Continue;
                }
            }
            ctx.signal_status_changed();
            return ConfigureMaintenanceUnitResult::Complete;
        }
    }

    ConfigureMaintenanceUnitResult::Continue
}

#[cfg(test)]
thread_local! {
    static VIEW_WORKING_TREE_HASHES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

struct ConfigureViewInput {
    family: String,
    git_common_dir: Option<PathBuf>,
    callgraph: bool,
    /// A linked worktree that has not needed its call graph yet; see
    /// `AppContext::defer_callgraph_build`.
    defer_callgraph: bool,
    read_only: bool,
    semantic: SemanticBackendConfig,
    limiter: Arc<crate::cold_build_limiter::ColdBuildLimiter>,
}

#[cfg(test)]
mod tests {

    #[test]
    fn watcher_spawn_failure_leaves_runtime_uninstalled() {
        let root = tempfile::tempdir().unwrap();
        let ctx = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config::default(),
        );
        super::start_project_watcher_with_spawn(
            &ctx,
            root.path(),
            Vec::new(),
            |_root, _extra, _tx| -> Result<(), &'static str> {
                panic!("attachment must not run without a worker thread")
            },
            |_builder, _job| Err(std::io::Error::other("injected thread exhaustion")),
        );
        assert!(!ctx.watcher_runtime_active());
        assert!(ctx.watcher_rx().lock().is_none());
        assert!(ctx.watcher().lock().is_none());
    }
    #[test]
    fn prepared_cache_reuse_preserves_unknown_and_opt_out_intent() {
        let candidate = crate::config::Config::default();
        let mut prepared = candidate.clone();
        assert!(super::prepared_cache_matches(&prepared, &candidate));
        prepared.cache_config_unknown = Some("invalid user file".into());
        assert!(!super::prepared_cache_matches(&prepared, &candidate));
        prepared.cache_config_unknown = None;
        prepared.cache_dir_opt_out = true;
        assert!(!super::prepared_cache_matches(&prepared, &candidate));
    }

    #[test]
    fn prepared_cache_warning_accepts_an_absent_cache_directory() {
        let prepared = crate::config::Config {
            cache_unavailable: Some("cache location is unknown".into()),
            ..Default::default()
        };
        let storage = std::env::temp_dir().join("durable-warning-root");
        let warning = super::prepared_cache_warning(&prepared, &storage).unwrap();
        assert_eq!(warning.value, storage.display().to_string());
    }
    use serde_json::{json, Value};

    use std::collections::BTreeSet;
    use std::ffi::OsString;
    use std::fs;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc, Barrier, Condvar, Mutex, OnceLock, RwLock};
    use std::time::{Duration, Instant};

    use super::{
        configure_artifact_post_gate_reached_for_test, configure_deferred_delay_reached_for_test,
        external_ignore_watch_paths, handle_configure, install_project_watcher_with,
        only_lsp_process_state_changed, parse_lsp_paths_extra, project_root_is_too_broad,
        release_callgraph_start_waiters_for_generation_change,
        release_configure_artifact_post_gate_for_test,
        reset_configure_deferred_delay_reached_for_test,
        set_configure_artifact_post_gate_delay_for_test, should_clear_failed_spawns,
        slow_configure_prefix_line, validate_storage_dir, ConfigureMaintenanceStage,
        INDEX_ORDER_GRACE_MS, INDEX_ORDER_TEST_LOCK, INDEX_ORDER_TIMEOUT_LOGS, WATCHER_GENERATION,
    };
    use crate::cache_freshness::{self, VerifyArtifact, WarmVerifyPlan};
    use crate::config::{Config, SemanticBackend, SemanticBackendConfig};
    use crate::context::{
        App, AppContext, CallgraphStoreAccess, SemanticRefreshEvent, SemanticRefreshRequest,
    };
    use crate::parser::{FileParser, SymbolCache, TreeSitterProvider};
    use crate::protocol::{ConfigureWarningsFrame, PushFrame, RawRequest, Response};
    use crate::search_index::{CacheLock, SearchIndex};
    use crate::semantic_index::{SemanticIndex, SemanticIndexFingerprint};
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

    fn bash_background_replay_fixture(
        count: usize,
        budget: Option<usize>,
    ) -> (
        tempfile::TempDir,
        TestContext,
        super::ConfigureMaintenanceState,
        Vec<String>,
    ) {
        use crate::bash_background::persistence::{create_task_layout, write_task, PersistedTask};
        #[cfg(unix)]
        {
            // Hundreds of recovered tasks retain pinned directory handles.
            // Leave headroom for sibling tests, and never lower their limit.
            let mut limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            assert_eq!(
                unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) },
                0
            );
            limit.rlim_cur = limit.rlim_cur.max(8192.min(limit.rlim_max));
            assert!(
                limit.rlim_cur >= 4096,
                "replay fixture needs descriptor headroom"
            );
            assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
        }
        let project = tempfile::tempdir().unwrap();
        init_git_fixture(project.path());
        let ctx = test_context();
        let response = handle_configure_for_test(
            &configure_request_with_session(
                json!({
                    "project_root": project.path(),
                    "storage_dir": ctx._storage.path(),
                    "harness": "opencode",
                    "config": [user_tier(json!({
                        "indexes": {"trigram": false, "semantic": false, "callgraph": false},
                        "views": {"enabled": false}
                    }))]
                }),
                "bounded-recovery",
            ),
            &ctx,
        );
        assert!(response.success, "{response:?}");
        if let Some(budget) = budget {
            ctx.bash_background()
                .set_project_replay_scan_budget_for_test(budget, Duration::from_secs(5));
        }
        let storage = crate::bash_background::task_storage_dir(&ctx);
        let task_ids: Vec<_> = (0..count).map(|i| format!("bash-{i:016x}")).collect();
        for task_id in &task_ids {
            let layout = create_task_layout(&storage, "bounded-recovery", task_id).unwrap();
            let paths = &layout.paths;
            let mut metadata = PersistedTask::starting(
                task_id.clone(),
                "bounded-recovery".into(),
                "printf recovered".into(),
                project.path().into(),
                Some(project.path().into()),
                Some(30000),
                true,
                true,
            );
            metadata.mark_terminal(
                crate::bash_background::BgTaskStatus::Completed,
                Some(0),
                None,
            );
            write_task(&paths.json, &metadata).unwrap();
            fs::write(&paths.stdout, "recovered\n").unwrap();
            fs::write(&paths.stderr, "").unwrap();
        }
        let mut state = super::ConfigureMaintenanceState::default();
        assert!(super::drain_standalone_configure_prefix(&ctx, &mut state));
        // Reach the yieldable tail without manually replaying or querying task
        // status: only scheduled maintenance may discover the remaining tasks.
        for _ in 0..30 {
            if state
                .jobs
                .front()
                .is_some_and(|job| job.stage == ConfigureMaintenanceStage::Status)
            {
                return (project, ctx, state, task_ids);
            }
            assert!(super::drain_deferred_configure_maintenance_unit(
                &ctx, &mut state
            ));
        }
        panic!("configure did not reach its status stage");
    }

    #[test]
    fn bash_background_restart_runner_first_preserves_opencode_completion() {
        use crate::bash_background::persistence::{
            create_task_layout, read_task, write_task, PersistedTask,
        };

        let _env_guard = home_env_mutex();
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _disable_watcher = EnvVarGuard::set("AFT_TEST_DISABLE_FILE_WATCHER", "1");
        let project = tempfile::tempdir().unwrap();
        init_git_fixture(project.path());
        let storage = tempfile::tempdir().unwrap();
        let session = "restart-opencode-head";
        let task_id = "bash-000000000000c10d";
        let layout =
            create_task_layout(&storage.path().join("opencode"), session, task_id).unwrap();
        let paths = &layout.paths;
        let mut metadata = PersistedTask::starting(
            task_id.into(),
            session.into(),
            "printf recovered".into(),
            project.path().into(),
            Some(project.path().into()),
            None,
            true,
            true,
        );
        metadata.harness = Some("opencode".into());
        metadata.status = crate::bash_background::BgTaskStatus::Running;
        metadata.completion_has_env_secrets = Some(false);
        metadata.completion_has_non_ticket_env_secrets = Some(false);
        write_task(&paths.json, &metadata).unwrap();
        fs::write(&paths.stdout, "recovered after restart\n").unwrap();
        fs::write(&paths.stderr, "").unwrap();
        // The detached wrapper finished while AFT was down, so the metadata
        // and database had not yet been updated.
        fs::write(&paths.exit, "0").unwrap();
        {
            let db = crate::db::open(&storage.path().join("aft.db")).unwrap();
            crate::db::bash_tasks::upsert_bash_task(
                &db,
                &metadata.to_bash_task_row("opencode", paths).unwrap(),
            )
            .unwrap();
        }
        let before = fs::read(&paths.json).unwrap();
        let restarted = test_context();
        let bind = |harness: &str, session: &str| {
            let response = handle_configure_for_test(
                &configure_request_with_session(
                    json!({
                        "project_root": project.path(),
                        "storage_dir": storage.path(),
                        "harness": harness,
                        "config": [user_tier(json!({
                            "indexes": {"trigram": false, "semantic": false, "callgraph": false},
                            "views": {"enabled": false}
                        }))]
                    }),
                    session,
                ),
                &restarted,
            );
            assert!(response.success, "{response:?}");
            super::drain_deferred_configure_maintenance(&restarted);
        };
        let (_, runner_logs) =
            crate::logging::capture_log_lines(|| bind("runner", "restart-runner"));
        let runner_completions = restarted
            .bash_background()
            .drain_completions_for_session(Some("restart-runner"));
        let runner_adopted = restarted.bash_background().task_for_test(task_id).is_some();
        let untouched = fs::read(&paths.json).unwrap() == before;
        let row_after_runner = {
            let db = crate::db::open(&storage.path().join("aft.db")).unwrap();
            crate::db::bash_tasks::get_bash_task(&db, "opencode", session, task_id)
                .unwrap()
                .unwrap()
        };

        bind("opencode", session);
        let completions = restarted
            .bash_background()
            .drain_completions_for_session(Some(session));
        assert_eq!(
            completions.len(),
            1,
            "head lost its completion after runner-first replay: {runner_logs:?}"
        );
        assert_eq!(completions[0].task_id, task_id);
        assert_eq!(
            completions[0].status,
            crate::bash_background::BgTaskStatus::Completed
        );
        assert_eq!(completions[0].exit_code, Some(0));
        assert!(completions[0]
            .output_preview
            .contains("recovered after restart"));
        let recovered = read_task(&paths.json).unwrap();
        assert_eq!(
            recovered.status,
            crate::bash_background::BgTaskStatus::Completed
        );
        assert_eq!(recovered.exit_code, Some(0));
        assert!(runner_completions.is_empty());
        assert!(!runner_adopted, "runner adopted the opencode task");
        assert!(untouched, "runner rewrote opencode metadata");
        assert_eq!(row_after_runner.status, "running");
        assert!(
            !row_after_runner.completion_delivered,
            "runner acknowledged opencode completion"
        );
        assert!(
            !runner_logs.iter().any(|line| line.contains(task_id)),
            "runner inspected another harness's task: {runner_logs:?}"
        );
        restarted.bash_background().detach();
    }

    #[test]
    fn bash_background_project_replay_maintenance_recovers_all_disk_tasks() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let (_project, ctx, mut state, task_ids) = bash_background_replay_fixture(520, None);
        let registry = ctx.bash_background();
        assert!(
            task_ids
                .iter()
                .filter(|id| registry.task_for_test(id).is_some())
                .count()
                < task_ids.len()
        );
        let (_, logs) = crate::logging::capture_log_lines(|| {
            for _ in 0..1000 {
                if !super::drain_deferred_configure_maintenance_unit(&ctx, &mut state) {
                    break;
                }
            }
        });
        assert!(state.jobs.is_empty(), "the recovery scan must complete");
        assert_eq!(
            task_ids
                .iter()
                .filter(|id| registry.task_for_test(id).is_some())
                .count(),
            task_ids.len(),
            "maintenance must recover every disk-only task: {logs:?}"
        );
        let completions = registry.drain_completions();
        assert_eq!(
            completions.len(),
            task_ids.len(),
            "every recovered task needs a completion notice"
        );
        assert!(
            logs.iter().any(|line| line.contains("scan complete")),
            "completion progress must be reported: {logs:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn bash_background_project_replay_parks_unreadable_session_without_blocking_tail() {
        use crate::bash_background::persistence::{create_task_layout, write_task, PersistedTask};
        use std::os::unix::fs::PermissionsExt;

        let _git_env = crate::test_env::hermetic_git_env_guard();
        let (project, ctx, mut state, _) = bash_background_replay_fixture(0, Some(8));
        let registry = ctx.bash_background();
        let storage = crate::bash_background::task_storage_dir(&ctx);
        let write_disk_task = |session: &str, task_id: &str| {
            let layout = create_task_layout(&storage, session, task_id).unwrap();
            let mut task = PersistedTask::starting(
                task_id.into(),
                session.into(),
                "printf recovered".into(),
                project.path().into(),
                Some(project.path().into()),
                Some(30000),
                true,
                true,
            );
            task.mark_terminal(
                crate::bash_background::BgTaskStatus::Completed,
                Some(0),
                None,
            );
            write_task(&layout.paths.json, &task).unwrap();
            fs::write(&layout.paths.stdout, "recovered\n").unwrap();
            fs::write(&layout.paths.stderr, "").unwrap();
        };
        let blocked_task = "bash-000000000000feed";
        let healthy_task = "bash-000000000000beef";
        write_disk_task("bounded-recovery", blocked_task);
        write_disk_task("healthy-recovery", healthy_task);
        let blocked =
            crate::bash_background::persistence::session_tasks_dir(&storage, "bounded-recovery");
        struct RestorePermissions(PathBuf);
        impl Drop for RestorePermissions {
            fn drop(&mut self) {
                fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700)).unwrap();
            }
        }
        let _restore = RestorePermissions(blocked.clone());
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0)).unwrap();
        assert_eq!(
            fs::read_dir(&blocked).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied,
            "the reproduction requires an actually unreadable session directory"
        );
        state.jobs.front_mut().unwrap().stage = ConfigureMaintenanceStage::SessionReplay;
        let (_, first_logs) = crate::logging::capture_log_lines(|| {
            assert!(super::drain_standalone_configure_prefix(&ctx, &mut state));
            while state.jobs.front().unwrap().stage != ConfigureMaintenanceStage::Status {
                assert!(super::drain_deferred_configure_maintenance_unit(
                    &ctx, &mut state
                ));
            }
        });
        ctx.park_configure_tail(state);
        assert!(handle_configure_for_test(
            &configure_request_with_session(
                json!({
                    "project_root": project.path(), "storage_dir": ctx._storage.path(), "harness": "opencode",
                    "config": [user_tier(json!({
                        "indexes": {"trigram": false, "semantic": false, "callgraph": false},
                        "views": {"enabled": false}
                    }))]
                }),
                "healthy-recovery",
            ),
            &ctx,
        ).success);

        let (finished_while_unreadable, logs) = std::thread::scope(|scope| {
            let (done_tx, done_rx) = mpsc::channel();
            let ctx = &ctx;
            let worker = scope.spawn(move || {
                let (_, logs) = crate::logging::capture_log_lines(|| {
                    super::drain_deferred_configure_maintenance(ctx);
                });
                done_tx.send(()).unwrap();
                logs
            });
            let finished = done_rx.recv_timeout(Duration::from_secs(2)).is_ok();
            if !finished {
                // Release a spinning tail before joining, so a regression fails
                // without leaving an infinite worker running in the test suite.
                fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700)).unwrap();
            }
            (finished, worker.join().unwrap())
        });
        assert!(finished_while_unreadable,
            "configure tail held its worker on an unreadable session; later configure jobs cannot finish; failed scans logged {} times",
            logs.iter().filter(|line| line.contains("failed to scan Bash task session")).count());
        assert!(
            fs::read_dir(&blocked).is_err(),
            "the failure must remain persistent throughout the drain"
        );
        assert!(
            registry.task_for_test(healthy_task).is_some(),
            "the later configure must recover its disk task"
        );
        assert!(registry.task_for_test(blocked_task).is_none());
        let root_health = serde_json::to_value(ctx.try_health_snapshot(project.path())).unwrap();
        let health = &root_health["bash"];
        assert_eq!(
            health["project_replay_failures"][0]["session_dir"],
            json!(blocked)
        );
        assert_eq!(
            health["project_replay_failures"][0]["reason"],
            "session_directory_permission_denied"
        );
        assert_eq!(health["project_replay_failures"][0]["attempts"], 1);
        assert_eq!(
            first_logs
                .iter()
                .chain(&logs)
                .filter(|line| line.contains("failed to scan Bash task session"))
                .count(),
            1
        );
        let mut parked = ctx.take_parked_configure_tail().unwrap();
        assert_eq!(
            parked.jobs.len(),
            1,
            "later configure jobs must finish, not queue duplicate retries"
        );
        assert_eq!(
            parked.jobs.front().unwrap().job.session_id,
            "bounded-recovery"
        );
        let retry_at = parked.jobs.front().unwrap().replay_retry_at.unwrap();
        parked.replay_retry_cutoff = Some(
            retry_at
                .checked_sub(Duration::from_nanos(1))
                .expect("the backoff deadline must be after the test cutoff"),
        );
        ctx.park_configure_tail(parked);
        assert!(
            !ctx.configure_tail_has_work(),
            "a retry before its backoff deadline must not occupy a maintenance worker"
        );
        let mut parked = ctx.take_parked_configure_tail().unwrap();
        let examined = registry.project_replay_scan_total_examined_for_test();
        assert!(
            !super::drain_deferred_configure_maintenance_unit(&ctx, &mut parked),
            "another maintenance unit before the deadline must not retry the failed scan"
        );
        assert_eq!(
            registry.project_replay_scan_total_examined_for_test(),
            examined,
            "another maintenance unit before the deadline must not retry the failed scan"
        );
        ctx.park_configure_tail(parked);

        let mut parked = ctx.take_parked_configure_tail().unwrap();
        parked.replay_retry_cutoff = Some(retry_at);
        ctx.park_configure_tail(parked);
        assert!(
            ctx.configure_tail_has_work(),
            "a retry is due at its backoff deadline"
        );
        let mut parked = ctx.take_parked_configure_tail().unwrap();
        parked.replay_retry_cutoff = None;
        ctx.park_configure_tail(parked);

        let make_retry_due = || {
            registry.expire_project_replay_backoff_for_test();
            let mut parked = ctx.take_parked_configure_tail().unwrap();
            for continuation in &mut parked.jobs {
                continuation.replay_retry_at = Some(Instant::now());
            }
            ctx.park_configure_tail(parked);
            assert!(
                ctx.configure_tail_has_work(),
                "the later tick must admit a due retry"
            );
        };
        for attempt in 2..=6 {
            make_retry_due();
            let (_, retry_logs) = crate::logging::capture_log_lines(|| {
                super::drain_deferred_configure_maintenance(&ctx);
            });
            let health = registry.try_health_counts().unwrap();
            let failure = &health.project_replay_failures[0];
            assert_eq!(failure.attempts, attempt, "each tick retries exactly once");
            let expected_ms = (500u64 << (attempt - 1)).min(5000);
            assert!(
                failure.retry_after_ms > expected_ms - 250 && failure.retry_after_ms <= expected_ms,
                "retry backoff must double up to five seconds: {failure:?}"
            );
            assert!(
                !retry_logs
                    .iter()
                    .any(|line| line.contains("failed to scan Bash task session")),
                "a persistent failure must log only once per streak: {retry_logs:?}"
            );
        }
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700)).unwrap();
        make_retry_due();
        super::drain_deferred_configure_maintenance(&ctx);
        assert!(
            registry.task_for_test(blocked_task).is_some(),
            "a later tick must recover the repaired session"
        );
        assert!(registry
            .try_health_counts()
            .unwrap()
            .project_replay_failures
            .is_empty());
        assert!(ctx.take_parked_configure_tail().is_none());
    }

    #[test]
    fn bash_background_project_replay_retry_eligibility_is_frozen_for_each_drain() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let (_project, ctx, mut state, _) = bash_background_replay_fixture(0, None);
        let now = Instant::now();
        state.replay_retry_cutoff = Some(now - Duration::from_secs(10));
        state.jobs.front_mut().unwrap().replay_retry_at = Some(now - Duration::from_secs(5));
        assert!(
            !state.has_ready_job(),
            "a retry becoming due during a drain must wait for the next tick"
        );
        assert!(!state.select_ready_job(&ctx));
        state.replay_retry_cutoff = None;
        assert!(
            state.has_ready_job(),
            "the later maintenance probe must see the due retry"
        );
        assert!(state.select_ready_job(&ctx));
    }

    #[test]
    fn bash_background_project_replay_maintenance_stops_on_unbind() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let (_project, ctx, mut state, task_ids) = bash_background_replay_fixture(40, Some(8));
        let registry = ctx.bash_background();
        let before = registry
            .project_replay_scan_total_examined_for_test()
            .unwrap();
        assert!(
            super::drain_deferred_configure_maintenance_unit(&ctx, &mut state),
            "an incomplete scan must keep maintenance scheduled"
        );
        let after = registry
            .project_replay_scan_total_examined_for_test()
            .unwrap();
        assert!(
            after > before,
            "the continuation must actually scan before unbind"
        );
        assert!(task_ids
            .iter()
            .any(|id| registry.task_for_test(id).is_none()));
        ctx.mark_subc_unbound();
        assert!(!super::drain_deferred_configure_maintenance_unit(
            &ctx, &mut state
        ));
        assert_eq!(
            registry.project_replay_scan_total_examined_for_test(),
            Some(after),
            "an unbound root must not scan another entry"
        );
        assert!(
            state.jobs.is_empty(),
            "unbound maintenance must be cancelled"
        );
    }

    fn assert_admitted_replay_finishes_after_unbind(stage: ConfigureMaintenanceStage) {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let (_project, ctx, mut state, _task_ids) = bash_background_replay_fixture(40, Some(8));
        state.jobs.front_mut().unwrap().stage = stage;
        let registry = ctx.bash_background();
        let before = registry
            .project_replay_scan_total_examined_for_test()
            .unwrap();
        let (gate, reached, release) =
            super::gate_configure_tail_stage_for_test(ctx.canonical_cache_root(), "replay_claimed");
        let ctx_ref = &*ctx;
        let (mut state, unbound) = std::thread::scope(|scope| {
            let worker = scope.spawn(move || {
                super::drain_deferred_configure_maintenance_unit(ctx_ref, &mut state);
                state
            });
            reached.recv_timeout(Duration::from_secs(10)).unwrap();
            assert!(
                registry.claim_project_replay_batch().is_none(),
                "the admitted batch must own the registry's replay reservation"
            );
            let (unbound_tx, unbound_rx) = mpsc::channel();
            scope.spawn(move || {
                ctx_ref.mark_subc_unbound();
                unbound_tx.send(()).unwrap();
            });
            // Unbind must finish while disk/recovery work is still held. Release
            // the worker even on timeout so a bad lock boundary fails, not hangs.
            let unbound = unbound_rx.recv_timeout(Duration::from_secs(10));
            release.send(()).unwrap();
            (worker.join().unwrap(), unbound)
        });
        drop(gate);
        unbound.expect("replay disk work held the lifecycle mutex");
        let after = registry
            .project_replay_scan_total_examined_for_test()
            .unwrap();
        assert!(after > before, "an already admitted batch may finish");
        assert!(after <= before + 8, "only one bounded scan was admitted");
        assert!(registry.claim_project_replay_batch().is_some());
        assert!(!super::drain_deferred_configure_maintenance_unit(
            &ctx, &mut state
        ));
        assert_eq!(
            registry.project_replay_scan_total_examined_for_test(),
            Some(after),
            "the next batch needs fresh lifecycle admission"
        );
    }

    #[test]
    fn bash_background_admitted_initial_replay_finishes_without_blocking_unbind() {
        assert_admitted_replay_finishes_after_unbind(ConfigureMaintenanceStage::SessionReplay);
    }

    #[test]
    fn bash_background_admitted_continuation_replay_finishes_without_blocking_unbind() {
        assert_admitted_replay_finishes_after_unbind(ConfigureMaintenanceStage::Status);
    }

    #[test]
    fn bash_background_replay_busy_claim_yields_without_scanning() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let (_project, ctx, mut state, _task_ids) = bash_background_replay_fixture(40, Some(8));
        let registry = ctx.bash_background();
        let before = registry.project_replay_scan_total_examined_for_test();
        let batch = registry.claim_project_replay_batch().unwrap();
        assert!(super::drain_deferred_configure_maintenance_unit(
            &ctx, &mut state
        ));
        assert_eq!(
            registry.project_replay_scan_total_examined_for_test(),
            before
        );
        assert_eq!(
            state.jobs.front().unwrap().stage,
            ConfigureMaintenanceStage::Status
        );
        drop(batch);
        assert!(super::drain_deferred_configure_maintenance_unit(
            &ctx, &mut state
        ));
        assert!(registry.project_replay_scan_total_examined_for_test() > before);
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

    fn symbol_file(path: &std::path::Path) -> crate::search_index::FileEntry {
        let modified = std::fs::metadata(path)
            .expect("stat source")
            .modified()
            .expect("source mtime");
        crate::search_index::FileEntry {
            path: path.to_path_buf(),
            size: 0,
            modified,
            content_hash: crate::cache_freshness::zero_hash(),
        }
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
                "indexes": { "trigram": false, "semantic": false }
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
    fn views_gate_off_does_not_create_view_storage() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _disable_watcher = EnvVarGuard::set("AFT_TEST_DISABLE_FILE_WATCHER", "1");
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        init_git_fixture(project.path());
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                storage_dir: Some(storage.path().to_path_buf()),
                ..Config::default()
            },
        );
        let response = handle_configure_for_test(
            &configure_request_with_params(json!({
                "project_root": project.path(),
                "storage_dir": storage.path(),
                "harness": "opencode",
                "config": [user_tier(json!({
                    "indexes": { "trigram": false, "semantic": false, "callgraph": false }
                }))]
            })),
            &ctx,
        );
        assert!(response.success, "{}", response.data);
        super::drain_deferred_configure_maintenance(&ctx);
        assert!(!storage.path().join("views").exists());
        assert!(true);
        assert!(ctx.build_status_snapshot().get("views").is_none());
        assert!(
            serde_json::to_value(ctx.try_health_snapshot(project.path()))
                .unwrap()
                .get("views")
                .is_none()
        );
    }

    #[test]
    fn checkpoint_before_deferred_maintenance_lands_in_the_harness_namespace() {
        // A subc route accepts tool calls once configure has replied, which can
        // be before the deferred maintenance job runs. A checkpoint created in
        // that window must already be written under
        // `<storage>/opencode/checkpoints`: if it went to
        // `<storage>/unbound/checkpoints`, the later switch hid it behind an
        // older checkpoint of the same name, and restore wrote that older one.
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _disable_watcher = EnvVarGuard::set("AFT_TEST_DISABLE_FILE_WATCHER", "1");
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        init_git_fixture(project.path());
        let target = project.path().join("target.txt");
        std::fs::write(&target, "v1\n").unwrap();
        let stale_target = elsewhere.path().join("stale.txt");
        std::fs::write(&stale_target, "stale\n").unwrap();

        // A different project directory that uses the same storage directory,
        // harness and session already saved `safe-point` (covering
        // stale.txt, outside this project).
        let mut other_root_store =
            crate::checkpoint::CheckpointStore::unbound_in_for_test(storage.path());
        other_root_store.set_storage_dir_for_harness(
            storage.path().to_path_buf(),
            crate::harness::Harness::Opencode,
        );
        other_root_store
            .create(
                crate::protocol::DEFAULT_SESSION_ID,
                "safe-point",
                vec![stale_target.clone()],
                &crate::backup::BackupStore::new(),
            )
            .expect("seed the other root's checkpoint");
        drop(other_root_store);
        // Restoring the other root's checkpoint would write "stale" back here.
        std::fs::write(&stale_target, "moved on\n").unwrap();

        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                storage_dir: Some(storage.path().to_path_buf()),
                ..Config::default()
            },
        );
        ctx.replace_checkpoint_store_for_test(
            crate::checkpoint::CheckpointStore::unbound_in_for_test(storage.path()),
        );
        let response = handle_configure_for_test(
            &configure_request_with_params(json!({
                "project_root": project.path(),
                "storage_dir": storage.path(),
                "harness": "opencode",
                "config": [user_tier(json!({
                    "indexes": { "trigram": false, "semantic": false, "callgraph": false }
                }))]
            })),
            &ctx,
        );
        assert!(response.success, "{}", response.data);
        assert!(ctx.configure_tail_has_work());

        let request = |command: &str, params: serde_json::Value| RawRequest {
            id: command.to_string(),
            command: command.to_string(),
            lsp_hints: None,
            session_id: None,
            params,
        };
        let created = crate::commands::checkpoint::handle_checkpoint(
            &request(
                "checkpoint",
                json!({ "name": "safe-point", "files": [&target] }),
            ),
            &ctx,
        );
        assert!(created.success, "{}", created.data);
        assert_eq!(created.data["file_count"], 1, "{}", created.data);
        let storage_path = created.data["storage_path"].as_str().unwrap_or_default();
        assert!(
            Path::new(storage_path).starts_with(storage.path().join("opencode")),
            "checkpoint created before deferred maintenance must use the harness namespace, got {storage_path}"
        );

        std::fs::write(&target, "v2\n").unwrap();
        super::drain_deferred_configure_maintenance(&ctx);

        let restored = crate::commands::restore_checkpoint::handle_restore_checkpoint(
            &request("restore_checkpoint", json!({ "name": "safe-point" })),
            &ctx,
        );
        assert!(restored.success, "{}", restored.data);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "v1\n");
        assert_eq!(
            std::fs::read_to_string(&stale_target).unwrap(),
            "moved on\n"
        );
    }

    #[test]
    fn configure_replies_while_the_checkpoint_store_and_its_file_lock_are_held() {
        // Configure runs before a subc bind reply, which has a hard deadline.
        // A checkpoint command can hold the store's mutex while it waits up to
        // 30 s for the cross-process checkpoint file lock, so configure must
        // select the checkpoint namespace without either.
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _disable_watcher = EnvVarGuard::set("AFT_TEST_DISABLE_FILE_WATCHER", "1");
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        init_git_fixture(project.path());
        let early_file = project.path().join("early.txt");
        std::fs::write(&early_file, "early\n").unwrap();

        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                storage_dir: Some(storage.path().to_path_buf()),
                ..Config::default()
            },
        );
        ctx.replace_checkpoint_store_for_test(
            crate::checkpoint::CheckpointStore::unbound_in_for_test(storage.path()),
        );
        // Saved before configure, under `<storage>/unbound/checkpoints`, so
        // binding the harness has checkpoints to move.
        ctx.checkpoint()
            .lock()
            .create(
                crate::protocol::DEFAULT_SESSION_ID,
                "early",
                vec![early_file.clone()],
                &crate::backup::BackupStore::new(),
            )
            .expect("checkpoint before configure");

        let lock_path = storage
            .path()
            .join("checkpoints")
            .join("test-project")
            .join("checkpoint.lock");
        let file_lock = crate::fs_lock::try_acquire(&lock_path, Duration::from_secs(5))
            .expect("hold the checkpoint file lock");
        let request = configure_request_with_params(json!({
            "project_root": project.path(),
            "storage_dir": storage.path(),
            "harness": "opencode",
            "config": [user_tier(json!({
                "indexes": { "trigram": false, "semantic": false, "callgraph": false }
            }))]
        }));
        let (locked_tx, locked_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        std::thread::scope(|scope| {
            // Hold the store's mutex on another thread, as a checkpoint command
            // waiting on the file lock would. Configure itself stays on this
            // thread, which holds the process env guards it needs. The holder
            // gives up after 15 s, so a configure that waits for the mutex
            // fails the elapsed check below instead of hanging the test.
            let ctx_ref = &ctx;
            scope.spawn(move || {
                let store_guard = ctx_ref.checkpoint().lock();
                locked_tx.send(()).expect("signal store locked");
                let _ = release_rx.recv_timeout(Duration::from_secs(15));
                drop(store_guard);
            });
            locked_rx.recv().expect("store locked");
            let started = Instant::now();
            let response = handle_configure_for_test(&request, &ctx);
            let elapsed = started.elapsed();
            let _ = release_tx.send(());
            assert!(response.success, "{}", response.data);
            // Well under both the holder's 15 s and the 30 s file-lock timeout.
            assert!(
                elapsed < Duration::from_secs(10),
                "configure waited on the checkpoint store: {elapsed:?}"
            );
        });
        drop(file_lock);

        // The next operation applies the namespace and moves the checkpoint
        // saved before configure into it.
        let listed = ctx
            .checkpoint()
            .lock()
            .list(crate::protocol::DEFAULT_SESSION_ID)
            .expect("list checkpoints");
        assert_eq!(
            listed
                .iter()
                .map(|info| info.name.as_str())
                .collect::<Vec<_>>(),
            vec!["early"]
        );
        let storage_path = listed[0].storage_path.clone().unwrap_or_default();
        assert!(
            storage_path.starts_with(storage.path().join("opencode")),
            "{}",
            storage_path.display()
        );
        super::drain_deferred_configure_maintenance(&ctx);
    }

    #[test]
    fn configure_tail_steps_aside_for_a_queued_writer_and_resumes_after_it() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _disable_watcher = EnvVarGuard::set("AFT_TEST_DISABLE_FILE_WATCHER", "1");
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        init_git_fixture(project.path());
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                storage_dir: Some(storage.path().to_path_buf()),
                ..Config::default()
            },
        );
        let response = handle_configure_for_test(
            &configure_request_with_params(json!({
                "project_root": project.path(),
                "storage_dir": storage.path(),
                "harness": "opencode",
                "config": [user_tier(json!({
                    "indexes": { "trigram": false, "semantic": false, "callgraph": false }
                }))]
            })),
            &ctx,
        );
        assert!(response.success, "{}", response.data);
        assert!(ctx.configure_tail_has_work());

        // A route bind for this root is queued behind the running tail.
        let waiting_writers = Arc::new(std::sync::atomic::AtomicUsize::new(1));
        {
            let _writer_waiting = crate::executor::install_actor_waiting_writers_for_test(
                Arc::clone(&waiting_writers),
            );
            assert!(
                super::drain_deferred_configure_maintenance_yielding(&ctx),
                "the tail must step aside while a writer waits on its actor"
            );
        }
        assert!(
            ctx.configure_tail_has_work(),
            "the unfinished tail stays visible to the maintenance probe"
        );
        let parked = ctx
            .take_parked_configure_tail()
            .expect("the rest of the tail is parked on the context");
        let stage = parked.jobs.front().map(|continuation| continuation.stage);
        assert!(
            stage.is_some_and(|stage| !stage.is_non_yielding_prefix()),
            "the tail may only stop after its request-critical prefix, parked at {stage:?}"
        );
        ctx.park_configure_tail(parked);

        // The writer ran; the requeued drain finishes the parked work.
        waiting_writers.store(0, std::sync::atomic::Ordering::Release);
        {
            let _no_writer = crate::executor::install_actor_waiting_writers_for_test(Arc::clone(
                &waiting_writers,
            ));
            assert!(!super::drain_deferred_configure_maintenance_yielding(&ctx));
        }
        assert!(!ctx.configure_tail_has_work());
        assert!(ctx.take_parked_configure_tail().is_none());
    }

    /// A newer configure that lands while a tail is stepped aside replaces the
    /// generation the parked work belongs to. The parked continuation's
    /// remaining units must not run on resume; the newer configure's own tail
    /// repeats every root-scoped stage.
    #[test]
    fn superseded_parked_tail_does_not_resume_after_a_newer_configure() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _disable_watcher = EnvVarGuard::set("AFT_TEST_DISABLE_FILE_WATCHER", "1");
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        init_git_fixture(project.path());
        let canonical_root = fs::canonicalize(project.path()).unwrap();
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                storage_dir: Some(storage.path().to_path_buf()),
                ..Config::default()
            },
        );
        let configure = |max_file_size: u64| {
            let response = handle_configure_for_test(
                &configure_request_with_params(json!({
                    "project_root": project.path(),
                    "storage_dir": storage.path(),
                    "harness": "opencode",
                    "search_index_max_file_size": max_file_size,
                    "config": [user_tier(json!({
                        "indexes": { "trigram": false, "semantic": false, "callgraph": false }
                    }))]
                })),
                &ctx,
            );
            assert!(response.success, "{}", response.data);
        };

        configure(1_000_000);
        let old_generation = ctx.configure_generation();
        let waiting_writers = Arc::new(std::sync::atomic::AtomicUsize::new(1));
        {
            let _writer_waiting = crate::executor::install_actor_waiting_writers_for_test(
                Arc::clone(&waiting_writers),
            );
            assert!(super::drain_deferred_configure_maintenance_yielding(&ctx));
        }
        let old_units_before = super::configure_unit_records_for_root(&canonical_root)
            .into_iter()
            .filter(|record| record.generation == old_generation)
            .count();
        assert!(old_units_before > 0, "the old tail ran its prefix");

        // The waiting writer was a configure that changes the warm key.
        configure(2_000_000);
        let new_generation = ctx.configure_generation();
        assert_ne!(new_generation, old_generation);

        waiting_writers.store(0, std::sync::atomic::Ordering::Release);
        {
            let _no_writer = crate::executor::install_actor_waiting_writers_for_test(Arc::clone(
                &waiting_writers,
            ));
            assert!(!super::drain_deferred_configure_maintenance_yielding(&ctx));
        }
        let records = super::configure_unit_records_for_root(&canonical_root);
        let old_units_after = records
            .iter()
            .filter(|record| record.generation == old_generation)
            .count();
        assert_eq!(
            old_units_after, old_units_before,
            "superseded units ran after the newer configure: {records:?}"
        );
        assert!(
            records
                .iter()
                .any(|record| record.generation == new_generation && record.stage == "status"),
            "the newer configure's tail runs to completion: {records:?}"
        );
        assert!(!ctx.configure_tail_has_work());
    }

    /// `lib.rs` of the on-demand call-graph fixtures: one caller, one callee
    /// and one function nothing calls, so callers, impact and dead code all
    /// have something to report.
    const LAZY_LIB: &str = "pub fn caller() { target(); }\npub fn target() {}\nfn unused() {}\n";

    struct LazyWorktree {
        _temp: tempfile::TempDir,
        main: PathBuf,
        linked: PathBuf,
        storage: PathBuf,
        /// The main checkout's context, kept alive for the fixture's
        /// lifetime; `None` when it was never bound in this process.
        _owner: Option<Arc<AppContext>>,
        borrower: Arc<AppContext>,
    }

    fn lazy_bind(root: &Path, storage: &Path, session: &str) -> Arc<AppContext> {
        let ctx = Arc::new(AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                storage_dir: Some(storage.to_path_buf()),
                ..Config::default()
            },
        ));
        let params = json!({
            "project_root": root, "storage_dir": storage, "harness": "opencode",
            "config": [user_tier(json!({ "views": { "enabled": true },
                "indexes": { "trigram": false, "semantic": false, "callgraph": true } }))]
        });
        let response =
            handle_configure_for_test(&configure_request_with_session(params, session), &ctx);
        assert!(response.success, "{}", response.data);
        super::drain_deferred_configure_maintenance(&ctx);
        ctx
    }

    /// Write `lib.rs` in `root` and commit it.
    fn lazy_commit(root: &Path, contents: &str, message: &str) {
        fs::write(root.join("lib.rs"), contents).unwrap();
        assert!(git_command(root)
            .args(["add", "lib.rs"])
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
                message,
            ])
            .status()
            .unwrap()
            .success());
    }

    fn lazy_view_dir(storage: &Path, root: &Path) -> PathBuf {
        storage
            .join("views")
            .join(crate::path_identity::project_scope_key(root))
    }

    /// Edit `lib.rs` in the worktree and deliver the change through the
    /// watcher drain, without publishing it yet.
    fn lazy_edit_through_watcher(fixture: &LazyWorktree, contents: &str) {
        let (tx, rx) = crossbeam_channel::unbounded();
        *fixture.borrower.watcher_rx().lock() = Some(rx);
        let source = fixture.linked.join("lib.rs");
        fs::write(&source, contents).unwrap();
        tx.send(crate::watcher_filter::WatcherDispatchEvent::Paths(vec![
            source,
        ]))
        .unwrap();
        let outcome = crate::runtime_drain::drain_watcher_events_bounded(&fixture.borrower, 1);
        assert_eq!(outcome.processed, 1);
    }

    #[test]
    fn build_key_memo_does_not_bypass_first_configure_identity_bind() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(project.path()).unwrap();
        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        let build_key = ctx.memoized_artifact_cache_key(&root);
        assert_eq!(
            ctx.cached_artifact_cache_key(&root),
            Some(build_key.clone())
        );
        assert_eq!(
            crate::root_identity::read(&root, storage.path()).unwrap(),
            None
        );

        init_git_fixture(&root);
        let bound_key = ctx
            .memoized_artifact_cache_key_for_configure(&root, &root, storage.path(), None)
            .unwrap()
            .unwrap();
        assert_ne!(bound_key, build_key);
        assert_eq!(ctx.cached_artifact_cache_key(&root), Some(bound_key));
        assert!(matches!(
            crate::root_identity::read(&root, storage.path()).unwrap(),
            Some(crate::root_identity::Identity::Git { .. })
        ));
    }

    #[test]
    fn configure_without_storage_dir_defaults_to_shared_storage_root() {
        let _env_guard = home_env_mutex();
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _disable_watcher = EnvVarGuard::set("AFT_TEST_DISABLE_FILE_WATCHER", "1");
        let _aft_cache_dir = EnvVarGuard::remove("AFT_CACHE_DIR");
        let temp = tempfile::tempdir().unwrap();
        let sandboxed_data_home = temp.path().join("xdg-data");
        let _xdg_data_home = EnvVarGuard::set(
            "XDG_DATA_HOME",
            sandboxed_data_home
                .to_str()
                .expect("temporary path is UTF-8"),
        );
        init_git_fixture(temp.path());

        // Test contexts normally seed a private per-App store. This fixture
        // deliberately exercises the shared default under its temporary XDG
        // home, so seed that production-resolved default instead. The configure
        // request still omits storage_dir, just like a plugin-less consumer.
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                storage_dir: Some(crate::bash_background::storage_dir(None)),
                ..Config::default()
            },
        );
        let req = configure_request_with_params(json!({
            "project_root": temp.path(),
            "harness": "opencode",
            "config": [user_tier(json!({
                "indexes": { "trigram": false, "semantic": false, "callgraph": false }
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
            Some(sandboxed_data_home.join("cortexkit").join("aft")),
            "configure must default storage_dir to the shared storage root"
        );
    }

    #[test]
    fn standalone_cache_dir_number_fails_closed() {
        cache_tier_failure("standalone", "number");
    }
    #[test]
    fn standalone_cache_file_missing_fails_closed() {
        cache_tier_failure("standalone", "missing");
    }
    #[test]
    fn standalone_cache_file_unparseable_fails_closed() {
        cache_tier_failure("standalone", "unparseable");
    }
    #[test]
    fn standalone_cache_file_unreadable_fails_closed() {
        cache_tier_failure("standalone", "unreadable");
    }
    #[test]
    fn subc_local_cache_dir_number_fails_closed() {
        cache_tier_failure("subc", "number");
    }
    #[test]
    fn subc_local_cache_file_missing_fails_closed() {
        cache_tier_failure("subc", "missing");
    }
    #[test]
    fn subc_local_cache_file_unparseable_fails_closed() {
        cache_tier_failure("subc", "unparseable");
    }
    #[test]
    fn subc_local_cache_file_unreadable_fails_closed() {
        cache_tier_failure("subc", "unreadable");
    }

    fn cache_tier_failure(transport: &str, failure: &str) {
        // Tests elsewhere set AFT_STORAGE_DIR process-wide, which outranks the
        // storage_dir this fixture configures; hold the shared env lock without it.
        let _storage_env = crate::test_env::without_storage_override();
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let storage = temp.path().join("storage");
        let cache = temp.path().join("cache-volume");
        let user = temp.path().join("user.jsonc");
        fs::create_dir(&project).unwrap();
        fs::create_dir(&cache).unwrap();
        init_git_fixture(&project);
        fs::create_dir_all(project.join(".cortexkit")).unwrap();
        fs::write(
            project.join(".cortexkit/aft.jsonc"),
            r#"{"indexes":{"semantic":false,"callgraph":false}}"#,
        )
        .unwrap();
        fs::write(project.join("needle.rs"), "fn cache_guard_needle() {}\n").unwrap();
        let mut split = Config {
            storage_dir: Some(storage.clone()),
            cache_dir: Some(cache),
            ..Default::default()
        };
        assert!(crate::root_cache::prepare_cache_root_for_test(&mut split).is_none());
        drop(split);
        match failure {
            "number" => fs::write(
                &user,
                r#"{"storage":{"cache_dir":123},"indexes":{"semantic":false,"callgraph":false}}"#,
            )
            .unwrap(),
            "unparseable" => fs::write(&user, "{").unwrap(),
            "unreadable" => fs::create_dir(&user).unwrap(),
            "missing" => {}
            _ => unreachable!(),
        }
        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        let mut params = json!({"project_root":project,"storage_dir":storage,"harness":"opencode"});
        if transport == "standalone" {
            // Plugins relay the raw document and its path, not their lenient
            // merged defaults. Native re-reading also detects unreadable files.
            params["cortexkit_user_config_path"] = json!(user);
            params["config"] = match fs::read_to_string(&user) {
                Ok(doc) => json!([{"tier":"user","source":user,"doc":doc}]),
                Err(_) => json!([]),
            };
        } else {
            let tiers =
                crate::subc_config::read_local_cortexkit_config_tiers(Some(&user), &project);
            params["config"] = json!(tiers
                .iter()
                .map(|tier| json!({"tier":tier.tier,"source":tier.source,"doc":tier.doc}))
                .collect::<Vec<_>>());
        }
        let response = handle_configure_for_test(&configure_request_with_params(params), &ctx);
        if matches!(failure, "unparseable" | "unreadable") {
            // A user document that cannot be read or parsed rejects the whole
            // candidate: nothing is published, so no default settings and no
            // cache location replace the last good configuration.
            assert!(!response.success, "{response:?}");
            assert_eq!(response.data["code"], "config_rejected", "{response:?}");
            assert!(
                response.data["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("invalid_config_document")),
                "{response:?}"
            );
        } else {
            assert!(response.success, "{response:?}");
            super::drain_deferred_configure_maintenance(&ctx);
            let expected = if failure == "missing" {
                "cache_dir_unavailable"
            } else {
                "cache_dir_config_unknown"
            };
            assert!(
                ctx.cache_unavailable_reason()
                    .is_some_and(|reason| reason.contains(expected)),
                "{response:?}"
            );
        }
        for domain in crate::root_cache::CACHE_DOMAINS {
            assert!(
                !storage.join(domain).exists(),
                "{transport}/{failure} wrote {domain} on primary storage"
            );
        }
    }

    /// A configured context whose user file later becomes unreadable or
    /// unparseable keeps its last good settings on the next configure, as for
    /// every other key: the rejected candidate changes nothing, including the
    /// cache location and its availability.
    #[test]
    fn configure_rejected_user_document_keeps_the_last_good_cache_location() {
        for failure in ["unparseable", "unreadable"] {
            let _storage_env = crate::test_env::without_storage_override();
            let temp = tempfile::tempdir().unwrap();
            let project = temp.path().join("project");
            let storage = temp.path().join("storage");
            let cache = temp.path().join("cache-volume");
            let user = temp.path().join("user.jsonc");
            fs::create_dir(&project).unwrap();
            fs::create_dir(&cache).unwrap();
            let _volume = crate::walk_boundary::simulated_volumes::simulate(&cache, u64::MAX - 17);
            init_git_fixture(&project);
            fs::write(project.join("needle.rs"), "fn last_good_needle() {}\n").unwrap();
            fs::write(
                &user,
                json!({
                    "storage": {"cache_dir": cache},
                    "format_on_edit": true,
                    "indexes": {"trigram": false, "semantic": false, "callgraph": false},
                })
                .to_string(),
            )
            .unwrap();
            let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
            let request = |id: &str| {
                let tiers =
                    crate::subc_config::read_local_cortexkit_config_tiers(Some(&user), &project);
                let mut request = configure_request_with_params(json!({
                    "project_root": project, "storage_dir": storage, "harness": "opencode",
                    "config": tiers
                        .iter()
                        .map(|tier| json!({"tier":tier.tier,"source":tier.source,"doc":tier.doc}))
                        .collect::<Vec<_>>(),
                }));
                request.id = id.to_owned();
                request
            };
            let first = handle_configure_for_test(&request("first"), &ctx);
            assert!(first.success, "{first:?}");
            super::drain_deferred_configure_maintenance(&ctx);
            let good = ctx.config();
            assert!(good.cache_dir.is_some() && good.format_on_edit);
            assert!(ctx.cache_unavailable_reason().is_none());

            match failure {
                "unparseable" => fs::write(&user, "{").unwrap(),
                _ => {
                    fs::remove_file(&user).unwrap();
                    fs::create_dir(&user).unwrap();
                }
            }
            let rejected = handle_configure_for_test(&request("rejected"), &ctx);
            assert_eq!(
                rejected.data["code"], "config_rejected",
                "{failure}: {rejected:?}"
            );
            let after = ctx.config();
            assert_eq!(after.cache_dir, good.cache_dir, "{failure}");
            assert_eq!(after.cache_epoch, good.cache_epoch, "{failure}");
            assert!(after.format_on_edit, "{failure}");
            assert!(
                ctx.cache_unavailable_reason().is_none(),
                "{failure}: {:?}",
                ctx.cache_unavailable_reason()
            );
        }
    }

    #[cfg(unix)]
    fn cache_retry_fixture() -> (tempfile::TempDir, PathBuf, AppContext, RawRequest) {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let storage = temp.path().join("storage");
        fs::create_dir(&project).unwrap();
        init_git_fixture(&project);
        fs::write(project.join("needle.rs"), "fn cache_retry_needle() {}\n").unwrap();
        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        let request = configure_request_with_params(json!({
            "project_root": project, "storage_dir": storage, "harness": "opencode",
            "config": [user_tier(json!({"indexes": {"trigram": true, "semantic": false, "callgraph": false}}))],
        }));
        (temp, project, ctx, request)
    }

    #[cfg(unix)]
    fn force_cache_coordination_busy(
        _: std::os::fd::RawFd,
        name: &std::ffi::OsStr,
        _: i32,
    ) -> Option<std::io::Result<std::fs::File>> {
        (name == std::ffi::OsStr::new(".aft-in-use.lock"))
            .then(|| Err(std::io::Error::from_raw_os_error(libc::ENOENT)))
    }

    #[cfg(unix)]
    #[test]
    fn configure_cache_coordination_busy_twice_then_free_retries_three_attempts() {
        use crate::provisional_identity_retry::{
            advance_clock_for_test, drain_inline, with_clock_for_test,
        };
        use std::sync::atomic::{AtomicUsize, Ordering};

        let _storage_override = crate::test_env::without_storage_override();
        with_clock_for_test(|| {
            let (_temp, project, ctx, request) = cache_retry_fixture();
            let calls = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&calls);
            crate::bash_background::persistence::with_openat_file_hook(
                Box::new(move |_, name, _| {
                    if name != std::ffi::OsStr::new(".aft-in-use.lock") {
                        return None;
                    }
                    // Each busy preparation exhausts six immediate syscall attempts.
                    // The third preparation is allowed to use the real opener.
                    (observed.fetch_add(1, Ordering::SeqCst) < 12)
                        .then(|| Err(std::io::Error::from_raw_os_error(libc::ENOENT)))
                }),
                || {
                    let response = handle_configure_for_test(&request, &ctx);
                    assert!(response.success, "{response:?}");
                    assert!(response.data["warnings"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|warning| warning["message"]
                            == crate::root_cache::CACHE_DIR_BUSY_MESSAGE));
                    assert_eq!(calls.load(Ordering::SeqCst), 6);
                    assert!(ctx
                        .cache_unavailable_reason()
                        .unwrap()
                        .contains(crate::root_cache::CACHE_DIR_BUSY_MESSAGE));
                    assert!(!drain_inline(&ctx), "first retry waits one second");

                    advance_clock_for_test(Duration::from_secs(1));
                    assert!(drain_inline(&ctx), "second preparation remains busy");
                    assert_eq!(calls.load(Ordering::SeqCst), 12);
                    assert!(ctx.provisional_identity_retry().is_armed());
                    assert_eq!(
                        ctx.provisional_identity_retry().attempts_for_test(),
                        Some(1)
                    );
                    assert!(ctx
                        .cache_unavailable_reason()
                        .unwrap()
                        .contains(crate::root_cache::CACHE_DIR_BUSY_MESSAGE));
                    advance_clock_for_test(Duration::from_secs(1));
                    assert!(
                        !drain_inline(&ctx),
                        "failed retry backs off for two seconds"
                    );
                    advance_clock_for_test(Duration::from_secs(1));
                    assert!(drain_inline(&ctx), "third preparation recovers");
                    assert_eq!(calls.load(Ordering::SeqCst), 13);
                    assert!(ctx.cache_unavailable_reason().is_none());
                    assert!(!ctx
                        .degraded_reasons()
                        .iter()
                        .any(|reason| reason == crate::root_cache::CACHE_DIR_BUSY));
                    assert!(ctx
                        .cached_artifact_cache_key(&fs::canonicalize(project).unwrap())
                        .is_some());
                    assert!(!ctx.provisional_identity_retry().is_armed());
                    advance_clock_for_test(Duration::from_secs(3600));
                    assert!(
                        !drain_inline(&ctx),
                        "there is no fourth preparation after success"
                    );
                    assert_eq!(calls.load(Ordering::SeqCst), 13);
                },
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn configure_cache_coordination_unbind_mid_backoff_cancels_retry() {
        use crate::provisional_identity_retry::{
            advance_clock_for_test, drain_inline, with_clock_for_test,
        };

        let _storage_override = crate::test_env::without_storage_override();
        with_clock_for_test(|| {
            let (_temp, project, ctx, request) = cache_retry_fixture();
            crate::bash_background::persistence::with_openat_file_hook(
                Box::new(force_cache_coordination_busy),
                || {
                    assert!(handle_configure_for_test(&request, &ctx).success);
                    advance_clock_for_test(Duration::from_secs(1));
                    assert!(drain_inline(&ctx));
                },
            );
            advance_clock_for_test(Duration::from_secs(1));
            assert!(ctx.provisional_identity_retry().is_armed());
            assert!(!ctx.provisional_identity_retry().is_due());
            assert!(ctx
                .cache_unavailable_reason()
                .unwrap()
                .contains(crate::root_cache::CACHE_DIR_BUSY_MESSAGE));
            ctx.mark_subc_unbound();
            assert!(!ctx.provisional_identity_retry().is_armed());
            assert!(!ctx
                .cache_unavailable_reason()
                .unwrap()
                .contains(crate::root_cache::CACHE_DIR_BUSY_MESSAGE));
            advance_clock_for_test(Duration::from_secs(3600));
            assert!(!drain_inline(&ctx));
            assert!(!ctx.provisional_identity_retry().is_due());
            assert!(ctx
                .cached_artifact_cache_key(&fs::canonicalize(project).unwrap())
                .is_none());
        });
    }

    #[cfg(unix)]
    #[test]
    fn configure_cache_coordination_two_busy_roots_retry_independently() {
        use crate::provisional_identity_retry::{
            advance_clock_for_test, drain_inline, with_clock_for_test,
        };

        let _storage_override = crate::test_env::without_storage_override();
        with_clock_for_test(|| {
            let (_first_temp, _, first, first_request) = cache_retry_fixture();
            let (_second_temp, _, second, second_request) = cache_retry_fixture();
            crate::bash_background::persistence::with_openat_file_hook(
                Box::new(force_cache_coordination_busy),
                || {
                    assert!(handle_configure_for_test(&first_request, &first).success);
                    assert!(handle_configure_for_test(&second_request, &second).success);
                },
            );
            advance_clock_for_test(Duration::from_secs(1));
            assert!(drain_inline(&first));
            assert!(first.cache_unavailable_reason().is_none());
            assert!(!first.provisional_identity_retry().is_armed());
            crate::bash_background::persistence::with_openat_file_hook(
                Box::new(force_cache_coordination_busy),
                || assert!(drain_inline(&second)),
            );
            assert!(second.provisional_identity_retry().is_armed());
            assert!(!second.provisional_identity_retry().is_due());
            assert!(!drain_inline(&first));
            // Cache recovery does not inherit the identity retry's five-minute give-up.
            advance_clock_for_test(Duration::from_secs(600));
            crate::bash_background::persistence::with_openat_file_hook(
                Box::new(force_cache_coordination_busy),
                || assert!(drain_inline(&second)),
            );
            assert_eq!(
                second.provisional_identity_retry().attempts_for_test(),
                Some(2)
            );
            assert!(second
                .cache_unavailable_reason()
                .unwrap()
                .contains(crate::root_cache::CACHE_DIR_BUSY_MESSAGE));
            assert!(first.cache_unavailable_reason().is_none());
            advance_clock_for_test(Duration::from_secs(3));
            assert!(
                !drain_inline(&second),
                "only the second root waits for its four-second backoff"
            );
            advance_clock_for_test(Duration::from_secs(1));
            assert!(drain_inline(&second));
            assert!(second.cache_unavailable_reason().is_none());
            assert!(!second.provisional_identity_retry().is_armed());
            assert!(!drain_inline(&first));
        });
    }

    #[cfg(unix)]
    #[test]
    fn configure_cache_coordination_message_requires_pending_or_due_retry() {
        use crate::provisional_identity_retry::{
            advance_clock_for_test, drain_inline, poll, with_clock_for_test,
        };

        let _storage_override = crate::test_env::without_storage_override();
        with_clock_for_test(|| {
            let (_temp, _, ctx, request) = cache_retry_fixture();
            crate::bash_background::persistence::with_openat_file_hook(
                Box::new(force_cache_coordination_busy),
                || assert!(handle_configure_for_test(&request, &ctx).success),
            );
            assert!(ctx
                .cache_unavailable_reason()
                .unwrap()
                .contains(crate::root_cache::CACHE_DIR_BUSY_MESSAGE));
            assert!(!ctx.provisional_identity_retry().is_due());
            advance_clock_for_test(Duration::from_secs(1));
            assert!(ctx.provisional_identity_retry().is_due());
            let replay = poll(&ctx).unwrap();
            assert!(poll(&ctx).is_none(), "one prepared replay per root");
            assert!(ctx
                .cache_unavailable_reason()
                .unwrap()
                .contains(crate::root_cache::CACHE_DIR_BUSY_MESSAGE));
            // Dropping an unadmitted replay leaves a bounded deadline, not a lost retry.
            drop(replay);
            assert!(!ctx.provisional_identity_retry().is_due());
            advance_clock_for_test(Duration::from_secs(2));
            assert!(drain_inline(&ctx));
            assert!(ctx.cache_unavailable_reason().is_none());

            let (_cancel_temp, _, ctx, request) = cache_retry_fixture();
            crate::bash_background::persistence::with_openat_file_hook(
                Box::new(force_cache_coordination_busy),
                || assert!(handle_configure_for_test(&request, &ctx).success),
            );
            assert!(ctx
                .cache_unavailable_reason()
                .unwrap()
                .contains(crate::root_cache::CACHE_DIR_BUSY_MESSAGE));
            super::cancel_deferred_configure_maintenance(&ctx);
            assert!(!ctx.provisional_identity_retry().is_armed());
            assert!(!ctx
                .cache_unavailable_reason()
                .unwrap()
                .contains(crate::root_cache::CACHE_DIR_BUSY_MESSAGE));
            advance_clock_for_test(Duration::from_secs(3600));
            assert!(!drain_inline(&ctx));
        });
    }

    #[cfg(unix)]
    #[test]
    fn configure_cache_coordination_busy_bind_retries_once_while_bound() {
        let _storage_override = crate::test_env::without_storage_override();
        fn inject_busy(
            _: std::os::fd::RawFd,
            name: &std::ffi::OsStr,
            _: i32,
        ) -> Option<std::io::Result<std::fs::File>> {
            (name == std::ffi::OsStr::new(".aft-in-use.lock"))
                .then(|| Err(std::io::Error::from_raw_os_error(libc::ENOENT)))
        }
        for outcome in ["recover", "edited", "unbound", "unbound_after_poll"] {
            crate::provisional_identity_retry::with_timing_for_test(
                Duration::ZERO,
                Duration::from_secs(1),
                || {
                    let temp = tempfile::tempdir().unwrap();
                    let project = temp.path().join("project");
                    let storage = temp.path().join("storage");
                    fs::create_dir(&project).unwrap();
                    init_git_fixture(&project);
                    fs::write(project.join("needle.rs"), "fn contention_needle() {}\n").unwrap();
                    let ctx =
                        AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
                    let request = configure_request_with_params(json!({
                        "project_root": project, "storage_dir": storage, "harness": "opencode",
                        "config": [user_tier(json!({"indexes": {"trigram": true, "semantic": false, "callgraph": false}}))],
                    }));
                    let response = crate::bash_background::persistence::with_openat_file_hook(
                        Box::new(inject_busy),
                        || handle_configure_for_test(&request, &ctx),
                    );
                    assert!(response.success, "{response:?}");
                    assert!(ctx
                        .degraded_reasons()
                        .iter()
                        .any(|reason| reason == crate::root_cache::CACHE_DIR_BUSY));
                    assert!(!ctx
                        .degraded_reasons()
                        .iter()
                        .any(|reason| reason == "cache_dir_unavailable"));
                    let search: RawRequest = serde_json::from_value(json!({
                        "id": "busy-search", "command": "semantic_search", "query": "contention_needle"
                    })).unwrap();
                    let search =
                        crate::commands::semantic_search::handle_semantic_search(&search, &ctx);
                    assert!(search.success, "{search:?}");
                    assert_eq!(
                        search.data["cache_unavailable_reason"],
                        crate::root_cache::CACHE_DIR_BUSY
                    );
                    assert_eq!(
                        ctx.try_health_snapshot(&project).cache_unavailable_reason,
                        Some(crate::root_cache::CACHE_DIR_BUSY)
                    );
                    for plane in [
                        crate::feature_status::IndexPlane::Trigram,
                        crate::feature_status::IndexPlane::Semantic,
                        crate::feature_status::IndexPlane::Callgraph,
                    ] {
                        assert_eq!(
                            crate::feature_status::observed_index_status(&ctx, plane)
                                .unavailable_reason
                                .as_deref(),
                            Some(crate::root_cache::CACHE_DIR_BUSY)
                        );
                    }
                    assert!(ctx.provisional_identity_retry().is_armed());
                    let root = fs::canonicalize(&project).unwrap();
                    assert!(ctx.cached_artifact_cache_key(&root).is_none());

                    if outcome == "edited" {
                        ctx.update_config(|config| config.indexes.trigram = false);
                    }
                    if outcome == "unbound_after_poll" {
                        let replay = crate::provisional_identity_retry::poll(&ctx).unwrap();
                        ctx.mark_subc_unbound();
                        let response =
                            replay.run(&ctx, |_| panic!("unbound root must not hand off or index"));
                        assert!(response.success);
                    } else if outcome == "unbound" {
                        ctx.mark_subc_unbound();
                        assert!(!crate::provisional_identity_retry::drain_inline(&ctx));
                    } else {
                        assert!(crate::provisional_identity_retry::drain_inline(&ctx));
                    }
                    assert!(
                        !ctx.provisional_identity_retry().is_armed(),
                        "settled or unbound roots must have no retry"
                    );
                    assert!(!crate::provisional_identity_retry::drain_inline(&ctx));
                    if outcome == "recover" || outcome == "edited" {
                        assert!(ctx.cache_unavailable_reason().is_none());
                        assert!(!ctx
                            .degraded_reasons()
                            .iter()
                            .any(|reason| reason == crate::root_cache::CACHE_DIR_BUSY));
                        assert_eq!(ctx.config().indexes.trigram, outcome == "recover");
                        assert_eq!(
                            ctx.cached_artifact_cache_key(&root).is_some(),
                            outcome == "recover"
                        );
                    } else {
                        assert!(ctx.cached_artifact_cache_key(&root).is_none());
                        assert!(ctx
                            .cache_unavailable_reason()
                            .unwrap()
                            .starts_with(crate::root_cache::CACHE_DIR_BUSY));
                    }
                },
            );
        }
    }

    #[test]
    fn configure_flat_cache_dir_parameter_cannot_bypass_tier_resolution() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        fs::create_dir(&project).unwrap();
        let evil = temp.path().join("wire-cache");
        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        let request = configure_request_with_params(
            json!({"project_root":project,"harness":"opencode","storage_dir":temp.path().join("storage"),"cache_dir":evil,"config":[user_tier(json!({"indexes":{"trigram":false,"semantic":false,"callgraph":false}}))]}),
        );
        let response = handle_configure_for_test(&request, &ctx);
        assert!(response.success, "{response:?}");
        assert!(ctx.config().cache_dir.is_none());
        assert_eq!(ctx.cache_dir(), ctx.storage_dir());
        assert!(!evil.exists());
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
            "config": [user_tier(json!({ "indexes": { "trigram": true, "semantic": false } }))],
        }))
    }

    fn configure_lexical_views(root: &Path, storage: &Path) -> RawRequest {
        configure_request_with_params(json!({
            "project_root": root, "harness": "opencode", "storage_dir": storage,
            "config": [user_tier(json!({
                "views": { "enabled": true },
                "indexes": { "trigram": true, "semantic": false, "callgraph": false }
            }))]
        }))
    }

    /// The navigation answers whose output the views route must reproduce.
    fn callgraph_views_answers(ctx: &AppContext, root: &Path) -> Vec<(String, Response)> {
        let requests = [
            ("callers", "a.ts", "target", 2),
            ("impact", "a.ts", "target", 3),
            ("call_tree", "c.ts", "top", 3),
            ("trace_to", "a.ts", "target", 5),
            ("callers", "b.ts", "caller", 1),
        ];
        requests
            .into_iter()
            .map(|(command, file, symbol, depth)| {
                let request = RawRequest {
                    id: format!("callgraph-views-{command}"),
                    command: command.into(),
                    lsp_hints: None,
                    session_id: None,
                    params: json!({"file": root.join(file), "symbol": symbol, "depth": depth}),
                };
                let response = match command {
                    "callers" => crate::commands::callers::handle_callers(&request, ctx),
                    "impact" => crate::commands::impact::handle_impact(&request, ctx),
                    "call_tree" => crate::commands::call_tree::handle_call_tree(&request, ctx),
                    _ => crate::commands::trace_to::handle_trace_to(&request, ctx),
                };
                (format!("{command} {file} {symbol}"), response)
            })
            .collect()
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
                "indexes": { "trigram": true, "semantic": false, "callgraph": false }
            }))],
        }))
    }

    fn configure_semantic_with_storage(
        root: &std::path::Path,
        storage: &std::path::Path,
        base_url: &str,
        semantic_search: bool,
    ) -> RawRequest {
        configure_semantic_with_options(root, storage, base_url, semantic_search, 64, false)
    }

    fn configure_semantic_with_options(
        root: &std::path::Path,
        storage: &std::path::Path,
        base_url: &str,
        semantic_search: bool,
        max_batch_size: usize,
        callgraph_store: bool,
    ) -> RawRequest {
        configure_request_with_params(json!({
            "project_root": root,
            "harness": "opencode",
            "storage_dir": storage,
            "config": [user_tier(json!({
                "indexes": {
                    "trigram": false,
                    "semantic": semantic_search,
                    "callgraph": callgraph_store,
                },
                "semantic": {
                    "backend": "openai_compatible",
                    "model": "counting-test-embedding",
                    "base_url": base_url,
                    "timeout_ms": 5_000,
                    "max_batch_size": max_batch_size,
                    "max_files": 1_000
                }
            }))],
        }))
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
        response_release: Arc<(Mutex<usize>, Condvar)>,
        fail_summaries: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl CountingEmbeddingServer {
        fn start() -> Self {
            Self::start_with_dimension(3)
        }

        fn start_with_dimension(dimension: usize) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind embedding mock");
            let addr = listener.local_addr().expect("embedding mock addr");
            listener
                .set_nonblocking(true)
                .expect("embedding mock nonblocking");
            let stop = Arc::new(AtomicBool::new(false));
            let requests = Arc::new(Mutex::new(Vec::new()));
            let request_changed = Arc::new(Condvar::new());
            let response_release = Arc::new((Mutex::new(0usize), Condvar::new()));
            let fail_summaries = Arc::new(AtomicBool::new(false));
            let stop_for_thread = Arc::clone(&stop);
            let requests_for_thread = Arc::clone(&requests);
            let request_changed_for_thread = Arc::clone(&request_changed);
            let response_release_for_thread = Arc::clone(&response_release);
            let fail_summaries_for_thread = Arc::clone(&fail_summaries);
            let handle = std::thread::spawn(move || {
                while !stop_for_thread.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let requests = Arc::clone(&requests_for_thread);
                            let request_changed = Arc::clone(&request_changed_for_thread);
                            let response_release = Arc::clone(&response_release_for_thread);
                            let fail_summaries = Arc::clone(&fail_summaries_for_thread);
                            std::thread::spawn(move || {
                                handle_counting_embedding_request(
                                    stream,
                                    requests,
                                    request_changed,
                                    response_release,
                                    fail_summaries,
                                    dimension,
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
                fail_summaries,
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

        fn release_response(&self) {
            let (remaining, changed) = &*self.response_release;
            let mut remaining = remaining
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *remaining = remaining.saturating_add(1);
            changed.notify_one();
        }

        fn release_responses(&self) {
            let (remaining, changed) = &*self.response_release;
            *remaining
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = usize::MAX;
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
        response_release: Arc<(Mutex<usize>, Condvar)>,
        fail_summaries: Arc<AtomicBool>,
        dimension: usize,
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
        if fail_summaries.load(Ordering::SeqCst)
            && inputs.iter().any(|text| text.contains("kind:file-summary"))
        {
            let body = "temporary summary backend outage";
            let response = format!("HTTP/1.1 503 Service Unavailable\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            let _ = stream.write_all(response.as_bytes());
            return;
        }
        if inputs
            .iter()
            .any(|text| text != "semantic index fingerprint probe")
        {
            let (remaining, changed) = &*response_release;
            let mut remaining = remaining
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while *remaining == 0 {
                remaining = changed
                    .wait(remaining)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            if *remaining != usize::MAX {
                *remaining -= 1;
            }
        }
        let data = inputs
            .iter()
            .enumerate()
            .map(|(index, _)| {
                let base = index as f64 + 1.0;
                json!({
                    "embedding": (0..dimension).map(|axis| base + axis as f64 * 0.1).collect::<Vec<_>>(),
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

    /// Every non-probe embedding input the server has received so far.
    fn non_probe_inputs(server: &CountingEmbeddingServer) -> Vec<String> {
        server
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .flatten()
            .filter(|text| text.as_str() != "semantic index fingerprint probe")
            .cloned()
            .collect()
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
    fn warm_key_ignores_keys_a_live_config_reload_may_change() {
        let root = std::path::Path::new("/warm-key-root");
        let key = |config: &Config| super::configure_warm_key(root, config, false, false, false);
        let base = Config::default();

        let mut live = base.clone();
        live.callgraph_chunk_size += 7;
        live.inspect.enabled = !live.inspect.enabled;
        live.semantic.query_timeout_ms += 1;
        live.semantic.query_instruction = "off".to_string();
        assert_eq!(key(&base), key(&live));

        let mut index_switch = base.clone();
        index_switch.indexes.trigram = !index_switch.indexes.trigram;
        assert_ne!(key(&base), key(&index_switch));
        let mut model = base.clone();
        model.semantic.model = "another-model".to_string();
        assert_ne!(key(&base), key(&model));
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

    /// Configure migrates a user file that uses retired keys and reports it
    /// once. A project file's recently retired keys are translated in memory,
    /// and its bytes are never touched; a project file that names a key
    /// retired in 0.62 or earlier rejects the configure.
    #[test]
    fn configure_migrates_the_user_file_and_translates_the_project_file() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_context();
        let user_path = temp.path().join("xdg/cortexkit/aft.jsonc");
        let project_path = temp.path().join(".cortexkit/aft.jsonc");
        let project_text =
            r#"{ "indexes": { "semantic": false }, "bridge": { "hang_threshold": 2 } }"#;
        write_config(&user_path, r#"{ "search_index": false }"#);
        write_config(&project_path, project_text);
        let request = || {
            configure_request_with_params(json!({
                "project_root": temp.path(),
                "harness": "opencode",
                "cortexkit_user_config_path": user_path,
            }))
        };

        let response = handle_configure_for_test(&request(), &ctx);
        assert!(response.success, "configure failed: {:?}", response.data);
        assert!(!ctx.config().indexes.trigram);
        assert!(!ctx.config().indexes.semantic);
        let migrated: Vec<&Value> = response.data["warnings"]
            .as_array()
            .expect("warnings")
            .iter()
            .filter(|warning| warning["kind"] == "config_migrated")
            .collect();
        assert_eq!(migrated.len(), 1, "{:?}", response.data["warnings"]);
        assert_eq!(migrated[0]["code"], "user_config_migrated");
        assert!(migrated[0]["hint"]
            .as_str()
            .unwrap()
            .contains("retired keys (search_index)"));
        let user_text = fs::read_to_string(&user_path).unwrap();
        assert!(!user_text.contains("search_index"), "{user_text}");
        assert_eq!(fs::read_to_string(&project_path).unwrap(), project_text);

        let again = handle_configure_for_test(&request(), &ctx);
        assert!(again.success, "configure failed: {:?}", again.data);
        assert!(!again.data["warnings"]
            .as_array()
            .is_some_and(|warnings| warnings
                .iter()
                .any(|warning| warning["kind"] == "config_migrated")));
        assert_eq!(fs::read_to_string(&project_path).unwrap(), project_text);

        let refused_text = r#"{ "semantic_search": false, "gh_read": { "enabled": true } }"#;
        write_config(&project_path, refused_text);
        let refused = handle_configure_for_test(&request(), &ctx);
        assert!(!refused.success, "{refused:?}");
        assert_eq!(refused.data["code"], "config_rejected", "{refused:?}");
        assert!(
            refused.data["message"].as_str().is_some_and(|message| message.contains(
                "removed_config_key:semantic_search; run npx @cortexkit/aft doctor --fix"
            ) && message.contains(
                "removed_config_key:gh_read; run npx @cortexkit/aft doctor --fix"
            )),
            "{refused:?}"
        );
        assert_eq!(fs::read_to_string(&project_path).unwrap(), refused_text);
    }

    #[test]
    fn prepared_bind_branches_do_not_reselect_config_files() {
        let source = include_str!("configure.rs");
        for marker in [
            "else if let Some(snapshot) = bind_config.as_ref() {",
            "let sources = if let Some(snapshot) = bind_config.as_ref() {",
            "let legacy_notice = if let Some(snapshot) = bind_config.as_ref() {",
        ] {
            let body = source
                .split_once(marker)
                .expect("prepared branch exists")
                .1
                .split_once("} else")
                .unwrap()
                .0;
            assert!(body.contains("snapshot."));
            for forbidden in [
                "resolve_config_tiers_for_configure",
                "from_configure",
                "project_config_read_path",
                ".exists(",
                "std::fs::",
            ] {
                assert!(
                    !body.contains(forbidden),
                    "prepared configure re-observes files: {forbidden}"
                );
            }
        }
    }

    #[test]
    fn prepared_bind_config_keeps_content_sources_and_notice_across_attempts() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_context();
        let user = temp.path().join("user.jsonc");
        let project = temp.path().join(".pi/aft.jsonc");
        let canonical = temp.path().join(".cortexkit/aft.jsonc");
        write_config(&user, r#"{"url_fetch_allow_private":true}"#);
        write_config(&project, r#"{"callgraph_chunk_size":7}"#);
        write_config(
            &temp.path().join(".opencode/aft.jsonc"),
            r#"{"callgraph_chunk_size":8}"#,
        );
        let snapshot = crate::subc_config::PreparedBindConfig::read(
            Some(&user),
            temp.path(),
            &crate::harness::Harness::Pi,
        )
        .unwrap();
        write_config(&user, r#"{"url_fetch_allow_private":false}"#);
        write_config(&canonical, r#"{"callgraph_chunk_size":9}"#);
        fs::remove_file(&project).unwrap();
        let request = configure_request_with_params(json!({
            "project_root": temp.path(), "harness":"pi", "cortexkit_user_config_path":user,
        }));
        for _ in 0..2 {
            let response =
                crate::subc_config::with_prepared_bind_config(Arc::clone(&snapshot), || {
                    handle_configure_for_test(&request, &ctx)
                });
            assert!(response.success, "{:?}", response.data);
            assert_eq!(ctx.config().callgraph_chunk_size, 7);
            assert!(ctx.config().url_fetch_allow_private);
            let sources = ctx.config_live().sources().expect("applied sources");
            assert_eq!(sources.project.path.as_deref(), Some(project.as_path()));
            assert!(sources.project.from_file);
            assert_eq!(
                sources.project.text.as_deref(),
                Some(r#"{"callgraph_chunk_size":7}"#)
            );
            assert!(response.data["warnings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|warning| warning["code"] == "legacy_project_config_location"));
        }
        assert!(crate::subc_config::current_prepared_bind_config().is_none());
    }

    #[test]
    fn configure_legacy_project_file_is_read_in_place_and_reports_its_location() {
        for harness in ["opencode", "pi"] {
            let temp = tempfile::tempdir().unwrap();
            let ctx = test_context();
            // Join one component at a time so the expected paths use the
            // host separator, as the paths the notice prints do; a joined
            // "dir/file" literal keeps its `/` on Windows and would not match.
            let project_path = temp.path().join(format!(".{harness}")).join("aft.jsonc");
            let canonical = temp.path().join(".cortexkit").join("aft.jsonc");
            let text = "{\n // committed config\n \"bash\": { \"compress\": false },\n \"callgraph_chunk_size\": 7,\n \"url_fetch_allow_private\": true\n}\n";
            write_config(&project_path, text);
            let request = configure_request_with_params(json!({
                "project_root": temp.path(),
                "harness": harness,
                "cortexkit_user_config_path": temp.path().join("missing-user.jsonc"),
                "config": [project_tier(json!({"callgraph_chunk_size": 4}))],
            }));
            let response = handle_configure_for_test(&request, &ctx);
            assert!(response.success, "{:?}", response.data);
            assert_eq!(ctx.config().callgraph_chunk_size, 7);
            assert!(!ctx.config().experimental_bash_compress);
            assert!(
                !ctx.config().url_fetch_allow_private,
                "legacy paths retain project-tier trust"
            );
            assert_eq!(fs::read_to_string(&project_path).unwrap(), text);
            assert!(!canonical.exists());
            let warnings = response.data["warnings"].as_array().unwrap();
            let notice = warnings
                .iter()
                .find(|warning| warning["code"] == "legacy_project_config_location")
                .expect("location notice");
            let message = notice["message"].as_str().unwrap();
            assert!(message.contains(&project_path.display().to_string()));
            assert!(message.contains(&canonical.display().to_string()));
            assert!(message.contains("doctor --fix"));
        }
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
                  "doc": "{ \"restrict_to_project_root\": true, \"indexes\": { \"trigram\": true }, \"backup\": { \"enabled\": false, \"max_depth\": 7 }, \"disabled_tools\": [\"aft_safety\"] }" },
                { "tier": "project", "source": "/p/.opencode/aft.jsonc",
                  "doc": "{ \"restrict_to_project_root\": false, \"semantic\": { \"api_key_env\": \"EVIL\" }, \"backup\": { \"enabled\": true, \"max_depth\": 1 }, \"disabled_tools\": [\"aft_safety\"] }" }
            ]
        }));

        let response = handle_configure_for_test(&req, &ctx);
        assert!(response.success, "configure failed: {:?}", response.data);

        // Core-resolved field applied: the user trigram setting survived.
        assert!(ctx.config().indexes.trigram);
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
                    "indexes": { "trigram": false, "semantic": false, "callgraph": false }
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
                    "indexes": { "trigram": false, "semantic": false, "callgraph": false }
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
                    "indexes": { "trigram": false, "semantic": false, "callgraph": false },
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
    fn configure_cancellation_stops_the_git_probe_retry_ladder() {
        let _probe_lock = crate::search_index::git_root_commit_probe_override_lock_for_test();
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        let storage = temp.path().join("storage");
        init_git_fixture(&root);
        let canonical_root = std::fs::canonicalize(&root).unwrap();
        let probe_attempts = Arc::new(AtomicUsize::new(0));
        let (release_probe, probe_release) = mpsc::channel();
        let _probe_guard =
            crate::search_index::force_git_root_commit_probe_held_transient_for_paths_for_test(
                vec![canonical_root.clone()],
                Arc::clone(&probe_attempts),
                probe_release,
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
                "indexes": { "trigram": true, "semantic": false, "callgraph": false },
            }))],
        }));
        let request_id = request.id.clone();
        let (mut response_rx, cancellation) = executor.submit_cancellable_async(
            root_id.clone(),
            crate::executor::Lane::Mutating,
            request_id,
            Box::new(move |ctx| super::handle_configure(&request, ctx)),
        );
        // Rebound here so it is the last binding and drops first if the test
        // panics: until it drops, the held probe attempt keeps the executor's
        // worker busy (the executor joins it on drop) and keeps the
        // probe-override lock that the probe guard's drop takes.
        let release_probe = release_probe;

        // Cancelling only tests the ladder once the ladder is running, so wait
        // for the first probe attempt. Everything configure does before it
        // (real git spawns, storage setup) can take seconds on a loaded Windows
        // runner, so the wait ends on one of the two events that settle it:
        // the probe starting, or configure finishing without ever reaching it.
        // The held stub cannot finish the ladder on its own, so the second
        // event really does mean the probe was never reached. The deadline is
        // only a hang guard.
        let hang_guard = Instant::now() + Duration::from_secs(120);
        while probe_attempts.load(Ordering::SeqCst) == 0 {
            match response_rx.try_recv() {
                Ok(response) => panic!(
                    "configure finished without reaching the stubbed git probe: {}",
                    response.data
                ),
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    panic!(
                        "configure dropped its completion sender before the stubbed git probe ran"
                    )
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
            }
            assert!(
                Instant::now() < hang_guard,
                "configure neither reached the stubbed git probe nor finished"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        // The first attempt is held in flight, so it is the only one counted.
        let attempts_at_cancel = probe_attempts.load(Ordering::SeqCst);
        assert_eq!(attempts_at_cancel, 1);
        assert_eq!(
            executor.cancel_job(&root_id, &cancellation),
            crate::executor::JobCancelOutcome::RunningSignalled
        );
        release_probe
            .send(())
            .expect("held git probe is waiting for release");

        // The probe returns Transient, so an uncancelled configure keeps going
        // round the retry ladder. What cancellation has to do is stop it, and
        // that is observable as attempts no longer growing -- independent of how
        // long the machine took to unwind, which a wall-clock bound is not. The
        // timeout here is only liveness: it must outlast the whole ladder, so
        // that a hang fails rather than a slow runner.
        let response = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("build cancellation test runtime")
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(30), response_rx)
                    .await
                    .expect("cancelled configure never completed")
                    .expect("configure completion sender")
            });
        assert!(!response.success);
        assert_eq!(response.data["code"], "request_cancelled");
        // The cancel landed while the first attempt was held in flight, so the
        // ladder must stop when that attempt returns; any further attempt is
        // the ladder continuing past a cancel.
        let attempts_after = probe_attempts.load(Ordering::SeqCst);
        assert_eq!(
            attempts_after, attempts_at_cancel,
            "cancelled configure kept retrying the git probe: {attempts_at_cancel} attempts at cancel, {attempts_after} after"
        );
    }

    #[test]
    fn handle_configure_keeps_unreadable_git_unindexed_without_saving_a_path_identity() {
        let _probe_lock = crate::search_index::git_root_commit_probe_override_lock_for_test();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        std::fs::create_dir_all(root.join(".git")).expect("create git marker");
        let storage = temp.path().join("storage");
        let canonical_root = std::fs::canonicalize(&root).expect("canonical root");
        let _override =
            crate::search_index::force_git_root_commit_probe_transient_for_paths_for_test(
                vec![root.clone(), canonical_root.clone()],
                "spawn failed: Too many open files (os error 24)",
            );
        let ctx = test_context();
        let req = configure_request_with_params(json!({
            "project_root": root.clone(),
            "harness": "opencode",
            "storage_dir": storage.clone(),
            "config": [user_tier(json!({
                "indexes": { "trigram": true, "semantic": false, "callgraph": false },
            }))],
        }));

        let response = handle_configure_for_test(&req, &ctx);

        assert!(
            response.success,
            "direct tools must remain usable: {response:?}"
        );
        assert!(ctx.artifact_identity_provisional());
        assert!(!ctx.config().indexes.trigram);
        assert!(!ctx.config().indexes.semantic);
        assert!(!ctx.config().indexes.callgraph);
        assert!(!false);
        let reason = ctx.artifact_identity_provisional_reason().unwrap();
        assert!(reason.contains("git_identity_unavailable"), "{reason}");
        assert!(reason.contains("Too many open files"), "{reason}");
        assert_eq!(ctx.cached_artifact_cache_key(&canonical_root), None);
        assert_eq!(crate::root_identity::read(&root, &storage).unwrap(), None);
        let path_key = crate::search_index::artifact_path_identity_key_for_test(&root);
        assert!(!storage.join("index").join(&path_key).exists());
        assert!(!storage.join("semantic").join(&path_key).exists());
        assert!(!storage.join("callgraph").join(&path_key).exists());
    }

    /// Hold aft.db in an exclusive transaction. In rollback-journal mode this
    /// also blocks readers, so the bind cannot read its legacy identity pin.
    fn hold_aft_db_exclusively(storage: &std::path::Path) -> rusqlite::Connection {
        std::fs::create_dir_all(storage).unwrap();
        let lock = rusqlite::Connection::open(storage.join("aft.db")).unwrap();
        lock.execute_batch("PRAGMA journal_mode=DELETE; BEGIN EXCLUSIVE;")
            .unwrap();
        lock
    }

    fn trigram_bind_request(root: &std::path::Path, storage: &std::path::Path) -> RawRequest {
        configure_request_with_params(json!({
            "project_root": root,
            "harness": "opencode",
            "storage_dir": storage,
            "config": [user_tier(json!({
                "indexes": { "trigram": true, "semantic": false, "callgraph": false },
                "views": { "enabled": false },
            }))],
        }))
    }

    /// A git checkout and its storage folder, with aft.db held busy.
    fn busy_bind_fixture() -> (tempfile::TempDir, PathBuf, PathBuf, rusqlite::Connection) {
        let temp = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(temp.path()).unwrap();
        let root = base.join("repo");
        init_git_fixture(&root);
        let storage = base.join("storage");
        let lock = hold_aft_db_exclusively(&storage);
        (temp, root, storage, lock)
    }

    fn has_busy_provisional_reason(ctx: &AppContext) -> bool {
        ctx.degraded_reasons()
            .iter()
            .any(|reason| reason == crate::search_index::PROVISIONAL_IDENTITY_REASON)
    }

    #[test]
    fn busy_aft_db_at_first_bind_enables_indexes_once_the_database_frees() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let (_temp, root, storage, lock) = busy_bind_fixture();
        let ctx = test_context();
        let request = trigram_bind_request(&root, &storage);
        crate::provisional_identity_retry::with_timing_for_test(
            Duration::ZERO,
            Duration::from_secs(600),
            || {
                let response = super::handle_configure(&request, &ctx);
                assert!(response.success, "{}", response.data);
                assert!(
                    has_busy_provisional_reason(&ctx),
                    "{:?}",
                    ctx.degraded_reasons()
                );
                assert!(!ctx.config().indexes.trigram);
                // The bind's own tail runs first; a retry never interleaves with it.
                super::drain_deferred_configure_maintenance(&ctx);

                // While aft.db stays busy an attempt changes nothing.
                super::drain_deferred_configure_maintenance(&ctx);
                assert_eq!(
                    ctx.provisional_identity_retry().attempts_for_test(),
                    Some(1)
                );
                assert!(!ctx.config().indexes.trigram);
                assert_eq!(crate::root_identity::read(&root, &storage).unwrap(), None);
                assert_eq!(ctx.cached_artifact_cache_key(&root), None);

                lock.execute_batch("ROLLBACK").unwrap();
                drop(lock);
                // No second bind: the next maintenance drain settles the root.
                super::drain_deferred_configure_maintenance(&ctx);
                assert!(
                    ctx.artifact_identity_provisional_reason().is_none(),
                    "{:?}",
                    ctx.degraded_reasons()
                );
                assert!(ctx.config().indexes.trigram);
                assert!(!ctx.provisional_identity_retry().is_armed());
                assert!(crate::root_identity::read(&root, &storage)
                    .unwrap()
                    .is_some());
                assert!(ctx.cached_artifact_cache_key(&root).is_some());
            },
        );
    }

    /// While aft.db is busy, a reader outside configure (standing-root
    /// maintenance reads every session actor's key) can only derive the
    /// root's key from Git. With an older standing path pin for the root in
    /// aft.db, the identity settles on the path key instead, so that Git-only
    /// key must never be cached as the root's key, and caching it must not
    /// stop the retry.
    #[test]
    fn provisional_root_caches_no_git_only_key_and_still_settles() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let temp = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(temp.path()).unwrap();
        let root = base.join("repo");
        init_git_fixture(&root);
        let storage = base.join("storage");
        std::fs::create_dir_all(&storage).unwrap();
        {
            let mut database = crate::db::open(&storage.join("aft.db")).unwrap();
            crate::db::standing_roots::ensure_standing_root(
                &mut database,
                &crate::db::standing_roots::StandingRootRecord {
                    literal_path: root.display().to_string(),
                    resolved_target: root.display().to_string(),
                    resolved_git_toplevel: None,
                    scoped_relative_path: None,
                },
                &[crate::config::IndexKind::Search],
            )
            .unwrap();
        }
        let lock = hold_aft_db_exclusively(&storage);
        let ctx = test_context();
        // A reader that ran before the bind published its degraded reasons.
        let git_key = ctx.memoized_artifact_cache_key(&root);
        assert_eq!(
            ctx.cached_artifact_cache_key(&root).as_deref(),
            Some(git_key.as_str())
        );
        crate::provisional_identity_retry::with_timing_for_test(
            Duration::ZERO,
            Duration::from_secs(600),
            || {
                let response =
                    super::handle_configure(&trigram_bind_request(&root, &storage), &ctx);
                assert!(response.success, "{}", response.data);
                assert!(
                    has_busy_provisional_reason(&ctx),
                    "{:?}",
                    ctx.degraded_reasons()
                );
                let cached_after_bind = ctx.cached_artifact_cache_key(&root);
                super::drain_deferred_configure_maintenance(&ctx);

                // A standing-maintenance pass reads the key while aft.db is busy.
                assert_eq!(ctx.memoized_artifact_cache_key(&root), git_key);
                let cached_after_pass = ctx.cached_artifact_cache_key(&root);

                lock.execute_batch("ROLLBACK").unwrap();
                drop(lock);
                super::drain_deferred_configure_maintenance(&ctx);
                assert!(
                    ctx.artifact_identity_provisional_reason().is_none(),
                    "{:?}",
                    ctx.degraded_reasons()
                );
                assert!(ctx.config().indexes.trigram);
                let settled = ctx
                    .cached_artifact_cache_key(&root)
                    .expect("the settled bind publishes its key");
                assert_eq!(
                    settled,
                    crate::search_index::artifact_path_identity_key_for_test(&root)
                );
                assert_ne!(
                    settled, git_key,
                    "the fixture must settle on a key the Git-only derivation does not produce"
                );
                assert_eq!(ctx.memoized_artifact_cache_key(&root), settled);
                assert_eq!(
                    cached_after_bind, None,
                    "a provisional bind keeps no key cached for its root"
                );
                assert_eq!(
                    cached_after_pass, None,
                    "a read of a provisional root's key does not cache it"
                );
            },
        );
    }

    #[test]
    fn prepared_bind_identity_replay_retains_the_original_observation() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let (_temp, root, storage, lock) = busy_bind_fixture();
        let ctx = test_context();
        let user = root.join("user.jsonc");
        let legacy = root.join(".pi/aft.jsonc");
        write_config(
            &user,
            r#"{"indexes":{"trigram":true,"semantic":false,"callgraph":false},"views":{"enabled":false},"url_fetch_allow_private":true}"#,
        );
        write_config(&legacy, r#"{"callgraph_chunk_size":7}"#);
        let snapshot = crate::subc_config::PreparedBindConfig::read(
            Some(&user),
            &root,
            &crate::harness::Harness::Pi,
        )
        .unwrap();
        let request = configure_request_with_params(
            json!({"project_root":root,"storage_dir":storage,"harness":"pi","cortexkit_user_config_path":user}),
        );
        crate::provisional_identity_retry::with_timing_for_test(
            Duration::ZERO,
            Duration::from_secs(600),
            || {
                let response = crate::subc_config::with_prepared_bind_config(snapshot, || {
                    super::handle_configure(&request, &ctx)
                });
                assert!(response.success, "{}", response.data);
                super::drain_deferred_configure_maintenance(&ctx);
                assert!(ctx.provisional_identity_retry().is_armed());
                write_config(&user, r#"{"url_fetch_allow_private":false}"#);
                write_config(
                    &root.join(".cortexkit/aft.jsonc"),
                    r#"{"callgraph_chunk_size":9}"#,
                );
                fs::remove_file(&legacy).unwrap();
                lock.execute_batch("ROLLBACK").unwrap();
                drop(lock);
                super::drain_deferred_configure_maintenance(&ctx);
                assert!(!ctx.provisional_identity_retry().is_armed());
                assert_eq!(ctx.config().callgraph_chunk_size, 7);
                assert!(ctx.config().url_fetch_allow_private);
                assert_eq!(
                    ctx.config_live().sources().unwrap().project.path.as_deref(),
                    Some(legacy.as_path())
                );
            },
        );
    }

    /// A replay can resolve and memoize the settled key and still be refused
    /// before its commit (for example by a storage error at its retention or
    /// artifact-owner step). The root then stays provisional with the settled
    /// key already in the context's key caches, and the retry must replay it
    /// again rather than read the cached key as proof that it settled.
    #[test]
    fn identity_retry_replays_again_after_a_replay_refused_past_its_key_step() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let (_temp, root, storage, lock) = busy_bind_fixture();
        let ctx = test_context();
        crate::provisional_identity_retry::with_timing_for_test(
            Duration::ZERO,
            Duration::from_secs(600),
            || {
                let response =
                    super::handle_configure(&trigram_bind_request(&root, &storage), &ctx);
                assert!(response.success, "{}", response.data);
                super::drain_deferred_configure_maintenance(&ctx);
                lock.execute_batch("ROLLBACK").unwrap();
                drop(lock);

                // Simulate the key caches after a replay resolves its key but is
                // refused before its commit.
                let key = ctx
                    .memoized_artifact_cache_key_for_configure(&root, &root, &storage, None)
                    .unwrap()
                    .expect("aft.db is free, so the identity settles");
                assert_eq!(
                    ctx.cached_artifact_cache_key(&root).as_deref(),
                    Some(key.as_str())
                );
                assert!(has_busy_provisional_reason(&ctx));
                assert!(ctx.provisional_identity_retry().is_armed());

                super::drain_deferred_configure_maintenance(&ctx);
                assert!(
                    ctx.artifact_identity_provisional_reason().is_none(),
                    "{:?}",
                    ctx.degraded_reasons()
                );
                assert!(ctx.config().indexes.trigram);
                assert!(!ctx.provisional_identity_retry().is_armed());
                assert_eq!(ctx.cached_artifact_cache_key(&root), Some(key));
            },
        );
    }

    #[test]
    fn busy_aft_db_retry_gives_up_and_names_the_remedy() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let (_temp, root, storage, lock) = busy_bind_fixture();
        let ctx = test_context();
        crate::provisional_identity_retry::with_timing_for_test(
            Duration::ZERO,
            Duration::ZERO,
            || {
                let response =
                    super::handle_configure(&trigram_bind_request(&root, &storage), &ctx);
                assert!(response.success, "{}", response.data);
                super::drain_deferred_configure_maintenance(&ctx);
                super::drain_deferred_configure_maintenance(&ctx);
            },
        );
        let reasons = ctx.degraded_reasons();
        assert!(
            reasons
                .iter()
                .any(|reason| reason == crate::search_index::PROVISIONAL_IDENTITY_EXHAUSTED_REASON),
            "{reasons:?}"
        );
        assert!(!has_busy_provisional_reason(&ctx), "{reasons:?}");
        assert!(ctx.artifact_identity_provisional());
        assert!(!ctx.provisional_identity_retry().is_armed());
        assert!(!ctx.config().indexes.trigram);
        drop(lock);
    }

    #[test]
    fn busy_aft_db_retry_stops_when_the_root_unbinds() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let (_temp, root, storage, lock) = busy_bind_fixture();
        let ctx = test_context();
        crate::provisional_identity_retry::with_timing_for_test(
            Duration::ZERO,
            Duration::from_secs(600),
            || {
                let response =
                    super::handle_configure(&trigram_bind_request(&root, &storage), &ctx);
                assert!(response.success, "{}", response.data);
                super::drain_deferred_configure_maintenance(&ctx);
                assert!(ctx.provisional_identity_retry().is_armed());

                ctx.mark_subc_unbound();
                lock.execute_batch("ROLLBACK").unwrap();
                drop(lock);
                super::drain_deferred_configure_maintenance(&ctx);
                super::drain_deferred_configure_maintenance(&ctx);
            },
        );
        assert!(!ctx.provisional_identity_retry().is_armed());
        assert!(!ctx.config().indexes.trigram);
        assert!(
            has_busy_provisional_reason(&ctx),
            "{:?}",
            ctx.degraded_reasons()
        );
        assert_eq!(crate::root_identity::read(&root, &storage).unwrap(), None);
        assert_eq!(ctx.cached_artifact_cache_key(&root), None);
    }

    #[test]
    fn quiescing_a_root_cancels_its_identity_retry() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let (_temp, root, storage, lock) = busy_bind_fixture();
        let ctx = test_context();
        let response = super::handle_configure(&trigram_bind_request(&root, &storage), &ctx);
        assert!(response.success, "{}", response.data);
        assert!(ctx.provisional_identity_retry().is_armed());
        super::cancel_deferred_configure_maintenance(&ctx);
        assert!(!ctx.provisional_identity_retry().is_armed());
        drop(lock);
    }

    #[test]
    fn dubious_ownership_identity_does_not_retry_in_the_background() {
        let _probe_lock = crate::search_index::git_root_commit_probe_override_lock_for_test();
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap().join("repo");
        std::fs::create_dir_all(root.join(".git")).expect("create git marker");
        let storage = temp.path().join("storage");
        let _override =
            crate::search_index::force_git_root_commit_probe_transient_for_paths_for_test(
                vec![root.clone()],
                "fatal: detected dubious ownership in repository",
            );
        let ctx = test_context();
        crate::provisional_identity_retry::with_timing_for_test(
            Duration::ZERO,
            Duration::from_secs(600),
            || {
                let response =
                    super::handle_configure(&trigram_bind_request(&root, &storage), &ctx);
                assert!(response.success, "{}", response.data);
                let reason = ctx.artifact_identity_provisional_reason().unwrap();
                assert!(reason.contains("dubious ownership"), "{reason}");
                assert!(!ctx.provisional_identity_retry().is_armed());
                let generation = ctx.configure_generation();
                super::drain_deferred_configure_maintenance(&ctx);
                super::drain_deferred_configure_maintenance(&ctx);
                assert_eq!(
                    ctx.configure_generation(),
                    generation,
                    "no bind may be replayed for an unreadable Git identity"
                );
            },
        );
        assert!(!ctx.config().indexes.trigram);
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

    /// A semantic configure as a route under `harness` and `session` would send
    /// it, with optional user-tier `index.roots`.
    fn configure_semantic_bind(
        root: &Path,
        storage: &Path,
        base_url: &str,
        harness: &str,
        session: &str,
        index_roots: Option<serde_json::Value>,
    ) -> RawRequest {
        let mut user = json!({
            "indexes": { "trigram": false, "semantic": true, "callgraph": false },
            "semantic": {
                "backend": "openai_compatible",
                "model": "counting-test-embedding",
                "base_url": base_url,
                "timeout_ms": 5_000,
                "max_batch_size": 64,
                "max_files": 1_000
            }
        });
        if let Some(roots) = index_roots {
            user["index"] = json!({ "roots": roots });
        }
        configure_request_with_session(
            json!({
                "project_root": root,
                "harness": harness,
                "storage_dir": storage,
                "config": [user_tier(user)],
            }),
            session,
        )
    }

    fn semantic_admission_project(parent: &Path) -> PathBuf {
        let project = parent.join("project");
        std::fs::create_dir_all(project.join("src")).unwrap();
        for index in 0..3 {
            std::fs::write(
                project.join("src").join(format!("opened_{index}.rs")),
                format!("pub fn opened_symbol_{index}() {{}}\n"),
            )
            .unwrap();
        }
        project
    }

    fn total_embedding_requests(server: &CountingEmbeddingServer) -> usize {
        server
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// Starts a semantic cold build over nine small files at one chunk per
    /// embed batch and returns once the first batch is parked in the embedding
    /// server, so the build holds its cold-build permit mid-loop.
    fn semantic_cold_build_parked_on_first_batch(
        grace: Duration,
    ) -> (
        TestContext,
        CountingEmbeddingServer,
        tempfile::TempDir,
        PathBuf,
        PathBuf,
    ) {
        let server = CountingEmbeddingServer::start();
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let storage = temp.path().join("storage");
        std::fs::create_dir_all(&project).unwrap();
        for index in 0..8 {
            std::fs::write(
                project.join(format!("batch_{index}.rs")),
                format!("pub fn abandoned_batch_symbol_{index}() -> usize {{ {index} }}\n"),
            )
            .unwrap();
        }
        init_git_fixture(&project);
        let request =
            configure_semantic_with_options(&project, &storage, &server.base_url, true, 1, false);
        let ctx = test_context();
        ctx.isolate_cold_build_limiter_for_test(4);
        ctx.set_unbound_build_abandon_grace_for_test(grace);

        let response = handle_configure_for_test(&request, &ctx);
        assert!(response.success, "configure failed: {:?}", response.data);
        super::drain_deferred_configure_maintenance(&ctx);
        assert!(
            server.wait_for_non_probe_request_count(1, Duration::from_secs(10)),
            "semantic cold build did not reach the embedding server"
        );
        (ctx, server, temp, project, storage)
    }

    fn configure_ram_overlay_worktree(root: &Path, storage: &Path) -> RawRequest {
        configure_request_with_params(json!({
            "project_root": root,
            "harness": "opencode",
            "storage_dir": storage,
            "config": [user_tier(json!({
                "indexes": { "trigram": true, "semantic": false, "callgraph": false },
                "worktree": { "ram_overlay": true }
            }))]
        }))
    }

    fn grep_matches_and_status(ctx: &AppContext, pattern: &str) -> (u64, String) {
        let response = crate::commands::grep::handle_grep(&grep_request(pattern), ctx);
        assert!(
            response.success,
            "grep {pattern} failed: {:?}",
            response.data
        );
        (
            response.data["total_matches"].as_u64().unwrap_or_default(),
            response.data["index_status"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        )
    }

    const PRE_EXISTING_DRIFT_TOKENS: [&str; 4] = [
        "BRANCH_EDITED_TOKEN",
        "BRANCH_NEW_FILE_TOKEN",
        "UNCOMMITTED_EDIT_TOKEN",
        "UNTRACKED_TOKEN",
    ];

    // ---------------------------------------------------------------------
    // A second checkout of an OLDER commit of the same repository borrows the
    // live checkout's search snapshot.
    //
    // All roots whose HEAD history shares a root commit derive the same
    // artifact key (`artifact_cache_key` hashes the root commit set), so a
    // clone, a `cp -R` copy or a linked worktree opens the live checkout's
    // `cache.bin`. With `worktree.ram_overlay` on (the default), the borrow-only
    // root reconciles that snapshot with its own files before reporting ready,
    // so it answers with its own files and contents. With the escape hatch
    // (`ram_overlay: false`) the snapshot is served as it is, and only the
    // on-disk backstop in glob and aft_search keeps the live checkout's files
    // out of the results. See
    // docs/investigations/borrowed-index-phantom-paths-2026-09.md.
    // ---------------------------------------------------------------------

    /// How the second, older checkout of the live repository is produced.
    #[derive(Clone, Copy, Debug)]
    enum OlderCheckoutKind {
        /// `git clone --shared live scratch`, then detach at the older commit.
        SharedClone,
        /// `git clone live scratch`, then detach at the older commit.
        PlainClone,
        /// `git worktree add --detach scratch <older commit>` from the live repo.
        LinkedWorktree,
        /// `cp -R live scratch`, then detach at the older commit. Unix only,
        /// like the test that uses it.
        #[cfg(unix)]
        CopiedTree,
    }

    /// File that exists only at the live checkout's HEAD.
    const PHANTOM_REL_PATH: &str = "extra/phantom.ts";

    fn git_ok(root: &Path, args: &[&str]) {
        let status = git_command(root).args(args).status().unwrap();
        assert!(
            status.success(),
            "git {args:?} failed in {}",
            root.display()
        );
    }

    /// Canonical temp root in a form `git worktree add` accepts: on Windows,
    /// `fs::canonicalize` returns a `\\?\` verbatim path, which git rejects.
    fn git_friendly_canonical(path: &Path) -> PathBuf {
        let canonical = std::fs::canonicalize(path).unwrap();
        #[cfg(windows)]
        let canonical = crate::windows_path::normalize_windows_path(&canonical);
        canonical
    }

    fn git_stdout(root: &Path, args: &[&str]) -> String {
        let output = git_command(root).args(args).output().unwrap();
        assert!(output.status.success(), "git {args:?} failed");
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn configure_ram_overlay_escape_hatch(root: &Path, storage: &Path) -> RawRequest {
        configure_request_with_params(json!({
            "project_root": root,
            "harness": "opencode",
            "storage_dir": storage,
            "config": [user_tier(json!({
                "indexes": { "trigram": true, "semantic": false, "callgraph": false },
                "worktree": { "ram_overlay": false }
            }))]
        }))
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
                "indexes": { "trigram": false, "semantic": false, "callgraph": false }
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

    struct IndexOrderTestGuard {
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl Drop for IndexOrderTestGuard {
        fn drop(&mut self) {
            INDEX_ORDER_GRACE_MS.store(30_000, Ordering::SeqCst);
            clear_index_order_timeout_logs();
        }
    }

    fn index_order_test_guard() -> IndexOrderTestGuard {
        IndexOrderTestGuard {
            _guard: INDEX_ORDER_TEST_LOCK
                .get_or_init(|| Mutex::new(()))
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        }
    }

    fn clear_index_order_timeout_logs() {
        INDEX_ORDER_TIMEOUT_LOGS
            .get_or_init(|| Mutex::new(Vec::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    fn callgraph_event(
        root: &Path,
        kind: crate::logging::IndexEventKind,
    ) -> crate::logging::IndexEvent {
        crate::logging::IndexEvent::new(
            kind,
            crate::logging::IndexPlane::Callgraph,
            "configure-order-test-build",
            root,
            "configure-order-test-key",
        )
    }

    fn index_build_started(events: &[String], plane: &str) -> bool {
        events.iter().any(|line| {
            line.contains("kind=build_started") && line.contains(&format!("plane={plane}"))
        })
    }

    fn lifecycle_fixture() -> (tempfile::TempDir, tempfile::TempDir) {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        init_git_fixture(root.path());
        std::fs::write(
            root.path().join("lib.rs"),
            "pub fn entry() { leaf(); }\npub fn leaf() {}\n",
        )
        .unwrap();
        (root, storage)
    }

    struct BusyObsoleteNonGitUpgrade {
        ctx: TestContext,
        _root: tempfile::TempDir,
        _storage: tempfile::TempDir,
        source: PathBuf,
        request: RawRequest,
        slots: Vec<crate::cold_build_limiter::ColdBuildPermit>,
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
            "indexes": { "trigram": false, "semantic": false, "callgraph": false }
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
                "indexes": { "trigram": false, "semantic": false, "callgraph": false }
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
                "indexes": { "trigram": false, "semantic": false, "callgraph": true }
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
        // Manifests are not part of the configure identity: a manifest change
        // is handed to the incremental callgraph refresh instead, so the
        // configure stays equivalent while the manifests are still read once.
        assert_eq!(ctx.configure_generation(), enabled_generation);
        assert_eq!(super::workspace_manifest_fingerprint_scans_for_test(), 2);
        assert!(handle_configure_for_test(&enabled, &ctx).success);
        assert_eq!(ctx.configure_generation(), enabled_generation);
        assert_eq!(super::workspace_manifest_fingerprint_scans_for_test(), 3);
    }

    /// Writes `packages/pkg-a/package.json` under `root` with `contents`.
    fn write_workspace_manifest(root: &std::path::Path, contents: &str) {
        std::fs::create_dir_all(root.join("packages/pkg-a")).unwrap();
        std::fs::write(root.join("packages/pkg-a/package.json"), contents).unwrap();
    }

    #[test]
    fn configure_publishes_bash_compression_before_deferred_maintenance() {
        let _env_guard = home_env_mutex();
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        init_git_fixture(root.path());
        let ctx = test_context();
        assert!(!ctx.bash_compress_flag().load(Ordering::Relaxed));
        let request = configure_request_with_params(json!({
            "project_root": root.path(),
            "harness": "opencode",
            "storage_dir": storage.path(),
            "config": [user_tier(json!({
                "indexes": { "trigram": false, "semantic": false, "callgraph": false },
                "bash": { "rewrite": false, "compress": true, "background": false }
            }))]
        }));
        assert!(handle_configure_for_test(&request, &ctx).success);
        assert!(
            ctx.bash_compress_flag().load(Ordering::Relaxed),
            "configure must publish compression before maintenance can run"
        );
    }

    fn callgraph_only_configure_request(
        root: &std::path::Path,
        storage: &std::path::Path,
    ) -> RawRequest {
        configure_request_with_params(json!({
            "project_root": root,
            "harness": "opencode",
            "storage_dir": storage,
            "config": [user_tier(json!({
                "indexes": { "trigram": false, "semantic": false, "callgraph": true }
            }))]
        }))
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
                "indexes": { "trigram": false, "semantic": false, "callgraph": false }
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
        // Biome is installed on many development machines; pin it as missing
        // so the test checks the warning rule, not the machine.
        let warnings = super::detect_missing_tools_with_cache(
            &languages,
            &config,
            std::collections::HashMap::from([("biome".to_string(), false)]),
        );
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

        let go = bin_dir.join("go");
        crate::executable_cache::write_executable(
            &go,
            b"#!/bin/sh\nif [ \"$1\" = \"version\" ]; then exit 0; fi\nif [ \"$1\" = \"--version\" ]; then exit 2; fi\nexit 1\n",
        )
        .unwrap();

        let gofmt = bin_dir.join("gofmt");
        crate::executable_cache::write_executable(
            &gofmt,
            b"#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then exit 2; fi\ncat >/dev/null\nexit 0\n",
        )
        .unwrap();

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

    fn assert_broad_configure_root(root: &std::path::Path) {
        let ctx = test_context();
        let req = configure_request_with_params(json!({
            "project_root": root, "harness": "runner",
            "storage_dir": ctx._storage.path(),
            "config": [user_tier(json!({
                "indexes": { "trigram": true, "semantic": true, "callgraph": true },
                "views": { "enabled": true },
            }))],
        }));
        assert!(handle_configure_for_test(&req, &ctx).success);
        assert!(
            !ctx.heavy_root_work_allowed(),
            "broad root must close the heavy-root-work gate"
        );
        assert!(ctx.degraded_reasons().contains(&"root_too_broad".into()));
        let assert_no_workers = || {
            assert!(!ctx.config().indexes.trigram);
            assert!(!ctx.config().indexes.semantic);
            assert!(!ctx.config().indexes.callgraph);
            assert!(!false);
            assert!(ctx.semantic_index_rx().lock().is_none());
            assert!(ctx.callgraph_store_rx().lock().is_none());
            assert!(
                ctx.data_checkout().is_none(),
                "broad root must bind no checkout"
            );
            assert!(true);
            assert!(crate::views::parent::session_for_root(root).is_none());
            assert!(!ctx.watcher_runtime_active());
        };
        assert_no_workers();
        super::drain_deferred_configure_maintenance(&ctx);
        super::ensure_project_watcher(&ctx);
        assert_no_workers();
        // The index store answers the search lane of a root it never binds as
        // off; the meaning lane keeps reporting itself disabled. Both name
        // the broad root as the reason.
        let status = ctx.build_status_snapshot();
        for (lane, word) in [("search_index", "off"), ("semantic_index", "disabled")] {
            assert_eq!(status[lane]["status"], word, "{lane}");
            assert_eq!(status[lane]["reason"], "root_too_broad");
        }
        for plane in crate::feature_status::IndexPlane::ALL {
            let observation =
                crate::feature_status::observed_index_status(&ctx, plane).consumer_json();
            assert_eq!(observation["status"], "off");
            assert_eq!(observation["reason"], "root_too_broad");
        }
        let inspect = RawRequest {
            id: "inspect-broad".into(),
            command: "inspect".into(),
            session_id: None,
            lsp_hints: None,
            params: json!({}),
        };
        let response = crate::commands::inspect::handle_inspect(&inspect, &ctx);
        assert!(!response.success);
        assert_eq!(response.data["code"], "root_too_broad");
        let search = RawRequest {
            id: "search-broad".into(),
            command: "semantic_search".into(),
            session_id: None,
            lsp_hints: None,
            params: json!({"query": "find the authentication handler"}),
        };
        let response = crate::commands::semantic_search::handle_semantic_search(&search, &ctx);
        assert!(!response.success);
        assert_eq!(response.data["code"], "no_search_lanes_enabled");
        for lane in ["trigram", "semantic"] {
            assert_eq!(response.data["lanes"][lane]["status"], "off");
            assert_eq!(response.data["lanes"][lane]["reason"], "root_too_broad");
        }
        assert!(response.data["message"]
            .as_str()
            .unwrap()
            .contains("off: root_too_broad"));
        // A backend left by an older bind must be removed even on the same root.
        install_project_watcher_with(&ctx, root, Vec::new(), |_, _, tx| Ok::<_, &'static str>(tx));
        assert!(ctx.watcher_runtime_active());
        assert!(handle_configure_for_test(&req, &ctx).success);
        super::drain_deferred_configure_maintenance(&ctx);
        super::ensure_project_watcher(&ctx);
        assert_no_workers();
    }

    #[cfg(unix)]
    #[test]
    fn configure_filesystem_root_disables_root_work() {
        let _env = home_env_mutex();
        assert_broad_configure_root(std::path::Path::new("/"));
    }

    #[test]
    fn configure_volume_root_disables_root_work() {
        let _env = home_env_mutex();
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        let (volume, _) = crate::volume::volume_anchor(&canonical).unwrap();
        assert_broad_configure_root(&volume);
    }

    #[cfg(windows)]
    #[test]
    fn configure_windows_drive_root_disables_root_work() {
        let _env = home_env_mutex();
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        assert_broad_configure_root(canonical.ancestors().last().unwrap());
    }

    #[test]
    fn account_home_and_ancestors_are_broad_when_environment_home_differs() {
        let _env = home_env_mutex();
        let account_tree = tempfile::tempdir().unwrap();
        let account_home = account_tree.path().join("account-home");
        std::fs::create_dir(&account_home).unwrap();
        let project = tempfile::tempdir().unwrap();
        let override_home = project.path().join(".home");
        std::fs::create_dir(&override_home).unwrap();
        let _home = EnvVarGuard::set("HOME", override_home.to_str().unwrap());
        let _profile = EnvVarGuard::set("USERPROFILE", override_home.to_str().unwrap());
        let account_home = account_home.canonicalize().unwrap();

        for root in account_home.ancestors().filter(|root| root.is_dir()) {
            assert!(
                project_root_is_too_broad(root, false, Some(&account_home)),
                "account home ancestor must be protected: {}",
                root.display()
            );
        }
        assert!(
            !project_root_is_too_broad(project.path(), false, Some(&account_home)),
            "a project must remain narrow when HOME points inside it"
        );
        assert!(
            !project_root_is_too_broad(project.path(), false, None),
            "an unknown account home must not broaden protection beyond the volume root"
        );
        let volume = crate::volume::volume_anchor(project.path()).unwrap().0;
        assert!(project_root_is_too_broad(&volume, false, None));
    }

    #[test]
    fn configure_home_override_inside_project_keeps_root_work_enabled() {
        let _env = home_env_mutex();
        let _git = crate::test_env::hermetic_git_env_guard();
        let _watcher = EnvVarGuard::set("AFT_TEST_DISABLE_FILE_WATCHER", "0");
        let _sync = EnvVarGuard::set("AFT_TEST_SYNC_FILE_WATCHER_START", "1");
        let project = tempfile::tempdir().unwrap();
        init_git_fixture(project.path());
        let override_home = project.path().join(".home");
        std::fs::create_dir(&override_home).unwrap();
        let _home = EnvVarGuard::set("HOME", override_home.to_str().unwrap());
        let _profile = EnvVarGuard::set("USERPROFILE", override_home.to_str().unwrap());
        let ctx = test_context();
        let req = configure_request_with_params(json!({
            "project_root": project.path(), "harness": "runner", "storage_dir": ctx._storage.path(),
            "config": [user_tier(json!({
                "indexes": {"trigram": true, "semantic": true, "callgraph": true},
                "views": {"enabled": true},
            }))],
        }));

        let response = handle_configure_for_test(&req, &ctx);
        assert!(response.success, "{response:?}");
        assert!(
            !ctx.root_too_broad(),
            "HOME inside a project must not protect its parent"
        );
        assert!(ctx.heavy_root_work_allowed());
        assert!(ctx.config().indexes.trigram);
        assert!(ctx.config().indexes.semantic);
        assert!(ctx.config().indexes.callgraph);
        super::drain_deferred_configure_maintenance(&ctx);
        ctx.stop_watcher_runtime();
    }

    #[cfg(unix)]
    #[test]
    fn configure_symlinked_home_disables_root_work() {
        let _env = home_env_mutex();
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let alias = dir.path().join("alias");
        std::fs::create_dir(&home).unwrap();
        create_dir_symlink(&home, &alias);
        let _home = EnvVarGuard::set("HOME", alias.to_str().unwrap());
        let _profile = EnvVarGuard::set("USERPROFILE", alias.to_str().unwrap());
        assert_broad_configure_root(&home);
        assert_broad_configure_root(&alias);
    }

    #[cfg(unix)]
    #[test]
    fn configure_filesystem_root_still_reads_absolute_path() {
        let _env = home_env_mutex();
        let ctx = test_context();
        let req = configure_request_with_params(json!({
            "project_root": "/", "harness": "runner", "storage_dir": ctx._storage.path(),
        }));
        assert!(handle_configure_for_test(&req, &ctx).success);
        let project = tempfile::tempdir().unwrap();
        let file = project.path().join("readable.txt");
        std::fs::write(&file, "explicit path still works\n").unwrap();
        let read = RawRequest {
            id: "read-root".into(),
            command: "read".into(),
            session_id: None,
            lsp_hints: None,
            params: json!({"file": file}),
        };
        let response = crate::commands::read::handle_read(&read, &ctx);
        assert!(response.success, "{response:?}");
        assert!(response
            .data
            .to_string()
            .contains("explicit path still works"));
    }

    #[test]
    fn configure_normal_project_keeps_root_work_enabled() {
        let _env = home_env_mutex();
        let _git = crate::test_env::hermetic_git_env_guard();
        let _watcher = EnvVarGuard::set("AFT_TEST_DISABLE_FILE_WATCHER", "0");
        let _sync = EnvVarGuard::set("AFT_TEST_SYNC_FILE_WATCHER_START", "1");
        let project = tempfile::tempdir().unwrap();
        init_git_fixture(project.path());
        let ctx = test_context();
        // Treat the temporary project's parent as an injected volume root to
        // exercise /app-style admission without creating a host root directory.
        let canonical_project = project.path().canonicalize().unwrap();
        assert_eq!(
            crate::path_identity::project_root_is_narrow(
                &canonical_project,
                &ctx._storage.path().canonicalize().unwrap(),
                canonical_project.parent().unwrap(),
            ),
            Some(true),
        );
        let req = configure_request_with_params(json!({
            "project_root": project.path(), "harness": "runner", "storage_dir": ctx._storage.path(),
            "config": [user_tier(json!({
                "indexes": {"trigram": true, "semantic": true, "callgraph": true},
                "semantic": {"backend": "openai_compatible", "base_url": "http://127.0.0.1:1"},
                "views": {"enabled": true},
            }))],
        }));
        let response = handle_configure_for_test(&req, &ctx);
        assert!(response.success, "{response:?}");
        assert!(ctx.heavy_root_work_allowed());
        assert!(ctx.config().indexes.trigram);
        assert!(ctx.config().indexes.semantic);
        assert!(ctx.config().indexes.callgraph);
        // The index store binds this root.
        assert!(ctx.data_layer_withheld().is_none());
        super::drain_deferred_configure_maintenance(&ctx);
        assert!(ctx.watcher_runtime_active());
        assert!(ctx.callgraph_writer());
        ctx.stop_watcher_runtime();
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
            "config": [user_tier(json!({ "indexes": { "trigram": true, "semantic": true } }))],
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
            !ctx.config().indexes.trigram,
            "search_index must be auto-disabled at HOME root"
        );
        assert!(
            !ctx.config().indexes.semantic,
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
                "indexes": { "trigram": true, "semantic": false, "callgraph": false },
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
        assert!(ctx.config().indexes.trigram);
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

    /// Fixture for the restart-ordering tests: a Rust root with an existing
    /// `target/`, configured, but whose configure tail (the stage that loads
    /// the ignore rules) was cancelled by a quiesce before it ran.
    fn configured_root_with_cancelled_tail(root: &tempfile::TempDir) -> (PathBuf, AppContext) {
        let root = std::fs::canonicalize(root.path()).unwrap();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(root.join(".gitignore"), "/target/\n").unwrap();
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(root.clone()),
                ..Config::default()
            },
        );
        ctx.set_canonical_cache_root(root.clone());
        assert!(!ctx.gitignore_published());
        (root, ctx)
    }

    /// The daemon-restart ordering that left roots watched with only `.git`
    /// excluded: each root is configured, its queued configure maintenance is
    /// cancelled when the root is quiesced, and the rebind then restores the
    /// watcher directly (the `ensure_project_watcher` path) on the daemon's
    /// request-processing loop. That thread must not walk any project for ignore
    /// files; the load is queued and runs on another thread, and the backend must
    /// end up with `target/` excluded.
    ///
    /// The fake backend uses the shared retry policy of the inotify and FSEvents
    /// backends: derive from the shared matcher at creation, retry unavailable
    /// planning, and re-derive when the matcher generation changes.
    #[test]
    fn watcher_restored_before_configure_tail_seeds_ecosystem_exclusions() {
        let _guard = watcher_test_mutex()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        struct FakeBackend {
            stop: Arc<AtomicBool>,
            join: Option<std::thread::JoinHandle<()>>,
            _tx: mpsc::Sender<notify::Result<notify::Event>>,
        }
        impl Drop for FakeBackend {
            fn drop(&mut self) {
                self.stop.store(true, Ordering::SeqCst);
                if let Some(join) = self.join.take() {
                    let _ = join.join();
                }
            }
        }
        type Installed = Arc<Mutex<(u64, Vec<PathBuf>)>>;

        let roots = (0..6)
            .map(|_| tempfile::tempdir().unwrap())
            .collect::<Vec<_>>();
        let contexts = roots
            .iter()
            .map(configured_root_with_cancelled_tail)
            .collect::<Vec<_>>();

        // Every root rebinds in one burst, as after a daemon restart. This
        // thread stands in for the frame loop.
        let walks_before = AppContext::ignore_walks_on_current_thread_for_test();
        let mut installed = Vec::<(PathBuf, Installed)>::new();
        let mut load_pending = Vec::new();
        let (created_tx, created_rx) = mpsc::channel();
        for (root, ctx) in &contexts {
            let matcher = ctx.shared_gitignore();
            let generation = ctx.gitignore_generation();
            let plan: Installed = Arc::new(Mutex::new((u64::MAX, Vec::new())));
            installed.push((root.clone(), Arc::clone(&plan)));
            let created_tx = created_tx.clone();
            install_project_watcher_with(ctx, root, Vec::new(), move |root, _extra, tx| {
                let retry_matcher = Arc::clone(&matcher);
                let derive = move |root: &Path| {
                    crate::watcher_filter::derive_watcher_exclusion_plan(
                        root,
                        &matcher,
                        Some(crate::watcher_filter::WATCHER_EXCLUSION_LIMIT),
                    )
                };
                let observed = generation.load(Ordering::Acquire);
                let counters = crate::context::watcher_counters_for_root(&root);
                let mut planning = crate::watcher_filter::WatcherExclusionRetry::default();
                let initial = planning
                    .attempt(&root, observed, &counters, || derive(&root))
                    .unwrap()
                    .unwrap_or_default();
                *plan.lock().unwrap() = (
                    observed,
                    crate::watcher_filter::watcher_exclusion_paths(&initial.selected),
                );
                created_tx.send(observed).unwrap();
                let mut exclusion_state =
                    crate::watcher_filter::WatcherExclusionPaths::new(&root, &initial.selected);
                let stop = Arc::new(AtomicBool::new(false));
                let thread_stop = Arc::clone(&stop);
                let join = std::thread::spawn(move || {
                    while !thread_stop.load(Ordering::SeqCst) {
                        let current = generation.load(Ordering::Acquire);
                        if planning
                            .replacement(
                                &root,
                                current,
                                &counters,
                                &mut exclusion_state,
                                &retry_matcher,
                                || derive(&root),
                            )
                            .is_some()
                        {
                            *plan.lock().unwrap() = (current, exclusion_state.paths());
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                });
                Ok::<_, &'static str>(FakeBackend {
                    stop,
                    join: Some(join),
                    _tx: tx,
                })
            });
            load_pending.push(crate::commands::configure::ignore_rules_load_pending(ctx));
        }
        drop(created_tx);
        let created = (0..contexts.len())
            .map(|_| created_rx.recv().expect("one backend per rebound root"))
            .collect::<Vec<_>>();
        assert_eq!(
            AppContext::ignore_walks_on_current_thread_for_test(),
            walks_before,
            "the rebind thread walked a project for ignore files"
        );
        assert_eq!(
            load_pending,
            vec![true; contexts.len()],
            "a restored watcher with no ignore rules must ask for the load"
        );
        assert_eq!(created, vec![0; contexts.len()], "backends saw no matcher");

        // The queued maintenance jobs run on another thread (the root actor).
        let job_walks = std::thread::scope(|scope| {
            let jobs = contexts
                .iter()
                .map(|(_, ctx)| {
                    scope.spawn(move || {
                        let published = super::load_ignore_rules_if_unpublished(ctx);
                        (
                            published,
                            AppContext::ignore_walks_on_current_thread_for_test(),
                        )
                    })
                })
                .collect::<Vec<_>>();
            jobs.into_iter()
                .map(|job| job.join().unwrap())
                .collect::<Vec<_>>()
        });

        for (root, plan) in &installed {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let (generation, paths) = plan.lock().unwrap().clone();
                if generation > 0 && paths.contains(&root.join("target")) {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "backend never re-derived target/ for {}: generation={generation} {paths:?}",
                    root.display()
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        for (published, walks) in job_walks {
            assert!(published);
            assert!(walks > 0, "the queued job is where the walk happens");
        }
        for (_, ctx) in &contexts {
            assert!(!crate::commands::configure::ignore_rules_load_pending(ctx));
            ctx.stop_watcher_runtime();
        }
    }

    /// End to end on the real OS backend: a watcher restored before any ignore
    /// rules were loaded starts with `.git` only and says so, and the queued
    /// load makes the running backend install `target/` without a restart.
    #[test]
    #[ignore = "requires a live OS file watcher service"]
    fn restored_watcher_installs_target_after_queued_ignore_load() {
        let _guard = watcher_test_mutex()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let (root, ctx) = configured_root_with_cancelled_tail(&temp);
        let counters = crate::context::watcher_counters_for_root(&root);

        let walks_before = AppContext::ignore_walks_on_current_thread_for_test();
        super::ensure_project_watcher(&ctx);
        assert_eq!(
            AppContext::ignore_walks_on_current_thread_for_test(),
            walks_before
        );
        assert!(super::ignore_rules_load_pending(&ctx));
        let deadline = Instant::now() + Duration::from_secs(5);
        while counters.backend_exclusions().paths.is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let before = counters.backend_exclusions();
        assert_eq!(before.matcher_generation, 0);
        assert!(!before.paths.contains(&root.join("target")));

        std::thread::scope(|scope| {
            scope.spawn(|| assert!(super::load_ignore_rules_if_unpublished(&ctx)));
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        while !counters
            .backend_exclusions()
            .paths
            .contains(&root.join("target"))
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        let after = counters.backend_exclusions();
        ctx.stop_watcher_runtime();
        assert!(after.matcher_generation > 0);
        assert!(
            after.paths.contains(&root.join("target")),
            "running backend never installed target/: {:?}",
            after.paths
        );
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

    #[test]
    fn only_plugin_lsp_process_state_is_fast_path_eligible() {
        let previous = Config::default();
        for mutate in [
            |config: &mut Config| config.lsp_paths_extra.push(PathBuf::from("/cache/lsp")),
            |config: &mut Config| {
                config
                    .lsp_auto_install_binaries
                    .insert("test-lsp".to_string());
            },
            |config: &mut Config| {
                config.lsp_inflight_installs.insert("test-lsp".to_string());
            },
        ] {
            let mut next = previous.clone();
            mutate(&mut next);
            assert!(only_lsp_process_state_changed(&previous, &next));
        }

        let mut semantic = previous.clone();
        semantic.semantic.max_files += 1;
        semantic.lsp_paths_extra.push(PathBuf::from("/cache/lsp"));
        assert!(!only_lsp_process_state_changed(&previous, &semantic));

        let mut sandbox = previous.clone();
        sandbox.sandbox.profile.head = crate::config::SandboxMode::Strict;
        sandbox.lsp_paths_extra.push(PathBuf::from("/cache/lsp"));
        assert!(!only_lsp_process_state_changed(&previous, &sandbox));

        let mut previous_with_disabled = previous;
        previous_with_disabled.disabled_lsp = ["pyright", "typescript"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let mut paths_with_disabled = previous_with_disabled.clone();
        paths_with_disabled
            .lsp_paths_extra
            .push(PathBuf::from("/cache/lsp"));
        assert!(only_lsp_process_state_changed(
            &previous_with_disabled,
            &paths_with_disabled
        ));
    }

    #[test]
    fn lsp_paths_only_reconfigure_makes_new_binary_lazy_start_discoverable() {
        let _env_guard = home_env_mutex();
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _disable_watcher = EnvVarGuard::set("AFT_TEST_DISABLE_FILE_WATCHER", "1");
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let lsp_bin = tempfile::tempdir().unwrap();
        let binary_name = "aft-test-configure-pushed-lsp";
        let source = root.path().join("sample.pushedpath");
        std::fs::write(&source, "test\n").unwrap();
        std::fs::write(root.path().join("custom-root.json"), "{}\n").unwrap();
        init_git_fixture(root.path());
        let ctx = test_context();
        let base_params = json!({
            "project_root": root.path(),
            "harness": "opencode",
            "storage_dir": storage.path(),
            "config": [user_tier(json!({
                "indexes": { "trigram": false, "semantic": false, "callgraph": false },
                "lsp": {"servers": {"pushed-path": {
                    "extensions": ["pushedpath"],
                    "binary": binary_name,
                    "args": [],
                    "root_markers": ["custom-root.json"],
                    "disabled": false
                }}}
            }))]
        });
        let initial = configure_request_with_session(base_params.clone(), "session-a");
        assert!(handle_configure_for_test(&initial, &ctx).success);
        super::drain_deferred_configure_maintenance(&ctx);
        assert!(!ctx
            .lsp()
            .navigation_requires_deferred_execution(&source, ctx.config().as_ref()));

        let binary = lsp_bin.path().join(binary_name);
        crate::executable_cache::write_executable(&binary, b"#!/bin/sh\nexit 0\n").unwrap();
        let generation = ctx.configure_generation();
        let mut lsp_params = base_params;
        lsp_params["lsp_paths_extra"] = json!([lsp_bin.path()]);
        let update = configure_request_with_session(lsp_params, "session-a");
        assert!(handle_configure_for_test(&update, &ctx).success);

        assert_eq!(ctx.configure_generation(), generation);
        assert!(ctx
            .lsp()
            .navigation_requires_deferred_execution(&source, ctx.config().as_ref()));
    }

    #[test]
    fn project_semantic_and_sandbox_reconfigures_still_take_full_path() {
        let _env_guard = home_env_mutex();
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _disable_watcher = EnvVarGuard::set("AFT_TEST_DISABLE_FILE_WATCHER", "1");

        let assert_full_path = |label: &str, changed_doc: Value| {
            let root = tempfile::tempdir().unwrap();
            let storage = tempfile::tempdir().unwrap();
            init_git_fixture(root.path());
            let ctx = test_context();
            let base = json!({
                "project_root": root.path(),
                "harness": "opencode",
                "storage_dir": storage.path(),
                "config": [user_tier(json!({
                    "indexes": { "trigram": false, "semantic": false, "callgraph": false }
                }))]
            });
            let initial = configure_request_with_session(base.clone(), "session-a");
            assert!(handle_configure_for_test(&initial, &ctx).success);
            super::drain_deferred_configure_maintenance(&ctx);
            let worktree_probes = ctx.worktree_bridge_probe_spawns_for_test();
            ctx.force_worktree_bridge_reprobe_for_test(true);

            let mut changed = base;
            changed["config"] = json!([user_tier(changed_doc)]);
            let update = configure_request_with_session(changed, "session-a");
            assert!(handle_configure_for_test(&update, &ctx).success);
            ctx.force_worktree_bridge_reprobe_for_test(false);
            assert!(
                ctx.worktree_bridge_probe_spawns_for_test() > worktree_probes,
                "{label} change should run the full configure path"
            );
        };

        assert_full_path(
            "semantic",
            json!({
                "indexes": { "trigram": false, "semantic": false, "callgraph": false },
                "semantic": {
                    "backend": "openai_compatible",
                    "model": "reconfigure-test-model",
                    "base_url": "http://127.0.0.1:9",
                    "timeout_ms": 1000,
                    "max_batch_size": 8,
                    "max_files": 1234
                }
            }),
        );
        assert_full_path(
            "sandbox",
            json!({
                "indexes": { "trigram": false, "semantic": false, "callgraph": false },
                "sandbox": {"profile": {"worker": "strict", "head": "strict"}}
            }),
        );

        let root_a = tempfile::tempdir().unwrap();
        let root_b = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        init_git_fixture(root_a.path());
        init_git_fixture(root_b.path());
        let ctx = test_context();
        let params_for = |root: &Path| {
            json!({
                "project_root": root,
                "harness": "opencode",
                "storage_dir": storage.path(),
                "config": [user_tier(json!({
                    "indexes": { "trigram": false, "semantic": false, "callgraph": false }
                }))]
            })
        };
        let initial = configure_request_with_session(params_for(root_a.path()), "session-a");
        assert!(handle_configure_for_test(&initial, &ctx).success);
        super::drain_deferred_configure_maintenance(&ctx);
        let generation = ctx.configure_generation();
        let changed = configure_request_with_session(params_for(root_b.path()), "session-a");
        assert!(handle_configure_for_test(&changed, &ctx).success);
        assert!(ctx.configure_generation() > generation);
    }

    #[test]
    fn slow_prefix_log_line_names_recorded_sync_phases() {
        let phases = "config_resolve=4ms,canonicalize=1ms,worktree_probe=12ms,cache_key_resolve=8ms,artifact_owner_claim=2ms,storage_capability_probe=1ms,state_commit=3ms,index_loading_state=5ms,maintenance_enqueue=1ms,ack_ready=0ms";
        assert_eq!(
            slow_configure_prefix_line(Duration::from_millis(1_234), phases),
            format!("configure prefix slow: total=1234ms {phases}")
        );
    }

    #[test]
    fn cache_key_resolve_profile_logs_only_slow_phases_with_wait_and_work() {
        let mut profile = super::CacheKeyResolveProfile::new(Path::new("checkout"));
        profile.key.git_spawns = 2;
        profile.key.git_usable = Duration::from_millis(3);
        profile.key.rev_list = Duration::from_millis(16);
        profile.key.retry_wait = Duration::from_millis(50);
        profile.key.memo_wait = Duration::from_millis(2000);
        profile.key.memo_hold = Duration::from_millis(400);
        profile.key.memo_load = Duration::from_millis(100);
        profile.key.memo_prune = Duration::from_millis(200);
        profile.key.memo_write = Duration::from_millis(50);
        profile.key.legacy_memo = Duration::from_millis(70);
        profile.key.context_memo_wait = Duration::from_millis(2);
        profile.retention_bind = Duration::from_millis(30);
        profile.format_preflight = Duration::from_millis(5);
        assert!(profile.slow_line(Duration::from_millis(999)).is_none());
        assert!(profile.slow_line(Duration::from_secs(1)).is_some());
        assert_eq!(profile.slow_line(Duration::from_secs(3)).unwrap(),
            "slow configure cache_key_resolve root=checkout total_ms=3000 context_cache_hit=false git_spawns=2 git_usable_ms=3 rev_list_wall_ms=16 retry_wait_ms=50 memo_wait_ms=2000 memo_hold_ms=400 memo_load_ms=100 memo_prune_ms=200 memo_write_ms=50 context_memo_wait_ms=2 retention_bind_ms=30 format_preflight_ms=5 legacy_memo_ms=70");
    }

    #[test]
    fn cache_key_resolve_profile_counts_cold_and_unchanged_configure_probes() {
        let _probe_lock = crate::search_index::git_root_commit_probe_override_lock_for_test();
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        init_git_fixture(root.path());
        let ctx = test_context();
        let mut cold = crate::search_index::ArtifactCacheKeyTimings::default();
        let key = ctx
            .memoized_artifact_cache_key_for_configure_profiled(
                root.path(),
                root.path(),
                storage.path(),
                None,
                &mut cold,
            )
            .unwrap();
        assert_eq!(cold.git_spawns, 1);
        assert!(!cold.context_cache_hit);
        assert!(!cold.rev_list.is_zero());
        assert!(!cold.memo_load.is_zero());
        assert!(!cold.memo_prune.is_zero());
        assert!(!cold.memo_write.is_zero());
        assert!(!cold.legacy_memo.is_zero());
        assert!(cold.memo_hold >= cold.memo_load + cold.memo_prune + cold.memo_write);
        for _ in 0..8 {
            let mut warm = crate::search_index::ArtifactCacheKeyTimings::default();
            let again = ctx
                .memoized_artifact_cache_key_for_configure_profiled(
                    root.path(),
                    root.path(),
                    storage.path(),
                    None,
                    &mut warm,
                )
                .unwrap();
            assert_eq!(again, key);
            assert_eq!(warm.git_spawns, 0);
            assert!(warm.context_cache_hit);
            assert!(warm.memo_wait.is_zero());
            assert!(warm.memo_hold.is_zero());
            assert!(warm.legacy_memo.is_zero());
        }
    }

    #[test]
    fn cache_key_resolve_shares_only_verified_unchanged_repository_history() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _probe_lock = crate::search_index::git_root_commit_probe_override_lock_for_test();
        let temp = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let main = temp.path().join("main");
        init_git_fixture(&main);
        let initial = git_stdout(&main, &["rev-parse", "HEAD"]);
        let bind = |root: &Path| {
            let root = root.canonicalize().unwrap();
            let mut timings = crate::search_index::ArtifactCacheKeyTimings::default();
            let key = test_context()
                .memoized_artifact_cache_key_for_configure_profiled(
                    &root,
                    &root,
                    storage.path(),
                    None,
                    &mut timings,
                )
                .unwrap()
                .unwrap();
            assert_eq!(timings.git_spawns, 1, "every fresh bind still verifies Git");
            (key, timings.root_walks)
        };
        let (key, walks) = bind(&main);
        assert_eq!(walks, 1);
        assert_eq!(
            key,
            crate::search_index::artifact_key_from_git_identity(&initial)
        );
        for name in ["first", "second", "third"] {
            let linked = temp.path().join(name);
            git_ok(
                &main,
                &[
                    "worktree",
                    "add",
                    "--quiet",
                    "--detach",
                    linked.to_str().unwrap(),
                    "HEAD",
                ],
            );
            assert_eq!(bind(&linked), (key.clone(), 0));
            let scoped = linked.join("src");
            std::fs::create_dir(&scoped).unwrap();
            let (scoped_key, walks) = bind(&scoped);
            assert_ne!(
                scoped_key, key,
                "sharing history must not share subfolder artifacts"
            );
            assert_eq!(walks, 0);
        }

        // An orphan branch has a different root even in the same object store.
        git_ok(&main, &["checkout", "--quiet", "--orphan", "unrelated"]);
        git_ok(
            &main,
            &[
                "-c",
                "user.name=AFT Tests",
                "-c",
                "user.email=aft-tests@example.com",
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "unrelated",
            ],
        );
        let (unrelated, walks) = bind(&main);
        assert_ne!(unrelated, key);
        assert_eq!(walks, 1);
        let other = temp.path().join("other");
        init_git_fixture(&other);
        assert_eq!(
            bind(&other).1,
            1,
            "another repository cannot reuse the walk"
        );
    }

    #[test]
    fn cache_key_resolve_shared_history_survives_sibling_commits() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _probe_lock = crate::search_index::git_root_commit_probe_override_lock_for_test();
        let temp = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let main = temp.path().join("main");
        init_git_fixture(&main);
        let initial = git_stdout(&main, &["rev-parse", "HEAD"]);
        let first = temp.path().join("first");
        let sibling = temp.path().join("sibling");
        for linked in [&first, &sibling] {
            git_ok(
                &main,
                &[
                    "worktree",
                    "add",
                    "--quiet",
                    "--detach",
                    linked.to_str().unwrap(),
                    &initial,
                ],
            );
        }
        let mut seed = crate::search_index::ArtifactCacheKeyTimings::default();
        let key = test_context()
            .memoized_artifact_cache_key_for_configure_profiled(
                &first,
                &first,
                storage.path(),
                None,
                &mut seed,
            )
            .unwrap()
            .unwrap();
        assert_eq!(seed.root_walks, 1);
        std::fs::write(sibling.join("tracked.rs"), "fn sibling_change() {}\n").unwrap();
        git_ok(&sibling, &["add", "tracked.rs"]);
        git_ok(
            &sibling,
            &[
                "-c",
                "user.name=AFT Tests",
                "-c",
                "user.email=aft-tests@example.com",
                "commit",
                "--quiet",
                "-m",
                "sibling change",
            ],
        );
        let committed = git_stdout(&sibling, &["rev-parse", "HEAD"]);
        assert_ne!(committed, initial);
        assert!(main
            .join(".git/objects")
            .join(&committed[..2])
            .join(&committed[2..])
            .is_file());

        let fresh = temp.path().join("fresh");
        git_ok(
            &main,
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                fresh.to_str().unwrap(),
                &initial,
            ],
        );
        let mut again = crate::search_index::ArtifactCacheKeyTimings::default();
        let shared = test_context()
            .memoized_artifact_cache_key_for_configure_profiled(
                &fresh,
                &fresh,
                storage.path(),
                None,
                &mut again,
            )
            .unwrap()
            .unwrap();
        assert_eq!(shared, key);
        assert_eq!(
            again.root_walks, 0,
            "unrelated new objects cannot change cached ancestry"
        );
        assert_eq!(
            again.git_spawns, 1,
            "a fresh bind must still verify HEAD with Git"
        );
    }

    #[test]
    fn cache_key_resolve_shared_history_requires_readable_head() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _probe_lock = crate::search_index::git_root_commit_probe_override_lock_for_test();
        let temp = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let main = temp.path().join("main");
        init_git_fixture(&main);
        test_context()
            .memoized_artifact_cache_key_for_configure(&main, &main, storage.path(), None)
            .unwrap()
            .unwrap();
        let linked = temp.path().join("linked");
        git_ok(
            &main,
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                linked.to_str().unwrap(),
                "HEAD",
            ],
        );
        let head = git_stdout(&main, &["rev-parse", "HEAD"]);
        let object = main.join(".git/objects").join(&head[..2]).join(&head[2..]);
        let mut permissions = std::fs::metadata(&object).unwrap().permissions();
        permissions.set_readonly(false);
        std::fs::set_permissions(&object, permissions).unwrap();
        std::fs::write(&object, b"broken").unwrap();
        let mut timings = crate::search_index::ArtifactCacheKeyTimings::default();
        assert!(test_context()
            .memoized_artifact_cache_key_for_configure_profiled(
                &linked,
                &linked,
                storage.path(),
                None,
                &mut timings,
            )
            .is_err());
        assert_eq!(
            timings.root_walks, 3,
            "failed verification must use the ordinary retry ladder"
        );
        assert_eq!(
            timings.git_spawns, 6,
            "each retry verifies HEAD then falls back to the full probe"
        );
        assert_eq!(
            crate::root_identity::read(&linked, storage.path()).unwrap(),
            None
        );
    }

    #[test]
    fn new_session_attach_skips_root_probes_and_keeps_watcher_thread() {
        let _watcher_guard = watcher_test_mutex()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _env_guard = home_env_mutex();
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _enable_watcher = EnvVarGuard::remove("AFT_TEST_DISABLE_FILE_WATCHER");
        let _sync_watcher = EnvVarGuard::set("AFT_TEST_SYNC_FILE_WATCHER_START", "1");
        super::reset_configure_replay_session_calls_for_test();
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let source = root.path().join("session-b.txt");
        std::fs::write(&source, "session b can read\n").unwrap();
        init_git_fixture(root.path());
        let ctx = test_context();
        let params = json!({
            "project_root": root.path(),
            "harness": "opencode",
            "storage_dir": storage.path(),
            "config": [user_tier(json!({
                "indexes": { "trigram": false, "semantic": false, "callgraph": false }
            }))]
        });
        let first = configure_request_with_session(params.clone(), "session-a");
        assert!(handle_configure_for_test(&first, &ctx).success);
        ctx.mark_subc_bound();
        super::drain_deferred_configure_maintenance(&ctx);
        let canonical_root = ctx.canonical_cache_root();
        let watcher_thread = ctx.watcher_runtime_thread_id_for_test().unwrap();
        let watcher_generation = WATCHER_GENERATION.load(Ordering::SeqCst);
        let worktree_probes = ctx.worktree_bridge_probe_spawns_for_test();

        let second = configure_request_with_session(params, "session-b");
        assert!(handle_configure_for_test(&second, &ctx).success);

        assert_eq!(ctx.worktree_bridge_probe_spawns_for_test(), worktree_probes);
        assert_eq!(
            WATCHER_GENERATION.load(Ordering::SeqCst),
            watcher_generation
        );
        assert_eq!(
            ctx.watcher_runtime_thread_id_for_test(),
            Some(watcher_thread)
        );
        assert!(ctx.has_configure_session_binding(&canonical_root, "session-b"));
        let read = RawRequest {
            id: "session-b-read".to_string(),
            command: "read".to_string(),
            lsp_hints: None,
            session_id: Some("session-b".to_string()),
            params: json!({"file": source}),
        };
        assert!(crate::commands::read::handle_read(&read, &ctx).success);
        super::drain_deferred_configure_maintenance(&ctx);
        assert_eq!(super::configure_replay_session_calls_for_test(), 2);
        assert_eq!(ctx.watcher_registry_count(), 1);
        ctx.stop_watcher_runtime();
    }

    #[test]
    fn same_root_reconfigure_keeps_watcher_runtime_when_matcher_is_unchanged() {
        let _watcher_guard = watcher_test_mutex()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _env_guard = home_env_mutex();
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _enable_watcher = EnvVarGuard::remove("AFT_TEST_DISABLE_FILE_WATCHER");
        let _sync_watcher = EnvVarGuard::set("AFT_TEST_SYNC_FILE_WATCHER_START", "1");
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        init_git_fixture(root.path());
        let ctx = test_context();
        let watcher_starts_before = WATCHER_GENERATION.load(Ordering::SeqCst);
        let base = json!({
            "project_root": root.path(),
            "harness": "opencode",
            "storage_dir": storage.path(),
            "config": [user_tier(json!({
                "indexes": { "trigram": false, "semantic": false, "callgraph": false }
            }))]
        });
        let initial = configure_request_with_session(base.clone(), "session-a");
        assert!(handle_configure_for_test(&initial, &ctx).success);
        ctx.mark_subc_bound();
        super::drain_deferred_configure_maintenance(&ctx);
        assert!(ctx.watcher_runtime_active());
        let runtime_generation = WATCHER_GENERATION.load(Ordering::SeqCst);
        assert_eq!(runtime_generation, watcher_starts_before.wrapping_add(1));
        let runtime_thread_id = ctx.watcher_runtime_thread_id_for_test().unwrap();
        let matcher_generation = ctx.gitignore_generation().load(Ordering::SeqCst);

        let identical = configure_request_with_session(base.clone(), "session-a");
        assert!(handle_configure_for_test(&identical, &ctx).success);
        super::drain_deferred_configure_maintenance(&ctx);
        assert_eq!(
            ctx.watcher_runtime_thread_id_for_test(),
            Some(runtime_thread_id),
            "identical configure replaced the watcher thread"
        );
        assert_eq!(
            WATCHER_GENERATION.load(Ordering::SeqCst),
            runtime_generation
        );
        assert_eq!(
            ctx.gitignore_generation().load(Ordering::SeqCst),
            matcher_generation
        );

        let mut changed = base;
        changed["config"] = json!([user_tier(json!({
            "indexes": { "trigram": false, "semantic": false, "callgraph": false },
            "semantic": {
                "backend": "openai_compatible",
                "model": "watcher-reconfigure-test",
                "base_url": "http://127.0.0.1:9",
                "timeout_ms": 1000,
                "max_batch_size": 8,
                "max_files": 1234
            }
        }))]);
        let reconfigure = configure_request_with_session(changed, "session-a");
        assert!(handle_configure_for_test(&reconfigure, &ctx).success);
        super::drain_deferred_configure_maintenance(&ctx);

        assert_eq!(
            WATCHER_GENERATION.load(Ordering::SeqCst),
            runtime_generation,
            "same-root reconfigure replaced the watcher runtime"
        );
        assert_eq!(
            ctx.watcher_runtime_thread_id_for_test(),
            Some(runtime_thread_id),
            "same-root reconfigure replaced the watcher thread"
        );
        assert_eq!(
            ctx.gitignore_generation().load(Ordering::SeqCst),
            matcher_generation,
            "same-root reconfigure rebuilt an unchanged matcher"
        );
        assert_eq!(ctx.watcher_registry_count(), 1);
        ctx.stop_watcher_runtime();
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
                "indexes": { "trigram": false, "semantic": false, "callgraph": false },
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

#[cfg(test)]
mod inspect_orphan_sweep_tests {
    use super::*;

    /// Write a URL download fetched at `fetched_at_ms` under `storage`, the
    /// way `url_fetch` lays it out, and return its content and metadata paths.
    fn url_download(storage: &Path, url: &str, fetched_at_ms: u64) -> (PathBuf, PathBuf) {
        let content = crate::url_fetch::cache_content_path_for_url(storage, url, ".md");
        let meta = crate::url_fetch::cache_meta_path_for_url(storage, url);
        fs::create_dir_all(content.parent().unwrap()).unwrap();
        fs::write(&content, "body").unwrap();
        let meta_json = serde_json::json!({
            "url": url,
            "contentType": "text/markdown",
            "extension": ".md",
            "fetchedAt": fetched_at_ms,
        });
        fs::write(&meta, serde_json::to_vec(&meta_json).unwrap()).unwrap();
        (content, meta)
    }
}

#[cfg(test)]
mod startup_view_sweep_tests {
    use super::*;
}
