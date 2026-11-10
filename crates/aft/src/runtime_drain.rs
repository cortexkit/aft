use crate as aft;
use crate::context::{
    AppContext, SemanticIndexStatus, SemanticRefreshRequest, WatcherDrainApplyPhase,
    WatcherDrainPhase, WatcherDrainSliceState,
};
use crate::log_ctx;
use crate::lsp::client::LspEvent;
use crate::protocol::PushFrame;
#[cfg(test)]
use crate::watcher_filter::RescanReason;
use crate::watcher_filter::{watcher_path_is_infra_skip_under, WatcherDispatchEvent};
#[cfg(any(test, feature = "test-timing-hooks"))]
use std::collections::HashMap;
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrainBatchOutcome {
    pub processed: usize,
    pub has_more: bool,
}

pub const WATCHER_PATH_DRAIN_BATCH_CAP: usize = 2_048;
pub const WATCHER_DRAIN_SLICE_BUDGET: Duration = Duration::from_millis(250);
// View assembly may waste one superseded build, but it must not inherit the
// semantic embedder's human-edit debounce before publishing callgraph data.
const IGNORE_RULE_REFRESH_QUIET_WINDOW: Duration = Duration::from_secs(5);
const IGNORE_RULE_CHANGED_PATHS_RETAINED: usize = 8;
const WATCHER_DRAIN_UNIT_WARN_AFTER: Duration = Duration::from_secs(5);
const WATCHER_DRAIN_UNIT_FINAL_AFTER: Duration = Duration::from_secs(30);
pub const LSP_EVENT_DRAIN_BATCH_CAP: usize = 256;
const MUTATION_REFRESH_ENTRY_CAP: usize = 1024;
const MUTATION_REFRESH_BUDGET: Duration = Duration::from_millis(250);

#[cfg(test)]
thread_local! {
    static LEGACY_WATCHER_REFRESHES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static IGNORE_RULE_REFRESHES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static IGNORE_RULE_CORPUS_REBUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static IGNORE_RULE_CALLGRAPH_INVALIDATIONS: std::cell::Cell<usize> = const {
        std::cell::Cell::new(0)
    };
    static IGNORE_RULE_REFRESH_TEST_NOW: std::cell::Cell<Option<Instant>> = const {
        std::cell::Cell::new(None)
    };
    static WATCHER_DRAIN_TEST_NOW: std::cell::Cell<Option<Instant>> = const {
        std::cell::Cell::new(None)
    };
    static MUTATION_DIRECTORY_READS: std::cell::RefCell<Vec<PathBuf>> = const {
        std::cell::RefCell::new(Vec::new())
    };
    static MUTATION_WALK_STATS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn watcher_drain_now() -> Instant {
    #[cfg(test)]
    if let Some(now) = WATCHER_DRAIN_TEST_NOW.with(std::cell::Cell::get) {
        return now;
    }
    Instant::now()
}

fn watcher_drain_elapsed(started: Instant) -> Duration {
    watcher_drain_now().saturating_duration_since(started)
}

#[cfg(test)]
struct WatcherDrainTestClock(Option<Instant>);

#[cfg(test)]
impl WatcherDrainTestClock {
    fn freeze() -> Self {
        Self(WATCHER_DRAIN_TEST_NOW.with(|clock| clock.replace(Some(Instant::now()))))
    }
}

#[cfg(test)]
impl Drop for WatcherDrainTestClock {
    fn drop(&mut self) {
        WATCHER_DRAIN_TEST_NOW.with(|clock| clock.set(self.0));
    }
}

#[cfg(test)]
fn reset_ignore_rule_refreshes_for_test() {
    IGNORE_RULE_REFRESHES.with(|count| count.set(0));
    IGNORE_RULE_CORPUS_REBUILDS.with(|count| count.set(0));
    IGNORE_RULE_CALLGRAPH_INVALIDATIONS.with(|count| count.set(0));
}

#[cfg(test)]
fn ignore_rule_refreshes_for_test() -> usize {
    IGNORE_RULE_REFRESHES.with(std::cell::Cell::get)
}

#[cfg(test)]
fn ignore_rule_corpus_rebuilds_for_test() -> usize {
    IGNORE_RULE_CORPUS_REBUILDS.with(std::cell::Cell::get)
}

#[cfg(test)]
fn ignore_rule_callgraph_invalidations_for_test() -> usize {
    IGNORE_RULE_CALLGRAPH_INVALIDATIONS.with(std::cell::Cell::get)
}

#[cfg(test)]
fn note_ignore_rule_refresh_for_test() {
    IGNORE_RULE_REFRESHES.with(|count| count.set(count.get() + 1));
    IGNORE_RULE_CORPUS_REBUILDS.with(|count| count.set(count.get() + 1));
    IGNORE_RULE_CALLGRAPH_INVALIDATIONS.with(|count| count.set(count.get() + 1));
}

#[cfg(test)]
fn note_scoped_ignore_rule_refresh_for_test() {
    IGNORE_RULE_REFRESHES.with(|count| count.set(count.get() + 1));
    IGNORE_RULE_CALLGRAPH_INVALIDATIONS.with(|count| count.set(count.get() + 1));
}

#[cfg(not(test))]
fn note_scoped_ignore_rule_refresh_for_test() {}

#[cfg(not(test))]
fn note_ignore_rule_refresh_for_test() {}

fn ignore_rule_refresh_now() -> Instant {
    #[cfg(test)]
    if let Some(now) = IGNORE_RULE_REFRESH_TEST_NOW.with(std::cell::Cell::get) {
        return now;
    }
    Instant::now()
}

#[cfg(test)]
fn set_ignore_rule_refresh_now_for_test(now: Option<Instant>) {
    IGNORE_RULE_REFRESH_TEST_NOW.with(|clock| clock.set(now));
}

#[cfg(test)]
static ARTIFACT_DRAIN_TEST_MUTEX: Mutex<()> = Mutex::new(());

#[cfg(any(test, feature = "test-timing-hooks"))]
struct WatcherPhaseCommitGate {
    reached_tx: crossbeam_channel::Sender<()>,
    release_rx: crossbeam_channel::Receiver<()>,
}

#[cfg(any(test, feature = "test-timing-hooks"))]
// Parallel tests must not replace another path's in-flight barrier.
static WATCHER_PHASE_COMMIT_GATE: std::sync::OnceLock<
    Mutex<HashMap<PathBuf, WatcherPhaseCommitGate>>,
> = std::sync::OnceLock::new();

#[cfg(test)]
struct WatcherRescanGate {
    context_id: usize,
    reached_tx: crossbeam_channel::Sender<()>,
    release_rx: crossbeam_channel::Receiver<()>,
}

#[cfg(test)]
static WATCHER_RESCAN_GATE: OnceLock<Mutex<Option<WatcherRescanGate>>> = OnceLock::new();

#[cfg(test)]
fn install_watcher_rescan_gate_for_test(
    ctx: &AppContext,
) -> (
    crossbeam_channel::Receiver<()>,
    crossbeam_channel::Sender<()>,
) {
    let (reached_tx, reached_rx) = crossbeam_channel::bounded(1);
    let (release_tx, release_rx) = crossbeam_channel::bounded(1);
    *WATCHER_RESCAN_GATE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("watcher rescan gate mutex poisoned") = Some(WatcherRescanGate {
        context_id: ctx as *const AppContext as usize,
        reached_tx,
        release_rx,
    });
    (reached_rx, release_tx)
}

#[cfg(test)]
fn wait_on_watcher_rescan_gate_for_test(ctx: &AppContext) {
    let mut slot = WATCHER_RESCAN_GATE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("watcher rescan gate mutex poisoned");
    if !slot
        .as_ref()
        .is_some_and(|gate| gate.context_id == ctx as *const AppContext as usize)
    {
        return;
    }
    let gate = slot.take();
    drop(slot);
    if let Some(gate) = gate {
        let _ = gate.reached_tx.send(());
        let _ = gate.release_rx.recv_timeout(Duration::from_secs(12));
    }
}

#[cfg(not(test))]
fn wait_on_watcher_rescan_gate_for_test(_ctx: &AppContext) {}

/// Hold one real watcher path application until its test releases the drain.
#[doc(hidden)]
#[cfg(any(test, feature = "test-timing-hooks"))]
pub fn install_watcher_phase_commit_gate_for_test(
    target: PathBuf,
) -> (
    crossbeam_channel::Receiver<()>,
    crossbeam_channel::Sender<()>,
) {
    let (reached_tx, reached_rx) = crossbeam_channel::bounded(1);
    let (release_tx, release_rx) = crossbeam_channel::bounded(1);
    WATCHER_PHASE_COMMIT_GATE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("watcher phase commit gate mutex poisoned")
        .insert(
            target,
            WatcherPhaseCommitGate {
                reached_tx,
                release_rx,
            },
        );
    (reached_rx, release_tx)
}

#[cfg(any(test, feature = "test-timing-hooks"))]
fn wait_on_watcher_phase_commit_gate_for_test(path: &Path) {
    let gate = WATCHER_PHASE_COMMIT_GATE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("watcher phase commit gate mutex poisoned")
        .remove(path);
    if let Some(gate) = gate {
        let _ = gate.reached_tx.send(());
        let _ = gate.release_rx.recv_timeout(Duration::from_secs(12));
    }
}

#[cfg(not(any(test, feature = "test-timing-hooks")))]
fn wait_on_watcher_phase_commit_gate_for_test(_path: &Path) {}

struct WatcherDrainUnitGuard<'a> {
    phase: &'static str,
    path: &'a Path,
    batch_len: usize,
    started: Instant,
}

impl<'a> WatcherDrainUnitGuard<'a> {
    fn start(phase: WatcherDrainApplyPhase, path: &'a Path) -> Self {
        Self {
            phase: watcher_drain_phase_name(phase),
            path,
            batch_len: 1,
            started: Instant::now(),
        }
    }
}

impl Drop for WatcherDrainUnitGuard<'_> {
    fn drop(&mut self) {
        let elapsed = self.started.elapsed();
        let (warn_after, final_after) = watcher_drain_unit_thresholds();
        if elapsed < warn_after {
            return;
        }
        let path = if self.batch_len == 1 {
            self.path.display().to_string()
        } else {
            format!("{} (+{} paths)", self.path.display(), self.batch_len - 1)
        };
        emit_watcher_drain_unit_log(format!(
            "watcher drain unit exceeded 5s: phase={} path={} elapsed={}ms",
            self.phase,
            path,
            elapsed.as_millis()
        ));
        if elapsed >= final_after {
            emit_watcher_drain_unit_log(format!(
                "watcher drain unit completed after 30s: phase={} path={} elapsed={}ms",
                self.phase,
                path,
                elapsed.as_millis()
            ));
        }
    }
}

fn watcher_drain_unit_thresholds() -> (Duration, Duration) {
    #[cfg(test)]
    if let Some(thresholds) = WATCHER_UNIT_TEST_THRESHOLDS.with(std::cell::Cell::get) {
        return thresholds;
    }
    (
        WATCHER_DRAIN_UNIT_WARN_AFTER,
        WATCHER_DRAIN_UNIT_FINAL_AFTER,
    )
}

fn emit_watcher_drain_unit_log(line: String) {
    log::warn!("{line}");
    #[cfg(test)]
    WATCHER_UNIT_TEST_LOGS.with(|logs| logs.borrow_mut().push(line));
}

#[cfg(test)]
thread_local! {
    static WATCHER_UNIT_TEST_DELAY: std::cell::Cell<Duration> = const { std::cell::Cell::new(Duration::ZERO) };
    static WATCHER_UNIT_TEST_THRESHOLDS: std::cell::Cell<Option<(Duration, Duration)>> = const { std::cell::Cell::new(None) };
    static WATCHER_UNIT_TEST_LOGS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
fn delay_watcher_unit_for_test() {
    let delay = WATCHER_UNIT_TEST_DELAY.with(std::cell::Cell::get);
    if !delay.is_zero() {
        // Advance an injected clock without sleeping when a test pins slice timing.
        let advanced = WATCHER_DRAIN_TEST_NOW.with(|clock| {
            if let Some(now) = clock.get() {
                clock.set(Some(now + delay));
                true
            } else {
                false
            }
        });
        if !advanced {
            thread::sleep(delay);
        }
    }
}

#[cfg(not(test))]
fn delay_watcher_unit_for_test() {}

#[cfg(test)]
thread_local! {
    static WATCHER_RECEIVE_TEST_DELAY: std::cell::Cell<Duration> = const { std::cell::Cell::new(Duration::ZERO) };
}

/// Stand in for an expensive receive step (per-path LSP applicability checks
/// on a large batch) by advancing the pinned drain clock.
#[cfg(test)]
fn delay_watcher_receive_for_test() {
    let delay = WATCHER_RECEIVE_TEST_DELAY.with(std::cell::Cell::get);
    if !delay.is_zero() {
        WATCHER_DRAIN_TEST_NOW.with(|clock| {
            if let Some(now) = clock.get() {
                clock.set(Some(now + delay));
            }
        });
    }
}

#[cfg(not(test))]
fn delay_watcher_receive_for_test() {}

/// Called by the standalone loop after each successful configure, as the
/// daemon's configure tail does: the data layer binds the root's checkout and
/// catches up, so its build starts at the configure rather than at the first
/// query, and a rebind compares its files with the disk again.
pub fn standalone_root_configured(ctx: &AppContext) {
    crate::data_layer::runtime::root_bound(ctx);
    crate::data_layer::outline::root_bound(ctx);
}

pub fn drain_deferred_configure_maintenance(ctx: &AppContext) {
    crate::commands::configure::drain_deferred_configure_maintenance(ctx);
    crate::data_layer::runtime::root_bound(ctx);
    crate::data_layer::outline::root_bound(ctx);
}

/// Once per configure of a standalone AFT: the root's checkout binds and
/// catches up, building and filling as it needs
/// ([`crate::data_layer::runtime::root_bound`]), as the daemon's configure
/// tail has it do; otherwise nothing would until the first query. A
/// checkout the idle reaper released since (it drops a checkout no tool has
/// used for `idle.checkout_hours`, and the next use binds it again) is not
/// bound again here.
pub fn follow_configure_with_data_layer(ctx: &AppContext) {
    if ctx.note_data_layer_followed_configure() {
        crate::data_layer::runtime::root_bound(ctx);
    }
}

/// Executor-maintenance form of [`drain_deferred_configure_maintenance`]:
/// returns `true` when the drain stepped aside for a queued interactive
/// writer on the same root and left the rest parked for a requeued drain.
pub(crate) fn drain_deferred_configure_maintenance_yielding(ctx: &AppContext) -> bool {
    crate::commands::configure::drain_deferred_configure_maintenance_yielding(ctx)
}

/// Tracks deferred configure work for the standalone NDJSON loop. The attached-
/// daemon (`subc`) path schedules this work through its executor instead.
#[derive(Debug)]
pub struct StandaloneConfigureMaintenance {
    inner: crate::commands::configure::ConfigureMaintenanceState,
}

impl Default for StandaloneConfigureMaintenance {
    fn default() -> Self {
        Self {
            inner: crate::commands::configure::ConfigureMaintenanceState::default(),
        }
    }
}

impl StandaloneConfigureMaintenance {
    pub fn has_pending(&mut self, ctx: &AppContext) -> bool {
        crate::commands::configure::standalone_configure_maintenance_pending(ctx, &mut self.inner)
    }

    pub fn drain_prefix(&mut self, ctx: &Arc<AppContext>) -> bool {
        crate::commands::configure::drain_standalone_configure_prefix(ctx, &mut self.inner)
    }

    pub fn drain_one(&mut self, ctx: &Arc<AppContext>) -> bool {
        crate::commands::configure::drain_standalone_configure_unit(ctx, &mut self.inner)
    }
}

const STANDALONE_LOG_SWEEP_ATTEMPT_INTERVAL: Duration = Duration::from_secs(60);
const CONFIGURE_MAINTENANCE_YIELD_LOG_INTERVAL: Duration = Duration::from_secs(5);
static STANDALONE_LOG_SWEEP_LAST_ATTEMPT: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
static STANDALONE_LOG_SWEEP_IN_FLIGHT: AtomicBool = AtomicBool::new(false);
static CONFIGURE_MAINTENANCE_YIELD_LAST_LOG: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();

/// Keep shared log-retention I/O off the standalone request loop. The logging
/// module retains its hourly sweep cadence; this coarser gate avoids spawning a
/// no-op thread on every 250 ms idle tick.
pub fn spawn_standalone_log_maintenance() {
    let now = Instant::now();
    let should_attempt = STANDALONE_LOG_SWEEP_LAST_ATTEMPT
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map(|mut last_attempt| {
            if last_attempt.is_some_and(|last| {
                now.duration_since(last) < STANDALONE_LOG_SWEEP_ATTEMPT_INTERVAL
            }) {
                false
            } else {
                *last_attempt = Some(now);
                true
            }
        })
        .unwrap_or(false);
    if !should_attempt || STANDALONE_LOG_SWEEP_IN_FLIGHT.swap(true, Ordering::AcqRel) {
        return;
    }

    if let Err(error) = thread::Builder::new()
        .name("aft-log-retention-sweep".to_string())
        .spawn(|| {
            crate::logging::maybe_sweep_logs();
            STANDALONE_LOG_SWEEP_IN_FLIGHT.store(false, Ordering::Release);
        })
    {
        STANDALONE_LOG_SWEEP_IN_FLIGHT.store(false, Ordering::Release);
        crate::slog_warn!("failed to spawn log retention maintenance thread: {error}");
    }
}

/// Report cooperative configure work displaced by already-buffered requests.
/// Bursty clients may trigger many unit boundaries, so emit at most once per
/// five seconds while preserving the queued count from the observed boundary.
pub fn note_configure_maintenance_yield(queued: usize) {
    if queued == 0 {
        return;
    }
    let now = Instant::now();
    let should_log = CONFIGURE_MAINTENANCE_YIELD_LAST_LOG
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map(|mut last_log| {
            if last_log.is_some_and(|last| {
                now.duration_since(last) < CONFIGURE_MAINTENANCE_YIELD_LOG_INTERVAL
            }) {
                false
            } else {
                *last_log = Some(now);
                true
            }
        })
        .unwrap_or(false);
    if should_log {
        crate::slog_info!(
            "configure maintenance yielded to {} queued request(s)",
            queued
        );
    }
}

pub fn drain_configure_warning_events(ctx: &AppContext) {
    for (generation, frame) in ctx.drain_configure_warnings() {
        if ctx.configure_generation() != generation {
            aft::slog_info!(
                "dropping stale configure_warnings for generation {} (current {})",
                generation,
                ctx.configure_generation()
            );
            continue;
        }

        if let Some(sender) = ctx.progress_sender_handle() {
            sender(PushFrame::ConfigureWarnings(frame));
        }
    }
}

pub fn drain_inspect_events(ctx: &AppContext) {
    drain_inspect_events_for_generation(ctx, ctx.configure_generation());
}

pub(crate) fn drain_inspect_events_for_generation(ctx: &AppContext, generation: u64) {
    let Some(drained) =
        ctx.run_if_subc_bound_generation(generation, || ctx.inspect_manager().drain_completions())
    else {
        return;
    };
    // A completed background TODO scan refreshes the status bar's TODO count,
    // so the bar reflects it on the next tool result without waiting for an
    // explicit aft_inspect call.
    if drained > 0 {
        if let Some(project_root) = ctx.config().project_root.clone() {
            // The inspect snapshot keys its project-scoped jobs by the
            // normalized root, so look the todos result up under the same form.
            let todos = ctx.inspect_manager().latest_project_todos_count(
                ctx.inspect_dir(),
                crate::path_identity::canonicalize_normalized(&project_root),
            );
            ctx.update_status_bar_todos(todos);
            ctx.signal_status_changed();
        }
    }
}

/// Return true when any background build-completion receiver is currently set.
///
/// Each receiver is checked under its own short lock; no lock is held while
/// checking the next subsystem.
pub fn any_build_in_flight(ctx: &AppContext) -> bool {
    {
        let rx = ctx.semantic_index_rx().lock();
        rx.is_some()
    }
}

/// [`any_build_in_flight`] without waiting: `None` when one of the receiver
/// slots is locked by another thread right now.
pub fn try_any_build_in_flight(ctx: &AppContext) -> Option<bool> {
    Some(ctx.semantic_index_rx().try_lock()?.is_some())
}

pub fn watcher_path_is_ignored_by_current_matcher(ctx: &AppContext, path: &Path) -> bool {
    if ctx
        .config()
        .project_root
        .as_ref()
        .is_some_and(|root| watcher_path_is_infra_skip_under(root, path))
    {
        return true;
    }

    if let Some(matcher) = ctx.gitignore() {
        if path.starts_with(matcher.path()) {
            let is_dir = path.is_dir();
            return matcher
                .matched_path_or_any_parents(path, is_dir)
                .is_ignore();
        }
    }

    false
}

pub fn watcher_path_is_semantic_source(path: &Path) -> bool {
    crate::semantic_index::is_semantic_indexed_extension(path)
}

/// Pair each watcher-reported semantic path with the file record the index
/// holds for it, so the disk comparison in
/// [`crate::semantic_index::files_with_changed_content`] can run after the
/// index lock is released.
///
/// Paths the current ignore rules exclude get no record, which sends them
/// down the invalidation path unconditionally: their vectors must go whether
/// or not the bytes changed.
fn semantic_paths_with_recorded_freshness(
    ctx: &AppContext,
    index: &crate::semantic_index::SemanticIndex,
    paths: Vec<PathBuf>,
) -> Vec<(PathBuf, Option<crate::cache_freshness::FileFreshness>)> {
    paths
        .into_iter()
        .map(|path| {
            let record = if watcher_path_is_ignored_by_current_matcher(ctx, &path) {
                None
            } else {
                index.recorded_file_freshness(&path)
            };
            (path, record)
        })
        .collect()
}

pub fn mark_semantic_corpus_refresh_success(ctx: &AppContext) {
    ctx.clear_all_semantic_refresh_retry_attempts();
    ctx.reset_semantic_refresh_circuit_after_success();
}

pub const MAX_RETRY_ATTEMPTS: usize = 6;
pub const BREAKER_TRIP_THRESHOLD: usize = 3;

#[cfg(test)]
static SEMANTIC_REFRESH_RETRY_DELAY_OVERRIDE_MS: AtomicU64 = AtomicU64::new(u64::MAX);

/// Backoff for live semantic refresh retries after a transient embedding backend
/// failure. Mirrors the cold-build retry cadence (15s -> 30s -> 60s capped) so
/// a down backend cannot spin the watcher/refresh loop hot while still
/// self-healing once the backend returns.
fn semantic_refresh_retry_backoff(attempt: usize) -> Duration {
    #[cfg(test)]
    {
        let override_ms = SEMANTIC_REFRESH_RETRY_DELAY_OVERRIDE_MS.load(Ordering::SeqCst);
        if override_ms != u64::MAX {
            return Duration::from_millis(override_ms);
        }
    }
    // Test seam, intentionally matching the build-level retry override.
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

struct SemanticRefreshRetryPlan {
    retry_paths: Vec<std::path::PathBuf>,
    capped_paths: Vec<std::path::PathBuf>,
    delay: Option<Duration>,
}

fn next_semantic_refresh_retry_plan(
    ctx: &AppContext,
    paths: Vec<std::path::PathBuf>,
) -> SemanticRefreshRetryPlan {
    let mut retry_paths = Vec::new();
    let mut capped_paths = Vec::new();
    let mut max_attempt = 0usize;

    ctx.with_semantic_refresh_retry_attempts_mut(|attempts| {
        for path in paths {
            let attempt = attempts.get(&path).copied().unwrap_or(0);
            if attempt >= MAX_RETRY_ATTEMPTS {
                capped_paths.push(path);
                continue;
            }
            max_attempt = max_attempt.max(attempt);
            attempts.insert(path.clone(), attempt.saturating_add(1));
            retry_paths.push(path);
        }
    });

    let delay = if retry_paths.is_empty() {
        None
    } else {
        Some(semantic_refresh_retry_backoff(max_attempt))
    };

    SemanticRefreshRetryPlan {
        retry_paths,
        capped_paths,
        delay,
    }
}

pub fn semantic_refresh_circuit_is_open(ctx: &AppContext) -> bool {
    ctx.semantic_refresh_circuit_is_open()
}

pub fn record_semantic_refresh_transient_failure(ctx: &AppContext, error: &str) -> bool {
    if aft::synapse_embed::LanePause::from_error(error).is_some() {
        return false;
    }
    let reason = aft::semantic_index::strip_transient_embedding_marker(error);
    ctx.record_semantic_refresh_transient_failure(BREAKER_TRIP_THRESHOLD, &reason)
}

#[doc(hidden)]
pub fn semantic_refresh_transient_failure_count_for_test(ctx: &AppContext) -> usize {
    ctx.semantic_refresh_transient_failure_count()
}

#[doc(hidden)]
pub fn semantic_refresh_probe_is_scheduled_for_test(ctx: &AppContext) -> bool {
    ctx.semantic_refresh_probe_is_scheduled()
}

pub fn schedule_semantic_refresh_retry(
    ctx: &AppContext,
    paths: Vec<std::path::PathBuf>,
    error: &str,
) -> bool {
    if paths.is_empty() {
        return false;
    }
    if ctx.semantic_refresh_sender().is_none() {
        return false;
    };

    let SemanticRefreshRetryPlan {
        retry_paths,
        capped_paths,
        delay,
    } = next_semantic_refresh_retry_plan(ctx, paths);

    if !capped_paths.is_empty() {
        aft::slog_warn!(
            "semantic refresh retry limit reached for {} file(s); preserving for next watcher/configure refresh",
            capped_paths.len(),
        );
        ctx.add_pending_semantic_index_paths(capped_paths);
    }

    let Some(delay) = delay else {
        return true;
    };

    let clean = aft::semantic_index::strip_transient_embedding_marker(error);
    aft::slog_warn!(
        "semantic refresh hit a transient backend error ({}); retrying {} file(s) in {}ms",
        clean,
        retry_paths.len(),
        delay.as_millis(),
    );

    let session_id = log_ctx::current_session();
    let generation = ctx.semantic_refresh_generation();
    let generation_flag = ctx.configure_generation_flag();
    let lifecycle = ctx.subc_lifecycle_admission();
    let (sender_slot, pending_paths_slot) = ctx.semantic_refresh_retry_slots();
    thread::spawn(move || {
        log_ctx::with_session(session_id, || {
            thread::sleep(delay);
            let _ = lifecycle.run_if_current(&generation_flag, generation, || {
                let sent = sender_slot.lock().as_ref().is_some_and(|sender| {
                    sender
                        .send(SemanticRefreshRequest::Files {
                            paths: retry_paths.clone(),
                        })
                        .is_ok()
                });
                if !sent {
                    pending_paths_slot.lock().extend(retry_paths);
                }
            });
        });
    });
    true
}

/// Source file extensions that the call graph supports.
const SOURCE_EXTENSIONS: &[&str] = &[
    "ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs", "py", "pyi", "rs", "go",
];

pub const WATCHER_BATCH_INLINE_CAP: usize = 256;

/// A `tsconfig.json` / `jsconfig.json` (including variant names like
/// `tsconfig.base.json`). A change to any of these can shift TypeScript build
/// membership (which files `tsc` checks), so the status-bar membership cache
/// must be invalidated. Deliberately broad on the variant suffix and ignorant
/// of `extends` graphs: the cache is cleared wholesale on a match, and base
/// configs almost always follow the `tsconfig*.json` naming. Non-standard base
/// names are covered on the next `tsconfig.json` change or `configure`.
pub fn watcher_path_is_tsconfig(path: &std::path::Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| {
            n == "tsconfig.json"
                || n == "jsconfig.json"
                || ((n.starts_with("tsconfig.") || n.starts_with("jsconfig."))
                    && n.ends_with(".json"))
        })
        .unwrap_or(false)
}

pub fn watcher_path_is_source(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| SOURCE_EXTENSIONS.contains(&ext))
}

/// A file the callgraph STORE would have indexed at cold-build time. The store
/// indexes every file `walk_project_files` yields (i.e. any detected language),
/// not just the trigram `SOURCE_EXTENSIONS` set. Gating the store's watcher
/// refresh on the narrower trigram set left edits to Java/C/C++/C#/Kotlin/Ruby/
/// PHP/… (all of which the store extracts calls for) serving stale results until
/// a full rebuild. Mirror cold-build exactly so refresh coverage == index
/// coverage.
pub fn watcher_path_is_callgraph_indexed(path: &std::path::Path) -> bool {
    aft::parser::detect_language(path).is_some()
}

pub fn semantic_corpus_refresh_in_progress(ctx: &AppContext) -> bool {
    let status = ctx
        .semantic_index_status()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    matches!(
        &*status,
        SemanticIndexStatus::Building { stage, .. } if stage == "refreshing_corpus"
    )
}

pub fn refresh_project_corpus(ctx: &AppContext) -> bool {
    let generation = ctx.configure_generation();
    ctx.run_if_subc_bound_generation(generation, || {
        let Some(_root) = ctx.canonical_cache_root_opt() else {
            return false;
        };
        let config = ctx.config();
        let mut status_changed = false;

        if config.indexes.semantic && !ctx.shared_artifacts_read_only() {
            if let Some(sender) = ctx.semantic_refresh_sender() {
                *ctx.semantic_index_status()
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    SemanticIndexStatus::Building {
                        stage: "refreshing_corpus".to_string(),
                        files: None,
                        entries_done: None,
                        entries_total: None,
                    };
                match sender.send(SemanticRefreshRequest::Corpus) {
                    Ok(()) => {
                        status_changed = true;
                    }
                    Err(error) => {
                        *ctx.semantic_index_status()
                            .write()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            SemanticIndexStatus::Failed(format!(
                                "semantic corpus refresh worker unavailable: {error}"
                            ));
                        status_changed = true;
                    }
                }
            } else if ctx.semantic_index_rx().lock().is_some() {
                ctx.mark_pending_semantic_corpus_refresh();
            }
        }

        status_changed
    })
    .unwrap_or(false)
}

pub fn refresh_corpus_after_ignore_change(ctx: &AppContext) -> bool {
    note_ignore_rule_refresh_for_test();
    refresh_project_corpus(ctx)
}

fn arm_ignore_rule_refresh(state: &mut WatcherDrainSliceState, paths: Vec<PathBuf>) {
    state.ignore_changed = false;
    state.ignore_refresh_due = Some(ignore_rule_refresh_now() + IGNORE_RULE_REFRESH_QUIET_WINDOW);
    state.ignore_changed_path_count = state.ignore_changed_path_count.saturating_add(paths.len());
    for path in paths {
        if state.ignore_changed_paths.len() == IGNORE_RULE_CHANGED_PATHS_RETAINED {
            break;
        }
        if !state.ignore_changed_paths.contains(&path) {
            state.ignore_changed_paths.push(path);
        }
    }
}

fn watcher_rescan_rss_bytes() -> Option<u64> {
    crate::memory::rss_bytes()
}

fn watcher_rescan_rss_delta(before: Option<u64>, after: Option<u64>) -> Option<i64> {
    let delta = i128::from(after?) - i128::from(before?);
    Some(delta.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64)
}

pub fn refresh_project_after_watcher_rescan(ctx: &AppContext) -> bool {
    if ctx.canonical_cache_root_opt().is_none() {
        return false;
    }
    let generation = ctx.configure_generation();
    let Some(mut status_changed) = ctx.run_if_subc_bound_generation(generation, || {
        if let Some(root) = ctx.canonical_cache_root_opt() {
            // A rescan means watcher events were LOST. Same-size,
            // preserved-mtime edits are exactly what stat-first verification
            // misses, so the memo must downgrade to strict content
            // verification, not just to stat-first.
            crate::cache_freshness::invalidate_verify_memo_strict(&root);
        }
        ctx.clear_pending_index_updates();
        ctx.reset_symbol_cache();
        ctx.clear_tsconfig_membership_cache();
        ctx.lsp_reload_rust_workspaces_after_lost_watcher_events();
        true
    }) else {
        return false;
    };

    status_changed |= refresh_project_corpus(ctx);

    // The shared corpus refresh only reconciles what is resident or has a live
    // worker. After lost events that is not enough: nothing may be resident
    // (evicted root), no refresh worker may exist yet, and read-only roots
    // skip watcher path application entirely. Force the reconciliation for
    // each lane regardless of residency.
    let hardened = ctx.run_if_subc_bound_generation(generation, || {
        let config = ctx.config();
        if ctx.shared_artifacts_read_only() {
            // Read-only roots reconcile by re-opening the shared artifacts:
            // drop the resident snapshots so the evicted-reload path fires on
            // the next query. With the RAM overlay on, that reload rebuilds
            // the overlay from disk. A load already in flight may have read
            // the disk before the lost events, so it is retired and the next
            // query starts a fresh one.
            if config.indexes.semantic {
                ctx.semantic_index()
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
                ctx.clear_semantic_refresh_worker();
            }
        } else if config.indexes.semantic
            && ctx.semantic_refresh_sender().is_none()
            && ctx.semantic_index_rx().lock().is_none()
        {
            // No worker exists to receive the corpus request and none is
            // building; retain the intent so the next worker replays it.
            ctx.mark_pending_semantic_corpus_refresh();
        }
    });
    status_changed |= hardened.is_some();
    status_changed
}

/// Drain pre-filtered watcher events and apply cache invalidations on the
/// dispatch thread. The watcher filter thread owns notify receive/decode,
/// metadata filtering, ignore matching, root-deleted detection, and path
/// coalescing; this drain only reacts to compact control events and surviving
/// paths because the cache/index state below is not Send.
pub fn drain_watcher_events(ctx: &AppContext) {
    // A request gets one slice, not one slice repeatedly until a checkout burst
    // empties. The continuation stays queued for subsequent requests/idle turns.
    drain_watcher_events_bounded(ctx, WATCHER_PATH_DRAIN_BATCH_CAP);
}

fn watcher_drain_phase_name(stage: WatcherDrainApplyPhase) -> &'static str {
    match stage {
        WatcherDrainApplyPhase::PendingTier2 => "pending_tier2",
        WatcherDrainApplyPhase::PendingIndexes => "pending_indexes",
        WatcherDrainApplyPhase::SymbolCache => "symbol_cache",
        WatcherDrainApplyPhase::SearchIndex => "search_index",
        WatcherDrainApplyPhase::SemanticIndex => "semantic_index",
        WatcherDrainApplyPhase::LspDiagnostics => "lsp_diagnostics",
        WatcherDrainApplyPhase::Complete => "complete",
    }
}

fn apply_watcher_path_phase(
    stage: WatcherDrainApplyPhase,
    paths: &mut VecDeque<PathBuf>,
    remaining: &mut usize,
    started: Instant,
    budget: Duration,
    mut apply: impl FnMut(&Path),
) -> bool {
    try_apply_watcher_path_phase(stage, paths, remaining, started, budget, |path| {
        apply(path);
        true
    })
}

fn try_apply_watcher_path_phase(
    stage: WatcherDrainApplyPhase,
    paths: &mut VecDeque<PathBuf>,
    remaining: &mut usize,
    started: Instant,
    budget: Duration,
    mut apply: impl FnMut(&Path) -> bool,
) -> bool {
    while *remaining > 0 {
        let path = paths
            .pop_front()
            .expect("watcher apply phase tracks its remaining paths");
        let applied = {
            let _watchdog = WatcherDrainUnitGuard::start(stage, &path);
            delay_watcher_unit_for_test();
            wait_on_watcher_phase_commit_gate_for_test(&path);
            apply(&path)
        };
        if !applied {
            // Retain the unfinished path for the next slice rather than
            // acknowledging work that a busy subsystem could not apply.
            paths.push_front(path);
            return false;
        }
        paths.push_back(path);
        *remaining -= 1;
        if watcher_drain_elapsed(started) >= budget {
            return false;
        }
    }
    true
}

fn next_watcher_apply_phase(stage: WatcherDrainApplyPhase) -> WatcherDrainApplyPhase {
    match stage {
        WatcherDrainApplyPhase::PendingTier2 => WatcherDrainApplyPhase::PendingIndexes,
        WatcherDrainApplyPhase::PendingIndexes => WatcherDrainApplyPhase::SymbolCache,
        WatcherDrainApplyPhase::SymbolCache => WatcherDrainApplyPhase::SearchIndex,
        WatcherDrainApplyPhase::SearchIndex => WatcherDrainApplyPhase::SemanticIndex,
        WatcherDrainApplyPhase::SemanticIndex => WatcherDrainApplyPhase::LspDiagnostics,
        WatcherDrainApplyPhase::LspDiagnostics => WatcherDrainApplyPhase::Complete,
        WatcherDrainApplyPhase::Complete => WatcherDrainApplyPhase::Complete,
    }
}

/// Reconcile the final disk state when an AFT mutation finishes, including
/// partial failures and rollback. Keep this guard outside batch validation so
/// a queued query never depends on delivery of an OS file notification.
#[must_use]
pub(crate) struct FileMutationGuard<'a> {
    ctx: &'a AppContext,
    revisit: Vec<PathBuf>,
    paths: std::collections::BTreeSet<PathBuf>,
    files_before: std::collections::BTreeMap<PathBuf, bool>,
    _view_intent: crate::write_intent::IntentGuard,
}

struct MutationRefreshBudget {
    started: Instant,
    examined: usize,
    skipped_ignored: usize,
}

impl MutationRefreshBudget {
    fn new() -> Self {
        Self {
            started: watcher_drain_now(),
            examined: 0,
            skipped_ignored: 0,
        }
    }

    fn exhausted(&self) -> bool {
        self.examined >= MUTATION_REFRESH_ENTRY_CAP
            || watcher_drain_elapsed(self.started) >= MUTATION_REFRESH_BUDGET
    }
}

fn collect_mutation_paths(
    ctx: &AppContext,
    roots: &[PathBuf],
    paths: &mut std::collections::BTreeSet<PathBuf>,
    mut revisit: Option<&mut Vec<PathBuf>>,
    budget: &mut MutationRefreshBudget,
) -> bool {
    let mut roots = roots.iter();
    let mut directories: Vec<std::fs::ReadDir> = Vec::new();
    loop {
        // Check before advancing ReadDir: collecting its children first would
        // make a wide directory unbounded even with a capped refresh loop.
        if budget.exhausted() {
            return false;
        }
        let (path, is_root) = if let Some(entries) = directories.last_mut() {
            match entries.next() {
                Some(Ok(entry)) => (entry.path(), false),
                Some(Err(_)) => return false,
                None => {
                    directories.pop();
                    continue;
                }
            }
        } else if let Some(root) = roots.next() {
            (root.clone(), true)
        } else {
            return true;
        };
        budget.examined += 1;
        if watcher_path_is_ignored_by_current_matcher(ctx, &path) {
            budget.skipped_ignored += 1;
            continue;
        }
        paths.insert(path.clone());
        #[cfg(test)]
        MUTATION_WALK_STATS.with(|stats| stats.set(stats.get() + 1));
        let metadata = std::fs::symlink_metadata(&path);
        let is_directory = metadata.as_ref().is_ok_and(|meta| meta.is_dir());
        if is_root && (is_directory || metadata.is_err()) {
            if let Some(revisit) = revisit.as_mut() {
                revisit.push(path.clone());
            }
        }
        // Only directories and initially absent roots need discovery at drop.
        // Plain files are refreshed once, without another classification walk.
        if is_directory {
            #[cfg(test)]
            MUTATION_DIRECTORY_READS.with(|reads| reads.borrow_mut().push(path.clone()));
            match std::fs::read_dir(&path) {
                Ok(entries) => directories.push(entries),
                Err(_) => return false,
            }
        }
    }
}

pub(crate) fn record_file_mutation<'a, 'p>(
    ctx: &'a AppContext,
    paths: impl IntoIterator<Item = &'p Path>,
) -> FileMutationGuard<'a> {
    let roots = paths.into_iter().map(Path::to_path_buf).collect::<Vec<_>>();
    let view_intent = crate::write_intent::record_paths(roots.iter().map(PathBuf::as_path));
    let mut paths = std::collections::BTreeSet::new();
    let mut revisit = Vec::new();
    let mut before = MutationRefreshBudget::new();
    let _ = collect_mutation_paths(ctx, &roots, &mut paths, Some(&mut revisit), &mut before);
    // Explicit targets also matter when ignored by the search index: a language
    // server can still watch and import a file that an agent writes there.
    let files_before = roots
        .iter()
        .chain(paths.iter())
        .map(|path| (path.clone(), path.is_file()))
        .collect();
    FileMutationGuard {
        ctx,
        revisit,
        paths,
        files_before,
        _view_intent: view_intent,
    }
}

impl Drop for FileMutationGuard<'_> {
    fn drop(&mut self) {
        // The index store learns of the write from its write intent. What is
        // left here is telling language servers which files appeared or went.
        if !self.revisit.is_empty() {
            let mut budget = MutationRefreshBudget::new();
            let mut discovered = std::collections::BTreeSet::new();
            let _ =
                collect_mutation_paths(self.ctx, &self.revisit, &mut discovered, None, &mut budget);
            for path in discovered.difference(&self.paths) {
                self.files_before.entry(path.clone()).or_insert(false);
            }
        }
        let events = self
            .files_before
            .iter()
            .filter_map(|(path, existed)| {
                let exists = path.is_file();
                (exists != *existed).then(|| {
                    (
                        path.clone(),
                        if exists {
                            lsp_types::FileChangeType::CREATED
                        } else {
                            lsp_types::FileChangeType::DELETED
                        },
                    )
                })
            })
            .collect::<Vec<_>>();
        self.ctx.lsp_notify_mutation_file_events(&events);
    }
}

fn apply_watcher_slice(ctx: &AppContext, state: &mut WatcherDrainSliceState, started: Instant) {
    state.lsp_diagnostics_lock_contended = false;
    let WatcherDrainPhase::Apply {
        mut stage,
        mut paths,
        mut remaining,
        oversized_inline_batch,
    } = std::mem::take(&mut state.phase)
    else {
        return;
    };
    let lifecycle_generation = ctx.configure_generation();
    // Mid-slice unbind: every per-path mutation below is lifecycle-gated, so
    // continuing would burn the whole batch as no-ops and DROP the paths at
    // Complete. Abort with the phase intact instead — the continuation is
    // retained across lifecycle-only generation changes and the rebind
    // rebases and replays it.
    if ctx
        .run_if_subc_bound_generation(lifecycle_generation, || ())
        .is_none()
    {
        state.phase = WatcherDrainPhase::Apply {
            stage,
            paths,
            remaining,
            oversized_inline_batch,
        };
        return;
    }
    if !paths.is_empty() || remaining > 0 {
        let _ = ctx.run_if_subc_bound_generation(lifecycle_generation, || {
            ctx.invalidate_warm_verify_memo();
        });
    }
    let heavy_root_work_allowed = ctx.heavy_root_work_allowed();
    let shared_artifacts_read_only = ctx.shared_artifacts_read_only();
    let apply_ram_search_updates = !shared_artifacts_read_only || ctx.ram_overlay_active();
    let mut semantic_refresh_paths = std::mem::take(&mut state.semantic_refresh_paths);
    let mut status_changed = state.status_changed;

    loop {
        let completed = match stage {
            WatcherDrainApplyPhase::PendingTier2 => apply_watcher_path_phase(
                WatcherDrainApplyPhase::PendingTier2,
                &mut paths,
                &mut remaining,
                started,
                WATCHER_DRAIN_SLICE_BUDGET,
                |path| {
                    if heavy_root_work_allowed && ctx.inspect_writer() {
                        let _ = ctx.run_if_subc_bound_generation(lifecycle_generation, || {
                            ctx.add_pending_tier2_paths([path.to_path_buf()]);
                        });
                    }
                },
            ),
            WatcherDrainApplyPhase::PendingIndexes => {
                let semantic_build_in_progress = ctx.semantic_index_rx().lock().is_some();
                let semantic_corpus_refresh_in_progress = semantic_corpus_refresh_in_progress(ctx);
                apply_watcher_path_phase(
                    WatcherDrainApplyPhase::PendingIndexes,
                    &mut paths,
                    &mut remaining,
                    started,
                    WATCHER_DRAIN_SLICE_BUDGET,
                    |path| {
                        if heavy_root_work_allowed
                            && (!shared_artifacts_read_only || ctx.ram_overlay_active())
                            && (semantic_build_in_progress || semantic_corpus_refresh_in_progress)
                            && watcher_path_is_semantic_source(path)
                        {
                            let _ = ctx.run_if_subc_bound_generation(lifecycle_generation, || {
                                ctx.add_pending_semantic_index_paths([path.to_path_buf()])
                            });
                        }
                    },
                )
            }
            WatcherDrainApplyPhase::SymbolCache => apply_watcher_path_phase(
                WatcherDrainApplyPhase::SymbolCache,
                &mut paths,
                &mut remaining,
                started,
                WATCHER_DRAIN_SLICE_BUDGET,
                |path| {
                    if apply_ram_search_updates {
                        let _ = ctx.run_if_subc_bound_generation(lifecycle_generation, || {
                            if let Ok(mut symbol_cache) = ctx.symbol_cache().write() {
                                symbol_cache.invalidate(path);
                            }
                        });
                    }
                },
            ),
            WatcherDrainApplyPhase::SearchIndex => apply_watcher_path_phase(
                WatcherDrainApplyPhase::SearchIndex,
                &mut paths,
                &mut remaining,
                started,
                WATCHER_DRAIN_SLICE_BUDGET,
                // The index store receives every change in the tier-2 phase
                // (`record_checkout_watcher_change`); no other text index
                // takes watcher updates.
                |_path| {},
            ),
            WatcherDrainApplyPhase::SemanticIndex => {
                // Every root masks stale shared vectors immediately. A writer or
                // an enabled RAM overlay also queues a private replacement.
                let mut invalidated_paths = Vec::new();
                let completed = apply_watcher_path_phase(
                    WatcherDrainApplyPhase::SemanticIndex,
                    &mut paths,
                    &mut remaining,
                    started,
                    WATCHER_DRAIN_SLICE_BUDGET,
                    |path| {
                        if heavy_root_work_allowed && watcher_path_is_semantic_source(path) {
                            invalidated_paths.push(path.to_path_buf());
                        }
                    },
                );

                if !invalidated_paths.is_empty() {
                    // Invalidate all semantic paths processed in this slice under
                    // one write lock so a multi-file edit scans the index once.
                    let _ = ctx.run_if_subc_bound_generation(lifecycle_generation, || {
                        // Read the index's file records under a read lock, then
                        // compare them with disk after releasing it: the
                        // comparison can hash files, and a burst of a thousand
                        // paths must not hold off queries while it runs.
                        let recorded = ctx
                            .semantic_index()
                            .read()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .as_ref()
                            .map(|index| {
                                semantic_paths_with_recorded_freshness(
                                    ctx,
                                    index,
                                    std::mem::take(&mut invalidated_paths),
                                )
                            });
                        let Some(recorded) = recorded else {
                            return;
                        };
                        let invalidated_paths =
                            crate::semantic_index::files_with_changed_content(recorded);
                        if invalidated_paths.is_empty() {
                            return;
                        }
                        let invalidated = {
                            let mut semantic_index_ref = ctx
                                .semantic_index()
                                .write()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            semantic_index_ref.as_mut().is_some_and(|index| {
                                index.invalidate_files(&invalidated_paths);
                                true
                            })
                        };
                        if invalidated && ctx.semantic_refresh_sender().is_some() {
                            let mut status = ctx
                                .semantic_index_status()
                                .write()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            if matches!(&*status, SemanticIndexStatus::Ready { .. }) {
                                for path in invalidated_paths {
                                    if watcher_path_is_ignored_by_current_matcher(ctx, &path) {
                                        continue;
                                    }
                                    status.add_refreshing_file(path.clone());
                                    semantic_refresh_paths.push(path);
                                }
                                status_changed = true;
                            }
                        }
                    });
                }
                completed
            }
            WatcherDrainApplyPhase::LspDiagnostics => try_apply_watcher_path_phase(
                WatcherDrainApplyPhase::LspDiagnostics,
                &mut paths,
                &mut remaining,
                started,
                WATCHER_DRAIN_SLICE_BUDGET,
                |path| {
                    ctx.run_if_subc_bound_generation(lifecycle_generation, || {
                        // Applicability refreshes also take this lock. Keep the
                        // invalidation pending when they overlap the watcher.
                        let Some(mut lsp) = ctx.try_lsp() else {
                            state.lsp_diagnostics_lock_contended = true;
                            return false;
                        };
                        if !path.exists() {
                            status_changed |= lsp.clear_diagnostics_for_file(path);
                            return true;
                        }
                        let stale = lsp.mark_diagnostics_stale_for_file(path);
                        drop(lsp);
                        status_changed |= stale.changed;
                        if stale.had_entries {
                            ctx.lsp_resync_changed_file_for_diagnostics(path);
                        }
                        true
                    })
                    .unwrap_or(true)
                },
            ),
            WatcherDrainApplyPhase::Complete => true,
        };

        // Per-path mutations are lifecycle-gated no-ops once the root unbinds,
        // so a stage that overlapped an unbind may have skipped paths (their
        // `remaining` was still decremented). Rewind the CURRENT stage (every
        // stage action is idempotent) and park the continuation; the rebased
        // replay after rebind re-runs it in full. This check must cover the
        // budget-exhausted park as well: a mid-stage park that kept the
        // decremented `remaining` would permanently skip the gated paths.
        if ctx
            .run_if_subc_bound_generation(lifecycle_generation, || ())
            .is_none()
        {
            state.status_changed = status_changed;
            state.semantic_refresh_paths = semantic_refresh_paths;
            remaining = paths.len();
            state.phase = WatcherDrainPhase::Apply {
                stage,
                paths,
                remaining,
                oversized_inline_batch,
            };
            return;
        }

        if !completed {
            state.status_changed = status_changed;
            state.semantic_refresh_paths = semantic_refresh_paths;
            state.phase = WatcherDrainPhase::Apply {
                stage,
                paths,
                remaining,
                oversized_inline_batch,
            };
            return;
        }

        if stage == WatcherDrainApplyPhase::LspDiagnostics {
            // Diagnostics work can yield across many slices, but the forwarding
            // cap must see the whole batch. Sending each slice independently
            // lets a large generated tree evade the cap and floods the server.
            // No paths have been forwarded before this point, so a lifecycle
            // rewind cannot replay an already-sent partial notification.
            if ctx
                .run_if_subc_bound_generation(lifecycle_generation, || {
                    ctx.lsp_forward_classified_watcher_file_events(paths.make_contiguous());
                })
                .is_none()
            {
                state.status_changed = status_changed;
                state.semantic_refresh_paths = semantic_refresh_paths;
                state.phase = WatcherDrainPhase::Apply {
                    stage,
                    paths,
                    remaining,
                    oversized_inline_batch,
                };
                return;
            }
        }
        if stage == WatcherDrainApplyPhase::Complete {
            break;
        }
        stage = next_watcher_apply_phase(stage);
        remaining = paths.len();
        if watcher_drain_elapsed(started) >= WATCHER_DRAIN_SLICE_BUDGET {
            state.status_changed = status_changed;
            state.semantic_refresh_paths = semantic_refresh_paths;
            state.phase = WatcherDrainPhase::Apply {
                stage,
                paths,
                remaining,
                oversized_inline_batch,
            };
            return;
        }
    }

    if !semantic_refresh_paths.is_empty() {
        // Distinguish "root unbound" (None) from "worker unavailable"
        // (Some(false)): losing admission here must PARK the collected paths
        // for the post-rebind replay, not drop them — the per-path
        // invalidation already ran, so these paths are the only record that
        // the semantic index is stale for them.
        match ctx.run_if_subc_bound_generation(lifecycle_generation, || {
            ctx.semantic_refresh_sender().is_some_and(|sender| {
                sender
                    .send(SemanticRefreshRequest::Files {
                        paths: semantic_refresh_paths.clone(),
                    })
                    .is_ok()
            })
        }) {
            Some(true) => {}
            Some(false) => {
                aft::slog_warn!(
                    "semantic refresh worker unavailable; dropping {} refreshing file(s)",
                    semantic_refresh_paths.len()
                );
                let mut status = ctx
                    .semantic_index_status()
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                for path in &semantic_refresh_paths {
                    status.cancel_refreshing_file(path);
                }
                status_changed = true;
            }
            None => {
                state.status_changed = status_changed;
                state.semantic_refresh_paths = semantic_refresh_paths;
                state.phase = WatcherDrainPhase::Apply {
                    stage: WatcherDrainApplyPhase::Complete,
                    paths,
                    remaining: 0,
                    oversized_inline_batch,
                };
                return;
            }
        }
    }

    aft::slog_info!("invalidated {} files", paths.len());
    if status_changed {
        ctx.signal_status_changed();
    }
    ctx.tick_tier2_refresh_scheduler(state.scheduler_changed_path_count);
    state.phase = WatcherDrainPhase::Collect;
    state.status_changed = false;
    state.scheduler_changed_path_count = 0;
    state.semantic_refresh_paths.clear();
}

/// Start a linked worktree's on-demand call-graph build that a query asked
/// for but could not start itself (it ran outside the standalone scope).
/// Standalone mode has no root actor, so the build detaches under the same
/// standalone scope configure maintenance uses. Cheap when nothing is owed.
pub fn drain_on_demand_callgraph_build(ctx: &Arc<AppContext>) {
    // Today's call-graph store is retired, so nothing is ever owed.
    let _ = ctx;
}

fn rebuild_gitignore_for_drain(
    ctx: &AppContext,
    state: &mut WatcherDrainSliceState,
    heavy_root_work_allowed: bool,
    changed_paths: Option<&[PathBuf]>,
) -> crate::context::IgnoreRuleChange {
    // The drain owns this continuation while dispatching control events. Return
    // it to the context so the matcher publication retires all parked queues
    // together, before acknowledging the rebuild to the watcher thread.
    let placeholder = WatcherDrainSliceState::new(
        state.configure_generation,
        state.configure_content_generation,
    );
    *ctx.watcher_drain_slice().lock() = Some(std::mem::replace(state, placeholder));
    let change = if heavy_root_work_allowed {
        match changed_paths {
            Some(paths) => ctx.rebuild_gitignore_after_changes(paths),
            None => {
                ctx.rebuild_gitignore();
                crate::context::IgnoreRuleChange::Full { scopes: Vec::new() }
            }
        }
    } else {
        ctx.clear_gitignore();
        crate::context::IgnoreRuleChange::Full { scopes: Vec::new() }
    };
    *state = ctx
        .watcher_drain_slice()
        .lock()
        .take()
        .expect("parked watcher continuation");
    change
}

pub fn drain_watcher_events_bounded(ctx: &AppContext, max_paths: usize) -> DrainBatchOutcome {
    let _in_flight = ctx.watcher_drain_guard();
    let started = watcher_drain_now();
    let configure_generation = ctx.configure_generation();
    let content_generation = ctx.configure_content_generation();
    let mut outcome = DrainBatchOutcome::default();
    // Ordinary admission rejects missing directories, but their cleanup must
    // still run. It retains the lifecycle gate and retries without a watcher.
    if ctx.try_cleanup_deleted_root(configure_generation) {
        return outcome;
    }
    // Admission before touching the continuation: an unbound invocation must
    // leave the retained state in place for the rebind to rebase, not take
    // and drop it.
    if ctx
        .run_if_subc_bound_generation(configure_generation, || ())
        .is_none()
    {
        return outcome;
    }
    ctx.record_checkout_pending_watcher_gap();
    let mut state = match ctx.watcher_drain_slice().lock().take() {
        Some(state) if state.configure_generation == configure_generation => state,
        // Lifecycle-only generation change (transient unbind + equivalent
        // rebind): the retained paths are still valid for this configuration.
        // Rebase onto the current generation and replay — every apply phase
        // is idempotent (re-indexing an unchanged file is a no-op).
        Some(mut state) if state.configure_content_generation == content_generation => {
            state.configure_generation = configure_generation;
            state
        }
        _ => WatcherDrainSliceState::new(configure_generation, content_generation),
    };
    let mut dispatch_events_received = 0usize;
    let mut watcher_failed = None;
    let mut root_deleted = false;

    {
        let rx_ref = ctx.watcher_rx().lock();
        // Idle verification must also run when no watcher receiver is installed.
        while let Some(rx) = rx_ref.as_ref() {
            match rx.try_recv() {
                Ok(
                    event @ (WatcherDispatchEvent::Paths(_)
                    | WatcherDispatchEvent::ContentOnlyPaths(_)),
                ) => {
                    let content_only = matches!(event, WatcherDispatchEvent::ContentOnlyPaths(_));
                    let (WatcherDispatchEvent::Paths(paths)
                    | WatcherDispatchEvent::ContentOnlyPaths(paths)) = event
                    else {
                        unreachable!()
                    };
                    dispatch_events_received += 1;
                    // Before the ignore filter: lockfiles and installed
                    // packages are often gitignored but still change which
                    // TypeScript server a file gets.
                    ctx.lsp_note_watcher_batch(&paths, content_only);
                    ctx.data_note_paths(&paths);
                    delay_watcher_receive_for_test();
                    if !state.rescan_required {
                        let mut accepted = false;
                        state.pending_paths.extend(
                            paths
                                .into_iter()
                                .filter(|path| {
                                    !crate::watcher_filter::queued_path_is_ignored_by_matcher(
                                        &ctx.shared_gitignore(),
                                        path,
                                    )
                                })
                                .inspect(|_| accepted = true),
                        );
                    }
                }
                Ok(WatcherDispatchEvent::RescanRequired(reason)) => {
                    // The individual changed paths are lost, so the data
                    // layer compares every file with its checkout's state.
                    ctx.data_rescan();
                    dispatch_events_received += 1;
                    // A rescan means the watcher dropped the individual
                    // changed paths, so any of them may have changed the
                    // installed TypeScript.
                    crate::lsp::typescript_project::clear_typescript_selection();
                    if let Some(root) = ctx.canonical_cache_root_opt() {
                        crate::cache_freshness::invalidate_verify_memo_strict(&root);
                    }
                    state.rescan_required = true;
                    state.rescan_ignore_checked = false;
                    state.rescan_reason = reason;
                    state.pending_paths.clear();
                    state.phase = WatcherDrainPhase::Collect;
                    state.semantic_refresh_paths.clear();
                    state.scheduler_changed_path_count = 0;
                }
                Ok(WatcherDispatchEvent::CookieObserved(_)) => {
                    dispatch_events_received += 1;
                }
                Ok(WatcherDispatchEvent::IgnoreRulesChanged { paths }) => {
                    dispatch_events_received += 1;
                    // The watcher filter drops an ignore file that ignores
                    // itself from the changed paths, so the data layer hears
                    // of it only here; recording it rescans its folder.
                    ctx.data_note_paths(&paths);
                    ctx.lsp_note_watcher_ignore_change();
                    if state.rescan_required {
                        state.rescan_ignore_checked = false;
                    }
                    if !state.rescan_required {
                        let heavy_root_work_allowed = ctx.heavy_root_work_allowed();
                        let change = ctx
                            .run_if_subc_bound_generation(configure_generation, || {
                                rebuild_gitignore_for_drain(
                                    ctx,
                                    &mut state,
                                    heavy_root_work_allowed,
                                    Some(&paths),
                                )
                            })
                            .unwrap_or(crate::context::IgnoreRuleChange::Full {
                                scopes: Vec::new(),
                            });
                        match change {
                            crate::context::IgnoreRuleChange::Unchanged => {}
                            crate::context::IgnoreRuleChange::AddOnly {
                                retired_paths,
                                scopes,
                            } => {
                                crate::slog_info!(
                                    "watcher: ignore rules added exclusions in {:?}; retiring {} path(s)",
                                    scopes,
                                    retired_paths.len()
                                );
                                state.pending_paths.extend(retired_paths);
                            }
                            crate::context::IgnoreRuleChange::Scoped {
                                affected_paths,
                                scopes,
                            } => {
                                crate::slog_info!(
                                    "watcher: ignore rules changed in {:?}; refreshing {} affected path(s)",
                                    scopes,
                                    affected_paths.len()
                                );
                                note_scoped_ignore_rule_refresh_for_test();
                                state.pending_paths.extend(affected_paths);
                            }
                            crate::context::IgnoreRuleChange::Full { scopes } => {
                                crate::slog_info!(
                                    "watcher: ignore rules changed in {:?}; rebuilt matcher",
                                    scopes
                                );
                                arm_ignore_rule_refresh(&mut state, paths);
                            }
                        }
                    }
                }
                Ok(WatcherDispatchEvent::RootDeleted) => {
                    dispatch_events_received += 1;
                    root_deleted = true;
                    break;
                }
                Ok(WatcherDispatchEvent::Error(error)) => {
                    dispatch_events_received += 1;
                    watcher_failed = Some(error);
                    break;
                }
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    watcher_failed = Some("watcher channel disconnected".to_string());
                    break;
                }
            }
            if watcher_drain_elapsed(started) >= WATCHER_DRAIN_SLICE_BUDGET {
                break;
            }
        }
    }

    crate::logging::note_watcher_events(dispatch_events_received);
    let receiver_has_more_after_receive = ctx
        .watcher_rx()
        .lock()
        .as_ref()
        .is_some_and(|rx| !rx.is_empty());

    if root_deleted {
        ctx.try_cleanup_deleted_root(configure_generation);
        return outcome;
    }
    if let Some(error) = watcher_failed {
        ctx.stop_watcher_runtime_in_background();
        let _ = ctx.add_degraded_reason("watcher_unavailable".to_string());
        aft::slog_warn!(
            "file watcher unavailable; continuing without live external-change invalidation: {}",
            error
        );
        ctx.signal_status_changed();
        return outcome;
    }

    if state.rescan_required {
        let _ = ctx.run_if_subc_bound_generation(configure_generation, || {
            // Exact candidate sets were proven against files whose changes
            // were lost; none of them may be reused.
            ctx.search_exact_memo().clear();
        });
    }
    if state.rescan_required && receiver_has_more_after_receive {
        outcome.has_more = true;
        *ctx.watcher_drain_slice().lock() = Some(state);
        return outcome;
    }

    if !state.rescan_required
        && !state.ignore_changed
        && state
            .ignore_refresh_due
            .is_some_and(|due| ignore_rule_refresh_now() >= due)
    {
        state.ignore_changed = true;
    }

    if state.rescan_required {
        if !state.rescan_ignore_checked && state.ignore_scan.is_none() {
            state.rescan_ignore_checked = true;
            if ctx.watcher_counters().take_overflow_ignore_relevance() {
                if ctx.heavy_root_work_allowed() {
                    state.ignore_scan = Some(ctx.start_gitignore_rescan());
                } else {
                    let _ = rebuild_gitignore_for_drain(ctx, &mut state, false, None);
                }
            }
        }
        if let Some(rx) = state.ignore_scan.as_ref() {
            match rx.try_recv() {
                Ok(prepared) => {
                    state.ignore_scan = None;
                    // Publication retires all parked queues together; only the
                    // snapshot discovery and matcher construction run off-thread.
                    let placeholder = WatcherDrainSliceState::new(
                        state.configure_generation,
                        state.configure_content_generation,
                    );
                    *ctx.watcher_drain_slice().lock() =
                        Some(std::mem::replace(&mut state, placeholder));
                    let published = ctx.run_if_subc_bound_generation(configure_generation, || {
                        ctx.publish_gitignore_rescan(prepared)
                    });
                    state = ctx
                        .watcher_drain_slice()
                        .lock()
                        .take()
                        .expect("parked watcher continuation");
                    if published.is_none() {
                        state.rescan_ignore_checked = false;
                    }
                }
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    state.ignore_scan = None;
                    state.rescan_ignore_checked = false;
                }
                Err(crossbeam_channel::TryRecvError::Empty) => {}
            }
            if state.ignore_scan.is_some() || !state.rescan_ignore_checked {
                outcome.has_more = true;
                *ctx.watcher_drain_slice().lock() = Some(state);
                return outcome;
            }
        }
        crate::logging::note_watcher_overflow();
        let root = ctx
            .canonical_cache_root_opt()
            .or_else(|| ctx.config().project_root.clone())
            .unwrap_or_else(|| PathBuf::from("<unconfigured>"));
        let watcher_counters = ctx.watcher_counters();
        watcher_counters.start_rescan();
        let rescan_reason = state.rescan_reason;
        let deferred_rescan = {
            let interval = watcher_counters.begin_rescan(rescan_reason);
            wait_on_watcher_rescan_gate_for_test(ctx);
            let app = ctx.app();
            let db = app.db();
            crate::watcher_filter::persist_watcher_observations(
                &root,
                &watcher_counters,
                db.as_ref(),
            );
            let rss_before = watcher_rescan_rss_bytes();
            let rescan_started = Instant::now();
            state.status_changed |= refresh_project_after_watcher_rescan(ctx);
            let cost_ms = rescan_started.elapsed().as_millis().min(u64::MAX as u128) as u64;
            let rss_delta_bytes = watcher_rescan_rss_delta(rss_before, watcher_rescan_rss_bytes());
            watcher_counters.finish_rescan(cost_ms, rss_delta_bytes);
            crate::logging::log_watcher_rescan(
                &root,
                rescan_reason,
                cost_ms,
                rss_delta_bytes,
                interval.raw_events,
            );
            // At most one pass per dispatch slice. A loss during this pass is
            // retained for the next slice, including its ignore-path evidence.
            watcher_counters.finish_rescan_walk()
        };
        state.scheduler_changed_path_count =
            aft::inspect::tier2_scheduler::TIER2_REFRESH_STORM_PATH_THRESHOLD + 1;
        if state.status_changed {
            ctx.signal_status_changed();
        }
        ctx.tick_tier2_refresh_scheduler(state.scheduler_changed_path_count);
        // Acknowledge the rescan only if the whole refresh sequence ran under
        // the original lifecycle generation: an unbind advances the
        // generation, and the internal admission gates then made parts of the
        // sequence no-ops. Keeping the flag parks the rescan for the
        // post-rebind replay instead of acknowledging a partial one.
        if ctx
            .run_if_subc_bound_generation(configure_generation, || ())
            .is_some()
        {
            state.rescan_required = deferred_rescan.is_some();
            state.rescan_ignore_checked = false;
            if let Some(reason) = deferred_rescan {
                state.rescan_reason = reason;
            }
            state.ignore_changed = false;
            state.ignore_refresh_due = None;
            state.ignore_changed_paths.clear();
            state.ignore_changed_path_count = 0;
        }
        state.status_changed = false;
        state.scheduler_changed_path_count = 0;
    } else if matches!(state.phase, WatcherDrainPhase::Collect) {
        let ignore_changed = state.ignore_changed;
        if ignore_changed {
            state.status_changed |= refresh_corpus_after_ignore_change(ctx);
            // Same partial-sequence rule as the rescan path: acknowledge only
            // when the refresh ran fully under this lifecycle generation.
            if ctx
                .run_if_subc_bound_generation(configure_generation, || ())
                .is_some()
            {
                state.ignore_changed = false;
                state.ignore_refresh_due = None;
                state.ignore_changed_paths.clear();
                state.ignore_changed_path_count = 0;
            }
        }

        if max_paths > 0 && !state.pending_paths.is_empty() {
            let mut unique = HashSet::new();
            let mut paths = VecDeque::new();
            while outcome.processed < max_paths {
                let Some(path) = state.pending_paths.pop_front() else {
                    break;
                };
                outcome.processed += 1;
                if unique.insert(path.clone()) {
                    paths.push_back(path);
                }
            }
            crate::logging::note_drain_paths(outcome.processed);

            if paths.is_empty() {
                if state.status_changed {
                    ctx.signal_status_changed();
                }
                ctx.tick_tier2_refresh_scheduler(usize::from(ignore_changed));
                state.status_changed = false;
            } else {
                state.path_slice_count += 1;
                state.scheduler_changed_path_count = if ignore_changed {
                    paths.len().max(1)
                } else {
                    paths.len()
                };
                if paths.iter().any(|path| watcher_path_is_tsconfig(path))
                    && ctx
                        .run_if_subc_bound_generation(configure_generation, || {
                            ctx.clear_tsconfig_membership_cache();
                        })
                        .is_some()
                {
                    state.status_changed = true;
                }

                let oversized_inline_batch = paths.len() > WATCHER_BATCH_INLINE_CAP;
                if oversized_inline_batch {
                    aft::slog_warn!(
                        "watcher batch of {} paths exceeds inline cap {}; applying bounded incremental refresh",
                        paths.len(),
                        WATCHER_BATCH_INLINE_CAP
                    );
                }
                let remaining = paths.len();
                state.phase = WatcherDrainPhase::Apply {
                    stage: WatcherDrainApplyPhase::PendingTier2,
                    paths,
                    remaining,
                    oversized_inline_batch,
                };
            }
        } else if ignore_changed {
            if state.status_changed {
                ctx.signal_status_changed();
            }
            ctx.tick_tier2_refresh_scheduler(1);
            state.status_changed = false;
        }
    }

    // Enter the apply phase even when receiving and batching already used the
    // whole slice budget. Each apply step checks the deadline only after it has
    // applied a path, so a late entry applies exactly one path and returns.
    // Skipping the apply phase here instead let a slice return with nothing
    // applied, and a steady stream of watcher events whose receive cost alone
    // fills the budget would then starve the queued batch on every slice.
    if matches!(state.phase, WatcherDrainPhase::Apply { .. }) {
        apply_watcher_slice(ctx, &mut state, started);
    }

    let receiver_has_more = ctx
        .watcher_rx()
        .lock()
        .as_ref()
        .is_some_and(|rx| !rx.is_empty());
    // The Tier-2 debounce waits for a quiet window, and every other tick site
    // in this drain runs while paths are being applied — those ticks arrive
    // WITH changes, which push the debounce forward. A pass that finds nothing
    // left to apply IS the quiet window, so evaluate the scheduler here too:
    // otherwise a refresh that came due during the silence waits for the next
    // file change, which restarts the window it was waiting on.
    if !state.has_pending_work() && !receiver_has_more {
        ctx.tick_tier2_refresh_scheduler(0);
    }
    outcome.has_more = state.has_pending_work() || receiver_has_more;
    // Retain the continuation across lifecycle-only generation changes (a
    // mid-drain unbind advances the generation; the rebind rebases). Only a
    // content change — a real reconfigure — discards it.
    if state.configure_content_generation == ctx.configure_content_generation() {
        *ctx.watcher_drain_slice().lock() = Some(state);
    }
    outcome
}

pub fn drain_lsp_events(ctx: &AppContext) {
    let _ = drain_lsp_events_bounded(ctx, usize::MAX);
}

/// Reap individually idle language servers, also releasing all unused clients
/// when the standalone runtime is idle. The daemon supplies each root's last
/// tool-call activity timestamp.
pub fn shutdown_idle_lsp(ctx: &AppContext) {
    shutdown_idle_lsp_at(ctx, Instant::now(), ctx.last_request_at());
}

pub fn shutdown_idle_lsp_at(ctx: &AppContext, now: Instant, last_activity: Instant) {
    let Some(ttl) = ctx.config().lsp_idle_minutes.ttl() else {
        return;
    };
    let idle = now.saturating_duration_since(last_activity);
    let clients = {
        // The subc frame loop runs this sweep, so it must not wait for the
        // manager. A held manager may be sending a request; the next sweep retries.
        let Some(mut lsp) = ctx.try_lsp() else {
            return;
        };
        if lsp.server_count() == 0 {
            return;
        }
        lsp.take_idle_clients(now, ttl, idle >= ttl)
    };
    crate::lsp::manager::LspManager::spawn_idle_lsp_reap(clients);
}

/// Bytes of LSP `params` kept in a debug line. Diagnostics can quote source and
/// `workspace/applyEdit` carries whole file text, so the rest is dropped.
const LSP_PARAMS_LOG_BYTES: usize = 512;

fn lsp_params_for_log(params: Option<serde_json::Value>) -> String {
    let rendered = params.unwrap_or(serde_json::Value::Null).to_string();
    let redacted = crate::log_redact::aft_redactor(&rendered);
    crate::log_redact::truncate_for_log(&redacted, LSP_PARAMS_LOG_BYTES).into_owned()
}

pub fn drain_lsp_events_bounded(ctx: &AppContext, max_events: usize) -> DrainBatchOutcome {
    let drained = {
        let Some(mut lsp) = ctx.try_lsp() else {
            // Keep demand alive without waiting for a producer's round trip.
            // Maintenance must retry zero progress on its next tick, not in
            // an immediate completion/requeue loop while this lock is held.
            return DrainBatchOutcome {
                processed: 0,
                has_more: true,
            };
        };
        lsp.drain_events_bounded(max_events)
    };
    let outcome = DrainBatchOutcome {
        processed: drained.processed,
        has_more: drained.has_more,
    };
    let mut status_changed = drained.diagnostics_changed;
    #[cfg(all(test, unix))]
    for instance in drained.instances.iter().flatten() {
        crate::lsp::manager::instance_regression_tests::pause_dispatch(*instance);
    }
    let Some(mut lsp) = ctx.try_lsp() else {
        // Store mutations already committed during the first lock acquisition.
        // Refresh their status without waiting or replaying the receipts; only
        // optional dispatch logging is lost when another caller owns the lock.
        if outcome.processed > 0 || status_changed {
            ctx.signal_status_changed();
        }
        return DrainBatchOutcome {
            has_more: true,
            ..outcome
        };
    };
    for (event, instance) in drained.events.into_iter().zip(drained.instances) {
        if !lsp.accept_event_instance(&event, instance) {
            continue;
        }
        match event {
            LspEvent::DocumentDiagnosticsPulled { .. } => {
                // The manager already stored and version-checked this reply.
                // Its diagnostics_changed flag drives the ordinary status signal.
            }
            LspEvent::Notification {
                server_kind,
                root,
                method,
                params,
            } => {
                log::debug!(
                    "[aft-lsp] notification {:?} {} {} {}",
                    server_kind,
                    root.display(),
                    method,
                    lsp_params_for_log(params)
                );
            }
            LspEvent::ServerRequest {
                server_kind,
                root,
                id,
                method,
                params,
            } => {
                status_changed |= matches!(
                    method.as_str(),
                    "client/registerCapability" | "client/unregisterCapability"
                );
                log::debug!(
                    "[aft-lsp] request {:?} {} {:?} {} {}",
                    server_kind,
                    root.display(),
                    id,
                    method,
                    lsp_params_for_log(params)
                );
            }
            LspEvent::ServerExited { .. } => {
                // `LspManager::handle_event` already wrote the one log line
                // for this exit, with its status and stderr tail.
                status_changed = true;
            }
        }
    }
    drop(lsp);
    if status_changed {
        ctx.signal_status_changed();
    }
    outcome
}

#[cfg(test)]
fn finish_watcher_drain_for_test(
    ctx: &AppContext,
    max_paths: usize,
    mut outcome: DrainBatchOutcome,
) -> DrainBatchOutcome {
    // End-of-batch effects are published only after every slice finishes.
    // The injected slice clock may be frozen, so use real time for the watchdog.
    let deadline = Instant::now() + Duration::from_secs(60);
    while outcome.has_more {
        assert!(Instant::now() < deadline, "watcher drain never completed");
        let next = drain_watcher_events_bounded(ctx, max_paths);
        outcome.processed += next.processed;
        outcome.has_more = next.has_more;
        if next.has_more && next.processed == 0 {
            std::thread::yield_now();
        }
    }
    outcome
}

#[cfg(test)]
pub(crate) fn drain_watcher_to_completion_for_test(
    ctx: &AppContext,
    max_paths: usize,
) -> DrainBatchOutcome {
    finish_watcher_drain_for_test(ctx, max_paths, drain_watcher_events_bounded(ctx, max_paths))
}

#[cfg(test)]
fn finish_watcher_apply_phase_for_test(ctx: &AppContext) {
    // Finish the admitted batch without consuming paths behind the path cap.
    let deadline = Instant::now() + Duration::from_secs(60);
    while ctx
        .watcher_drain_slice()
        .lock()
        .as_ref()
        .is_some_and(|state| matches!(state.phase, WatcherDrainPhase::Apply { .. }))
    {
        assert!(
            Instant::now() < deadline,
            "watcher apply phase never completed"
        );
        assert_eq!(drain_watcher_events_bounded(ctx, 0).processed, 0);
        std::thread::yield_now();
    }
}

#[cfg(test)]
mod request_lsp_contention_tests {
    use super::*;

    #[test]
    fn request_lsp_drain_defers_a_held_manager_without_waiting() {
        let ctx = Arc::new(AppContext::new(
            crate::context::default_language_provider_factory(),
            crate::config::Config::default(),
        ));
        let held = ctx.lsp();
        let worker_ctx = Arc::clone(&ctx);
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let outcome = drain_lsp_events_bounded(&worker_ctx, 10);
            tx.send(outcome).unwrap();
        });
        let outcome = rx.recv_timeout(Duration::from_secs(5));
        drop(held);
        worker.join().unwrap();
        let outcome = outcome.expect("request drain must not acquire a contended manager");
        assert_eq!(outcome.processed, 0);
        assert!(
            outcome.has_more,
            "contention must preserve maintenance demand"
        );
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn issue645_rescan_yields_before_followup() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let (ctx, tx) = watcher_context(&root);
        let ctx = Arc::new(ctx);
        ctx.watcher_counters().note_overflow_ignore_relevance(false);
        tx.send(WatcherDispatchEvent::RescanRequired(RescanReason::Unknown))
            .unwrap();
        let (reached, release) = install_watcher_rescan_gate_for_test(&ctx);
        let worker_ctx = ctx.clone();
        let drain = std::thread::spawn(move || drain_watcher_events_bounded(&worker_ctx, 16));
        reached.recv_timeout(Duration::from_secs(2)).unwrap();
        ctx.watcher_counters().note_overflow_ignore_relevance(false);
        assert!(ctx
            .watcher_counters()
            .note_overflow(RescanReason::Unknown, Vec::new()));
        release.send(()).unwrap();
        let outcome = drain.join().unwrap();
        assert!(
            outcome.has_more,
            "a mid-rescan overflow must yield, not loop on dispatch"
        );
        assert_eq!(ctx.watcher_counters().snapshot().rescans_unknown_total, 1);
    }

    #[test]
    fn issue645_tool_dispatch_does_not_wait_for_ignore_scan() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let (ctx, tx) = watcher_context(&root);
        let ctx = Arc::new(ctx);
        let (reached_tx, reached) = crossbeam_channel::bounded(1);
        let (release, release_rx) = crossbeam_channel::bounded(1);
        *crate::context::IGNORE_RESCAN_GATE
            .get_or_init(Default::default)
            .lock() = Some(crate::context::IgnoreRescanGate {
            root,
            reached: reached_tx,
            release: release_rx,
        });
        tx.send(WatcherDispatchEvent::RescanRequired(RescanReason::Unknown))
            .unwrap();
        let (done_tx, done) = crossbeam_channel::bounded(1);
        let dispatch_ctx = ctx.clone();
        let dispatch = std::thread::spawn(move || {
            let result = drain_watcher_events_bounded(&dispatch_ctx, 16);
            // The next tool's config and matcher reads share these locks.
            let _config = dispatch_ctx.config();
            let _matcher = dispatch_ctx.gitignore();
            done_tx.send(result).unwrap();
        });
        reached
            .recv_timeout(Duration::from_secs(2))
            .expect("ignore scanner did not start");
        let result = done.recv_timeout(Duration::from_secs(2));
        release.send(()).unwrap();
        dispatch.join().unwrap();
        assert!(
            result
                .expect("tool dispatch blocked behind ignore scan")
                .has_more
        );
    }

    #[test]
    fn issue645_overflow_without_ignore_paths_skips_ignore_scan() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let (ctx, tx) = watcher_context(&root);
        let generation = ctx
            .gitignore_generation()
            .load(std::sync::atomic::Ordering::SeqCst);
        ctx.watcher_counters().note_overflow_ignore_relevance(false);
        tx.send(WatcherDispatchEvent::RescanRequired(RescanReason::Unknown))
            .unwrap();
        let outcome = drain_watcher_events_bounded(&ctx, 16);
        assert!(
            !outcome.has_more,
            "metadata-only overflow scheduled an ignore walk"
        );
        assert_eq!(
            ctx.gitignore_generation()
                .load(std::sync::atomic::Ordering::SeqCst),
            generation
        );
    }

    #[test]
    fn issue645_overflow_ignore_walk_never_runs_on_dispatch_thread() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        let (ctx, tx) = watcher_context(&root);
        let before = AppContext::ignore_walks_on_current_thread_for_test();
        tx.send(WatcherDispatchEvent::RescanRequired(RescanReason::Unknown))
            .unwrap();
        drain_watcher_events_bounded(&ctx, 16);
        assert_eq!(
            AppContext::ignore_walks_on_current_thread_for_test(),
            before,
            "overflow must not walk ignore files on the tool dispatch thread"
        );
    }

    use super::*;
    use crate::config::Config;
    use crate::context::{default_language_provider_factory, AppContext};

    #[test]
    fn inspect_failure_drain_does_not_delay_unrelated_read() {
        use crate::inspect::job::{InspectCategory, JobOutcome};
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        std::fs::write(root.join("hello.txt"), "hello from read\n").unwrap();
        let (ctx, _) = watcher_context(&root);
        let _writer =
            crate::inspect::cache::tests::foreign_writer(&root, &ctx.inspect_dir(), false);
        let request = crate::protocol::RawRequest {
            id: "unrelated-read".into(),
            command: "read".into(),
            params: serde_json::json!({"file": root.join("hello.txt")}),
            session_id: None,
            lsp_hints: None,
        };
        assert!(crate::commands::read::handle_read(&request, &ctx).success);
        for _ in 0..3 {
            ctx.inspect_manager().record_tier2_attempt_outcome_for_test(
                InspectCategory::Duplicates,
                JobOutcome::Failed {
                    message: "failed to open inspect cache: writer_lease_timeout: inspect writer lease deadline elapsed".into(),
                },
            );
            let start = Instant::now();
            drain_inspect_events(&ctx);
            let response = crate::commands::read::handle_read(&request, &ctx);
            assert!(response.success, "{response:?}");
            assert!(
                start.elapsed() < Duration::from_millis(500),
                "inspect status blocked an unrelated read for {:?}",
                start.elapsed()
            );
            assert_eq!(
                ctx.inspect_manager()
                    .tier2_builder_state(InspectCategory::Duplicates),
                crate::inspect::InspectBuilderState::WaitingForCacheWriterLease
            );
        }
    }

    #[test]
    fn lsp_secret_is_masked_before_truncation() {
        let prefix = "x".repeat(LSP_PARAMS_LOG_BYTES - 20);
        let rendered = lsp_params_for_log(Some(
            serde_json::json!({ "text": format!("{prefix} ghp_1234567890abcdefghijklmnop") }),
        ));
        let logged = crate::log_redact::aft_redactor(&rendered);
        assert!(
            !logged.contains("ghp_"),
            "truncated credential prefix survived: {logged}"
        );
    }

    #[test]
    fn lsp_params_debug_rendering_is_bounded() {
        let file_text = "x".repeat(10_000);
        let rendered = lsp_params_for_log(Some(serde_json::json!({ "text": file_text })));
        let full_len = serde_json::json!({ "text": "x".repeat(10_000) })
            .to_string()
            .len();
        assert!(
            rendered.len() < LSP_PARAMS_LOG_BYTES + 32,
            "{}",
            rendered.len()
        );
        assert!(
            rendered.ends_with(&format!("…(+{} bytes)", full_len - LSP_PARAMS_LOG_BYTES)),
            "{rendered}"
        );
        assert_eq!(lsp_params_for_log(None), "null");
    }

    fn watcher_context(
        root: &Path,
    ) -> (AppContext, crossbeam_channel::Sender<WatcherDispatchEvent>) {
        let ctx = AppContext::new(default_language_provider_factory(), Config::default());
        ctx.update_config(|config| {
            config.project_root = Some(root.to_path_buf());
        });
        ctx.set_canonical_cache_root(root.to_path_buf());
        let (tx, rx) = crossbeam_channel::unbounded();
        *ctx.watcher_rx().lock() = Some(rx);
        (ctx, tx)
    }

    #[test]
    fn watcher_drain_fifty_content_batches_keep_applicability_cache() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let source = root.join("main.py");
        std::fs::write(&source, "print(0)").unwrap();
        let (ctx, tx) = watcher_context(&root);
        let config = ctx.config();
        crate::lsp::manager::walk_applicable_area(&root, None, &config, None).unwrap();
        let before = crate::lsp::manager::applicability_metrics(&root).full_walks;
        for n in 1..=50 {
            std::fs::write(&source, format!("print({n})")).unwrap();
            tx.send(WatcherDispatchEvent::ContentOnlyPaths(vec![source.clone()]))
                .unwrap();
            for slice in 0..64 {
                if !drain_watcher_events_bounded(&ctx, WATCHER_PATH_DRAIN_BATCH_CAP).has_more {
                    break;
                }
                assert!(slice < 63, "watcher batch must finish draining");
            }
            crate::lsp::manager::walk_applicable_area(&root, None, &config, None).unwrap();
        }
        let metrics = crate::lsp::manager::applicability_metrics(&root);
        assert_eq!(metrics.full_walks - before, 0);
        assert_eq!(metrics.retained_content_batches, 50);
        assert!(metrics.invalidations.is_empty());
    }

    struct IgnoreRuleRefreshTestReset;

    impl Drop for IgnoreRuleRefreshTestReset {
        fn drop(&mut self) {
            set_ignore_rule_refresh_now_for_test(None);
            reset_ignore_rule_refreshes_for_test();
        }
    }

    fn begin_ignore_rule_refresh_test() -> IgnoreRuleRefreshTestReset {
        reset_ignore_rule_refreshes_for_test();
        set_ignore_rule_refresh_now_for_test(Some(Instant::now()));
        IgnoreRuleRefreshTestReset
    }

    fn advance_ignore_rule_refresh_clock(duration: Duration) {
        IGNORE_RULE_REFRESH_TEST_NOW.with(|clock| {
            let now = clock.get().expect("ignore refresh test clock is installed");
            clock.set(Some(now + duration));
        });
    }

    fn send_ignore_rule_change(
        tx: &crossbeam_channel::Sender<WatcherDispatchEvent>,
        path: PathBuf,
    ) {
        tx.send(WatcherDispatchEvent::IgnoreRulesChanged { paths: vec![path] })
            .unwrap();
    }

    #[test]
    fn six_ignore_rule_events_over_four_seconds_coalesce_into_one_refresh() {
        let _reset = begin_ignore_rule_refresh_test();
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let (ctx, tx) = watcher_context(&root);
        let ignore_path = root.join(".gitignore");
        std::fs::write(&ignore_path, "seed/\n").unwrap();
        ctx.rebuild_gitignore();

        for index in 0..6 {
            std::fs::write(&ignore_path, format!("generated-{index}/\n")).unwrap();
            send_ignore_rule_change(&tx, ignore_path.clone());
            let outcome = drain_watcher_events_bounded(&ctx, WATCHER_PATH_DRAIN_BATCH_CAP);
            assert!(
                !outcome.has_more,
                "an armed quiet window must not busy-loop"
            );
            if index < 5 {
                advance_ignore_rule_refresh_clock(Duration::from_millis(800));
            }
        }

        assert_eq!(
            ignore_rule_refreshes_for_test(),
            0,
            "the six-event burst must stay quiet until five seconds after its last event"
        );
        advance_ignore_rule_refresh_clock(Duration::from_millis(4_999));
        drain_watcher_events_bounded(&ctx, WATCHER_PATH_DRAIN_BATCH_CAP);
        assert_eq!(ignore_rule_refreshes_for_test(), 0);

        advance_ignore_rule_refresh_clock(Duration::from_millis(1));
        drain_watcher_events_bounded(&ctx, WATCHER_PATH_DRAIN_BATCH_CAP);
        assert_eq!(ignore_rule_refreshes_for_test(), 1);
    }

    #[test]
    fn single_ignore_rule_event_refreshes_after_five_second_quiet_window() {
        let _reset = begin_ignore_rule_refresh_test();
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let (ctx, tx) = watcher_context(&root);
        let ignore_path = root.join(".aftignore");
        std::fs::write(&ignore_path, "old/\n").unwrap();
        ctx.rebuild_gitignore();
        std::fs::write(&ignore_path, "generated/\n").unwrap();
        send_ignore_rule_change(&tx, ignore_path);

        let outcome = drain_watcher_events_bounded(&ctx, WATCHER_PATH_DRAIN_BATCH_CAP);

        assert!(
            !outcome.has_more,
            "the deadline is not immediately runnable work"
        );
        assert_eq!(ignore_rule_refreshes_for_test(), 0);
        {
            let state = ctx.watcher_drain_slice().lock();
            let state = state.as_ref().expect("armed ignore refresh state");
            assert!(state.ignore_refresh_due.is_some());
            assert!(!state.ignore_changed);
            assert!(
                !state.status_changed,
                "arming must not change search health"
            );
        }

        advance_ignore_rule_refresh_clock(Duration::from_millis(4_999));
        drain_watcher_events_bounded(&ctx, WATCHER_PATH_DRAIN_BATCH_CAP);
        assert_eq!(ignore_rule_refreshes_for_test(), 0);

        advance_ignore_rule_refresh_clock(Duration::from_millis(1));
        drain_watcher_events_bounded(&ctx, WATCHER_PATH_DRAIN_BATCH_CAP);
        assert_eq!(ignore_rule_refreshes_for_test(), 1);
    }

    #[test]
    fn repeated_bare_wildcard_ignore_rewrites_have_zero_downstream_work() {
        const REWRITES: usize = 8;

        let _reset = begin_ignore_rule_refresh_test();
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let build_dir = root.join("engram-worker/build");
        std::fs::create_dir_all(&build_dir).unwrap();
        let ignore_path = build_dir.join(".gitignore");
        std::fs::write(&ignore_path, b"*").unwrap();
        let (ctx, tx) = watcher_context(&root);
        ctx.rebuild_gitignore();
        let rebuilds = ctx.gitignore_matcher_rebuild_count_for_test();
        let config = crate::watcher_filter::WatcherFilterConfig::new(root, None);
        let mut rule_changes = 0;

        for _ in 0..REWRITES {
            std::fs::write(&ignore_path, b"*").unwrap();
            let filtered = crate::watcher_filter::filter_watcher_raw_paths_for_test(
                &config,
                &ctx.shared_gitignore(),
                [ignore_path.clone()],
            );
            if filtered.ignore_file_changed {
                rule_changes += 1;
                tx.send(WatcherDispatchEvent::IgnoreRulesChanged {
                    paths: filtered.ignore_file_paths.into_iter().collect(),
                })
                .unwrap();
            }
            drain_watcher_events_bounded(&ctx, WATCHER_PATH_DRAIN_BATCH_CAP);
        }

        assert_eq!(rule_changes, REWRITES);
        assert_eq!(ctx.gitignore_matcher_rebuild_count_for_test(), rebuilds);
        assert_eq!(ignore_rule_refreshes_for_test(), 0);
        assert_eq!(ignore_rule_callgraph_invalidations_for_test(), 0);
    }

    #[test]
    fn identical_self_matching_root_gitignore_rewrite_does_not_rebuild_matcher() {
        let _reset = begin_ignore_rule_refresh_test();
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let ignore_path = root.join(".gitignore");
        std::fs::write(&ignore_path, b".gitignore\n").unwrap();
        let (ctx, tx) = watcher_context(&root);
        ctx.rebuild_gitignore();
        let rebuilds = ctx.gitignore_matcher_rebuild_count_for_test();
        let config = crate::watcher_filter::WatcherFilterConfig::new(root, None);

        std::fs::write(&ignore_path, b".gitignore\n").unwrap();
        let filtered = crate::watcher_filter::filter_watcher_raw_paths_for_test(
            &config,
            &ctx.shared_gitignore(),
            [ignore_path.clone()],
        );
        assert_eq!(
            filtered.ignore_file_paths,
            std::collections::BTreeSet::from([ignore_path])
        );
        tx.send(WatcherDispatchEvent::IgnoreRulesChanged {
            paths: filtered.ignore_file_paths.into_iter().collect(),
        })
        .unwrap();
        drain_watcher_events_bounded(&ctx, WATCHER_PATH_DRAIN_BATCH_CAP);

        assert_eq!(ctx.gitignore_matcher_rebuild_count_for_test(), rebuilds);
        assert_eq!(ignore_rule_refreshes_for_test(), 0);
        assert_eq!(ignore_rule_corpus_rebuilds_for_test(), 0);
    }

    #[test]
    fn repeated_identical_nested_ignore_writes_do_no_refresh_work() {
        const REWRITES: usize = 8;

        let _reset = begin_ignore_rule_refresh_test();
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let nested = root.join("worker");
        std::fs::create_dir_all(nested.join("generated")).unwrap();
        std::fs::write(nested.join("generated/file.rs"), "fn generated() {}\n").unwrap();
        let ignore_path = nested.join(".gitignore");
        std::fs::write(&ignore_path, b"generated/\n").unwrap();
        let (ctx, tx) = watcher_context(&root);
        ctx.rebuild_gitignore();
        let rebuilds = ctx.gitignore_matcher_rebuild_count_for_test();
        let generation = ctx.gitignore_generation().load(Ordering::SeqCst);

        for _ in 0..REWRITES {
            std::fs::write(&ignore_path, b"generated/\n").unwrap();
            send_ignore_rule_change(&tx, ignore_path.clone());
            let outcome = drain_watcher_events_bounded(&ctx, WATCHER_PATH_DRAIN_BATCH_CAP);
            assert!(!outcome.has_more);
        }

        assert_eq!(ctx.gitignore_matcher_rebuild_count_for_test(), rebuilds);
        assert_eq!(ignore_rule_refreshes_for_test(), 0);
        assert_eq!(ignore_rule_callgraph_invalidations_for_test(), 0);
        assert_eq!(
            ctx.gitignore_generation().load(Ordering::SeqCst),
            generation + REWRITES as u64,
            "each no-op event must still acknowledge the waiting watcher thread"
        );
        assert!(ctx
            .watcher_drain_slice()
            .lock()
            .as_ref()
            .is_some_and(|state| state.ignore_refresh_due.is_none()));

        std::fs::write(&ignore_path, b"other/\n").unwrap();
        send_ignore_rule_change(&tx, ignore_path);
        while drain_watcher_events_bounded(&ctx, WATCHER_PATH_DRAIN_BATCH_CAP).has_more {}
        assert_eq!(ctx.gitignore_matcher_rebuild_count_for_test(), rebuilds + 1);
        assert_eq!(ignore_rule_refreshes_for_test(), 1);
        assert_eq!(ignore_rule_corpus_rebuilds_for_test(), 0);
        assert_eq!(ignore_rule_callgraph_invalidations_for_test(), 1);
    }

    #[test]
    fn newly_created_nested_ignore_file_changes_effective_input_set() {
        let _reset = begin_ignore_rule_refresh_test();
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let nested = root.join("worker");
        std::fs::create_dir_all(nested.join("generated")).unwrap();
        let generated = nested.join("generated/file.rs");
        std::fs::write(&generated, "fn generated() {}\n").unwrap();
        let (ctx, tx) = watcher_context(&root);
        ctx.rebuild_gitignore();
        let rebuilds = ctx.gitignore_matcher_rebuild_count_for_test();
        assert!(!watcher_path_is_ignored_by_current_matcher(
            &ctx, &generated
        ));

        let ignore_path = nested.join(".gitignore");
        std::fs::write(&ignore_path, b"generated/\n").unwrap();
        send_ignore_rule_change(&tx, ignore_path);
        drain_watcher_events_bounded(&ctx, WATCHER_PATH_DRAIN_BATCH_CAP);

        assert_eq!(ctx.gitignore_matcher_rebuild_count_for_test(), rebuilds + 1);
        assert!(watcher_path_is_ignored_by_current_matcher(&ctx, &generated));
    }

    #[test]
    fn subc_watcher_probe_becomes_ready_only_when_ignore_window_expires() {
        let _reset = begin_ignore_rule_refresh_test();
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let (ctx, tx) = watcher_context(&root);
        send_ignore_rule_change(&tx, root.join(".aftignore"));
        drain_watcher_events_bounded(&ctx, WATCHER_PATH_DRAIN_BATCH_CAP);

        assert!(!ctx.watcher_drain_has_work());
        let expired = Instant::now() - Duration::from_millis(1);
        ctx.watcher_drain_slice()
            .lock()
            .as_mut()
            .expect("armed watcher state")
            .ignore_refresh_due = Some(expired);
        set_ignore_rule_refresh_now_for_test(Some(Instant::now()));
        assert!(ctx.watcher_drain_has_work());

        drain_watcher_events_bounded(&ctx, WATCHER_PATH_DRAIN_BATCH_CAP);
        assert_eq!(ignore_rule_refreshes_for_test(), 1);
        assert!(!ctx.watcher_drain_has_work());
    }

    #[test]
    fn ignore_rule_refresh_armed_during_unbind_replays_after_rebind() {
        let _reset = begin_ignore_rule_refresh_test();
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let (ctx, tx) = watcher_context(&root);
        send_ignore_rule_change(&tx, root.join(".gitignore"));
        drain_watcher_events_bounded(&ctx, WATCHER_PATH_DRAIN_BATCH_CAP);

        ctx.mark_subc_unbound();
        advance_ignore_rule_refresh_clock(IGNORE_RULE_REFRESH_QUIET_WINDOW);
        drain_watcher_events_bounded(&ctx, WATCHER_PATH_DRAIN_BATCH_CAP);
        assert_eq!(ignore_rule_refreshes_for_test(), 0);

        ctx.mark_subc_bound();
        drain_watcher_events_bounded(&ctx, WATCHER_PATH_DRAIN_BATCH_CAP);
        assert_eq!(ignore_rule_refreshes_for_test(), 1);
        let state = ctx.watcher_drain_slice().lock();
        let state = state.as_ref().expect("replayed watcher state");
        assert!(state.ignore_refresh_due.is_none());
        assert!(state.ignore_changed_paths.is_empty());
    }

    #[test]
    fn watcher_semantic_phase_batches_invalidation_into_one_retain_pass() {
        // This checks batching within one slice, not filesystem latency. Keep
        // the slice clock fixed so a descheduled thread cannot split the batch.
        let _clock = WatcherDrainTestClock::freeze();
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let files = (0..4)
            .map(|ordinal| {
                let file = root_path.join(format!("source_{ordinal}.rs"));
                std::fs::write(&file, format!("pub fn source_{ordinal}() {{}}\n")).unwrap();
                file
            })
            .collect::<Vec<_>>();
        let mut embed = |texts: Vec<String>| {
            Ok::<_, String>(texts.into_iter().map(|_| vec![1.0, 0.5]).collect())
        };
        let index = crate::semantic_index::SemanticIndex::build(
            &root_path,
            &files,
            &mut embed,
            files.len(),
        )
        .unwrap();
        assert!(index.entry_count() >= files.len());

        let (ctx, watcher_tx) = watcher_context(&root_path);
        ctx.mark_subc_bound();
        ctx.set_heavy_root_work_allowed(true);
        ctx.set_cache_writer_capabilities(true, true);
        *ctx.semantic_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::ready();
        // Invalidation now skips files whose content still matches the index,
        // so every file really changes here to exercise the batched removal.
        for (ordinal, file) in files.iter().enumerate() {
            std::fs::write(file, format!("pub fn source_{ordinal}_edited() {{}}\n")).unwrap();
        }
        watcher_tx
            .send(WatcherDispatchEvent::Paths(files.clone()))
            .unwrap();

        let outcome = drain_watcher_to_completion_for_test(&ctx, files.len());

        assert_eq!(outcome.processed, files.len());
        assert!(!outcome.has_more);
        let index = ctx
            .semantic_index()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let index = index.as_ref().unwrap();
        assert_eq!(index.entry_count(), 0);
        assert_eq!(index.removal_retain_passes_for_test(), 1);
    }

    #[test]
    fn watcher_path_phase_yields_when_injected_slice_clock_expires() {
        let _clock = WatcherDrainTestClock::freeze();
        let started = watcher_drain_now();
        let mut paths = VecDeque::from([PathBuf::from("first.rs"), PathBuf::from("second.rs")]);
        let mut remaining = paths.len();
        let mut applied = Vec::new();
        let complete = apply_watcher_path_phase(
            WatcherDrainApplyPhase::SemanticIndex,
            &mut paths,
            &mut remaining,
            started,
            WATCHER_DRAIN_SLICE_BUDGET,
            |path| {
                applied.push(path.to_path_buf());
                WATCHER_DRAIN_TEST_NOW.with(|clock| {
                    clock.set(Some(started + WATCHER_DRAIN_SLICE_BUDGET));
                });
            },
        );
        assert!(!complete);
        assert_eq!(remaining, 1);
        assert_eq!(applied, vec![PathBuf::from("first.rs")]);
        assert_eq!(paths.front(), Some(&PathBuf::from("second.rs")));
    }

    #[test]
    fn watcher_path_phase_retains_busy_path_without_replaying_completed_paths() {
        let _clock = WatcherDrainTestClock::freeze();
        let started = watcher_drain_now();
        let first = PathBuf::from("first.rs");
        let second = PathBuf::from("second.rs");
        let mut paths = VecDeque::from([first.clone(), second.clone()]);
        let mut remaining = paths.len();
        let mut applied = Vec::new();
        assert!(!try_apply_watcher_path_phase(
            WatcherDrainApplyPhase::LspDiagnostics,
            &mut paths,
            &mut remaining,
            started,
            WATCHER_DRAIN_SLICE_BUDGET,
            |path| {
                if path == second {
                    return false;
                }
                applied.push(path.to_path_buf());
                true
            },
        ));
        assert_eq!(remaining, 1);
        assert_eq!(paths.front(), Some(&second));
        assert!(try_apply_watcher_path_phase(
            WatcherDrainApplyPhase::LspDiagnostics,
            &mut paths,
            &mut remaining,
            started,
            WATCHER_DRAIN_SLICE_BUDGET,
            |path| {
                applied.push(path.to_path_buf());
                true
            },
        ));
        assert_eq!(remaining, 0);
        assert_eq!(applied, vec![first, second]);
    }

    #[test]
    fn watcher_lsp_retry_probe_requires_a_failed_lock_attempt() {
        let _clock = WatcherDrainTestClock::freeze();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let (ctx, _tx) = watcher_context(&root);
        let file = root.join("main.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let mut state = WatcherDrainSliceState::new(
            ctx.configure_generation(),
            ctx.configure_content_generation(),
        );
        state.phase = WatcherDrainPhase::Apply {
            stage: WatcherDrainApplyPhase::LspDiagnostics,
            paths: VecDeque::from([file]),
            remaining: 1,
            oversized_inline_batch: false,
        };
        *ctx.watcher_drain_slice().lock() = Some(state);
        // A slice can stop at this phase before ever trying the manager lock.
        assert!(!ctx.watcher_lsp_diagnostics_retry_pending_for_test());
        {
            let _held = ctx.lsp();
            assert!(drain_watcher_events_bounded(&ctx, 1).has_more);
            assert!(ctx.watcher_lsp_diagnostics_retry_pending_for_test());
        }
        assert!(!drain_watcher_to_completion_for_test(&ctx, 1).has_more);
        assert!(!ctx.watcher_lsp_diagnostics_retry_pending_for_test());
    }

    #[test]
    fn watcher_empty_diagnostics_phase_finishes_with_busy_manager() {
        let _clock = WatcherDrainTestClock::freeze();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let (ctx, _tx) = watcher_context(&root);
        let mut state = WatcherDrainSliceState::new(
            ctx.configure_generation(),
            ctx.configure_content_generation(),
        );
        state.phase = WatcherDrainPhase::Apply {
            stage: WatcherDrainApplyPhase::LspDiagnostics,
            paths: VecDeque::new(),
            remaining: 0,
            oversized_inline_batch: false,
        };
        *ctx.watcher_drain_slice().lock() = Some(state);
        // No outstanding paths means no diagnostics lock is needed, even
        // while another producer owns the manager.
        let _held_lsp = ctx.lsp();

        let outcome = drain_watcher_events_bounded(&ctx, 1);

        assert_eq!(outcome.processed, 0);
        assert!(
            !outcome.has_more,
            "an empty diagnostics phase cannot defer work"
        );
        assert!(!ctx.watcher_lsp_diagnostics_retry_pending_for_test());
        assert!(matches!(
            ctx.watcher_drain_slice().lock().as_ref().unwrap().phase,
            WatcherDrainPhase::Collect
        ));
    }

    /// A burst of watcher events for files whose bytes still match the index
    /// (a copied tree, a checkout restoring the same content, FSEvents replaying
    /// events from before the watcher started) must leave every vector in place,
    /// queue nothing for re-embedding, and leave the status `ready`.
    #[test]
    fn watcher_burst_for_unchanged_files_keeps_vectors_and_queues_no_refresh() {
        // The drain may defer file checks when its wall-clock budget expires.
        // Freeze that clock so the no-refresh assertion observes all checks,
        // rather than passing because processing has not reached them yet.
        let _clock = WatcherDrainTestClock::freeze();
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let file_count = crate::context::SEMANTIC_MASS_REFRESH_MIN_FILES + 8;
        let files = (0..file_count)
            .map(|ordinal| {
                let file = root_path.join(format!("unchanged_{ordinal}.rs"));
                std::fs::write(&file, format!("pub fn unchanged_{ordinal}() {{}}\n")).unwrap();
                file
            })
            .collect::<Vec<_>>();
        let mut embed = |texts: Vec<String>| {
            Ok::<_, String>(texts.into_iter().map(|_| vec![1.0, 0.5]).collect())
        };
        let index = crate::semantic_index::SemanticIndex::build(
            &root_path,
            &files,
            &mut embed,
            files.len(),
        )
        .unwrap();
        let entries_before = index.entry_count();
        let vectors_before = index.entry_vectors_for_test();
        assert!(entries_before >= files.len());

        // Half the files are rewritten with identical bytes, so their mtime
        // moves and the content-hash comparison is what keeps them.
        std::thread::sleep(Duration::from_millis(20));
        for (ordinal, file) in files.iter().enumerate().step_by(2) {
            std::fs::write(file, format!("pub fn unchanged_{ordinal}() {{}}\n")).unwrap();
        }

        let (ctx, watcher_tx) = watcher_context(&root_path);
        ctx.mark_subc_bound();
        ctx.set_heavy_root_work_allowed(true);
        ctx.set_cache_writer_capabilities(true, true);
        *ctx.semantic_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::ready();
        let (request_tx, request_rx) = crossbeam_channel::unbounded();
        let (_event_tx, event_rx) = crossbeam_channel::unbounded();
        ctx.install_semantic_refresh_worker_for_build_epoch(
            request_tx,
            event_rx,
            Arc::new(Mutex::new(None)),
            ctx.semantic_index_rx_epoch(),
        );
        watcher_tx
            .send(WatcherDispatchEvent::Paths(files.clone()))
            .unwrap();

        // A background language-server discovery walk can briefly own the manager.
        // Hold it explicitly so diagnostics always defer, while the retry probe
        // below proves that every earlier semantic check has finished.
        let held_lsp = ctx.lsp();
        let outcome = drain_watcher_events_bounded(&ctx, files.len());

        assert_eq!(outcome.processed, files.len());
        assert!(
            outcome.has_more,
            "diagnostics must wait for the held manager"
        );
        assert!(ctx.watcher_lsp_diagnostics_retry_pending_for_test());
        drop(held_lsp);
        let index = ctx
            .semantic_index()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let index = index.as_ref().unwrap();
        assert_eq!(index.entry_count(), entries_before);
        assert_eq!(index.entry_vectors_for_test(), vectors_before);
        assert_eq!(index.removal_retain_passes_for_test(), 0);
        assert!(
            request_rx.try_recv().is_err(),
            "unchanged files must not be queued for re-embedding"
        );
        let status = ctx
            .semantic_index_status()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(status.refreshing_count(), 0);
        assert_eq!(index.status_label(&status), "ready");
    }

    /// A burst that really changes many files masks them all; status must stop
    /// saying `ready` for as long as they are pending.
    #[test]
    fn watcher_burst_of_real_changes_reports_refreshing() {
        // The drain can defer unfinished work when its wall-clock budget expires,
        // and it sends refresh requests only after processing the whole batch.
        // Freeze the slice clock, then finish any retained diagnostics work before
        // inspecting the end-of-batch refresh queue.
        let _clock = WatcherDrainTestClock::freeze();
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let file_count = crate::context::SEMANTIC_MASS_REFRESH_MIN_FILES + 8;
        let files = (0..file_count)
            .map(|ordinal| {
                let file = root_path.join(format!("changed_{ordinal}.rs"));
                std::fs::write(&file, format!("pub fn before_{ordinal}() {{}}\n")).unwrap();
                file
            })
            .collect::<Vec<_>>();
        let mut embed = |texts: Vec<String>| {
            Ok::<_, String>(texts.into_iter().map(|_| vec![1.0, 0.5]).collect())
        };
        let index = crate::semantic_index::SemanticIndex::build(
            &root_path,
            &files,
            &mut embed,
            files.len(),
        )
        .unwrap();
        for (ordinal, file) in files.iter().enumerate() {
            std::fs::write(file, format!("pub fn after_edit_{ordinal}() {{}}\n")).unwrap();
        }

        let (ctx, watcher_tx) = watcher_context(&root_path);
        ctx.mark_subc_bound();
        ctx.set_heavy_root_work_allowed(true);
        ctx.set_cache_writer_capabilities(true, true);
        *ctx.semantic_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::ready();
        let (request_tx, request_rx) = crossbeam_channel::unbounded();
        let (_event_tx, event_rx) = crossbeam_channel::unbounded();
        ctx.install_semantic_refresh_worker_for_build_epoch(
            request_tx,
            event_rx,
            Arc::new(Mutex::new(None)),
            ctx.semantic_index_rx_epoch(),
        );
        watcher_tx
            .send(WatcherDispatchEvent::Paths(files.clone()))
            .unwrap();

        let outcome = {
            let _held_lsp = ctx.lsp();
            let outcome = drain_watcher_events_bounded(&ctx, files.len());
            assert!(outcome.has_more);
            assert!(ctx.watcher_lsp_diagnostics_retry_pending_for_test());
            assert!(
                request_rx.try_recv().is_err(),
                "a refresh is published only after the diagnostics phase finishes"
            );
            outcome
        };
        let outcome = finish_watcher_drain_for_test(&ctx, files.len(), outcome);

        match request_rx.try_recv().expect("changed files are queued") {
            SemanticRefreshRequest::Files { paths } => assert_eq!(paths.len(), files.len()),
            SemanticRefreshRequest::Corpus => panic!("unexpected corpus refresh"),
        }
        assert_eq!(outcome.processed, files.len());
        assert!(!outcome.has_more);
        let mut status = ctx
            .semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let index = ctx
            .semantic_index()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let index = index.as_ref().unwrap();
        assert_eq!(index.entry_count(), 0);
        assert_eq!(status.mass_refresh_pending(), Some(files.len()));
        assert_eq!(index.status_label(&status), "refreshing");

        for file in &files {
            status.complete_refreshing_file(file);
        }
        assert_eq!(index.status_label(&status), "ready");
    }

    #[test]
    fn watcher_phase_commit_gates_are_path_scoped() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first.rs");
        let second = temp.path().join("second.rs");
        let (first_reached, first_release) =
            install_watcher_phase_commit_gate_for_test(first.clone());
        let (second_reached, second_release) =
            install_watcher_phase_commit_gate_for_test(second.clone());
        let first_apply =
            std::thread::spawn(move || wait_on_watcher_phase_commit_gate_for_test(&first));
        let second_apply =
            std::thread::spawn(move || wait_on_watcher_phase_commit_gate_for_test(&second));
        let first_result = first_reached.recv_timeout(Duration::from_secs(2));
        let second_result = second_reached.recv_timeout(Duration::from_secs(2));
        let _ = first_release.send(());
        let _ = second_release.send(());
        first_apply.join().unwrap();
        second_apply.join().unwrap();
        first_result.expect("first path's barrier must survive installing the second");
        second_result.expect("second path's barrier must be independent");
    }

    #[test]
    fn watcher_overflow_invalidates_artifact_freshness_memo() {
        let root = tempfile::tempdir().unwrap();
        let artifact = root.path().join("semantic.bin");
        std::fs::write(&artifact, b"artifact").unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let generation = crate::cache_freshness::artifact_generation(&artifact);
        let ticket = crate::cache_freshness::capture_verify_ticket(&canonical_root);
        assert!(
            crate::cache_freshness::record_verify_completed_if_unchanged(
                &canonical_root,
                crate::cache_freshness::VerifyArtifact::Semantic,
                generation,
                ticket,
            )
        );
        assert_eq!(
            crate::cache_freshness::warm_verify_plan(
                &canonical_root,
                crate::cache_freshness::VerifyArtifact::Semantic,
                generation,
            ),
            crate::cache_freshness::WarmVerifyPlan::Skip
        );

        let ctx = AppContext::new(
            default_language_provider_factory(),
            Config {
                project_root: Some(canonical_root.clone()),
                ..Config::default()
            },
        );
        ctx.set_canonical_cache_root(canonical_root.clone());
        refresh_project_after_watcher_rescan(&ctx);

        assert_eq!(
            crate::cache_freshness::warm_verify_plan(
                &canonical_root,
                crate::cache_freshness::VerifyArtifact::Semantic,
                generation,
            ),
            crate::cache_freshness::WarmVerifyPlan::Strict,
            "lost watcher events force STRICT verification: stat-first would \
             miss same-size, preserved-mtime edits made during the gap"
        );
    }

    #[test]
    fn rescan_arriving_while_unbound_executes_fully_after_rebind() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let (ctx, watcher_tx) = watcher_context(&root);
        // Warm Skip memo: the rescan's strict invalidation is its observable
        // effect, so the memo downgrade proves the refresh actually ran.
        let artifact = root.join("artifact.bin");
        std::fs::write(&artifact, b"artifact").unwrap();
        let generation = crate::cache_freshness::artifact_generation(&artifact);
        let ticket = crate::cache_freshness::capture_verify_ticket(&root);
        assert!(
            crate::cache_freshness::record_verify_completed_if_unchanged(
                &root,
                crate::cache_freshness::VerifyArtifact::Search,
                generation,
                ticket,
            )
        );
        watcher_tx
            .send(WatcherDispatchEvent::RescanRequired(RescanReason::Unknown))
            .unwrap();

        // While unbound the drain must not consume (and then lose) the
        // rescan: nothing may execute, so the memo stays warm.
        ctx.mark_subc_unbound();
        drain_watcher_events_bounded(&ctx, WATCHER_PATH_DRAIN_BATCH_CAP);
        assert_eq!(
            crate::cache_freshness::warm_verify_plan(
                &root,
                crate::cache_freshness::VerifyArtifact::Search,
                generation,
            ),
            crate::cache_freshness::WarmVerifyPlan::Skip,
            "an unbound drain must not run (or half-run) the rescan"
        );

        // After rebind the retained rescan executes in full: strict memo and
        // acknowledged flag.
        ctx.mark_subc_bound();
        let deadline = Instant::now() + Duration::from_secs(5);
        while drain_watcher_events_bounded(&ctx, WATCHER_PATH_DRAIN_BATCH_CAP).has_more {
            assert!(Instant::now() < deadline, "rescan replay must finish");
            std::thread::yield_now();
        }
        assert_eq!(
            crate::cache_freshness::warm_verify_plan(
                &root,
                crate::cache_freshness::VerifyArtifact::Search,
                generation,
            ),
            crate::cache_freshness::WarmVerifyPlan::Strict,
            "the post-rebind drain must execute the retained rescan strictly"
        );
        assert!(
            !ctx.watcher_drain_slice()
                .lock()
                .as_ref()
                .is_some_and(|state| state.rescan_required),
            "a fully-bound rescan must be acknowledged"
        );
    }

    #[test]
    fn own_mutation_plain_file_is_not_walked_again_at_drop() {
        let _clock = WatcherDrainTestClock::freeze();
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let file = root.join("source.ts");
        std::fs::write(&file, "const value = 'before';\n").unwrap();
        let ctx = AppContext::new(
            default_language_provider_factory(),
            Config {
                project_root: Some(root.clone()),
                ..Config::default()
            },
        );
        MUTATION_WALK_STATS.with(|stats| stats.set(0));
        {
            let _mutation = record_file_mutation(&ctx, [file.as_path()]);
            MUTATION_WALK_STATS.with(|stats| assert_eq!(stats.get(), 1));
        }
        MUTATION_WALK_STATS.with(|stats| assert_eq!(stats.get(), 1));
    }

    #[test]
    fn own_mutation_expired_walk_budget_does_not_advance_iterator() {
        let _clock = WatcherDrainTestClock::freeze();
        let temp = tempfile::tempdir().unwrap();
        let ctx = AppContext::new(default_language_provider_factory(), Config::default());
        let mut budget = MutationRefreshBudget::new();
        WATCHER_DRAIN_TEST_NOW
            .with(|now| now.set(Some(watcher_drain_now() + MUTATION_REFRESH_BUDGET)));
        let mut paths = std::collections::BTreeSet::new();
        MUTATION_DIRECTORY_READS.with(|reads| reads.borrow_mut().clear());
        assert!(!collect_mutation_paths(
            &ctx,
            &[temp.path().to_path_buf()],
            &mut paths,
            None,
            &mut budget
        ));
        assert_eq!(budget.examined, 0);
        assert!(paths.is_empty());
        MUTATION_DIRECTORY_READS.with(|reads| assert!(reads.borrow().is_empty()));
    }

    /// Waits up to 60 s for `ctx`'s checkout in the data layer to publish
    /// both its text search index and its call graph, with no recorded edit
    /// pending. The text index is published first, and the call graph's
    /// publication is a new one that changes the view's text generation, so
    /// a test counting the reads of one text generation waits for both.
    fn wait_for_data_layer_settled(ctx: &AppContext) {
        use crate::data_layer::surfaces::{report, READY};
        let started = Instant::now();
        loop {
            let report = report(ctx).expect("the data layer is on");
            if report.text.status == READY
                && report.callgraph.status == READY
                && report.pending_edits == 0
            {
                return;
            }
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "the data layer never settled: {report:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A configured context over `root`, with the data layer on for the
    /// calling thread and its storage folder (where the data layer keeps its
    /// index store) at `storage`.
    fn data_layer_context(root: &Path, storage: &Path) -> AppContext {
        let ctx = AppContext::new(
            default_language_provider_factory(),
            Config {
                project_root: Some(root.to_path_buf()),
                storage_dir: Some(storage.to_path_buf()),
                ..Config::default()
            },
        );
        ctx.set_canonical_cache_root(root.to_path_buf());
        ctx
    }

    #[test]
    fn exact_memo_is_retired_by_recorded_edits_but_keeps_warm_hits() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(project.path()).unwrap();
        let seed = root.join("seed.rs");
        let edited = root.join("edited.rs");
        std::fs::write(&seed, "// quartz meadow beacon\n").unwrap();
        std::fs::write(&edited, "// unrelated\n").unwrap();
        let ctx = data_layer_context(&root, storage.path());
        *ctx.semantic_index_status().write().unwrap() =
            crate::context::SemanticIndexStatus::Disabled;
        ctx.data_checkout().expect("the data layer binds the root");
        wait_for_data_layer_settled(&ctx);
        let request = serde_json::from_value(serde_json::json!({
            "id": "exact-memo-publication",
            "command": "semantic_search",
            "query": "quartz meadow beacon",
            "hint": "literal",
            "top_k": 10,
        }))
        .unwrap();
        let search = || {
            let response = serde_json::to_value(
                crate::commands::semantic_search::handle_semantic_search(&request, &ctx),
            )
            .unwrap();
            assert_eq!(response["success"], true, "{response}");
            response
        };
        // A repeated query is answered from the memo: one verification.
        assert_eq!(search()["result_count"], 1);
        assert_eq!(search()["result_count"], 1);
        assert_eq!(ctx.search_exact_memo().verifier_call_count(), 1);

        // An edit recorded as the watcher records it moves the view's text
        // generation on, so the memo's answer for the old text is retired
        // and the new one is verified once.
        std::fs::write(&edited, "// quartz meadow beacon\n").unwrap();
        ctx.data_note_paths(std::slice::from_ref(&edited));
        assert_eq!(search()["result_count"], 2);
        assert_eq!(search()["result_count"], 2);
        assert_eq!(ctx.search_exact_memo().verifier_call_count(), 2);

        std::fs::write(&seed, "// retired\n").unwrap();
        ctx.data_note_paths(std::slice::from_ref(&seed));
        assert_eq!(search()["result_count"], 1);
        assert_eq!(search()["result_count"], 1);
        assert_eq!(ctx.search_exact_memo().verifier_call_count(), 3);
    }

    /// A watcher overflow drops the changed paths; the rescan that follows
    /// must reach the data layer, or a file changed only during the overflow
    /// stays out of grep until the root is bound again.
    #[test]
    fn a_watcher_rescan_reaches_the_data_layer() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(project.path()).unwrap();
        std::fs::write(root.join("seed.rs"), "fn seed_marker() {}\n").unwrap();
        let ctx = data_layer_context(&root, storage.path());
        let (tx, rx) = crossbeam_channel::unbounded();
        *ctx.watcher_rx().lock() = Some(rx);
        ctx.data_checkout().expect("the data layer binds the root");
        wait_for_data_layer_settled(&ctx);
        let grep = |pattern: &str| {
            let request: crate::protocol::RawRequest = serde_json::from_value(serde_json::json!({
                "id": "rescan-grep",
                "command": "grep",
                "pattern": pattern,
            }))
            .unwrap();
            let response = crate::commands::grep::handle_grep(&request, &ctx);
            assert!(response.success, "{:?}", response.data);
            response.data["total_matches"].clone()
        };
        assert_eq!(grep("seed_marker"), 1, "grep reads the data layer's view");

        std::fs::write(
            root.join("overflow_only.rs"),
            "fn overflow_rescan_marker() {}\n",
        )
        .unwrap();
        ctx.watcher_counters().note_overflow_ignore_relevance(false);
        tx.send(WatcherDispatchEvent::RescanRequired(RescanReason::Unknown))
            .unwrap();
        for _ in 0..64 {
            if !drain_watcher_events_bounded(&ctx, 16).has_more {
                break;
            }
        }
        assert_eq!(
            ctx.watcher_counters().snapshot().rescans_unknown_total,
            1,
            "the rescan ran"
        );
        // The checkout compares its files with its state in the background,
        // so grep finds the file once that comparison has recorded it.
        let started = Instant::now();
        loop {
            let found = grep("overflow_rescan_marker");
            if found == 1 {
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "a file changed during the overflow must reach grep after the rescan: {found}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn semantic_owner_context(root: &Path) -> Arc<AppContext> {
        let ctx = Arc::new(AppContext::new(
            default_language_provider_factory(),
            Config {
                project_root: Some(root.to_path_buf()),
                indexes: crate::config::IndexesConfig {
                    trigram: false,
                    semantic: true,
                    callgraph: false,
                },
                ..Config::default()
            },
        ));
        ctx.set_canonical_cache_root(root.to_path_buf());
        ctx
    }

    #[test]
    fn lane_paused_refresh_does_not_advance_failure_breaker() {
        let directory = tempfile::tempdir().unwrap();
        let ctx = semantic_owner_context(directory.path());
        assert!(!record_semantic_refresh_transient_failure(
            &ctx,
            "[transient] connection refused"
        ));
        let error = crate::synapse_embed::SynapseEmbeddingError::LanePaused(
            crate::synapse_embed::LanePause {
                lane_id: "qwen3-embedding-0.6b-ane".into(),
                fingerprint: Some("pinned".into()),
                retry_after_ms: Some(2_700_000),
                paused_until_ms: 2_800_000,
                reason: "maintenance".into(),
                cause: Default::default(),
                safe_to_retry_same_request: true,
                record_id: 0,
            },
        )
        .to_string();
        for _ in 0..100 {
            assert!(!record_semantic_refresh_transient_failure(&ctx, &error));
        }
        assert_eq!(semantic_refresh_transient_failure_count_for_test(&ctx), 1);
        assert!(!semantic_refresh_circuit_is_open(&ctx));
    }

    #[test]
    fn delayed_semantic_retry_targets_same_generation_replacement_worker() {
        let _guard = ARTIFACT_DRAIN_TEST_MUTEX.lock().unwrap();
        SEMANTIC_REFRESH_RETRY_DELAY_OVERRIDE_MS.store(20, Ordering::SeqCst);
        struct RetryDelayReset;
        impl Drop for RetryDelayReset {
            fn drop(&mut self) {
                SEMANTIC_REFRESH_RETRY_DELAY_OVERRIDE_MS.store(u64::MAX, Ordering::SeqCst);
            }
        }
        let _delay_reset = RetryDelayReset;

        let root = tempfile::tempdir().unwrap();
        let ctx = AppContext::new(
            default_language_provider_factory(),
            Config {
                project_root: Some(root.path().to_path_buf()),
                indexes: crate::config::IndexesConfig {
                    trigram: false,
                    semantic: true,
                    callgraph: true,
                },
                ..Config::default()
            },
        );
        let (old_request_tx, _old_request_rx) = crossbeam_channel::unbounded();
        let (_old_event_tx, old_event_rx) = crossbeam_channel::unbounded();
        ctx.install_semantic_refresh_worker_for_build_epoch(
            old_request_tx,
            old_event_rx,
            Arc::new(Mutex::new(None)),
            ctx.semantic_index_rx_epoch(),
        );
        let retry_path = root.path().join("retry.rs");
        assert!(schedule_semantic_refresh_retry(
            &ctx,
            vec![retry_path.clone()],
            "transient embedding failure",
        ));

        let (new_request_tx, new_request_rx) = crossbeam_channel::unbounded();
        let (_new_event_tx, new_event_rx) = crossbeam_channel::unbounded();
        ctx.install_semantic_refresh_worker_for_build_epoch(
            new_request_tx,
            new_event_rx,
            Arc::new(Mutex::new(None)),
            ctx.semantic_index_rx_epoch(),
        );

        let request = new_request_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("retry should resolve the replacement sender when it fires");
        assert!(matches!(
            request,
            SemanticRefreshRequest::Files { paths } if paths == vec![retry_path]
        ));
    }

    #[test]
    fn watcher_drain_batch_cap_yields_with_events_remaining() {
        // Check the admission cap independently of wall-clock preemption.
        let _clock = WatcherDrainTestClock::freeze();
        let temp = tempfile::tempdir().unwrap();
        let (ctx, tx) = watcher_context(temp.path());
        let cap = 3;
        for index in 0..(cap * 2 + 1) {
            tx.send(WatcherDispatchEvent::Paths(vec![temp
                .path()
                .join(format!("file-{index}.rs"))]))
                .unwrap();
        }

        let first = drain_watcher_events_bounded(&ctx, cap);

        assert_eq!(first.processed, cap);
        assert!(first.has_more);
        assert_eq!(ctx.pending_tier2_paths().len(), cap);
    }

    #[test]
    fn watcher_drain_requeues_until_all_events_are_applied() {
        let temp = tempfile::tempdir().unwrap();
        let (ctx, tx) = watcher_context(temp.path());
        let cap = 4;
        let total = cap * 2 + 3;
        for index in 0..total {
            tx.send(WatcherDispatchEvent::Paths(vec![temp
                .path()
                .join(format!("file-{index}.rs"))]))
                .unwrap();
        }

        let mut processed = 0;
        loop {
            let outcome = drain_watcher_events_bounded(&ctx, cap);
            assert!(outcome.processed <= cap);
            processed += outcome.processed;
            if !outcome.has_more {
                break;
            }
        }

        assert_eq!(processed, total);
        assert_eq!(ctx.pending_tier2_paths().len(), total);
    }
}

#[cfg(test)]
mod watcher_slice_tests {
    use super::*;
    use crate::config::Config;
    use crate::context::{default_language_provider_factory, AppContext};

    #[test]
    fn request_watcher_drain_applies_at_most_one_slice() {
        // This checks the path cap, not wall-clock preemption before the first phase.
        let _clock = WatcherDrainTestClock::freeze();
        let root = tempfile::tempdir().unwrap();
        let (ctx, tx) = context_with_watcher(root.path());
        let total = WATCHER_PATH_DRAIN_BATCH_CAP * 2 + 1;
        tx.send(WatcherDispatchEvent::Paths(
            (0..total)
                .map(|index| root.path().join(format!("changed-{index}.rs")))
                .collect(),
        ))
        .unwrap();
        drain_watcher_events(&ctx);
        let applied = ctx.pending_tier2_paths().len();
        assert!(
            applied > 0 && applied <= WATCHER_PATH_DRAIN_BATCH_CAP,
            "request applied {applied} paths (burst {total})"
        );
        drain_watcher_to_completion_for_test(&ctx, WATCHER_PATH_DRAIN_BATCH_CAP);
        assert_eq!(ctx.pending_tier2_paths().len(), total);
    }

    fn context_with_watcher(
        root: &Path,
    ) -> (AppContext, crossbeam_channel::Sender<WatcherDispatchEvent>) {
        let ctx = AppContext::new(default_language_provider_factory(), Config::default());
        ctx.update_config(|config| config.project_root = Some(root.to_path_buf()));
        ctx.set_canonical_cache_root(root.to_path_buf());
        let (tx, rx) = crossbeam_channel::unbounded();
        *ctx.watcher_rx().lock() = Some(rx);
        (ctx, tx)
    }

    fn set_watcher_unit_test_seam(delay: Duration, thresholds: Option<(Duration, Duration)>) {
        WATCHER_UNIT_TEST_DELAY.with(|value| value.set(delay));
        WATCHER_UNIT_TEST_THRESHOLDS.with(|value| value.set(thresholds));
        WATCHER_UNIT_TEST_LOGS.with(|logs| logs.borrow_mut().clear());
    }

    fn clear_watcher_unit_test_seam() {
        set_watcher_unit_test_seam(Duration::ZERO, None);
    }

    #[test]
    fn watcher_slice_applies_a_path_when_receiving_exhausts_the_budget() {
        // Every slice receives one new event whose processing alone uses the
        // whole slice budget (the pinned clock advances by the budget). The
        // queued batch must still move forward by one path per slice instead
        // of waiting for the event stream to go quiet.
        struct ResetReceiveDelay;
        impl Drop for ResetReceiveDelay {
            fn drop(&mut self) {
                WATCHER_RECEIVE_TEST_DELAY.with(|delay| delay.set(Duration::ZERO));
            }
        }
        let _clock = WatcherDrainTestClock::freeze();
        WATCHER_RECEIVE_TEST_DELAY.with(|delay| delay.set(WATCHER_DRAIN_SLICE_BUDGET));
        let _reset = ResetReceiveDelay;
        let temp = tempfile::tempdir().unwrap();
        let (ctx, tx) = context_with_watcher(temp.path());
        let batch = 4;
        tx.send(WatcherDispatchEvent::Paths(
            (0..batch)
                .map(|index| temp.path().join(format!("first-{index}.rs")))
                .collect(),
        ))
        .unwrap();
        let apply_position = |ctx: &AppContext| {
            let slice = ctx.watcher_drain_slice().lock();
            let state = slice.as_ref().expect("watcher continuation is retained");
            let WatcherDrainPhase::Apply {
                stage, remaining, ..
            } = &state.phase
            else {
                panic!("the first batch must still be applying");
            };
            (*stage, *remaining, state.pending_paths.len())
        };

        let outcome = drain_watcher_events_bounded(&ctx, WATCHER_PATH_DRAIN_BATCH_CAP);
        assert_eq!(outcome.processed, batch);
        assert!(outcome.has_more);
        assert_eq!(
            apply_position(&ctx),
            (WatcherDrainApplyPhase::PendingTier2, batch - 1, 0),
            "a slice whose receive step used the budget must still apply one path"
        );

        for slice in 1..batch {
            tx.send(WatcherDispatchEvent::Paths(vec![temp
                .path()
                .join(format!("later-{slice}.rs"))]))
                .unwrap();
            assert!(drain_watcher_events_bounded(&ctx, WATCHER_PATH_DRAIN_BATCH_CAP).has_more);
            assert_eq!(
                apply_position(&ctx),
                (
                    WatcherDrainApplyPhase::PendingTier2,
                    batch - 1 - slice,
                    slice
                ),
                "slice {slice} under sustained events must apply one more path"
            );
        }
        // The emptied stage hands the batch to the next one on the next slice.
        tx.send(WatcherDispatchEvent::Paths(vec![temp
            .path()
            .join("later-final.rs")]))
            .unwrap();
        drain_watcher_events_bounded(&ctx, WATCHER_PATH_DRAIN_BATCH_CAP);
        assert_eq!(
            apply_position(&ctx),
            (WatcherDrainApplyPhase::PendingIndexes, batch, batch)
        );
    }

    #[test]
    fn lsp_watcher_phase_defers_forwarding_until_all_slices_complete() {
        let _clock = WatcherDrainTestClock::freeze();
        let temp = tempfile::tempdir().unwrap();
        let ctx = AppContext::new(
            default_language_provider_factory(),
            Config {
                project_root: Some(temp.path().to_path_buf()),
                ..Config::default()
            },
        );
        ctx.capture_classified_watcher_forwards_for_test();
        let paths: VecDeque<_> = (0..crate::lsp::manager::WATCHED_FILE_FORWARD_CAP + 10)
            .map(|index| temp.path().join(format!("generated_{index}.rs")))
            .chain(std::iter::once(temp.path().join("Cargo.toml")))
            .collect();
        let count = paths.len();
        let expected: Vec<_> = paths.iter().cloned().collect();
        let mut state = WatcherDrainSliceState::new(ctx.configure_generation(), 0);
        state.phase = WatcherDrainPhase::Apply {
            stage: WatcherDrainApplyPhase::LspDiagnostics,
            paths,
            remaining: count,
            oversized_inline_batch: false,
        };
        // A contended diagnostics slice retains the entire batch. Forwarding
        // cannot start until that work is retried after the manager is free.
        {
            let _held = ctx.lsp();
            apply_watcher_slice(&ctx, &mut state, watcher_drain_now());
            assert!(state.lsp_diagnostics_lock_contended);
            assert!(matches!(
                &state.phase,
                WatcherDrainPhase::Apply {
                    stage: WatcherDrainApplyPhase::LspDiagnostics,
                    remaining,
                    ..
                } if *remaining == count
            ));
            assert!(ctx.take_classified_watcher_forwards_for_test().is_empty());
        }
        // Once uncontended, an expired budget applies exactly one path per
        // slice. Capture forwarding calls directly: the uncontended send path
        // does not need a queue-serving thread.
        for applied in 1..=count {
            apply_watcher_slice(
                &ctx,
                &mut state,
                watcher_drain_now() - WATCHER_DRAIN_SLICE_BUDGET,
            );
            assert!(matches!(
                &state.phase,
                WatcherDrainPhase::Apply { remaining, .. } if *remaining == count - applied
            ));
            assert!(
                ctx.take_classified_watcher_forwards_for_test().is_empty(),
                "a partial diagnostics slice must not forward a partial watcher batch"
            );
        }
        apply_watcher_slice(&ctx, &mut state, watcher_drain_now());
        assert!(matches!(state.phase, WatcherDrainPhase::Collect));
        assert_eq!(
            ctx.take_classified_watcher_forwards_for_test(),
            vec![expected],
            "forward the complete batch exactly once after all diagnostics slices finish"
        );
        apply_watcher_slice(&ctx, &mut state, watcher_drain_now());
        assert!(ctx.take_classified_watcher_forwards_for_test().is_empty());
    }

    #[test]
    fn watcher_unit_watchdog_names_slow_phase_and_path() {
        let temp = tempfile::tempdir().unwrap();
        let slow_path = temp.path().join("slow.rs");
        let mut paths = VecDeque::from([slow_path.clone()]);
        let mut remaining = 1;
        set_watcher_unit_test_seam(
            Duration::from_millis(5),
            Some((Duration::from_millis(1), Duration::from_secs(1))),
        );

        let completed = apply_watcher_path_phase(
            WatcherDrainApplyPhase::SemanticIndex,
            &mut paths,
            &mut remaining,
            Instant::now(),
            WATCHER_DRAIN_SLICE_BUDGET,
            |_| {},
        );
        let logs = WATCHER_UNIT_TEST_LOGS.with(|logs| logs.borrow().clone());
        clear_watcher_unit_test_seam();

        assert!(completed);
        assert_eq!(logs.len(), 1);
        assert!(logs[0].contains("watcher drain unit exceeded 5s"));
        assert!(logs[0].contains("phase=semantic_index"));
        assert!(logs[0].contains(&format!("path={}", slow_path.display())));
    }

    #[test]
    fn watcher_single_dispatch_event_is_sliced_by_path_count() {
        let temp = tempfile::tempdir().unwrap();
        let (ctx, tx) = context_with_watcher(temp.path());
        let path_count = 1_024;
        let path_cap = 256;
        tx.send(WatcherDispatchEvent::Paths(
            (0..path_count)
                .map(|index| temp.path().join(format!("single-event-{index}.txt")))
                .collect(),
        ))
        .unwrap();

        let mut slices = 0;
        let mut processed = 0;
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let outcome = drain_watcher_events_bounded(&ctx, path_cap);
            slices += 1;
            processed += outcome.processed;
            assert!(outcome.processed <= path_cap);
            if !outcome.has_more {
                break;
            }
            // A busy diagnostics manager can yield without applying another
            // path, so convergence cannot be bounded by the number of paths.
            assert!(
                Instant::now() < deadline,
                "single dispatch event did not converge"
            );
            if outcome.processed == 0 {
                std::thread::yield_now();
            }
        }

        assert_eq!(processed, path_count);
        // At least ceil(1024/256) slices from the path budget; the time budget
        // may only add slices, never remove them.
        assert!(
            slices >= 4,
            "expected at least 4 path-budgeted slices, got {slices}"
        );
        assert_eq!(ctx.pending_tier2_paths().len(), path_count);
    }

    #[test]
    fn watcher_overflows_during_rescan_coalesce_one_followup_without_filter_stall() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let ctx = Arc::new(AppContext::new(
            default_language_provider_factory(),
            Config::default(),
        ));
        ctx.update_config(|config| config.project_root = Some(root.clone()));
        ctx.set_canonical_cache_root(root.clone());
        let counters = ctx.watcher_counters();
        let matcher = Arc::new(std::sync::RwLock::new(None));
        let matcher_generation = Arc::new(AtomicU64::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (dispatch_tx, dispatch_rx) = crossbeam_channel::bounded(8);
        *ctx.watcher_rx().lock() = Some(dispatch_rx);
        let (raw_sender_tx, raw_sender_rx) = crossbeam_channel::bounded(1);
        let filter_shutdown = Arc::clone(&shutdown);
        let filter_root = root.clone();
        let filter = std::thread::spawn(move || {
            crate::watcher_filter::run_watcher_thread(
                crate::watcher_filter::WatcherFilterConfig::new(filter_root, None),
                Vec::new(),
                matcher,
                matcher_generation,
                dispatch_tx,
                filter_shutdown,
                move |_root, _extra, raw_tx| {
                    raw_sender_tx.send(raw_tx).unwrap();
                    Ok::<(), std::io::Error>(())
                },
            );
        });
        let raw_tx = raw_sender_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let overflow =
            || notify::Event::new(notify::EventKind::Other).set_flag(notify::event::Flag::Rescan);
        raw_tx.send(Ok(overflow())).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while (counters.snapshot().overflows_total != 1
            || ctx
                .watcher_rx()
                .lock()
                .as_ref()
                .is_none_or(crossbeam_channel::Receiver::is_empty))
            && Instant::now() < deadline
        {
            std::thread::yield_now();
        }
        assert_eq!(counters.snapshot().overflows_total, 1);
        assert!(ctx
            .watcher_rx()
            .lock()
            .as_ref()
            .is_some_and(|receiver| !receiver.is_empty()));
        let _ = crate::watcher_filter::take_watcher_overflow_logs_for_test();

        let (rescan_reached, release_rescan) = install_watcher_rescan_gate_for_test(&ctx);
        let drain_ctx = Arc::clone(&ctx);
        let drain = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            while drain_watcher_events_bounded(&drain_ctx, usize::MAX).has_more {
                assert!(Instant::now() < deadline, "async rescan did not converge");
                std::thread::yield_now();
            }
        });
        rescan_reached
            .recv_timeout(Duration::from_secs(2))
            .expect("first rescan did not reach the test gate");

        raw_tx.send(Ok(overflow())).unwrap();
        raw_tx.send(Ok(overflow())).unwrap();
        let callback_deadline = Instant::now() + Duration::from_millis(50);
        while counters.snapshot().overflows_total != 3 && Instant::now() < callback_deadline {
            std::thread::yield_now();
        }
        assert_eq!(counters.snapshot().overflows_total, 3);
        assert!(ctx
            .watcher_rx()
            .lock()
            .as_ref()
            .is_some_and(crossbeam_channel::Receiver::is_empty));
        release_rescan.send(()).unwrap();
        drain.join().unwrap();

        let snapshot = counters.snapshot();
        assert_eq!(snapshot.overflows_during_rescan, 2);
        assert_eq!(snapshot.rescans_unknown_total, 2);
        assert!(!counters.rescan_in_progress());
        assert!(ctx
            .watcher_rx()
            .lock()
            .as_ref()
            .is_some_and(crossbeam_channel::Receiver::is_empty));

        shutdown.store(true, Ordering::SeqCst);
        drop(raw_tx);
        filter.join().unwrap();
        let lines = crate::watcher_filter::take_watcher_overflow_logs_for_test()
            .into_iter()
            .filter(|line| line.contains(&format!("root={}", root.display())))
            .collect::<Vec<_>>();
        assert_eq!(
            lines.len(),
            1,
            "one warning per burst, not per lost-event flag"
        );
        assert!(lines[0].contains("count=3"), "{}", lines[0]);
    }

    #[test]
    fn watcher_rescan_supersedes_pending_paths() {
        let _clock = WatcherDrainTestClock::freeze();
        let temp = tempfile::tempdir().unwrap();
        let (ctx, tx) = context_with_watcher(temp.path());
        tx.send(WatcherDispatchEvent::Paths(
            (0..5)
                .map(|index| temp.path().join(format!("before-rescan-{index}.txt")))
                .collect(),
        ))
        .unwrap();
        let first = drain_watcher_events_bounded(&ctx, 2);
        assert_eq!(first.processed, 2);
        assert!(first.has_more);
        finish_watcher_apply_phase_for_test(&ctx);
        assert_eq!(ctx.watcher_drain_pending_path_count(), 3);

        let counters = ctx.watcher_counters();
        counters.note_raw_event();
        counters.note_raw_event();
        tx.send(WatcherDispatchEvent::RescanRequired(
            RescanReason::KernelDropped,
        ))
        .unwrap();
        let (second, lines) = crate::logging::capture_index_events(|| {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let result = drain_watcher_events_bounded(&ctx, 2);
                if !result.has_more {
                    break result;
                }
                assert!(Instant::now() < deadline, "async rescan did not converge");
                std::thread::yield_now();
            }
        });

        assert_eq!(second.processed, 0);
        assert!(!second.has_more);
        assert_eq!(ctx.watcher_drain_pending_path_count(), 0);
        let snapshot = counters.snapshot();
        assert_eq!(snapshot.raw_events_total, 2);
        assert_eq!(snapshot.raw_events_since_last_rescan, 0);
        assert_eq!(snapshot.rescans_kernel_dropped_total, 1);
        assert_eq!(snapshot.rescans_user_dropped_total, 0);
        assert_eq!(snapshot.rescans_unknown_total, 0);
        assert!(snapshot.last_rescan_at_ms.is_some());
        assert!(snapshot.last_rescan_cost_ms.is_some());
        // Other tests on this thread pool emit index_event lines too; the
        // capture is process-wide, so select this test's line by content —
        // kind and this test's own root (matched by the temp dir's unique
        // name, since the line may spell the root canonically or not).
        let root_name = temp
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let rescan_lines = lines
            .iter()
            .filter(|line| {
                line.contains("kind=watcher_rescan plane=watcher") && line.contains(&root_name)
            })
            .collect::<Vec<_>>();
        assert_eq!(rescan_lines.len(), 1, "lines: {lines:?}");
        assert!(rescan_lines[0].contains("reason=kernel_dropped"));
        assert!(rescan_lines[0].contains("raw_events_since_last=2"));
    }

    #[test]
    fn watcher_lifecycle_generation_change_rebases_continuation() {
        let _clock = WatcherDrainTestClock::freeze();
        let temp = tempfile::tempdir().unwrap();
        let (ctx, tx) = context_with_watcher(temp.path());
        tx.send(WatcherDispatchEvent::Paths(
            (0..5)
                .map(|index| temp.path().join(format!("old-generation-{index}.txt")))
                .collect(),
        ))
        .unwrap();
        let first = {
            let _held_lsp = ctx.lsp();
            let first = drain_watcher_events_bounded(&ctx, 2);
            assert!(ctx.watcher_lsp_diagnostics_retry_pending_for_test());
            first
        };
        assert_eq!(first.processed, 2);
        assert!(first.has_more);

        // A lifecycle-only generation change (transient unbind + equivalent
        // rebind) must NOT lose the in-flight paths: the continuation rebases
        // onto the new generation and keeps draining.
        ctx.advance_configure_generation();
        let completed = finish_watcher_drain_for_test(&ctx, 2, first);
        assert_eq!(completed.processed, 5);
        assert_eq!(ctx.watcher_drain_pending_path_count(), 0);
        assert_eq!(
            ctx.pending_tier2_paths().len(),
            5,
            "every path survives the lifecycle-only generation change"
        );
    }

    #[test]
    fn ram_overlay_is_on_by_default_and_transport_independent() {
        let temp = tempfile::tempdir().unwrap();
        let (ctx, _) = context_with_watcher(temp.path());
        ctx.set_cache_role(true, None);
        assert!(
            ctx.shared_artifacts_read_only(),
            "worktree role is borrow-only"
        );
        // A borrowed snapshot describes another checkout, so every borrow-only
        // root reconciles and overlays it unless the escape hatch turns it off.
        assert!(
            ctx.ram_overlay_active(),
            "worktree.ram_overlay defaults on for borrow-only roots"
        );

        ctx.update_config(|config| config.worktree.ram_overlay = false);
        assert!(
            !ctx.ram_overlay_active(),
            "overlay follows config + borrow-only, not bind/transport identity"
        );

        ctx.update_config(|config| config.worktree.ram_overlay = true);
        ctx.set_cache_role(false, None);
        assert!(
            !ctx.ram_overlay_active(),
            "owner roots stay on the normal writer path even when the gate is on"
        );
    }
}

// Every test here spawns a real child process, so the module is Unix-only.
#[cfg(all(test, unix))]
mod idle_lsp_tests {
    use super::{drain_watcher_events_bounded, shutdown_idle_lsp_at};
    use crate::config::Config;
    use crate::context::AppContext;
    use crate::lsp::child_registry::LspChildRegistry;
    use crate::lsp::client::LspClient;
    use crate::lsp::registry::ServerKind;
    use std::collections::HashMap;
    use std::path::Path;
    use std::time::{Duration, Instant};

    #[test]
    fn idle_lsp_standalone_reaps_unused_package_while_requests_keep_root_busy() {
        let fixture = crate::lsp::manager::regression_tests::IdleLspFixture::new();
        let busy_pid = fixture.pid(&fixture.busy).unwrap();
        for _ in 0..3 {
            fixture.hover(&fixture.busy);
            super::shutdown_idle_lsp(&fixture.ctx);
        }
        fixture.assert_only_busy();
        assert_eq!(fixture.pid(&fixture.busy), Some(busy_pid));
    }

    #[cfg(unix)]
    fn spawn_sleep_client(ctx: &AppContext, root: std::path::PathBuf) -> u32 {
        let registry = LspChildRegistry::new();
        ctx.lsp().set_child_registry(registry.clone());
        let client = LspClient::spawn_with_reclaim_root(
            ServerKind::TypeScript,
            root,
            Path::new("sh"),
            &["-c".to_string(), "exec sleep 60".to_string()],
            &HashMap::new(),
            ctx.lsp().event_sender_for_test(),
            registry,
            None,
            false,
        )
        .expect("spawn idle-lsp stand-in");
        let pid = client.child_pid();
        ctx.lsp().insert_client_for_test(client);
        pid
    }

    #[cfg(unix)]
    fn wait_until_dead(pid: u32, timeout: Duration) -> bool {
        let started = Instant::now();
        while started.elapsed() < timeout {
            if !crate::process_identity::is_process_alive(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    fn deleted_root_context(
        root: &Path,
    ) -> (
        AppContext,
        crossbeam_channel::Sender<crate::watcher_filter::WatcherDispatchEvent>,
    ) {
        std::fs::create_dir(root).unwrap();
        let mut config = Config::default();
        config.project_root = Some(root.to_path_buf());
        let ctx = AppContext::new(crate::context::default_language_provider_factory(), config);
        ctx.set_canonical_cache_root(root.to_path_buf());
        let (tx, rx) = crossbeam_channel::unbounded();
        *ctx.watcher_rx().lock() = Some(rx);
        (ctx, tx)
    }

    #[test]
    fn deleted_root_drain_shuts_down_running_lsp() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("project");
        let (ctx, tx) = deleted_root_context(&root);
        let pid = spawn_sleep_client(&ctx, root.clone());
        assert!(crate::process_identity::is_process_alive(pid));

        std::fs::remove_dir_all(&root).unwrap();
        tx.send(crate::watcher_filter::WatcherDispatchEvent::RootDeleted)
            .unwrap();
        let (_, logs) = crate::logging::capture_log_lines(|| {
            drain_watcher_events_bounded(&ctx, 1);
            drain_watcher_events_bounded(&ctx, 1);
        });

        assert_eq!(
            ctx.lsp().server_count(),
            0,
            "deleted root must release LSP clients"
        );
        assert!(
            wait_until_dead(pid, Duration::from_secs(6)),
            "deleted-root LSP child must exit"
        );
        assert!(ctx.watcher_rx().lock().is_none());
        let cleanup_logs = logs
            .iter()
            .filter(|line| line.contains("project root deleted"))
            .collect::<Vec<_>>();
        assert_eq!(
            cleanup_logs.len(),
            1,
            "cleanup must log only once: {logs:?}"
        );
        assert!(cleanup_logs[0].contains(&root.display().to_string()));
        assert!(cleanup_logs[0].contains("shut down 1 language server(s)"));
    }

    #[test]
    fn deleted_root_drain_retries_busy_lsp_without_watcher() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("project");
        let (ctx, _tx) = deleted_root_context(&root);
        let pid = spawn_sleep_client(&ctx, root.clone());
        std::fs::remove_dir_all(&root).unwrap();

        let held = ctx.lsp();
        let started = Instant::now();
        drain_watcher_events_bounded(&ctx, 1);
        assert!(started.elapsed() < Duration::from_millis(250));
        assert_eq!(held.server_count(), 1);
        assert!(
            ctx.watcher_rx().lock().is_none(),
            "the retry must not depend on a watcher"
        );
        assert!(
            ctx.watcher_drain_has_work(),
            "subc maintenance must schedule the retry"
        );
        drop(held);

        drain_watcher_events_bounded(&ctx, 1);
        assert_eq!(
            ctx.lsp().server_count(),
            0,
            "later drain must retry the busy manager"
        );
        assert!(!ctx.watcher_drain_has_work());
        assert!(wait_until_dead(pid, Duration::from_secs(6)));
    }

    #[test]
    fn deleted_root_drain_preserves_unbind_admission() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("project");
        let (ctx, _tx) = deleted_root_context(&root);
        let pid = spawn_sleep_client(&ctx, root.clone());
        ctx.mark_subc_unbound();
        std::fs::remove_dir_all(&root).unwrap();

        drain_watcher_events_bounded(&ctx, 1);
        assert_eq!(ctx.lsp().server_count(), 1);
        assert!(ctx.watcher_rx().lock().is_some());
        assert!(crate::process_identity::is_process_alive(pid));

        ctx.mark_subc_bound();
        drain_watcher_events_bounded(&ctx, 1);
        assert_eq!(ctx.lsp().server_count(), 0);
        assert!(wait_until_dead(pid, Duration::from_secs(6)));
    }

    #[test]
    fn deleted_root_inconclusive_check_preserves_runtime_until_root_is_present() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("project");
        let (ctx, _tx) = deleted_root_context(&root);
        let pid = spawn_sleep_client(&ctx, root.clone());
        let generation = ctx.configure_generation();

        let (_, logs) = crate::logging::capture_log_lines(|| {
            for _ in 0..2 {
                assert!(ctx.try_cleanup_deleted_root_with_probe(generation, |_| {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "injected root metadata failure",
                    ))
                }));
            }
        });

        assert_eq!(ctx.lsp().server_count(), 1);
        assert!(ctx.watcher_rx().lock().is_some());
        assert!(crate::process_identity::is_process_alive(pid));
        assert!(ctx.watcher_drain_has_work());
        let inconclusive_logs = logs
            .iter()
            .filter(|line| line.contains("project root check inconclusive"))
            .collect::<Vec<_>>();
        assert_eq!(inconclusive_logs.len(), 1, "{logs:?}");

        let outcome = drain_watcher_events_bounded(&ctx, 1);
        assert!(!outcome.has_more);
        assert!(ctx.watcher_rx().lock().is_some());
        assert_eq!(ctx.lsp().server_count(), 1);
        assert!(crate::process_identity::is_process_alive(pid));
    }

    #[test]
    fn deleted_root_cleanup_defers_contended_watcher_receiver() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("project");
        let (ctx, _tx) = deleted_root_context(&root);
        let pid = spawn_sleep_client(&ctx, root.clone());
        std::fs::remove_dir_all(&root).unwrap();
        let held = ctx.watcher_rx().lock();
        let started = Instant::now();
        assert!(ctx.try_cleanup_deleted_root(ctx.configure_generation()));
        assert!(started.elapsed() < Duration::from_millis(250));
        assert!(held.is_some());
        assert!(ctx.watcher_drain_has_work());
        assert_eq!(ctx.lsp().server_count(), 1);
        drop(held);

        drain_watcher_events_bounded(&ctx, 1);
        assert_eq!(ctx.lsp().server_count(), 0);
        assert!(wait_until_dead(pid, Duration::from_secs(6)));
    }

    #[test]
    fn deleted_root_cleanup_rejects_stale_generation_after_recreation() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("project");
        let (ctx, _tx) = deleted_root_context(&root);
        let old_generation = ctx.configure_generation();
        let old_pid = spawn_sleep_client(&ctx, root.clone());
        std::fs::remove_dir_all(&root).unwrap();
        drain_watcher_events_bounded(&ctx, 1);
        assert!(wait_until_dead(old_pid, Duration::from_secs(6)));

        std::fs::create_dir(&root).unwrap();
        ctx.advance_configure_generation();
        let pid = spawn_sleep_client(&ctx, root.clone());
        assert!(!ctx.try_cleanup_deleted_root(ctx.configure_generation()));
        let (tx, rx) = crossbeam_channel::unbounded();
        *ctx.watcher_rx().lock() = Some(rx);
        tx.send(crate::watcher_filter::WatcherDispatchEvent::RootDeleted)
            .unwrap();
        drain_watcher_events_bounded(&ctx, 1);
        assert_eq!(
            ctx.lsp().server_count(),
            1,
            "an old deletion event must not reap a recreated root"
        );
        std::fs::remove_dir_all(&root).unwrap();
        assert!(!ctx.try_cleanup_deleted_root(old_generation));
        assert_eq!(
            ctx.lsp().server_count(),
            1,
            "stale cleanup must not touch the replacement"
        );
        assert!(crate::process_identity::is_process_alive(pid));

        assert!(ctx.try_cleanup_deleted_root(ctx.configure_generation()));
        assert!(wait_until_dead(pid, Duration::from_secs(6)));
    }

    #[test]
    fn deleted_root_drain_rejects_initializing_lsp_with_no_published_clients() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("project");
        let (ctx, _tx) = deleted_root_context(&root);
        let ctx = std::sync::Arc::new(ctx);
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname='deleted'\nversion='0.1.0'\n",
        )
        .unwrap();
        let file = root.join("lib.rs");
        std::fs::write(&file, "pub fn f() {}\n").unwrap();
        let signal = tmp.path().join("initializing");
        ctx.lsp().override_binary(
            ServerKind::Rust,
            crate::fake_lsp_test_helper::fake_server_binary(),
        );
        ctx.lsp()
            .set_extra_env("AFT_FAKE_LSP_INIT_DELAY_MS", "1500");
        ctx.lsp()
            .set_extra_env("AFT_FAKE_LSP_INIT_DELAY_SIGNAL", signal.to_str().unwrap());
        let worker_ctx = std::sync::Arc::clone(&ctx);
        let worker = std::thread::spawn(move || {
            crate::lsp::manager::start_servers_for_file_unlocked(
                || worker_ctx.lsp(),
                &file,
                &worker_ctx.config(),
            );
        });
        let deadline = Instant::now() + Duration::from_secs(30);
        while !signal.exists() {
            assert!(
                Instant::now() < deadline,
                "fake LSP never received initialize"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !worker.is_finished(),
            "initialize delay must still hold the server"
        );
        assert_eq!(ctx.lsp().server_count(), 0);
        let pids = ctx.lsp_child_registry().pids();
        assert_eq!(pids.len(), 1);

        std::fs::remove_dir_all(&root).unwrap();
        drain_watcher_events_bounded(&ctx, 1);
        worker.join().unwrap();
        assert_eq!(
            ctx.lsp().server_count(),
            0,
            "initializing server must not publish after deletion"
        );
        assert!(wait_until_dead(pids[0], Duration::from_secs(6)));
    }

    #[cfg(unix)]
    #[test]
    fn idle_lsp_ttl_shuts_down_stale_client_and_keeps_recent() {
        let tmp = tempfile::tempdir().unwrap();
        let mut stale_config = Config::default();
        stale_config.lsp_idle_minutes = crate::config::LspIdleMinutes::Minutes(5);
        let stale = AppContext::new(
            crate::context::default_language_provider_factory(),
            stale_config,
        );
        let pid = spawn_sleep_client(&stale, tmp.path().to_path_buf());
        assert_eq!(stale.lsp().server_count(), 1);
        let now = Instant::now();
        stale.set_last_request_at_for_test(now - Duration::from_secs(301));
        let started = Instant::now();
        shutdown_idle_lsp_at(&stale, now, stale.last_request_at());
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "idle lsp reap must return without waiting on Shutdown, took {:?}",
            started.elapsed()
        );
        assert_eq!(
            stale.lsp().server_count(),
            0,
            "a root idle longer than lsp.idle_minutes must shut down its language servers"
        );
        assert!(
            wait_until_dead(pid, Duration::from_secs(6)),
            "idle-reaped child must die within SHUTDOWN_TIMEOUT + 1s"
        );

        let mut fresh_config = Config::default();
        fresh_config.lsp_idle_minutes = crate::config::LspIdleMinutes::Minutes(5);
        let fresh = AppContext::new(
            crate::context::default_language_provider_factory(),
            fresh_config,
        );
        spawn_sleep_client(&fresh, tmp.path().to_path_buf());
        assert_eq!(fresh.lsp().server_count(), 1);
        shutdown_idle_lsp_at(&fresh, Instant::now(), fresh.last_request_at());
        assert_eq!(
            fresh.lsp().server_count(),
            1,
            "a root with recent activity must keep its language servers"
        );
    }

    #[cfg(unix)]
    #[test]
    fn lsp_idle_never_keeps_server_past_any_idle_window() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.lsp_idle_minutes = crate::config::LspIdleMinutes::Never;
        let ctx = AppContext::new(crate::context::default_language_provider_factory(), config);
        let pid = spawn_sleep_client(&ctx, tmp.path().to_path_buf());
        let last_activity = Instant::now();
        shutdown_idle_lsp_at(
            &ctx,
            last_activity + Duration::from_secs(60 * 60 * 24 * 365),
            last_activity,
        );
        assert_eq!(ctx.lsp().server_count(), 1);
        assert!(crate::process_identity::is_process_alive(pid));
        ctx.lsp().shutdown_all();
    }
}
