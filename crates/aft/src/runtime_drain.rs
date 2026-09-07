use crate as aft;
use crate::callgraph_store::{invalidates_workspace_crate_prefix_cache, CallGraphStore};
use crate::context::{
    AppContext, CallGraphStoreBuildEvent, SemanticIndexEvent, SemanticIndexStatus,
    SemanticRefreshEvent, SemanticRefreshRequest, WatcherDrainApplyPhase, WatcherDrainPhase,
    WatcherDrainSliceState,
};
use crate::log_ctx;
use crate::lsp::client::LspEvent;
use crate::protocol::PushFrame;
use crate::watcher_filter::{watcher_path_is_infra_skip, WatcherDispatchEvent};
use std::collections::{BTreeSet, HashSet, VecDeque};
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
const WATCHER_DRAIN_UNIT_WARN_AFTER: Duration = Duration::from_secs(5);
const WATCHER_DRAIN_UNIT_FINAL_AFTER: Duration = Duration::from_secs(30);
pub const LSP_EVENT_DRAIN_BATCH_CAP: usize = 256;

#[cfg(test)]
struct ArtifactDrainCommitGate {
    context_id: usize,
    reached_tx: crossbeam_channel::Sender<()>,
    release_rx: crossbeam_channel::Receiver<()>,
}

#[cfg(test)]
static ARTIFACT_DRAIN_COMMIT_GATE: OnceLock<Mutex<Option<ArtifactDrainCommitGate>>> =
    OnceLock::new();
#[cfg(test)]
static ARTIFACT_DRAIN_TEST_MUTEX: Mutex<()> = Mutex::new(());

#[cfg(test)]
struct SemanticRefreshRecoveryGate {
    context_id: usize,
    reached_tx: crossbeam_channel::Sender<()>,
    release_rx: crossbeam_channel::Receiver<()>,
}

#[cfg(test)]
static SEMANTIC_REFRESH_RECOVERY_GATE: OnceLock<Mutex<Option<SemanticRefreshRecoveryGate>>> =
    OnceLock::new();

#[cfg(test)]
struct WatcherPhaseCommitGate {
    target: PathBuf,
    reached_tx: crossbeam_channel::Sender<()>,
    release_rx: crossbeam_channel::Receiver<()>,
}

#[cfg(test)]
static WATCHER_PHASE_COMMIT_GATE: std::sync::OnceLock<Mutex<Option<WatcherPhaseCommitGate>>> =
    std::sync::OnceLock::new();

#[cfg(test)]
fn install_watcher_phase_commit_gate_for_test(
    target: PathBuf,
) -> (
    crossbeam_channel::Receiver<()>,
    crossbeam_channel::Sender<()>,
) {
    let (reached_tx, reached_rx) = crossbeam_channel::bounded(1);
    let (release_tx, release_rx) = crossbeam_channel::bounded(1);
    *WATCHER_PHASE_COMMIT_GATE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("watcher phase commit gate mutex poisoned") = Some(WatcherPhaseCommitGate {
        target,
        reached_tx,
        release_rx,
    });
    (reached_rx, release_tx)
}

#[cfg(test)]
fn wait_on_watcher_phase_commit_gate_for_test(path: &Path) {
    let mut slot = WATCHER_PHASE_COMMIT_GATE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("watcher phase commit gate mutex poisoned");
    if !slot.as_ref().is_some_and(|gate| gate.target == path) {
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
fn wait_on_watcher_phase_commit_gate_for_test(_path: &Path) {}

#[cfg(test)]
fn install_artifact_drain_commit_gate_for_test(
    ctx: &AppContext,
) -> (
    crossbeam_channel::Receiver<()>,
    crossbeam_channel::Sender<()>,
) {
    let (reached_tx, reached_rx) = crossbeam_channel::bounded(1);
    let (release_tx, release_rx) = crossbeam_channel::bounded(1);
    *ARTIFACT_DRAIN_COMMIT_GATE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("artifact drain commit gate mutex poisoned") = Some(ArtifactDrainCommitGate {
        context_id: ctx as *const AppContext as usize,
        reached_tx,
        release_rx,
    });
    (reached_rx, release_tx)
}

#[cfg(test)]
fn wait_on_artifact_drain_commit_gate_for_test(ctx: &AppContext) {
    let mut slot = ARTIFACT_DRAIN_COMMIT_GATE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("artifact drain commit gate mutex poisoned");
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
fn wait_on_artifact_drain_commit_gate_for_test(_ctx: &AppContext) {}

#[cfg(test)]
fn install_semantic_refresh_recovery_gate_for_test(
    ctx: &AppContext,
) -> (
    crossbeam_channel::Receiver<()>,
    crossbeam_channel::Sender<()>,
) {
    let (reached_tx, reached_rx) = crossbeam_channel::bounded(1);
    let (release_tx, release_rx) = crossbeam_channel::bounded(1);
    *SEMANTIC_REFRESH_RECOVERY_GATE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("semantic refresh recovery gate mutex poisoned") =
        Some(SemanticRefreshRecoveryGate {
            context_id: ctx as *const AppContext as usize,
            reached_tx,
            release_rx,
        });
    (reached_rx, release_tx)
}

#[cfg(test)]
fn wait_on_semantic_refresh_recovery_gate_for_test(ctx: &AppContext) {
    let mut slot = SEMANTIC_REFRESH_RECOVERY_GATE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("semantic refresh recovery gate mutex poisoned");
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
fn wait_on_semantic_refresh_recovery_gate_for_test(_ctx: &AppContext) {}

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

    fn start_batch(phase: WatcherDrainApplyPhase, path: &'a Path, batch_len: usize) -> Self {
        Self {
            phase: watcher_drain_phase_name(phase),
            path,
            batch_len,
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
        thread::sleep(delay);
    }
}

#[cfg(not(test))]
fn delay_watcher_unit_for_test() {}

pub fn drain_deferred_configure_maintenance(ctx: &AppContext) {
    crate::commands::configure::drain_deferred_configure_maintenance(ctx);
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
            inner: crate::commands::configure::ConfigureMaintenanceState::standalone(),
        }
    }
}

impl StandaloneConfigureMaintenance {
    pub fn has_pending(&mut self, ctx: &AppContext) -> bool {
        crate::commands::configure::standalone_configure_maintenance_pending(ctx, &mut self.inner)
    }

    pub fn drain_prefix(&mut self, ctx: &AppContext) -> bool {
        crate::commands::configure::drain_standalone_configure_prefix(ctx, &mut self.inner)
    }

    pub fn drain_one(&mut self, ctx: &AppContext) -> bool {
        crate::commands::configure::drain_deferred_configure_maintenance_unit(ctx, &mut self.inner)
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
    let Some((drained, reuse_completed)) = ctx.run_if_subc_bound_generation(generation, || {
        let drained = ctx.inspect_manager().drain_completions();
        // Watcher-driven Tier-2 scans complete via the reuse path, which bypasses
        // `result_rx`/`drain_completions`. Poll the manager's reuse counter so a
        // background scan still refreshes the bar (#3), otherwise the counts and
        // `~` marker would only update on a manual `aft_inspect`.
        (drained, ctx.take_new_reuse_completions())
    }) else {
        return;
    };
    // A completed background Tier-2 scan refreshes the agent status-bar counts
    // to the freshly-persisted aggregate, and clears the stale marker, so the
    // bar reflects the new numbers on the next tool result without waiting for
    // an explicit aft_inspect call.
    if drained > 0 || reuse_completed {
        if let Some(project_root) = ctx.config().project_root.clone() {
            let inspect_dir = ctx.inspect_dir();
            let (dead_code, unused_exports, duplicates) = ctx
                .inspect_manager()
                .latest_tier2_counts(inspect_dir.clone(), project_root.clone());
            // Don't clear the `~` stale marker until the whole serial Tier-2
            // cycle has drained. While any category is still in flight the
            // already-persisted categories may predate the latest edit, so
            // claiming fresh would be premature. `None` counts preserve the
            // last-known value rather than fabricating a `0`.
            let stale = ctx.inspect_manager().tier2_any_in_flight();
            ctx.update_status_bar_tier2(dead_code, unused_exports, duplicates, None, stale);
            // Health must distinguish "tier2 still building" from "tier2 complete
            // except dead_code, which is blocked on the callgraph store". Refresh
            // the flag from the same latest aggregate the counts came from so the
            // two never disagree.
            let blocked = ctx
                .inspect_manager()
                .dead_code_blocked_on_callgraph(inspect_dir, project_root);
            ctx.set_status_bar_tier2_dead_code_blocked_on_callgraph(blocked);
            ctx.status_emitter().signal(ctx.build_status_snapshot());
        }
    }
}

/// Drain all background build-completion receivers in standalone order.
///
/// Search installs first so watcher/pending updates apply to the freshest index,
/// followed by callgraph store and semantic index completion.
pub fn drain_build_completions(ctx: &AppContext) {
    drain_search_index_events(ctx);
    drain_callgraph_store_events(ctx);
    drain_semantic_index_events(ctx);
}

/// Return true when any background build-completion receiver is currently set.
///
/// Each receiver is checked under its own short lock; no lock is held while
/// checking the next subsystem.
pub fn any_build_in_flight(ctx: &AppContext) -> bool {
    {
        let rx = ctx
            .search_index_rx()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if rx.is_some() {
            return true;
        }
    }

    {
        let rx = ctx.callgraph_store_rx().lock();
        if rx.is_some() {
            return true;
        }
    }

    {
        let rx = ctx.semantic_index_rx().lock();
        rx.is_some()
    }
}

pub fn watcher_path_is_ignored_by_current_matcher(ctx: &AppContext, path: &Path) -> bool {
    if watcher_path_is_infra_skip(path) {
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

fn replay_search_index_pending_updates(
    ctx: &AppContext,
    index: &mut crate::search_index::SearchIndex,
    pending_paths: Vec<std::path::PathBuf>,
) {
    for path in pending_paths {
        if path.exists() {
            if watcher_path_is_ignored_by_current_matcher(ctx, &path) {
                index.remove_file(&path);
            } else {
                index.update_file(&path);
            }
        } else {
            index.remove_file(&path);
        }
    }
}

pub fn watcher_path_is_semantic_source(path: &Path) -> bool {
    crate::semantic_index::is_semantic_indexed_extension(path)
}

pub fn mark_semantic_corpus_refresh_success(ctx: &AppContext) {
    ctx.clear_all_semantic_refresh_retry_attempts();
    ctx.reset_semantic_refresh_circuit_after_success();
}

pub fn drain_search_index_events(ctx: &AppContext) {
    let (latest, disconnected, receiver_generation, receiver_epoch) = {
        let rx_ref = ctx
            .search_index_rx()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(rx) = rx_ref.as_ref() else {
            return;
        };

        let mut latest = None;
        let mut disconnected = false;
        loop {
            match rx.try_recv() {
                Ok(index) => latest = Some(index),
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        (
            latest,
            disconnected,
            ctx.search_index_rx_generation(),
            ctx.search_index_rx_epoch(),
        )
    };

    let mut installed_index = false;
    if let Some(mut index) = latest {
        wait_on_artifact_drain_commit_gate_for_test(ctx);
        installed_index = ctx
            .with_current_search_index_rx(receiver_generation, receiver_epoch, |receiver| {
                let pending_paths = ctx.take_pending_search_index_paths();
                if !pending_paths.is_empty() {
                    replay_search_index_pending_updates(ctx, &mut index, pending_paths);
                }
                *ctx.search_index()
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);
                *receiver = None;
                true
            })
            .unwrap_or(false);
        if !installed_index {
            return;
        }
        ctx.note_search_index_load_succeeded();
    } else if disconnected {
        let cleared = ctx
            .with_current_search_index_rx(receiver_generation, receiver_epoch, |receiver| {
                *receiver = None;
                let mut search_index = ctx
                    .search_index()
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                // A build-denied index is a terminal settled state (write access
                // refused), not an in-progress build, so keep it: clearing it
                // would flip health back to "building" for a root that can never
                // produce a real index here.
                if search_index
                    .as_ref()
                    .is_some_and(|index| !index.ready && !index.build_denied)
                {
                    *search_index = None;
                }
                true
            })
            .unwrap_or(false);
        if !cleared {
            return;
        }
        // The worker dropped its sender without delivering an index (a lost
        // load). Left alone, the root now has no index and no receiver, so health
        // would report "building" forever — nothing reschedules the load until a
        // later search query happens to run. Schedule one automatic replacement
        // load (capped per configure generation) so the root recovers on its own.
        crate::commands::configure::restart_search_index_after_load_disconnect(ctx);
    }

    if installed_index || disconnected {
        ctx.status_emitter().signal(ctx.build_status_snapshot());
    }
}

pub fn drain_callgraph_store_events(ctx: &AppContext) {
    let (
        latest,
        denied,
        suspended,
        settled,
        disconnected,
        fulfilled_force_token,
        receiver_generation,
        receiver_epoch,
    ) = {
        let rx_ref = ctx.callgraph_store_rx().lock();
        let Some(rx) = rx_ref.as_ref() else {
            return;
        };

        let mut latest = None;
        let mut denied = None;
        let mut suspended = None;
        let mut settled = false;
        let mut fulfilled_force_token = None;
        let mut disconnected = false;
        loop {
            match rx.try_recv() {
                Ok(CallGraphStoreBuildEvent::Ready {
                    store,
                    fulfilled_force_token: token,
                    publication_epoch,
                }) => {
                    if ctx.callgraph_persist_epoch_flag().current() == publication_epoch {
                        latest = Some(store);
                        fulfilled_force_token = token;
                    } else {
                        // A newer configure advanced the persist epoch after this
                        // build published; its generation is already superseded on
                        // disk, so treat the event as settled instead of installing
                        // the stale handle in RAM.
                        drop(store);
                        settled = true;
                    }
                }
                Ok(CallGraphStoreBuildEvent::Denied { reason }) => denied = Some(reason),
                Ok(CallGraphStoreBuildEvent::Suspended { suspension }) => {
                    suspended = Some(suspension)
                }
                Ok(CallGraphStoreBuildEvent::Settled) => settled = true,
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        (
            latest,
            denied,
            suspended,
            settled,
            disconnected,
            fulfilled_force_token,
            ctx.callgraph_store_rx_generation(),
            ctx.callgraph_store_rx_epoch(),
        )
    };

    let ready_received = latest.is_some();
    let terminal =
        ready_received || denied.is_some() || suspended.is_some() || settled || disconnected;
    if !terminal {
        return;
    }
    if let Some(project_root) = ctx.callgraph_project_root() {
        crate::logging::release_index_build_start_waiters(
            crate::logging::IndexPlane::Callgraph,
            &project_root,
        );
    }
    wait_on_artifact_drain_commit_gate_for_test(ctx);

    let mut reopened = None;
    if let Some(store) = latest {
        // Release the cold-build writer lease before opening the published
        // generation through its read-only pointer.
        drop(store);
        if let Some(project_root) = ctx.callgraph_project_root() {
            match CallGraphStore::open_readonly(ctx.callgraph_store_dir(), project_root) {
                Ok(Some(store)) => reopened = Some(Arc::new(store)),
                Ok(None) => {
                    crate::slog_warn!(
                        "callgraph store build completed without a readable published generation"
                    );
                }
                Err(error) => {
                    crate::slog_warn!("failed to install read-only callgraph store: {}", error);
                }
            }
        }
    }

    let mut pending = Vec::new();
    let installed =
        ctx.with_current_callgraph_store_rx(receiver_generation, receiver_epoch, |receiver| {
            let installed = if let Some(store) = reopened {
                ctx.clear_callgraph_store_build_denied();
                *ctx.callgraph_store()
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(store);
                // This take and the refresh worker's post-defer re-check form a
                // check-then-act handoff: one site sees parked paths with a
                // ready current store, so neither site needs to poll alone.
                pending = ctx
                    .take_pending_callgraph_store_paths()
                    .into_iter()
                    .filter(|path| {
                        invalidates_workspace_crate_prefix_cache(path)
                            || !watcher_path_is_generated_for_callgraph(ctx, path)
                    })
                    .collect();
                true
            } else {
                false
            };
            if let Some(reason) = denied {
                ctx.record_callgraph_store_build_denied(receiver_generation, reason);
            }
            if let Some(suspension) = suspended {
                ctx.record_callgraph_store_build_suspension(receiver_generation, suspension);
            }
            if terminal {
                *receiver = None;
            }
            if installed {
                if let Some(force_token) = fulfilled_force_token {
                    ctx.fulfill_callgraph_store_force_token(force_token);
                }
            }
            installed
        });
    let Some(installed) = installed else {
        return;
    };

    if installed {
        if !pending.is_empty() {
            let _ = ctx.enqueue_callgraph_store_refresh(pending);
        }
        let _ = ctx.request_tier2_refresh_pull();
    }
    if terminal {
        ctx.status_emitter().signal(ctx.build_status_snapshot());
    }
}

pub fn drain_semantic_index_events(ctx: &AppContext) {
    let (events, disconnected, receiver_generation, receiver_epoch) = {
        let rx_ref = ctx.semantic_index_rx().lock();
        let Some(rx) = rx_ref.as_ref() else {
            return;
        };

        let mut events = Vec::new();
        let mut disconnected = false;
        loop {
            match rx.try_recv() {
                Ok(event) => events.push(event),
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        (
            events,
            disconnected,
            ctx.semantic_index_rx_generation(),
            ctx.semantic_index_rx_epoch(),
        )
    };

    if events.is_empty() && !disconnected {
        return;
    }

    wait_on_artifact_drain_commit_gate_for_test(ctx);
    let mut terminal = false;
    let mut status_changed = false;
    let mut replay_refresh_paths = Vec::new();
    let mut replay_corpus_refresh = false;
    let mut cold_seed_resumes = Vec::new();

    for event in events {
        match event {
            SemanticIndexEvent::Progress {
                stage,
                files,
                entries_done,
                entries_total,
            } => {
                let committed = ctx
                    .with_current_semantic_index_rx(
                        receiver_generation,
                        receiver_epoch,
                        |_receiver| {
                            *ctx.semantic_index_status()
                                .write()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                SemanticIndexStatus::Building {
                                    stage,
                                    files,
                                    entries_done,
                                    entries_total,
                                };
                            true
                        },
                    )
                    .unwrap_or(false);
                if !committed {
                    return;
                }
                status_changed = true;
            }
            SemanticIndexEvent::ColdSeedGateCleared => {
                let resume = ctx.with_current_semantic_index_rx(
                    receiver_generation,
                    receiver_epoch,
                    |_receiver| ctx.take_semantic_cold_seed_resume(true),
                );
                let Some(resume) = resume else {
                    return;
                };
                cold_seed_resumes.push(resume);
            }
            SemanticIndexEvent::Ready(mut index) => {
                let committed = ctx.with_current_semantic_index_rx(
                    receiver_generation,
                    receiver_epoch,
                    |receiver| {
                        mark_semantic_corpus_refresh_success(ctx);
                        let refresh_paths = ctx
                            .take_pending_semantic_index_paths()
                            .into_iter()
                            .filter(|path| watcher_path_is_semantic_source(path))
                            .collect::<Vec<_>>();
                        index.invalidate_files(&refresh_paths);
                        let corpus_refresh = ctx.take_pending_semantic_corpus_refresh();
                        if let Some(root) = ctx.canonical_cache_root_opt() {
                            let semantic_config = ctx.config().semantic.clone();
                            let _ = index.adopt_frozen_base_for_root(&root, &semantic_config);
                        }
                        *ctx.semantic_index()
                            .write()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);
                        *ctx.semantic_index_status()
                            .write()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            SemanticIndexStatus::ready();
                        *receiver = None;
                        (
                            ctx.take_semantic_cold_seed_resume(false),
                            refresh_paths,
                            corpus_refresh,
                        )
                    },
                );
                let Some((resume, refresh_paths, corpus_refresh)) = committed else {
                    return;
                };
                cold_seed_resumes.push(resume);
                replay_refresh_paths.extend(refresh_paths);
                replay_corpus_refresh = corpus_refresh;
                terminal = true;
                status_changed = true;
            }
            SemanticIndexEvent::Failed(error) => {
                let committed = ctx.with_current_semantic_index_rx(
                    receiver_generation,
                    receiver_epoch,
                    |receiver| {
                        let _ = ctx.take_pending_semantic_index_paths();
                        let _ = ctx.take_pending_semantic_corpus_refresh();
                        *ctx.semantic_index()
                            .write()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                        ctx.clear_semantic_refresh_worker();
                        *ctx.semantic_index_status()
                            .write()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            SemanticIndexStatus::Failed(error);
                        *receiver = None;
                        ctx.take_semantic_cold_seed_resume(false)
                    },
                );
                let Some(resume) = committed else {
                    return;
                };
                cold_seed_resumes.push(resume);
                terminal = true;
                status_changed = true;
            }
        }
    }

    if terminal
        && ctx.config().views.enabled
        && matches!(
            &*ctx
                .semantic_index_status()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            SemanticIndexStatus::Ready { .. }
        )
    {
        if let Some(_permit) = ctx.cold_build_limiter().try_acquire() {
            if let Err(error) = ctx.publish_view_paths(BTreeSet::new(), true) {
                aft::slog_warn!("semantic-ready view publication failed: {}", error);
            }
        }
    }

    if disconnected && !terminal {
        let committed =
            ctx.with_current_semantic_index_rx(receiver_generation, receiver_epoch, |receiver| {
                let _ = ctx.take_pending_semantic_index_paths();
                let _ = ctx.take_pending_semantic_corpus_refresh();
                *ctx.semantic_index()
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                ctx.clear_semantic_refresh_worker();
                *ctx.semantic_index_status()
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    SemanticIndexStatus::Failed(
                        "semantic index build worker disconnected before reporting completion"
                            .to_string(),
                    );
                *receiver = None;
                ctx.take_semantic_cold_seed_resume(false)
            });
        let Some(resume) = committed else {
            return;
        };
        cold_seed_resumes.push(resume);
        status_changed = true;
    }

    if terminal || disconnected {
        ctx.set_semantic_build_progress(None);
    }

    for resume in cold_seed_resumes {
        ctx.apply_semantic_cold_seed_resume(resume);
    }

    if replay_corpus_refresh {
        let replayed = ctx.run_if_subc_bound_generation(receiver_generation, || {
            if ctx.semantic_index_rx_epoch() != receiver_epoch
                || ctx.canonical_cache_root_opt().is_none()
            {
                return false;
            }
            *ctx.semantic_index_status()
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                SemanticIndexStatus::Building {
                    stage: "refreshing_corpus".to_string(),
                    files: None,
                    entries_done: None,
                    entries_total: None,
                };
            let sent = ctx
                .semantic_refresh_sender()
                .is_some_and(|sender| sender.send(SemanticRefreshRequest::Corpus).is_ok());
            if !sent {
                *ctx.semantic_index_status()
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    SemanticIndexStatus::Failed(
                        "semantic corpus refresh worker unavailable".to_string(),
                    );
            }
            true
        });
        if replayed != Some(true) {
            return;
        }
        status_changed = true;
    } else if !replay_refresh_paths.is_empty() {
        let replayed = ctx.run_if_subc_bound_generation(receiver_generation, || {
            if ctx.semantic_index_rx_epoch() != receiver_epoch {
                return false;
            }
            {
                let mut status = ctx
                    .semantic_index_status()
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if matches!(&*status, SemanticIndexStatus::Ready { .. }) {
                    for path in &replay_refresh_paths {
                        status.add_refreshing_file(path.clone());
                    }
                }
            }
            let sent = ctx.semantic_refresh_sender().is_some_and(|sender| {
                sender
                    .send(SemanticRefreshRequest::Files {
                        paths: replay_refresh_paths.clone(),
                    })
                    .is_ok()
            });
            if !sent {
                crate::slog_warn!(
                    "semantic refresh worker unavailable; dropping {} replayed file(s)",
                    replay_refresh_paths.len()
                );
                let mut status = ctx
                    .semantic_index_status()
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                for path in &replay_refresh_paths {
                    status.cancel_refreshing_file(path);
                }
            }
            true
        });
        if replayed != Some(true) {
            return;
        }
        status_changed = true;
    }

    if status_changed {
        ctx.status_emitter().signal(ctx.build_status_snapshot());
    }
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

fn clear_semantic_refresh_retry_attempts(ctx: &AppContext, paths: &[std::path::PathBuf]) {
    ctx.clear_semantic_refresh_retry_attempts(paths);
}

fn clear_completed_pending_semantic_index_paths(
    ctx: &AppContext,
    completed_paths: &[std::path::PathBuf],
) {
    if completed_paths.is_empty() {
        return;
    }

    let completed = completed_paths.iter().cloned().collect::<HashSet<_>>();
    let remaining = ctx
        .take_pending_semantic_index_paths()
        .into_iter()
        .filter(|path| !completed.contains(path))
        .collect::<Vec<_>>();
    if !remaining.is_empty() {
        ctx.add_pending_semantic_index_paths(remaining);
    }
}

fn semantic_refresh_probe_delay() -> Duration {
    semantic_refresh_retry_backoff(usize::MAX)
}

pub fn semantic_refresh_circuit_is_open(ctx: &AppContext) -> bool {
    ctx.semantic_refresh_circuit_is_open()
}

pub fn record_semantic_refresh_transient_failure(ctx: &AppContext) -> bool {
    ctx.record_semantic_refresh_transient_failure(BREAKER_TRIP_THRESHOLD)
}

fn reset_semantic_refresh_transient_failure_count(ctx: &AppContext) {
    ctx.reset_semantic_refresh_transient_failure_count();
}

fn reset_semantic_refresh_circuit_after_success(ctx: &AppContext) {
    ctx.reset_semantic_refresh_circuit_after_success();
}

fn mark_semantic_refresh_success(ctx: &AppContext, completed_paths: &[std::path::PathBuf]) {
    clear_semantic_refresh_retry_attempts(ctx, completed_paths);
    clear_completed_pending_semantic_index_paths(ctx, completed_paths);
    reset_semantic_refresh_circuit_after_success(ctx);
}

#[doc(hidden)]
pub fn semantic_refresh_transient_failure_count_for_test(ctx: &AppContext) -> usize {
    ctx.semantic_refresh_transient_failure_count()
}

#[doc(hidden)]
pub fn semantic_refresh_probe_is_scheduled_for_test(ctx: &AppContext) -> bool {
    ctx.semantic_refresh_probe_is_scheduled()
}

fn ensure_semantic_refresh_probe_scheduled(ctx: &AppContext) {
    ctx.ensure_semantic_refresh_probe_scheduled(semantic_refresh_probe_delay());
}

fn maybe_fire_semantic_refresh_probe(ctx: &AppContext) {
    let generation = ctx.semantic_refresh_generation();
    let _ = ctx.run_if_subc_bound_generation(generation, || {
        if !ctx.take_semantic_refresh_probe_ready() {
            return;
        }
        if !semantic_refresh_circuit_is_open(ctx) {
            return;
        }

        if ctx.take_pending_semantic_corpus_refresh() {
            // Stamp the status BEFORE sending: the worker emits CorpusStarted
            // only after walking the project, and an unbind cancellation in
            // that window preserves corpus intent by reading
            // corpus_refresh_in_flight() from the status. A send without the
            // stamp would lose the intent entirely.
            let previous_status = {
                let mut status = ctx
                    .semantic_index_status()
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let previous = status.clone();
                *status = SemanticIndexStatus::Building {
                    stage: "refreshing_corpus".to_string(),
                    files: None,
                    entries_done: None,
                    entries_total: None,
                };
                previous
            };
            let sent = ctx
                .semantic_refresh_sender()
                .is_some_and(|sender| sender.send(SemanticRefreshRequest::Corpus).is_ok());
            if !sent {
                *ctx.semantic_index_status()
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = previous_status;
                ctx.mark_pending_semantic_corpus_refresh();
            }
            return;
        }

        let pending_paths = ctx.take_pending_semantic_index_paths();
        if pending_paths.is_empty() {
            return;
        }

        let sent = ctx.semantic_refresh_sender().is_some_and(|sender| {
            sender
                .send(SemanticRefreshRequest::Files {
                    paths: pending_paths.clone(),
                })
                .is_ok()
        });
        if !sent {
            ctx.add_pending_semantic_index_paths(pending_paths);
        }
    });
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

pub fn drain_semantic_refresh_events(ctx: &AppContext) {
    let (events, disconnected, receiver_generation, receiver_epoch) = {
        let rx_ref = ctx.semantic_refresh_event_rx().lock();
        let Some(rx) = rx_ref.as_ref() else {
            return;
        };

        let mut events = Vec::new();
        let mut disconnected = false;
        loop {
            match rx.try_recv() {
                Ok(event) => events.push(event),
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        (
            events,
            disconnected,
            ctx.semantic_refresh_generation(),
            ctx.semantic_refresh_epoch(),
        )
    };

    if events.is_empty() && !disconnected {
        maybe_fire_semantic_refresh_probe(ctx);
        return;
    }

    wait_on_artifact_drain_commit_gate_for_test(ctx);
    let committed = ctx.with_current_semantic_refresh_rx(
        receiver_generation,
        receiver_epoch,
        || {
        let had_events = !events.is_empty();
        let mut status_changed = false;
        let mut replay_refresh_paths = Vec::new();
        let mut schedule_breaker_probe = false;
        for event in events {
        match event {
            SemanticRefreshEvent::Started { paths } => {
                let mut status = ctx
                    .semantic_index_status()
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if matches!(&*status, SemanticIndexStatus::Ready { .. }) {
                    for path in paths {
                        status.start_refreshing_file(path);
                    }
                    status_changed = true;
                }
            }
            SemanticRefreshEvent::CorpusStarted { files } => {
                *ctx.semantic_index_status()
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    SemanticIndexStatus::Building {
                        stage: "refreshing_corpus".to_string(),
                        files: Some(files),
                        entries_done: None,
                        entries_total: None,
                    };
                status_changed = true;
            }
            SemanticRefreshEvent::Completed {
                added_entries,
                updated_metadata,
                completed_paths,
            } => {
                if let Some(index) = ctx
                    .semantic_index()
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_mut()
                {
                    index.apply_refresh_update(added_entries, updated_metadata, &completed_paths);
                }
                mark_semantic_refresh_success(ctx, &completed_paths);
                let mut status = ctx
                    .semantic_index_status()
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if matches!(&*status, SemanticIndexStatus::Ready { .. }) {
                    for path in &completed_paths {
                        status.complete_refreshing_file(path);
                    }
                    status_changed = true;
                }
            }
            SemanticRefreshEvent::CorpusCompleted {
                mut index,
                changed,
                added,
                deleted,
                total_processed,
            } => {
                aft::runtime_drain::mark_semantic_corpus_refresh_success(ctx);
                if changed > 0 || added > 0 || deleted > 0 {
                    aft::slog_info!(
                        "semantic corpus refresh completed: {} changed, {} new, {} deleted, {} total processed",
                        changed,
                        added,
                        deleted,
                        total_processed
                    );
                }
                let pending_paths = ctx.take_pending_semantic_index_paths();
                let mut invalidated_paths = Vec::new();
                for path in pending_paths {
                    if !aft::runtime_drain::watcher_path_is_semantic_source(&path) {
                        continue;
                    }
                    if !aft::runtime_drain::watcher_path_is_ignored_by_current_matcher(ctx, &path) {
                        replay_refresh_paths.push(path.clone());
                    }
                    invalidated_paths.push(path);
                }
                index.invalidate_files(&invalidated_paths);
                *ctx.semantic_index()
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);
                *ctx.semantic_index_status()
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    SemanticIndexStatus::ready();
                status_changed = true;
            }
            SemanticRefreshEvent::Failed { paths, error } => {
                if aft::semantic_index::embedding_failure_is_transient(&error) {
                    if record_semantic_refresh_transient_failure(ctx) {
                        ctx.add_pending_semantic_index_paths(paths);
                        schedule_breaker_probe = true;
                    } else if !schedule_semantic_refresh_retry(ctx, paths.clone(), &error) {
                        aft::slog_warn!(
                            "semantic refresh worker unavailable; preserving {} transiently failed file(s) for retry",
                            paths.len(),
                        );
                        ctx.add_pending_semantic_index_paths(paths);
                    }
                } else {
                    aft::slog_warn!("semantic refresh failed: {}", error);
                    reset_semantic_refresh_transient_failure_count(ctx);
                    clear_semantic_refresh_retry_attempts(ctx, &paths);
                    let mut status = ctx
                        .semantic_index_status()
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if matches!(&*status, SemanticIndexStatus::Ready { .. }) {
                        for path in &paths {
                            status.complete_refreshing_file(path);
                        }
                        status_changed = true;
                    }
                }
            }
            SemanticRefreshEvent::CorpusFailed { error } => {
                // A transient backend blip during a corpus refresh must NOT
                // destroy the working index — the prior index is still valid and
                // serving. Keep it Ready and let the next watcher/ignore change
                // re-trigger the refresh, rather than nuking everything to
                // `Failed` over a connection hiccup (the same park-forever trap
                // the initial build now rides out). Permanent errors (dimension
                // mismatch, too-many-files) still drop the index and surface the
                // real failure.
                if aft::semantic_index::embedding_failure_is_transient(&error) {
                    let clean = aft::semantic_index::strip_transient_embedding_marker(&error);
                    let has_index = ctx
                        .semantic_index()
                        .read()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .is_some();
                    ctx.mark_pending_semantic_corpus_refresh();
                    ctx.trip_semantic_refresh_circuit(BREAKER_TRIP_THRESHOLD);
                    schedule_breaker_probe = true;
                    if has_index {
                        aft::slog_warn!(
                            "semantic corpus refresh hit a transient backend error ({}); keeping the existing index",
                            clean,
                        );
                        *ctx.semantic_index_status()
                            .write()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            SemanticIndexStatus::ready();
                    } else {
                        // No index to fall back on — surface the clean message.
                        aft::slog_warn!("semantic corpus refresh failed: {}", clean);
                        *ctx.semantic_index_status()
                            .write()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            SemanticIndexStatus::Failed(clean);
                    }
                    status_changed = true;
                } else {
                    aft::slog_warn!("semantic corpus refresh failed: {}", error);
                    let _ = ctx.take_pending_semantic_index_paths();
                    *ctx.semantic_index()
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                    *ctx.semantic_index_status()
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        SemanticIndexStatus::Failed(error);
                    status_changed = true;
                }
            }
            }
        }

        if disconnected {
        let refreshing_paths = {
            let status = ctx
                .semantic_index_status()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match &*status {
                SemanticIndexStatus::Ready { refreshing, .. } => refreshing.clone(),
                _ => Vec::new(),
            }
        };
        if !refreshing_paths.is_empty() {
            let mut status = ctx
                .semantic_index_status()
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for path in &refreshing_paths {
                status.cancel_refreshing_file(path);
            }
        }
        if !refreshing_paths.is_empty() || had_events {
            status_changed = true;
        }
    }

    if !replay_refresh_paths.is_empty() {
        {
            let mut status = ctx
                .semantic_index_status()
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if matches!(&*status, SemanticIndexStatus::Ready { .. }) {
                for path in &replay_refresh_paths {
                    status.add_refreshing_file(path.clone());
                }
                status_changed = true;
            }
        }
        let sent = ctx.semantic_refresh_sender().is_some_and(|sender| {
            sender
                .send(SemanticRefreshRequest::Files {
                    paths: replay_refresh_paths.clone(),
                })
                .is_ok()
        });
        if !sent {
            aft::slog_warn!(
                "semantic refresh worker unavailable; dropping {} replayed corpus file(s)",
                replay_refresh_paths.len()
            );
            let mut status = ctx
                .semantic_index_status()
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for path in &replay_refresh_paths {
                status.cancel_refreshing_file(path);
            }
            status_changed = true;
        }
        }

        (status_changed, schedule_breaker_probe)
    },
    );
    let Some((mut status_changed, schedule_breaker_probe)) = committed else {
        return;
    };
    if schedule_breaker_probe && semantic_refresh_circuit_is_open(ctx) {
        ensure_semantic_refresh_probe_scheduled(ctx);
    }
    if disconnected {
        if let Some(disconnected_build_epoch) =
            ctx.clear_semantic_refresh_worker_if_current(receiver_generation, receiver_epoch)
        {
            wait_on_semantic_refresh_recovery_gate_for_test(ctx);
            let _ = crate::commands::configure::restart_semantic_artifacts_after_refresh_disconnect(
                ctx,
                disconnected_build_epoch,
            );
            status_changed = true;
        }
    }

    maybe_fire_semantic_refresh_probe(ctx);

    if status_changed {
        ctx.status_emitter().signal(ctx.build_status_snapshot());
    }
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

struct SearchRebuildPublishGate {
    reached_tx: crossbeam_channel::Sender<()>,
    release_rx: crossbeam_channel::Receiver<()>,
}

static SEARCH_REBUILD_PUBLISH_GATE: OnceLock<Mutex<Option<SearchRebuildPublishGate>>> =
    OnceLock::new();
static SEARCH_REBUILD_SHUTDOWN_WAIT_SIGNAL: OnceLock<Mutex<Option<crossbeam_channel::Sender<()>>>> =
    OnceLock::new();

#[doc(hidden)]
pub fn install_search_rebuild_publish_gate_for_test() -> (
    crossbeam_channel::Receiver<()>,
    crossbeam_channel::Receiver<()>,
    crossbeam_channel::Sender<()>,
) {
    let (reached_tx, reached_rx) = crossbeam_channel::bounded(1);
    let (shutdown_waiting_tx, shutdown_waiting_rx) = crossbeam_channel::bounded(1);
    let (release_tx, release_rx) = crossbeam_channel::bounded(1);
    *SEARCH_REBUILD_PUBLISH_GATE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("search rebuild publish gate mutex poisoned") = Some(SearchRebuildPublishGate {
        reached_tx,
        release_rx,
    });
    *SEARCH_REBUILD_SHUTDOWN_WAIT_SIGNAL
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("search rebuild shutdown wait signal mutex poisoned") = Some(shutdown_waiting_tx);
    (reached_rx, shutdown_waiting_rx, release_tx)
}

pub(crate) fn note_search_rebuild_shutdown_wait_for_test() {
    let signal = SEARCH_REBUILD_SHUTDOWN_WAIT_SIGNAL
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("search rebuild shutdown wait signal mutex poisoned")
        .take();
    if let Some(signal) = signal {
        let _ = signal.send(());
    }
}

fn wait_on_search_rebuild_publish_gate_for_test() {
    let gate = SEARCH_REBUILD_PUBLISH_GATE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("search rebuild publish gate mutex poisoned")
        .take();
    if let Some(gate) = gate {
        let _ = gate.reached_tx.send(());
        let _ = gate.release_rx.recv_timeout(Duration::from_secs(12));
    }
}

pub fn spawn_search_corpus_refresh(
    ctx: &AppContext,
    root: std::path::PathBuf,
    config: Arc<aft::config::Config>,
) {
    let generation = ctx.configure_generation();
    let _ = ctx.run_if_subc_bound_generation(generation, || {
        spawn_search_corpus_refresh_admitted(ctx, root, config, generation);
    });
}

fn spawn_search_corpus_refresh_admitted(
    ctx: &AppContext,
    root: std::path::PathBuf,
    config: Arc<aft::config::Config>,
    generation: u64,
) {
    {
        let mut search_index = ctx
            .search_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(index) = search_index.as_mut() {
            index.ready = false;
        }
    }

    let (tx, rx): (
        crossbeam_channel::Sender<aft::search_index::SearchIndex>,
        crossbeam_channel::Receiver<aft::search_index::SearchIndex>,
    ) = crossbeam_channel::unbounded();
    let receiver_epoch = ctx.install_search_index_rx(rx, generation);
    let receiver_terminal_guard = ctx.search_index_rx_terminal_guard(receiver_epoch);
    ctx.reset_symbol_cache();

    let shared_artifacts_read_only = ctx.shared_artifacts_read_only();
    let project_key = ctx.memoized_artifact_cache_key(&root);
    let session_id = log_ctx::current_session();
    let generation_flag = ctx.configure_generation_flag();
    let content_generation = ctx.configure_content_generation();
    let content_generation_flag = ctx.configure_content_generation_flag();
    let persist_epoch_flag = ctx.search_persist_epoch_flag();
    let persist_epoch = ctx.next_search_persist_epoch();
    let lifecycle = ctx.subc_lifecycle_admission();
    let cold_build_limiter = ctx.cold_build_limiter();
    thread::spawn(move || {
        let _terminal_guard = receiver_terminal_guard;
        log_ctx::with_session(session_id, || {
            let Some(_permit) = crate::cold_build_limiter::acquire_blocking_while_with_limiter(
                &cold_build_limiter,
                "search corpus refresh",
                || lifecycle.is_current(&generation_flag, generation),
            ) else {
                return;
            };
            if !lifecycle.is_current(&generation_flag, generation)
                || persist_epoch_flag.current() != persist_epoch
            {
                return;
            }
            let cache_dir = aft::search_index::resolve_cache_dir_with_key(
                &project_key,
                config.storage_dir.as_deref(),
            );
            let cache_lock = if shared_artifacts_read_only {
                None
            } else {
                match aft::search_index::CacheLock::acquire(&cache_dir, &root) {
                    Ok(lock) => Some(lock),
                    Err(error) => {
                        aft::slog_warn!(
                            "failed to acquire search cache lock for ignore refresh: {}",
                            error
                        );
                        None
                    }
                }
            };
            let mut index = aft::search_index::SearchIndex::build_with_limit_to_cache_dir(
                &root,
                config.search_index_max_file_size,
                &cache_dir,
            );
            wait_on_search_rebuild_publish_gate_for_test();
            // cache_lock is None for borrow-only roots, so ignore-refresh
            // never writes cache.bin even when ram_overlay is on.
            if cache_lock.is_some()
                && content_generation_flag.load(std::sync::atomic::Ordering::SeqCst)
                    == content_generation
            {
                let _ = persist_epoch_flag.run_if_current(persist_epoch, || {
                    let head = index.stored_git_head().map(str::to_owned);
                    index.write_to_disk(&cache_dir, head.as_deref());
                });
            }
            let _ = lifecycle.run_if_current(&generation_flag, generation, || {
                let _ = tx.send(index);
            });
        });
    });
}

pub fn refresh_project_corpus(
    ctx: &AppContext,
    reason: &str,
    _invalidate_ignore_paths: bool,
) -> bool {
    let generation = ctx.configure_generation();
    ctx.run_if_subc_bound_generation(generation, || {
        let Some(root) = ctx.canonical_cache_root_opt() else {
            return false;
        };
        let config = ctx.config();
        let mut status_changed = false;

        if ctx.callgraph_writer() {
            // Do NOT cold-build the callgraph store synchronously here. This function
            // runs on the single-threaded dispatch loop from `drain_watcher_events`,
            // which fires before EVERY request (and on idle ticks). A full O(repo)
            // `refresh_corpus` (= `cold_build`: parse all files + resolve refs +
            // rewrite SQLite) blocks ALL queued requests — including `configure` and
            // `bash` — for its entire duration, which exceeds the 30s transport
            // timeout on a large repo. On a long-lived bridge (OpenCode Desktop) an
            // FSEvents overflow triggers this drain, so the user sees configure/bash
            // time out (regression: the watcher-overflow path that calls this is new
            // in 0.39.1; the ignore-rule path that also calls this had the same
            // latent inline block, just rarely triggered).
            //
            // Instead, drop the resident store and force a BACKGROUND rebuild: the
            // next `callgraph_store_for_ops()` spawns the cold build off-thread and
            // returns `Building` (callgraph ops + dead_code projection already handle
            // `Building`/unavailable gracefully). This mirrors the search/semantic
            // refreshes below, which are already async. A build already in flight
            // keeps running; the resident drop + force flag make the next op converge
            // to a fresh full rebuild.
            // Mirror the original "act only when the callgraph is actually loaded or
            // building" guard, but reschedule instead of inline-building.
            let callgraph_store_resident = {
                let guard = ctx
                    .callgraph_store()
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                guard.is_some()
            };
            if callgraph_store_resident || ctx.callgraph_store_rx().lock().is_some() {
                *ctx.callgraph_store()
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                ctx.mark_callgraph_store_force_rebuild();
                status_changed = true;
                aft::slog_info!(
                    "callgraph store scheduled for background rebuild after {}",
                    reason
                );
            }
        }

        if config.search_index && !ctx.shared_artifacts_read_only() {
            spawn_search_corpus_refresh_admitted(ctx, root.clone(), config.clone(), generation);
            status_changed = true;
            aft::slog_info!("started search index refresh after {}", reason);
        }

        if config.semantic_search && !ctx.shared_artifacts_read_only() {
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
    refresh_project_corpus(ctx, "ignore-rule change", true)
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
        let _ = ctx.mark_status_bar_tier2_stale();
        ctx.clear_tsconfig_membership_cache();
        true
    }) else {
        return false;
    };

    status_changed |= refresh_project_corpus(ctx, "watcher overflow", false);

    // The shared corpus refresh only reconciles what is resident or has a live
    // worker. After lost events that is not enough: nothing may be resident
    // (evicted root), no refresh worker may exist yet, and read-only roots
    // skip watcher path application entirely. Force the reconciliation for
    // each lane regardless of residency.
    let hardened = ctx.run_if_subc_bound_generation(generation, || {
        let config = ctx.config();
        if ctx.callgraph_writer()
            && config.callgraph_store
            && ctx.pending_callgraph_store_force_token().is_none()
        {
            // The corpus refresh above forces a rebuild only when a store was
            // resident or building; lost events invalidate the disk
            // generation either way.
            ctx.mark_callgraph_store_force_rebuild();
        }
        if ctx.shared_artifacts_read_only() {
            // Read-only roots reconcile by re-opening the shared artifacts:
            // drop the resident snapshots so the evicted-reload path fires on
            // the next query.
            ctx.search_index()
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if config.semantic_search {
                ctx.semantic_index()
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
            }
        } else if config.semantic_search
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

fn watcher_path_is_generated_for_callgraph(ctx: &AppContext, path: &Path) -> bool {
    ctx.callgraph_project_root()
        .is_some_and(|project_root| crate::inspect::is_generated_file(&project_root, path))
}

pub fn refresh_callgraph_store_for_watcher(
    ctx: &AppContext,
    changed: &HashSet<std::path::PathBuf>,
) {
    if !ctx.heavy_root_work_allowed() {
        return;
    }
    let refresh_paths = changed
        .iter()
        .filter(|path| {
            invalidates_workspace_crate_prefix_cache(path)
                || (watcher_path_is_callgraph_indexed(path)
                    && !watcher_path_is_generated_for_callgraph(ctx, path))
        })
        .cloned()
        .collect::<Vec<_>>();
    if refresh_paths.is_empty() {
        return;
    }
    // This is intentionally the only watcher call-site action. Opening and
    // mutating SQLite belongs to the process-wide store worker, outside every
    // executor lane and its epoch gate.
    ctx.enqueue_callgraph_store_refresh(refresh_paths);
}

/// Drain pre-filtered watcher events and apply cache invalidations on the
/// dispatch thread. The watcher filter thread owns notify receive/decode,
/// metadata filtering, ignore matching, root-deleted detection, and path
/// coalescing; this drain only reacts to compact control events and surviving
/// paths because the cache/index state below is not Send.
pub fn drain_watcher_events(ctx: &AppContext) {
    loop {
        let outcome = drain_watcher_events_bounded(ctx, WATCHER_PATH_DRAIN_BATCH_CAP);
        if !outcome.has_more {
            break;
        }
    }
}

fn watcher_drain_phase_name(stage: WatcherDrainApplyPhase) -> &'static str {
    match stage {
        WatcherDrainApplyPhase::PendingTier2 => "pending_tier2",
        WatcherDrainApplyPhase::PendingIndexes => "pending_indexes",
        WatcherDrainApplyPhase::SymbolCache => "symbol_cache",
        WatcherDrainApplyPhase::Callgraph => "callgraph",
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
    while *remaining > 0 {
        let path = paths
            .pop_front()
            .expect("watcher apply phase tracks its remaining paths");
        {
            let _watchdog = WatcherDrainUnitGuard::start(stage, &path);
            delay_watcher_unit_for_test();
            wait_on_watcher_phase_commit_gate_for_test(&path);
            apply(&path);
        }
        paths.push_back(path);
        *remaining -= 1;
        if started.elapsed() >= budget {
            return false;
        }
    }
    true
}

fn apply_callgraph_watcher_phase(
    ctx: &AppContext,
    paths: &mut VecDeque<PathBuf>,
    remaining: &mut usize,
    started: Instant,
    budget: Duration,
    enabled: bool,
    mut refresh: impl FnMut(&AppContext, &HashSet<PathBuf>),
) -> bool {
    let mut changed = HashSet::new();
    if enabled {
        // Include manifest invalidation before the budgeted path loop. If this
        // phase yields before reaching Cargo.toml, an earlier source sub-batch
        // must still discard the root's old workspace map before resolving refs.
        changed.extend(
            paths
                .iter()
                .filter(|path| invalidates_workspace_crate_prefix_cache(path))
                .cloned(),
        );
    }
    let mut generated_skipped = 0usize;
    let completed = apply_watcher_path_phase(
        WatcherDrainApplyPhase::Callgraph,
        paths,
        remaining,
        started,
        budget,
        |path| {
            if !enabled {
                return;
            }
            if invalidates_workspace_crate_prefix_cache(path) {
                changed.insert(path.to_path_buf());
            } else if watcher_path_is_callgraph_indexed(path) {
                if watcher_path_is_generated_for_callgraph(ctx, path) {
                    generated_skipped += 1;
                } else {
                    changed.insert(path.to_path_buf());
                }
            }
        },
    );
    if generated_skipped > 0 {
        log::debug!(
            "callgraph refresh skipped {} generated file(s)",
            generated_skipped
        );
    }
    if !changed.is_empty() {
        let first = changed
            .iter()
            .min()
            .expect("non-empty callgraph watcher batch has a first path");
        let _watchdog = WatcherDrainUnitGuard::start_batch(
            WatcherDrainApplyPhase::Callgraph,
            first,
            changed.len(),
        );
        delay_watcher_unit_for_test();
        refresh(ctx, &changed);
    }
    completed
}

fn next_watcher_apply_phase(stage: WatcherDrainApplyPhase) -> WatcherDrainApplyPhase {
    match stage {
        WatcherDrainApplyPhase::PendingTier2 => WatcherDrainApplyPhase::PendingIndexes,
        WatcherDrainApplyPhase::PendingIndexes => WatcherDrainApplyPhase::SymbolCache,
        WatcherDrainApplyPhase::SymbolCache => WatcherDrainApplyPhase::Callgraph,
        WatcherDrainApplyPhase::Callgraph => WatcherDrainApplyPhase::SearchIndex,
        WatcherDrainApplyPhase::SearchIndex => WatcherDrainApplyPhase::SemanticIndex,
        WatcherDrainApplyPhase::SemanticIndex => WatcherDrainApplyPhase::LspDiagnostics,
        WatcherDrainApplyPhase::LspDiagnostics => WatcherDrainApplyPhase::Complete,
        WatcherDrainApplyPhase::Complete => WatcherDrainApplyPhase::Complete,
    }
}

fn apply_watcher_slice(ctx: &AppContext, state: &mut WatcherDrainSliceState, started: Instant) {
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
                let search_build_in_progress = ctx
                    .search_index_rx()
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .is_some();
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
                            && !shared_artifacts_read_only
                            && search_build_in_progress
                        {
                            let _ = ctx.run_if_subc_bound_generation(lifecycle_generation, || {
                                ctx.add_pending_search_index_paths([path.to_path_buf()])
                            });
                        }
                        if heavy_root_work_allowed
                            && !shared_artifacts_read_only
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
            WatcherDrainApplyPhase::Callgraph => apply_callgraph_watcher_phase(
                ctx,
                &mut paths,
                &mut remaining,
                started,
                WATCHER_DRAIN_SLICE_BUDGET,
                heavy_root_work_allowed,
                |ctx, changed| {
                    let _ = ctx.enqueue_callgraph_store_refresh_for_generation(
                        changed.iter().cloned(),
                        lifecycle_generation,
                    );
                },
            ),
            WatcherDrainApplyPhase::SearchIndex => apply_watcher_path_phase(
                WatcherDrainApplyPhase::SearchIndex,
                &mut paths,
                &mut remaining,
                started,
                WATCHER_DRAIN_SLICE_BUDGET,
                |path| {
                    if heavy_root_work_allowed && apply_ram_search_updates {
                        let _ = ctx.run_if_subc_bound_generation(lifecycle_generation, || {
                            let mut index_ref = ctx
                                .search_index()
                                .write()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            if let Some(index) = index_ref.as_mut() {
                                if path.exists() {
                                    index.update_file(path);
                                } else {
                                    index.remove_file(path);
                                }
                            }
                        });
                    }
                },
            ),
            WatcherDrainApplyPhase::SemanticIndex => {
                // Semantic stays frozen under worktree.ram_overlay: embedding
                // cost is out of scope. Only a writer root invalidates here.
                let mut invalidated_paths = Vec::new();
                let completed = apply_watcher_path_phase(
                    WatcherDrainApplyPhase::SemanticIndex,
                    &mut paths,
                    &mut remaining,
                    started,
                    WATCHER_DRAIN_SLICE_BUDGET,
                    |path| {
                        if heavy_root_work_allowed
                            && !shared_artifacts_read_only
                            && watcher_path_is_semantic_source(path)
                        {
                            invalidated_paths.push(path.to_path_buf());
                        }
                    },
                );

                if !invalidated_paths.is_empty() {
                    // Invalidate all semantic paths processed in this slice under
                    // one write lock so a multi-file edit scans the index once.
                    let _ = ctx.run_if_subc_bound_generation(lifecycle_generation, || {
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
                        if invalidated {
                            let mut status = ctx
                                .semantic_index_status()
                                .write()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            if matches!(&*status, SemanticIndexStatus::Ready { .. }) {
                                for path in invalidated_paths {
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
            WatcherDrainApplyPhase::LspDiagnostics => apply_watcher_path_phase(
                WatcherDrainApplyPhase::LspDiagnostics,
                &mut paths,
                &mut remaining,
                started,
                WATCHER_DRAIN_SLICE_BUDGET,
                |path| {
                    let _ = ctx.run_if_subc_bound_generation(lifecycle_generation, || {
                        if !path.exists() {
                            status_changed |= ctx.lsp_clear_diagnostics_for_file(path);
                            return;
                        }
                        let stale = ctx.lsp_mark_diagnostics_stale_for_file(path);
                        status_changed |= stale.changed;
                        if stale.had_entries {
                            ctx.lsp_resync_changed_file_for_diagnostics(path);
                        }
                    });
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

        if stage == WatcherDrainApplyPhase::Complete {
            break;
        }
        stage = next_watcher_apply_phase(stage);
        remaining = paths.len();
        if started.elapsed() >= WATCHER_DRAIN_SLICE_BUDGET {
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
        ctx.status_emitter().signal(ctx.build_status_snapshot());
    }
    ctx.tick_tier2_refresh_scheduler(state.scheduler_changed_path_count);
    state.phase = WatcherDrainPhase::Collect;
    state.status_changed = false;
    state.scheduler_changed_path_count = 0;
    state.semantic_refresh_paths.clear();
}

fn publish_view_if_quiet(ctx: &AppContext, state: &mut WatcherDrainSliceState) {
    if !ctx.config().views.enabled
        || !matches!(state.phase, WatcherDrainPhase::Collect)
        || state
            .view_publication_due
            .is_none_or(|due| Instant::now() < due)
    {
        return;
    }
    let Some(root) = ctx.canonical_cache_root_opt() else {
        return;
    };
    let changed = state
        .view_publication_paths
        .iter()
        .filter_map(|path| path.strip_prefix(&root).ok())
        .filter_map(|path| aft::views::RelPath::from_os_path(path).ok())
        .map(|path| path.as_bytes().to_vec())
        .collect::<BTreeSet<_>>();
    let Some(_permit) = ctx.cold_build_limiter().try_acquire() else {
        state.view_publication_due = Some(Instant::now() + Duration::from_millis(100));
        return;
    };
    match ctx.publish_view_paths(changed, !ctx.shared_artifacts_read_only()) {
        Ok(report) => {
            aft::slog_info!(
                "content-addressed view publication published={} blob_puts={} pending_paths={}",
                report.published,
                report.blob_puts,
                report.pending_paths.len()
            );
            state.view_publication_paths.clear();
            state.view_publication_due = None;
        }
        Err(error) => {
            aft::slog_warn!("content-addressed view publication failed: {}", error);
            state.view_publication_due = Some(Instant::now() + Duration::from_secs(1));
        }
    }
}

pub fn drain_watcher_events_bounded(ctx: &AppContext, max_paths: usize) -> DrainBatchOutcome {
    let started = Instant::now();
    let configure_generation = ctx.configure_generation();
    let content_generation = ctx.configure_content_generation();
    let mut outcome = DrainBatchOutcome::default();
    // Admission before touching the continuation: an unbound invocation must
    // leave the retained state in place for the rebind to rebase, not take
    // and drop it.
    if ctx
        .run_if_subc_bound_generation(configure_generation, || ())
        .is_none()
    {
        return outcome;
    }
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
        let Some(rx) = rx_ref.as_ref() else {
            ctx.tick_tier2_refresh_scheduler(0);
            return outcome;
        };

        loop {
            match rx.try_recv() {
                Ok(WatcherDispatchEvent::Paths(paths)) => {
                    dispatch_events_received += 1;
                    if !state.rescan_required {
                        state.pending_paths.extend(paths);
                    }
                }
                Ok(WatcherDispatchEvent::RescanRequired) => {
                    dispatch_events_received += 1;
                    state.rescan_required = true;
                    state.pending_paths.clear();
                    state.phase = WatcherDrainPhase::Collect;
                    state.semantic_refresh_paths.clear();
                    state.scheduler_changed_path_count = 0;
                }
                Ok(WatcherDispatchEvent::IgnoreRulesChanged { path }) => {
                    dispatch_events_received += 1;
                    state.ignore_changed = true;
                    log::debug!(
                        "watcher: ignore rules changed at {}, rebuilding matcher",
                        path.display()
                    );
                    if !state.rescan_required {
                        let heavy_root_work_allowed = ctx.heavy_root_work_allowed();
                        let _ = ctx.run_if_subc_bound_generation(configure_generation, || {
                            if heavy_root_work_allowed {
                                ctx.rebuild_gitignore();
                            } else {
                                ctx.clear_gitignore();
                            }
                        });
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
            if started.elapsed() >= WATCHER_DRAIN_SLICE_BUDGET {
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
        ctx.stop_watcher_runtime_in_background();
        let _ = ctx.add_degraded_reason("project_root_deleted".to_string());
        aft::slog_warn!(
            "project root deleted; dropping watcher to avoid delete-storm: {:?}",
            ctx.canonical_cache_root_opt()
        );
        ctx.status_emitter().signal(ctx.build_status_snapshot());
        return outcome;
    }
    if let Some(error) = watcher_failed {
        ctx.stop_watcher_runtime_in_background();
        let _ = ctx.add_degraded_reason("watcher_unavailable".to_string());
        aft::slog_warn!(
            "file watcher unavailable; continuing without live external-change invalidation: {}",
            error
        );
        ctx.status_emitter().signal(ctx.build_status_snapshot());
        return outcome;
    }

    if state.rescan_required && receiver_has_more_after_receive {
        outcome.has_more = true;
        *ctx.watcher_drain_slice().lock() = Some(state);
        return outcome;
    }

    if state.rescan_required {
        crate::logging::note_watcher_overflow();
        aft::slog_warn!("watcher overflow: forcing project rescan");
        if ctx.heavy_root_work_allowed() {
            ctx.rebuild_gitignore();
        } else {
            ctx.clear_gitignore();
        }
        state.status_changed |= refresh_project_after_watcher_rescan(ctx);
        state.scheduler_changed_path_count =
            aft::inspect::tier2_scheduler::TIER2_REFRESH_STORM_PATH_THRESHOLD + 1;
        if state.status_changed {
            ctx.status_emitter().signal(ctx.build_status_snapshot());
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
            state.rescan_required = false;
            state.ignore_changed = false;
            if ctx.config().views.enabled {
                state.view_publication_paths.clear();
                state.view_publication_due = Some(
                    Instant::now() + crate::commands::configure::semantic_refresh_quiet_window(),
                );
            }
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
                if ctx.config().views.enabled {
                    state.view_publication_paths.clear();
                    state.view_publication_due = Some(
                        Instant::now()
                            + crate::commands::configure::semantic_refresh_quiet_window(),
                    );
                }
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
                    ctx.status_emitter().signal(ctx.build_status_snapshot());
                }
                ctx.tick_tier2_refresh_scheduler(usize::from(ignore_changed));
                state.status_changed = false;
            } else {
                if ctx.config().views.enabled {
                    state.view_publication_paths.extend(paths.iter().cloned());
                    state.view_publication_due = Some(
                        Instant::now()
                            + crate::commands::configure::semantic_refresh_quiet_window(),
                    );
                }
                state.path_slice_count += 1;
                state.scheduler_changed_path_count = if ignore_changed {
                    paths.len().max(1)
                } else {
                    paths.len()
                };
                if ctx
                    .run_if_subc_bound_generation(configure_generation, || {
                        ctx.mark_status_bar_tier2_stale()
                    })
                    .unwrap_or(false)
                {
                    state.status_changed = true;
                }
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
                ctx.status_emitter().signal(ctx.build_status_snapshot());
            }
            ctx.tick_tier2_refresh_scheduler(1);
            state.status_changed = false;
        }
    }

    if matches!(state.phase, WatcherDrainPhase::Apply { .. })
        && started.elapsed() < WATCHER_DRAIN_SLICE_BUDGET
    {
        apply_watcher_slice(ctx, &mut state, started);
    }

    publish_view_if_quiet(ctx, &mut state);

    let receiver_has_more = ctx
        .watcher_rx()
        .lock()
        .as_ref()
        .is_some_and(|rx| !rx.is_empty());
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

/// Shut down language servers for a standalone runtime whose last request is
/// older than `idle.lsp_ttl_minutes`. Subc uses the same helper with its own
/// per-root activity stamp.
pub fn shutdown_idle_lsp(ctx: &AppContext) {
    shutdown_idle_lsp_at(ctx, Instant::now(), ctx.last_request_at());
}

pub fn shutdown_idle_lsp_at(ctx: &AppContext, now: Instant, last_activity: Instant) {
    let ttl = ctx.config().idle.lsp_ttl();
    let idle = now.saturating_duration_since(last_activity);
    if idle < ttl {
        return;
    }
    let clients = {
        let mut lsp = ctx.lsp();
        if lsp.server_count() == 0 {
            return;
        }
        lsp.take_all_clients()
    };
    let n = clients.len();
    if n == 0 {
        return;
    }
    let root = ctx
        .config()
        .project_root
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "standalone".to_string());
    aft::slog_info!(
        "idle lsp reap {root}: shut down {n} server(s) after {}m (ttl {}m)",
        idle.as_secs() / 60,
        ctx.config().idle.lsp_ttl_minutes
    );
    crate::lsp::manager::LspManager::spawn_idle_lsp_reap(clients);
}

pub fn drain_lsp_events_bounded(ctx: &AppContext, max_events: usize) -> DrainBatchOutcome {
    let drained = {
        let mut lsp = ctx.lsp();
        lsp.drain_events_bounded(max_events)
    };
    let outcome = DrainBatchOutcome {
        processed: drained.events.len(),
        has_more: drained.has_more,
    };
    let mut status_changed = drained.diagnostics_changed;
    for event in drained.events {
        match event {
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
                    params.unwrap_or(serde_json::Value::Null)
                );
            }
            LspEvent::ServerRequest {
                server_kind,
                root,
                id,
                method,
                params,
            } => {
                log::debug!(
                    "[aft-lsp] request {:?} {} {:?} {} {}",
                    server_kind,
                    root.display(),
                    id,
                    method,
                    params.unwrap_or(serde_json::Value::Null)
                );
            }
            LspEvent::ServerExited {
                server_kind,
                root,
                reason,
            } => {
                aft::slog_info!("exited {:?} {} ({reason})", server_kind, root.display());
                status_changed = true;
            }
        }
    }
    if status_changed {
        ctx.status_emitter().signal(ctx.build_status_snapshot());
    }
    outcome
}

#[cfg(test)]
pub(crate) fn configure_search_order_context_for_test(
    root: &Path,
    storage: &Path,
) -> (AppContext, std::path::PathBuf) {
    std::fs::write(root.join(".gitignore"), "ignored.rs\n").unwrap();
    std::fs::write(root.join("ignored.rs"), "fn ignored_marker() {}\n").unwrap();

    let ctx = AppContext::new(
        crate::context::default_language_provider_factory(),
        crate::config::Config {
            project_root: Some(root.to_path_buf()),
            storage_dir: Some(storage.to_path_buf()),
            ..crate::config::Config::default()
        },
    );
    let canonical_root = std::fs::canonicalize(root).unwrap();
    let ignored_path = canonical_root.join("ignored.rs");
    ctx.set_canonical_cache_root(canonical_root.clone());
    ctx.set_harness(crate::harness::Harness::Opencode);
    ctx.enqueue_configure_maintenance(crate::context::ConfigureMaintenanceJob {
        generation: ctx.configure_generation(),
        root_path: root.to_path_buf(),
        canonical_cache_root: canonical_root,
        harness: crate::harness::Harness::Opencode,
        storage_root: storage.to_path_buf(),
        harness_dir: storage.join("opencode"),
        session_id: "order-test".to_string(),
        home_match: false,
        format_tool_cache_clear_needed: false,
        run_bash_replay: false,
        refresh_project_runtime: true,
        sync_bash_compress_flag: false,
        reset_filter_registry: false,
        clear_failed_spawns: false,
        warm_callgraph_store: false,
        supersede_search_artifact_persistence: false,
        supersede_callgraph_artifact_persistence: false,
        supersede_semantic_artifact_persistence: false,
        search_artifact_load_start: None,
        semantic_artifact_load_start: None,
    })
    .expect("test configure maintenance queue has capacity");

    let (search_tx, search_rx) = crossbeam_channel::unbounded();
    search_tx
        .send(crate::search_index::SearchIndex::new())
        .unwrap();
    drop(search_tx);
    *ctx.search_index_rx()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(search_rx);
    ctx.add_pending_search_index_paths([ignored_path.clone()]);
    (ctx, ignored_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::context::{default_language_provider_factory, AppContext};

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
    fn watcher_semantic_phase_batches_invalidation_into_one_retain_pass() {
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
        watcher_tx
            .send(WatcherDispatchEvent::Paths(files.clone()))
            .unwrap();

        let outcome = drain_watcher_events_bounded(&ctx, files.len());

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
    fn newer_watcher_refresh_prevents_older_configure_build_from_overwriting_disk() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let source = root_path.join("marker.rs");
        std::fs::write(&source, "fn old_generation_marker() {}\n").unwrap();

        let ctx = AppContext::new(
            default_language_provider_factory(),
            Config {
                project_root: Some(root_path.clone()),
                storage_dir: Some(storage.path().to_path_buf()),
                ..Config::default()
            },
        );
        ctx.set_canonical_cache_root(root_path.clone());
        ctx.set_harness(crate::harness::Harness::Opencode);

        let project_key = ctx.memoized_artifact_cache_key(&root_path);
        let cache_dir =
            crate::search_index::resolve_cache_dir_with_key(&project_key, Some(storage.path()));
        let mut older_index = crate::search_index::SearchIndex::build(&root_path);
        let older_epoch = ctx.next_search_persist_epoch();
        let persist_epoch = ctx.search_persist_epoch_flag();
        let (older_reached_tx, older_reached_rx) = std::sync::mpsc::channel();
        let (older_release_tx, older_release_rx) = std::sync::mpsc::channel();
        let older_root = root_path.clone();
        let older_cache = cache_dir.clone();
        let older_writer = std::thread::spawn(move || {
            older_reached_tx.send(()).unwrap();
            older_release_rx.recv().unwrap();
            let _lock = crate::search_index::CacheLock::acquire(&older_cache, &older_root)
                .expect("older build should acquire the persistence lock");
            let _ = persist_epoch.run_if_current(older_epoch, || {
                older_index.write_to_disk(&older_cache, None);
            });
        });
        older_reached_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("older configure build did not reach its persistence barrier");

        std::fs::write(&source, "fn new_watcher_marker() {}\n").unwrap();
        spawn_search_corpus_refresh(&ctx, root_path.clone(), ctx.config());
        let refresh_rx = ctx
            .search_index_rx()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .expect("watcher refresh receiver")
            .clone();
        refresh_rx
            .recv_timeout(Duration::from_secs(12))
            .expect("watcher refresh did not complete");

        older_release_tx.send(()).unwrap();
        older_writer.join().unwrap();

        let disk = crate::search_index::SearchIndex::read_from_disk(&cache_dir, &root_path)
            .expect("persisted search index");
        assert_eq!(
            disk.grep("new_watcher_marker", true, &[], &[], &root_path, 10)
                .matches
                .len(),
            1,
            "newer watcher refresh must remain on disk"
        );
        assert!(
            disk.grep("old_generation_marker", true, &[], &[], &root_path, 10)
                .matches
                .is_empty(),
            "older configure build must not overwrite the newer watcher refresh"
        );
    }

    #[test]
    fn watcher_phase_dequeued_before_unbind_cannot_index_after_teardown() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let root = root.as_path();
        let source = root.join("changed.rs");
        std::fs::write(&source, "fn watcher_marker() {}\n").unwrap();
        let (ctx, watcher_tx) = watcher_context(root);
        *ctx.search_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(crate::search_index::SearchIndex::new());
        watcher_tx
            .send(WatcherDispatchEvent::Paths(vec![source.clone()]))
            .unwrap();

        let ctx = Arc::new(ctx);
        let (reached_rx, release_tx) = install_watcher_phase_commit_gate_for_test(source.clone());
        let drain_ctx = Arc::clone(&ctx);
        let drain = std::thread::spawn(move || {
            while drain_watcher_events_bounded(&drain_ctx, WATCHER_PATH_DRAIN_BATCH_CAP).has_more {}
        });
        reached_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("watcher phase did not reach its commit barrier");
        ctx.mark_subc_unbound();
        release_tx.send(()).unwrap();
        drain.join().unwrap();

        {
            let search = ctx
                .search_index()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(
                search
                    .as_ref()
                    .expect("search index")
                    .grep("watcher_marker", true, &[], &[], root, 10)
                    .matches
                    .is_empty(),
                "watcher work dequeued before teardown must not mutate the index after unbind"
            );
        }

        // The unapplied path survives the unbound window in the retained
        // continuation; an equivalent rebind rebases and replays it.
        ctx.mark_subc_bound();
        let mut guard = 0;
        while drain_watcher_events_bounded(&ctx, WATCHER_PATH_DRAIN_BATCH_CAP).has_more {
            guard += 1;
            assert!(guard < 16, "rebased replay must finish");
        }
        let search = ctx
            .search_index()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(
            search
                .as_ref()
                .expect("search index")
                .grep("watcher_marker", true, &[], &[], root, 10)
                .matches
                .len(),
            1,
            "post-rebind replay must apply the retained watcher path"
        );
    }

    #[test]
    fn standalone_configure_tail_precedes_completed_search_install() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let (ctx, ignored_path) =
            configure_search_order_context_for_test(root.path(), storage.path());
        assert!(!watcher_path_is_ignored_by_current_matcher(
            &ctx,
            &ignored_path
        ));

        drain_deferred_configure_maintenance(&ctx);
        drain_configure_warning_events(&ctx);
        drain_search_index_events(&ctx);

        assert!(watcher_path_is_ignored_by_current_matcher(
            &ctx,
            &ignored_path
        ));
        assert_eq!(
            ctx.search_index()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .expect("completed search index installed")
                .file_count(),
            0,
            "configure must install the ignore matcher before pending paths replay"
        );
        ctx.stop_watcher_runtime();
    }

    #[test]
    fn post_ack_semantic_ready_transition_pushes_status_changed() {
        let root = tempfile::tempdir().unwrap();
        let config = Config {
            project_root: Some(root.path().to_path_buf()),
            semantic_search: true,
            ..Config::default()
        };
        let ctx = AppContext::new(default_language_provider_factory(), config);
        ctx.set_canonical_cache_root(root.path().to_path_buf());
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Building {
            stage: "loading_artifacts".to_string(),
            files: None,
            entries_done: None,
            entries_total: None,
        };
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        *ctx.semantic_index_rx().lock() = Some(event_rx);
        let (push_tx, push_rx) = std::sync::mpsc::channel();
        ctx.set_progress_sender(Some(std::sync::Arc::new(Box::new(move |frame| {
            let _ = push_tx.send(frame);
        }))));

        event_tx
            .send(SemanticIndexEvent::Ready(
                crate::semantic_index::SemanticIndex::new(root.path().to_path_buf(), 3),
            ))
            .unwrap();
        drain_semantic_index_events(&ctx);

        assert!(matches!(
            &*ctx
                .semantic_index_status()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            SemanticIndexStatus::Ready { .. }
        ));
        let pushed = push_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("ready transition should push status_changed");
        assert!(matches!(
            pushed,
            crate::protocol::PushFrame::StatusChanged(_)
        ));
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
    fn superseded_callgraph_worker_settles_receiver_and_allows_retry() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("lib.rs"), "pub fn marker() {}\n").unwrap();
        let ctx = AppContext::new(
            default_language_provider_factory(),
            Config {
                project_root: Some(root.path().to_path_buf()),
                storage_dir: Some(storage.path().to_path_buf()),
                callgraph_chunk_size: 1,
                ..Config::default()
            },
        );
        let generation = ctx.configure_generation();
        let (worker_tx, worker_rx) = crossbeam_channel::unbounded();
        ctx.note_callgraph_store_rx_generation(generation);
        ctx.next_callgraph_store_rx_epoch();
        *ctx.callgraph_store_rx().lock() = Some(worker_rx);

        drain_callgraph_store_events(&ctx);
        assert!(
            ctx.callgraph_store_rx().lock().is_some(),
            "an empty running receiver remains in flight"
        );
        ctx.next_callgraph_persist_epoch();
        worker_tx.send(CallGraphStoreBuildEvent::Settled).unwrap();
        drain_callgraph_store_events(&ctx);
        assert!(
            ctx.callgraph_store_rx().lock().is_none(),
            "a superseded worker must explicitly retire its receiver"
        );

        assert!(matches!(
            ctx.callgraph_store_for_ops(),
            crate::context::CallgraphStoreAccess::Building
                | crate::context::CallgraphStoreAccess::Ready(_)
        ));
        assert!(
            ctx.callgraph_store_rx().lock().is_some()
                || ctx
                    .callgraph_store()
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .is_some(),
            "a later operation must be able to retry the callgraph build"
        );
    }

    #[test]
    fn failed_forced_callgraph_build_preserves_durable_demand() {
        let root = tempfile::tempdir().unwrap();
        let ctx = AppContext::new(
            default_language_provider_factory(),
            Config {
                project_root: Some(root.path().to_path_buf()),
                ..Config::default()
            },
        );
        let force_token = ctx.mark_callgraph_store_force_rebuild();
        assert_eq!(ctx.pending_callgraph_store_force_token(), Some(force_token));

        let generation = ctx.configure_generation();
        let (tx, rx) = crossbeam_channel::unbounded();
        ctx.note_callgraph_store_rx_generation(generation);
        ctx.next_callgraph_store_rx_epoch();
        *ctx.callgraph_store_rx().lock() = Some(rx);
        tx.send(CallGraphStoreBuildEvent::Settled).unwrap();
        drain_callgraph_store_events(&ctx);

        assert!(
            ctx.pending_callgraph_store_force_token().is_some(),
            "the current failed forced build must preserve retry demand"
        );
        assert!(ctx.callgraph_store_rx().lock().is_none());
    }

    #[test]
    fn newer_forced_callgraph_demand_survives_older_publication() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let source = root.path().join("lib.rs");
        std::fs::write(&source, "pub fn marker() {}\n").unwrap();
        let project_root = std::fs::canonicalize(root.path()).unwrap();
        let ctx = AppContext::new(
            default_language_provider_factory(),
            Config {
                project_root: Some(project_root.clone()),
                storage_dir: Some(storage.path().to_path_buf()),
                ..Config::default()
            },
        );
        ctx.set_canonical_cache_root(project_root.clone());
        let project_key = crate::search_index::artifact_cache_key(&project_root);
        crate::root_cache::configure_artifact_access(&project_root, &project_key, false);
        let (store, _stats) = CallGraphStore::cold_build_with_lease_chunked(
            ctx.callgraph_store_dir(),
            project_root,
            &[source],
            1,
        )
        .unwrap();
        let older = ctx.mark_callgraph_store_force_rebuild();
        let generation = ctx.configure_generation();
        let (tx, rx) = crossbeam_channel::unbounded();
        ctx.note_callgraph_store_rx_generation(generation);
        ctx.next_callgraph_store_rx_epoch();
        *ctx.callgraph_store_rx().lock() = Some(rx);
        tx.send(CallGraphStoreBuildEvent::Ready {
            store,
            fulfilled_force_token: Some(older),
            publication_epoch: ctx.callgraph_persist_epoch_flag().current(),
        })
        .unwrap();
        let newer = ctx.mark_callgraph_store_force_rebuild();

        drain_callgraph_store_events(&ctx);

        assert!(ctx.callgraph_store().read().unwrap().is_some());
        assert_eq!(ctx.pending_callgraph_store_force_token(), Some(newer));
        assert!(matches!(
            ctx.callgraph_store_for_ops(),
            crate::context::CallgraphStoreAccess::Building
        ));
        assert!(ctx.callgraph_store_rx().lock().is_some());

        let deadline = Instant::now() + Duration::from_secs(10);
        while ctx.pending_callgraph_store_force_token().is_some() {
            drain_callgraph_store_events(&ctx);
            assert!(
                Instant::now() < deadline,
                "newer forced callgraph rebuild did not publish"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(ctx.callgraph_store().read().unwrap().is_some());
    }

    #[test]
    fn callgraph_ready_without_published_pointer_settles_and_preserves_pending_paths() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let source = root.path().join("lib.rs");
        std::fs::write(&source, "pub fn marker() {}\n").unwrap();
        let project_root = std::fs::canonicalize(root.path()).unwrap();
        let ctx = AppContext::new(
            default_language_provider_factory(),
            Config {
                project_root: Some(project_root.clone()),
                storage_dir: Some(storage.path().to_path_buf()),
                callgraph_chunk_size: 1,
                ..Config::default()
            },
        );
        ctx.set_canonical_cache_root(project_root.clone());
        let project_key = crate::search_index::artifact_cache_key(&project_root);
        crate::root_cache::configure_artifact_access(&project_root, &project_key, false);
        let callgraph_dir = ctx.callgraph_store_dir();
        let (store, _stats) = CallGraphStore::cold_build_with_lease_chunked(
            callgraph_dir.clone(),
            project_root,
            &[source],
            1,
        )
        .unwrap();
        let pointer = callgraph_dir.join(format!("{}.current", store.project_key()));
        std::fs::remove_file(pointer).unwrap();

        let pending = root.path().join("pending.rs");
        ctx.add_pending_callgraph_store_paths([pending.clone()]);
        let generation = ctx.configure_generation();
        let (tx, rx) = crossbeam_channel::unbounded();
        {
            let mut receiver = ctx.callgraph_store_rx().lock();
            ctx.note_callgraph_store_rx_generation(generation);
            ctx.next_callgraph_store_rx_epoch();
            *receiver = Some(rx);
        }
        tx.send(CallGraphStoreBuildEvent::Ready {
            store,
            fulfilled_force_token: None,
            publication_epoch: ctx.callgraph_persist_epoch_flag().current(),
        })
        .unwrap();
        drop(tx);

        drain_callgraph_store_events(&ctx);

        assert!(
            ctx.callgraph_store_rx().lock().is_none(),
            "Ready is terminal even when reopening the pointer fails"
        );
        assert_eq!(
            ctx.take_pending_callgraph_store_paths(),
            vec![pending],
            "failed reopen must preserve pending watcher paths for the retry"
        );
    }

    #[test]
    fn callgraph_ready_transition_schedules_tier2_dead_code_rescan() {
        // When the callgraph store transitions to ready, the drain must request a
        // tier2 refresh pull. dead_code is suppressed (callgraph_available:false)
        // while no store is ready; this pull is what eventually re-runs dead_code
        // against the now-ready store and replaces that aggregate, flipping the
        // root to genuinely complete instead of "building" forever.
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let source = root.path().join("lib.rs");
        std::fs::write(&source, "pub fn marker() {}\n").unwrap();
        let project_root = std::fs::canonicalize(root.path()).unwrap();
        let ctx = AppContext::new(
            default_language_provider_factory(),
            Config {
                project_root: Some(project_root.clone()),
                storage_dir: Some(storage.path().to_path_buf()),
                callgraph_chunk_size: 1,
                ..Config::default()
            },
        );
        ctx.set_canonical_cache_root(project_root.clone());
        let project_key = crate::search_index::artifact_cache_key(&project_root);
        crate::root_cache::configure_artifact_access(&project_root, &project_key, false);
        let (store, _stats) = CallGraphStore::cold_build_with_lease_chunked(
            ctx.callgraph_store_dir(),
            project_root,
            &[source],
            1,
        )
        .unwrap();

        assert!(
            !ctx.tier2_pull_demand_pending(),
            "no tier2 pull demand before the callgraph store is ready"
        );

        let generation = ctx.configure_generation();
        let (tx, rx) = crossbeam_channel::unbounded();
        ctx.note_callgraph_store_rx_generation(generation);
        ctx.next_callgraph_store_rx_epoch();
        *ctx.callgraph_store_rx().lock() = Some(rx);
        tx.send(CallGraphStoreBuildEvent::Ready {
            store,
            fulfilled_force_token: None,
            publication_epoch: ctx.callgraph_persist_epoch_flag().current(),
        })
        .unwrap();
        drop(tx);

        drain_callgraph_store_events(&ctx);

        assert!(
            ctx.callgraph_store().read().unwrap().is_some(),
            "the ready callgraph store must install"
        );
        assert!(
            ctx.tier2_pull_demand_pending(),
            "the callgraph-ready transition must schedule a tier2 refresh pull so dead_code is rescanned against the ready store"
        );
    }

    #[test]
    fn stale_callgraph_receiver_cannot_clear_newer_same_generation_receiver() {
        let _guard = ARTIFACT_DRAIN_TEST_MUTEX.lock().unwrap();
        let root = tempfile::tempdir().unwrap();
        let ctx = Arc::new(AppContext::new(
            default_language_provider_factory(),
            Config {
                project_root: Some(root.path().to_path_buf()),
                ..Config::default()
            },
        ));
        let generation = ctx.configure_generation();
        let (old_tx, old_rx) = crossbeam_channel::unbounded();
        ctx.note_callgraph_store_rx_generation(generation);
        ctx.next_callgraph_store_rx_epoch();
        *ctx.callgraph_store_rx().lock() = Some(old_rx);
        old_tx.send(CallGraphStoreBuildEvent::Settled).unwrap();
        let (reached, release) = install_artifact_drain_commit_gate_for_test(&ctx);

        let drain_ctx = Arc::clone(&ctx);
        let drain = std::thread::spawn(move || drain_callgraph_store_events(&drain_ctx));
        reached
            .recv_timeout(Duration::from_secs(2))
            .expect("stale callgraph receiver was not dequeued");

        let (_new_tx, new_rx) = crossbeam_channel::unbounded();
        ctx.note_callgraph_store_rx_generation(generation);
        ctx.next_callgraph_store_rx_epoch();
        *ctx.callgraph_store_rx().lock() = Some(new_rx);
        release.send(()).unwrap();
        drain.join().unwrap();

        assert!(
            ctx.callgraph_store_rx().lock().is_some(),
            "a stale callgraph drain must not clear the replacement receiver"
        );
        assert!(
            ctx.pending_callgraph_store_force_token().is_none(),
            "a stale terminal event must not create force demand for its replacement"
        );
    }

    #[test]
    fn dequeued_search_completion_cannot_clear_newer_same_generation_receiver() {
        let _guard = ARTIFACT_DRAIN_TEST_MUTEX.lock().unwrap();
        let root = tempfile::tempdir().unwrap();
        let ctx = Arc::new(AppContext::new(
            default_language_provider_factory(),
            Config {
                project_root: Some(root.path().to_path_buf()),
                ..Config::default()
            },
        ));
        let generation = ctx.configure_generation();
        let (old_tx, old_rx) = crossbeam_channel::unbounded();
        old_tx
            .send(crate::search_index::SearchIndex::new())
            .unwrap();
        ctx.note_search_index_rx_generation(generation);
        ctx.next_search_index_rx_epoch();
        *ctx.search_index_rx()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(old_rx);
        let (reached, release) = install_artifact_drain_commit_gate_for_test(&ctx);

        let drain_ctx = Arc::clone(&ctx);
        let drain = std::thread::spawn(move || drain_search_index_events(&drain_ctx));
        reached
            .recv_timeout(Duration::from_secs(2))
            .expect("old search completion was not dequeued");

        let (_new_tx, new_rx) = crossbeam_channel::unbounded();
        ctx.note_search_index_rx_generation(generation);
        ctx.next_search_index_rx_epoch();
        *ctx.search_index_rx()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(new_rx);
        release.send(()).unwrap();
        drain.join().unwrap();

        assert!(
            ctx.search_index()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_none(),
            "an older same-generation receiver must not publish after replacement"
        );
        assert!(
            ctx.search_index_rx()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some(),
            "an older same-generation drain must not clear the newer receiver"
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
            .send(WatcherDispatchEvent::RescanRequired)
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
        let mut guard = 0;
        while drain_watcher_events_bounded(&ctx, WATCHER_PATH_DRAIN_BATCH_CAP).has_more {
            guard += 1;
            assert!(guard < 16, "rescan replay must finish");
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
    fn budget_interrupted_stage_rewinds_when_unbind_lands_mid_stage() {
        // An unbind mid-stage makes the remaining per-path actions gated
        // no-ops while `remaining` still decrements. The park must rewind the
        // stage so the post-rebind replay re-runs it in full — for BOTH park
        // shapes (budget-exhausted and stage-complete).
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let first = root.join("first.rs");
        let second = root.join("second.rs");
        std::fs::write(&first, "fn first_marker() {}\n").unwrap();
        std::fs::write(&second, "fn second_marker() {}\n").unwrap();
        let (ctx, watcher_tx) = watcher_context(&root);
        *ctx.search_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(crate::search_index::SearchIndex::new());
        watcher_tx
            .send(WatcherDispatchEvent::Paths(vec![
                first.clone(),
                second.clone(),
            ]))
            .unwrap();

        // Gate on the SECOND path so the unbind lands after `first` was
        // already applied within the same stage pass.
        let ctx = Arc::new(ctx);
        let (reached_rx, release_tx) = install_watcher_phase_commit_gate_for_test(second.clone());
        let drain_ctx = Arc::clone(&ctx);
        let drain = std::thread::spawn(move || {
            while drain_watcher_events_bounded(&drain_ctx, WATCHER_PATH_DRAIN_BATCH_CAP).has_more {}
        });
        reached_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("watcher phase did not reach the second path");
        ctx.mark_subc_unbound();
        release_tx.send(()).unwrap();
        drain.join().unwrap();

        ctx.mark_subc_bound();
        let mut guard = 0;
        while drain_watcher_events_bounded(&ctx, WATCHER_PATH_DRAIN_BATCH_CAP).has_more {
            guard += 1;
            assert!(guard < 32, "rebased replay must finish");
        }
        let search = ctx
            .search_index()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let index = search.as_ref().expect("search index");
        for (marker, path) in [("first_marker", &first), ("second_marker", &second)] {
            assert_eq!(
                index.grep(marker, true, &[], &[], &root, 10).matches.len(),
                1,
                "post-rebind replay must apply {} ({})",
                marker,
                path.display()
            );
        }
    }

    #[test]
    fn pending_paths_retained_across_transient_unbind_repair_next_installed_index() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let source = root.join("edited-during-unbind.rs");
        std::fs::write(&source, "fn repaired_marker() {}\n").unwrap();
        let ctx = AppContext::new(
            default_language_provider_factory(),
            Config {
                project_root: Some(root.clone()),
                ..Config::default()
            },
        );
        ctx.set_canonical_cache_root(root.clone());

        // Watcher recorded the edit while a build was in flight, then the
        // route unbound. The transient-unbind cleanup retires the receiver but
        // must keep the pending path: it is the only record that any artifact
        // a pre-unbind worker persisted is content-stale.
        ctx.add_pending_search_index_paths([source.clone()]);
        ctx.mark_subc_unbound();
        ctx.cancel_unbound_artifact_work();
        assert!(ctx.search_index_rx().read().unwrap().is_none());

        // Equivalent rebind starts a replacement build; its install must
        // replay the retained path into the fresh index.
        ctx.mark_subc_bound();
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut stale_index = crate::search_index::SearchIndex::build(&root);
        // Simulate the pre-unbind worker's artifact predating the edit.
        stale_index.remove_file(&source);
        tx.send(stale_index).unwrap();
        ctx.install_search_index_rx(rx, ctx.configure_generation());

        drain_search_index_events(&ctx);

        let search = ctx
            .search_index()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(
            search
                .as_ref()
                .expect("installed search index")
                .grep("repaired_marker", true, &[], &[], &root, 10)
                .matches
                .len(),
            1,
            "retained pending path must repair the stale artifact on install"
        );
    }

    #[test]
    fn disconnected_search_refresh_clears_nonready_index_and_preserves_pending_paths() {
        let root = tempfile::tempdir().unwrap();
        let ctx = AppContext::new(
            default_language_provider_factory(),
            Config {
                project_root: Some(root.path().to_path_buf()),
                ..Config::default()
            },
        );
        let mut index = crate::search_index::SearchIndex::new();
        index.ready = false;
        *ctx.search_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);
        let pending = root.path().join("pending.rs");
        ctx.add_pending_search_index_paths([pending.clone()]);
        let generation = ctx.configure_generation();
        let (tx, rx) = crossbeam_channel::unbounded();
        drop(tx);
        ctx.install_search_index_rx(rx, generation);

        drain_search_index_events(&ctx);

        assert!(
            ctx.search_index()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_none(),
            "a disconnected refresh must not leave a permanently non-ready index"
        );
        assert!(ctx.search_index_rx().read().unwrap().is_none());
        assert_eq!(ctx.take_pending_search_index_paths(), vec![pending]);
    }

    #[test]
    fn search_index_disconnect_reschedule_caps_at_one_per_generation() {
        // The drain path replaces a lost search-index load at most once per
        // configure generation; after that the query-triggered reload is the
        // recovery path, so a persistently failing worker cannot be relaunched in
        // a loop on the drain thread.
        let ctx = AppContext::new(default_language_provider_factory(), Config::default());
        assert!(
            ctx.allow_search_index_disconnect_reschedule(),
            "the first automatic replacement in a generation must be allowed"
        );
        assert!(
            !ctx.allow_search_index_disconnect_reschedule(),
            "a second automatic replacement in the same generation must be denied"
        );
        assert!(
            !ctx.search_index_query_reload_allowed(),
            "queued queries must use fallback during the retry cooldown"
        );
        ctx.advance_configure_generation();
        assert!(
            ctx.search_index_query_reload_allowed(),
            "a new configure generation must clear the retry cooldown"
        );
        assert!(
            ctx.allow_search_index_disconnect_reschedule(),
            "advancing the configure generation must reset the replacement cap"
        );
    }

    #[test]
    fn lost_search_load_disconnect_schedules_one_replacement_that_installs() {
        let _guard = ARTIFACT_DRAIN_TEST_MUTEX.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let storage = temp.path().join("storage");
        std::fs::create_dir_all(&storage).unwrap();
        std::fs::write(
            root.join("lib.rs"),
            "pub fn LostLoadNeedle() -> bool { true }\n",
        )
        .unwrap();
        let ctx = AppContext::new(
            default_language_provider_factory(),
            Config {
                project_root: Some(root.clone()),
                storage_dir: Some(storage),
                search_index: true,
                semantic_search: false,
                callgraph_store: false,
                ..Config::default()
            },
        );
        ctx.set_canonical_cache_root(root.clone());

        // Simulate a lost post-configure load: the build worker dropped its
        // sender without delivering an index, leaving a not-ready index and a
        // disconnected receiver. Before the fix nothing rescheduled this, so
        // health reported "building" forever.
        let mut stranded = crate::search_index::SearchIndex::new();
        stranded.ready = false;
        *ctx.search_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(stranded);
        let generation = ctx.configure_generation();
        let (tx, rx) = crossbeam_channel::unbounded::<crate::search_index::SearchIndex>();
        drop(tx); // disconnect without sending
        ctx.install_search_index_rx(rx, generation);

        drain_search_index_events(&ctx);

        assert!(
            ctx.search_index_rx().read().unwrap().is_some(),
            "a lost load must schedule a replacement search-index load (fresh receiver installed)"
        );

        // The replacement worker builds and publishes; draining installs it.
        // Poll until ready (bounded so a regression fails instead of hanging).
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            drain_search_index_events(&ctx);
            let ready = ctx
                .search_index()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .is_some_and(|index| index.ready);
            if ready {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "replacement search-index load did not install before the deadline"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        let matches = ctx
            .search_index()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .expect("installed replacement index")
            .grep("LostLoadNeedle", true, &[], &[], &root, 10)
            .matches
            .len();
        assert_eq!(
            matches, 1,
            "the replacement index must actually serve queries"
        );

        // The one-per-generation cap is now consumed. A second lost load in the
        // same generation must NOT schedule another replacement (no loop); the
        // query-triggered reload remains the recovery path.
        let mut stranded_again = crate::search_index::SearchIndex::new();
        stranded_again.ready = false;
        *ctx.search_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(stranded_again);
        let (tx2, rx2) = crossbeam_channel::unbounded::<crate::search_index::SearchIndex>();
        drop(tx2);
        ctx.install_search_index_rx(rx2, ctx.configure_generation());

        drain_search_index_events(&ctx);

        assert!(
            ctx.search_index_rx().read().unwrap().is_none(),
            "the cap must prevent a second automatic replacement in the same generation"
        );
    }

    #[test]
    fn dequeued_search_completion_cannot_publish_after_unbind() {
        let _guard = ARTIFACT_DRAIN_TEST_MUTEX.lock().unwrap();
        let root = tempfile::tempdir().unwrap();
        let ctx = Arc::new(AppContext::new(
            default_language_provider_factory(),
            Config {
                project_root: Some(root.path().to_path_buf()),
                ..Config::default()
            },
        ));
        ctx.set_canonical_cache_root(root.path().to_path_buf());
        let generation = ctx.configure_generation();
        let (tx, rx) = crossbeam_channel::unbounded();
        tx.send(crate::search_index::SearchIndex::new()).unwrap();
        ctx.note_search_index_rx_generation(generation);
        *ctx.search_index_rx()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(rx);
        let (reached, release) = install_artifact_drain_commit_gate_for_test(&ctx);

        let drain_ctx = Arc::clone(&ctx);
        let drain = std::thread::spawn(move || drain_search_index_events(&drain_ctx));
        reached
            .recv_timeout(Duration::from_secs(2))
            .expect("search completion was not dequeued");
        ctx.mark_subc_unbound();
        release.send(()).unwrap();
        drain.join().unwrap();

        assert!(
            ctx.search_index()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_none(),
            "a dequeued completion must re-check lifecycle admission at commit"
        );
    }

    #[test]
    fn dequeued_semantic_completion_cannot_publish_after_unbind() {
        let _guard = ARTIFACT_DRAIN_TEST_MUTEX.lock().unwrap();
        let root = tempfile::tempdir().unwrap();
        let ctx = Arc::new(AppContext::new(
            default_language_provider_factory(),
            Config {
                project_root: Some(root.path().to_path_buf()),
                semantic_search: true,
                ..Config::default()
            },
        ));
        ctx.set_canonical_cache_root(root.path().to_path_buf());
        let generation = ctx.configure_generation();
        let (tx, rx) = crossbeam_channel::unbounded();
        tx.send(SemanticIndexEvent::Ready(
            crate::semantic_index::SemanticIndex::new(root.path().to_path_buf(), 3),
        ))
        .unwrap();
        ctx.note_semantic_index_rx_generation(generation);
        *ctx.semantic_index_rx().lock() = Some(rx);
        let (reached, release) = install_artifact_drain_commit_gate_for_test(&ctx);

        let drain_ctx = Arc::clone(&ctx);
        let drain = std::thread::spawn(move || drain_semantic_index_events(&drain_ctx));
        reached
            .recv_timeout(Duration::from_secs(2))
            .expect("semantic completion was not dequeued");
        ctx.mark_subc_unbound();
        release.send(()).unwrap();
        drain.join().unwrap();

        assert!(
            ctx.semantic_index()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_none(),
            "a dequeued completion must re-check lifecycle admission at commit"
        );
    }

    #[test]
    fn dequeued_semantic_refresh_cannot_publish_after_unbind() {
        let _guard = ARTIFACT_DRAIN_TEST_MUTEX.lock().unwrap();
        let root = tempfile::tempdir().unwrap();
        let ctx = Arc::new(AppContext::new(
            default_language_provider_factory(),
            Config {
                project_root: Some(root.path().to_path_buf()),
                semantic_search: true,
                ..Config::default()
            },
        ));
        ctx.set_canonical_cache_root(root.path().to_path_buf());
        let (request_tx, _request_rx) = crossbeam_channel::unbounded();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        ctx.install_semantic_refresh_worker_for_build_epoch(
            request_tx,
            event_rx,
            Arc::new(Mutex::new(None)),
            ctx.semantic_index_rx_epoch(),
        );
        event_tx
            .send(SemanticRefreshEvent::CorpusCompleted {
                index: crate::semantic_index::SemanticIndex::new(root.path().to_path_buf(), 3),
                changed: 0,
                added: 0,
                deleted: 0,
                total_processed: 0,
            })
            .unwrap();
        let (reached, release) = install_artifact_drain_commit_gate_for_test(&ctx);

        let drain_ctx = Arc::clone(&ctx);
        let drain = std::thread::spawn(move || drain_semantic_refresh_events(&drain_ctx));
        reached
            .recv_timeout(Duration::from_secs(2))
            .expect("semantic refresh completion was not dequeued");
        ctx.mark_subc_unbound();
        release.send(()).unwrap();
        drain.join().unwrap();

        assert!(
            ctx.semantic_index()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_none(),
            "a dequeued refresh must re-check lifecycle admission at commit"
        );
    }

    #[test]
    fn dequeued_semantic_refresh_cannot_publish_after_bound_replacement() {
        let _guard = ARTIFACT_DRAIN_TEST_MUTEX.lock().unwrap();
        let root = tempfile::tempdir().unwrap();
        let ctx = Arc::new(AppContext::new(
            default_language_provider_factory(),
            Config {
                project_root: Some(root.path().to_path_buf()),
                semantic_search: true,
                ..Config::default()
            },
        ));
        ctx.set_canonical_cache_root(root.path().to_path_buf());
        let (old_request_tx, _old_request_rx) = crossbeam_channel::unbounded();
        let (old_event_tx, old_event_rx) = crossbeam_channel::unbounded();
        ctx.install_semantic_refresh_worker_for_build_epoch(
            old_request_tx,
            old_event_rx,
            Arc::new(Mutex::new(None)),
            ctx.semantic_index_rx_epoch(),
        );
        old_event_tx
            .send(SemanticRefreshEvent::CorpusCompleted {
                index: crate::semantic_index::SemanticIndex::new(root.path().to_path_buf(), 3),
                changed: 0,
                added: 0,
                deleted: 0,
                total_processed: 0,
            })
            .unwrap();
        let (reached, release) = install_artifact_drain_commit_gate_for_test(&ctx);

        let drain_ctx = Arc::clone(&ctx);
        let drain = std::thread::spawn(move || drain_semantic_refresh_events(&drain_ctx));
        reached
            .recv_timeout(Duration::from_secs(2))
            .expect("old semantic refresh completion was not dequeued");

        let (new_request_tx, _new_request_rx) = crossbeam_channel::unbounded();
        let (_new_event_tx, new_event_rx) = crossbeam_channel::unbounded();
        ctx.install_semantic_refresh_worker_for_build_epoch(
            new_request_tx,
            new_event_rx,
            Arc::new(Mutex::new(None)),
            ctx.semantic_index_rx_epoch(),
        );
        release.send(()).unwrap();
        drain.join().unwrap();

        assert!(
            ctx.semantic_index()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_none(),
            "an old refresh event must not be relabeled as the replacement worker"
        );
        assert!(
            ctx.semantic_refresh_event_rx().lock().is_some(),
            "the stale drain must not clear the replacement refresh receiver"
        );
    }

    #[test]
    fn current_semantic_refresh_disconnect_requests_full_reload() {
        let _guard = ARTIFACT_DRAIN_TEST_MUTEX.lock().unwrap();
        crate::commands::configure::set_semantic_refresh_restart_result_for_test(Some(true));
        struct RestartOverrideReset;
        impl Drop for RestartOverrideReset {
            fn drop(&mut self) {
                crate::commands::configure::set_semantic_refresh_restart_result_for_test(None);
            }
        }
        let _reset = RestartOverrideReset;

        let root = tempfile::tempdir().unwrap();
        let ctx = AppContext::new(
            default_language_provider_factory(),
            Config {
                project_root: Some(root.path().to_path_buf()),
                semantic_search: true,
                ..Config::default()
            },
        );
        ctx.set_canonical_cache_root(root.path().to_path_buf());
        *ctx.semantic_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(
            crate::semantic_index::SemanticIndex::new(root.path().to_path_buf(), 3),
        );
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::ready();
        let (_build_tx, build_rx) = crossbeam_channel::unbounded();
        let disconnected_build_epoch =
            ctx.install_semantic_index_rx(build_rx, ctx.configure_generation());
        let (request_tx, _request_rx) = crossbeam_channel::unbounded();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        ctx.install_semantic_refresh_worker_for_build_epoch(
            request_tx,
            event_rx,
            Arc::new(Mutex::new(None)),
            disconnected_build_epoch,
        );
        drop(event_tx);

        drain_semantic_refresh_events(&ctx);

        assert_eq!(
            crate::commands::configure::semantic_refresh_restart_attempts_for_test(),
            1
        );
        assert!(
            ctx.semantic_index()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_none(),
            "recovery must force a full reload rather than retain an index without a refresh worker"
        );
        assert!(ctx.semantic_refresh_event_rx().lock().is_none());
        assert!(
            ctx.semantic_index_rx().lock().is_none(),
            "a build receiver from the disconnected refresh generation must not be adopted"
        );
        assert!(matches!(
            &*ctx
                .semantic_index_status()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            SemanticIndexStatus::Building { stage, .. } if stage == "restarting_refresh_worker"
        ));
    }

    #[test]
    fn finished_refresh_worker_wakes_maintenance_after_last_event_is_drained() {
        let _guard = ARTIFACT_DRAIN_TEST_MUTEX.lock().unwrap();
        crate::commands::configure::set_semantic_refresh_restart_result_for_test(Some(true));
        struct RestartOverrideReset;
        impl Drop for RestartOverrideReset {
            fn drop(&mut self) {
                crate::commands::configure::set_semantic_refresh_restart_result_for_test(None);
            }
        }
        let _reset = RestartOverrideReset;

        let root = tempfile::tempdir().unwrap();
        let ctx = AppContext::new(
            default_language_provider_factory(),
            Config {
                project_root: Some(root.path().to_path_buf()),
                semantic_search: true,
                ..Config::default()
            },
        );
        ctx.set_canonical_cache_root(root.path().to_path_buf());
        *ctx.semantic_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(
            crate::semantic_index::SemanticIndex::new(root.path().to_path_buf(), 3),
        );
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::ready();

        let (request_tx, _request_rx) = crossbeam_channel::unbounded();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let (event_sent_tx, event_sent_rx) = crossbeam_channel::bounded(1);
        let (finish_tx, finish_rx) = crossbeam_channel::bounded(1);
        let worker = std::thread::spawn(move || {
            event_tx
                .send(SemanticRefreshEvent::Started { paths: Vec::new() })
                .unwrap();
            event_sent_tx.send(()).unwrap();
            finish_rx.recv().unwrap();
        });
        let worker_slot = Arc::new(Mutex::new(Some(worker)));
        ctx.install_semantic_refresh_worker_for_build_epoch(
            request_tx,
            event_rx,
            Arc::clone(&worker_slot),
            ctx.semantic_index_rx_epoch(),
        );
        event_sent_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        drain_semantic_refresh_events(&ctx);
        assert!(
            !ctx.completion_drains_have_work(),
            "a live worker with an empty event queue should not cause maintenance churn"
        );

        finish_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !ctx.completion_drains_have_work() {
            assert!(
                Instant::now() < deadline,
                "finished refresh worker did not wake maintenance"
            );
            std::thread::yield_now();
        }
        drain_semantic_refresh_events(&ctx);

        assert_eq!(
            crate::commands::configure::semantic_refresh_restart_attempts_for_test(),
            1
        );
        assert!(ctx.semantic_refresh_event_rx().lock().is_none());
    }

    #[test]
    fn semantic_disconnect_does_not_overwrite_replacement_loader_state() {
        let _guard = ARTIFACT_DRAIN_TEST_MUTEX.lock().unwrap();
        let root = tempfile::tempdir().unwrap();
        let ctx = Arc::new(AppContext::new(
            default_language_provider_factory(),
            Config {
                project_root: Some(root.path().to_path_buf()),
                semantic_search: true,
                ..Config::default()
            },
        ));
        *ctx.semantic_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(
            crate::semantic_index::SemanticIndex::new(root.path().to_path_buf(), 3),
        );
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::ready();
        let (old_request_tx, _old_request_rx) = crossbeam_channel::unbounded();
        let (old_event_tx, old_event_rx) = crossbeam_channel::unbounded();
        ctx.install_semantic_refresh_worker_for_build_epoch(
            old_request_tx,
            old_event_rx,
            Arc::new(Mutex::new(None)),
            ctx.semantic_index_rx_epoch(),
        );
        drop(old_event_tx);
        let (reached, release) = install_semantic_refresh_recovery_gate_for_test(&ctx);

        let drain_ctx = Arc::clone(&ctx);
        let drain = std::thread::spawn(move || drain_semantic_refresh_events(&drain_ctx));
        reached
            .recv_timeout(Duration::from_secs(2))
            .expect("old worker was not cleared before recovery");

        let (build_tx, build_rx) = crossbeam_channel::unbounded::<SemanticIndexEvent>();
        ctx.install_semantic_index_rx(build_rx, ctx.configure_generation());
        let (new_request_tx, _new_request_rx) = crossbeam_channel::unbounded();
        let (new_event_tx, new_event_rx) = crossbeam_channel::unbounded();
        ctx.install_semantic_refresh_worker_for_build_epoch(
            new_request_tx,
            new_event_rx,
            Arc::new(Mutex::new(None)),
            ctx.semantic_index_rx_epoch(),
        );
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Building {
            stage: "replacement_loader".to_string(),
            files: None,
            entries_done: None,
            entries_total: None,
        };
        release.send(()).unwrap();
        drain.join().unwrap();

        assert!(ctx.semantic_index_rx().lock().is_some());
        assert!(ctx.semantic_refresh_event_rx().lock().is_some());
        assert!(matches!(
            &*ctx
                .semantic_index_status()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            SemanticIndexStatus::Building { stage, .. } if stage == "replacement_loader"
        ));
        drop(build_tx);
        drop(new_event_tx);
    }

    #[test]
    fn semantic_disconnect_preserves_newer_build_receiver_before_refresh_install() {
        let _guard = ARTIFACT_DRAIN_TEST_MUTEX.lock().unwrap();
        let root = tempfile::tempdir().unwrap();
        let ctx = Arc::new(AppContext::new(
            default_language_provider_factory(),
            Config {
                project_root: Some(root.path().to_path_buf()),
                semantic_search: true,
                ..Config::default()
            },
        ));
        ctx.set_canonical_cache_root(root.path().to_path_buf());
        let (old_request_tx, _old_request_rx) = crossbeam_channel::unbounded();
        let (old_event_tx, old_event_rx) = crossbeam_channel::unbounded();
        ctx.install_semantic_refresh_worker_for_build_epoch(
            old_request_tx,
            old_event_rx,
            Arc::new(Mutex::new(None)),
            ctx.semantic_index_rx_epoch(),
        );
        drop(old_event_tx);
        let (reached, release) = install_semantic_refresh_recovery_gate_for_test(&ctx);

        let drain_ctx = Arc::clone(&ctx);
        let drain = std::thread::spawn(move || drain_semantic_refresh_events(&drain_ctx));
        reached
            .recv_timeout(Duration::from_secs(2))
            .expect("semantic refresh recovery did not reach the post-clear gate");

        let (_replacement_tx, replacement_rx) = crossbeam_channel::unbounded();
        ctx.install_semantic_index_rx(replacement_rx, ctx.configure_generation());
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Building {
            stage: "replacement_loader".to_string(),
            files: None,
            entries_done: None,
            entries_total: None,
        };
        release.send(()).unwrap();
        drain.join().unwrap();

        assert!(
            ctx.semantic_index_rx().lock().is_some(),
            "the old disconnect must not retire a newer build receiver while its refresh worker is being installed"
        );
        assert!(matches!(
            &*ctx
                .semantic_index_status()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            SemanticIndexStatus::Building { stage, .. } if stage == "replacement_loader"
        ));
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
                semantic_search: true,
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
    fn callgraph_phase_batches_all_indexed_paths_into_one_refresh() {
        let temp = tempfile::tempdir().unwrap();
        let (ctx, _) = context_with_watcher(temp.path());
        let generated = temp.path().join("compiled.ts");
        std::fs::write(&generated, "// @generated\nexport const compiled = true;\n").unwrap();
        let mut paths = VecDeque::from([
            temp.path().join("a.rs"),
            temp.path().join("b.ts"),
            generated,
            temp.path().join("ignored.txt"),
            temp.path().join("Cargo.toml"),
        ]);
        let mut remaining = paths.len();
        let mut refreshed = Vec::new();

        let completed = apply_callgraph_watcher_phase(
            &ctx,
            &mut paths,
            &mut remaining,
            Instant::now(),
            WATCHER_DRAIN_SLICE_BUDGET,
            true,
            |_, changed| refreshed.push(changed.clone()),
        );

        assert!(completed);
        assert_eq!(remaining, 0);
        assert_eq!(refreshed.len(), 1);
        assert_eq!(refreshed[0].len(), 3);
        assert!(refreshed[0].contains(&temp.path().join("Cargo.toml")));
    }

    #[test]
    fn callgraph_phase_includes_manifest_before_budget_yield() {
        let temp = tempfile::tempdir().unwrap();
        let (ctx, _) = context_with_watcher(temp.path());
        let source = temp.path().join("first.rs");
        let manifest = temp.path().join("Cargo.toml");
        let mut paths = VecDeque::from([source.clone(), manifest.clone()]);
        let mut remaining = paths.len();
        let mut refreshed = Vec::new();
        set_watcher_unit_test_seam(Duration::from_millis(2), None);

        let completed = apply_callgraph_watcher_phase(
            &ctx,
            &mut paths,
            &mut remaining,
            Instant::now(),
            Duration::from_millis(1),
            true,
            |_, changed| refreshed.push(changed.clone()),
        );
        clear_watcher_unit_test_seam();

        assert!(!completed);
        assert_eq!(remaining, 1);
        assert_eq!(refreshed.len(), 1);
        assert_eq!(refreshed[0], HashSet::from([source, manifest]));
    }

    #[test]
    fn callgraph_phase_flushes_once_per_slice_before_requeue() {
        let temp = tempfile::tempdir().unwrap();
        let (ctx, _) = context_with_watcher(temp.path());
        let mut paths =
            VecDeque::from([temp.path().join("first.rs"), temp.path().join("second.rs")]);
        let mut remaining = paths.len();
        let mut refreshed = Vec::new();
        set_watcher_unit_test_seam(Duration::from_millis(2), None);

        let first_completed = apply_callgraph_watcher_phase(
            &ctx,
            &mut paths,
            &mut remaining,
            Instant::now(),
            Duration::from_millis(1),
            true,
            |_, changed| refreshed.push(changed.clone()),
        );
        assert!(!first_completed);
        assert_eq!(remaining, 1);
        assert_eq!(refreshed.len(), 1, "the yielded slice must flush its batch");

        let second_completed = apply_callgraph_watcher_phase(
            &ctx,
            &mut paths,
            &mut remaining,
            Instant::now(),
            Duration::from_millis(1),
            true,
            |_, changed| refreshed.push(changed.clone()),
        );
        clear_watcher_unit_test_seam();

        assert!(!second_completed);
        assert_eq!(remaining, 0);
        assert_eq!(refreshed.len(), 2);
        assert!(refreshed.iter().all(|batch| batch.len() == 1));
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
    fn watcher_callgraph_refresh_defers_when_ready_store_is_unavailable() {
        let temp = tempfile::tempdir().unwrap();
        let (ctx, _) = context_with_watcher(temp.path());
        ctx.update_config(|config| config.callgraph_store = true);
        ctx.set_cache_role(false, None);
        let source = temp.path().join("pending.rs");
        let generated = temp.path().join("compiled.ts");
        std::fs::write(&generated, "// @generated\nexport const compiled = true;\n").unwrap();

        refresh_callgraph_store_for_watcher(&ctx, &HashSet::from([source.clone(), generated]));

        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            let pending = ctx.take_pending_callgraph_store_paths();
            if !pending.is_empty() {
                assert_eq!(pending, vec![source]);
                break;
            }
            assert!(
                Instant::now() < deadline,
                "refresh worker did not defer the unavailable store batch"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn watcher_callgraph_refresh_keeps_worktree_paths_pending() {
        let temp = tempfile::tempdir().unwrap();
        let (ctx, _) = context_with_watcher(temp.path());
        ctx.update_config(|config| config.callgraph_store = true);
        ctx.set_cache_role(true, None);
        let source = temp.path().join("worktree.rs");

        refresh_callgraph_store_for_watcher(&ctx, &HashSet::from([source.clone()]));

        assert_eq!(ctx.take_pending_callgraph_store_paths(), vec![source]);
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
        loop {
            let outcome = drain_watcher_events_bounded(&ctx, path_cap);
            slices += 1;
            processed += outcome.processed;
            assert!(outcome.processed <= path_cap);
            if !outcome.has_more {
                break;
            }
            assert!(slices < 8, "single dispatch event did not converge");
        }

        assert_eq!(processed, path_count);
        // At least ceil(1024/256) slices from the path budget; the 250ms time
        // budget may end a slice early under parallel test load, so an exact
        // slice count would be load-sensitive.
        assert!(
            (4..=8).contains(&slices),
            "expected 4-8 path-budgeted slices, got {slices}"
        );
        assert_eq!(ctx.pending_tier2_paths().len(), path_count);
    }

    #[test]
    fn watcher_rescan_supersedes_pending_paths() {
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
        assert_eq!(ctx.watcher_drain_pending_path_count(), 3);

        tx.send(WatcherDispatchEvent::RescanRequired).unwrap();
        let second = drain_watcher_events_bounded(&ctx, 2);

        assert_eq!(second.processed, 0);
        assert!(!second.has_more);
        assert_eq!(ctx.watcher_drain_pending_path_count(), 0);
    }

    #[test]
    fn watcher_lifecycle_generation_change_rebases_continuation() {
        let temp = tempfile::tempdir().unwrap();
        let (ctx, tx) = context_with_watcher(temp.path());
        tx.send(WatcherDispatchEvent::Paths(
            (0..5)
                .map(|index| temp.path().join(format!("old-generation-{index}.txt")))
                .collect(),
        ))
        .unwrap();
        let first = drain_watcher_events_bounded(&ctx, 2);
        assert_eq!(first.processed, 2);
        assert!(first.has_more);

        // A lifecycle-only generation change (transient unbind + equivalent
        // rebind) must NOT lose the in-flight paths: the continuation rebases
        // onto the new generation and keeps draining.
        ctx.advance_configure_generation();
        let second = drain_watcher_events_bounded(&ctx, 2);
        assert_eq!(second.processed, 2);
        assert!(second.has_more);

        let mut guard = 0;
        while drain_watcher_events_bounded(&ctx, 2).has_more {
            guard += 1;
            assert!(guard < 16, "rebased continuation must finish draining");
        }
        assert_eq!(ctx.watcher_drain_pending_path_count(), 0);
        assert_eq!(
            ctx.pending_tier2_paths().len(),
            5,
            "every path survives the lifecycle-only generation change"
        );
    }

    #[test]
    fn watcher_content_generation_change_discards_continuation() {
        let temp = tempfile::tempdir().unwrap();
        let (ctx, tx) = context_with_watcher(temp.path());
        tx.send(WatcherDispatchEvent::Paths(
            (0..5)
                .map(|index| temp.path().join(format!("old-content-{index}.txt")))
                .collect(),
        ))
        .unwrap();
        let first = drain_watcher_events_bounded(&ctx, 2);
        assert_eq!(first.processed, 2);
        assert!(first.has_more);

        // A real reconfigure (content change) rebuilds artifacts wholesale;
        // the stale continuation is discarded, not replayed.
        ctx.configure_content_generation_flag()
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        ctx.advance_configure_generation();
        let second = drain_watcher_events_bounded(&ctx, 2);

        assert_eq!(second.processed, 0);
        assert!(!second.has_more);
        assert_eq!(ctx.watcher_drain_pending_path_count(), 0);
        assert_eq!(ctx.pending_tier2_paths().len(), 2);
    }

    fn install_search_index(
        ctx: &AppContext,
        root: &Path,
        storage: &Path,
        file: &Path,
        contents: &[u8],
    ) -> (PathBuf, PathBuf) {
        let canonical = std::fs::canonicalize(root).expect("canonical root");
        ctx.update_config(|config| {
            config.storage_dir = Some(storage.to_path_buf());
            config.search_index = true;
        });
        ctx.set_canonical_cache_root(canonical.clone());
        let cache_dir = crate::search_index::resolve_cache_dir(&canonical, Some(storage));
        let mut index = crate::search_index::SearchIndex::build(&canonical);
        index.index_file(file, contents);
        let git_head = index.stored_git_head().map(str::to_owned);
        assert!(
            index.write_to_disk(&cache_dir, git_head.as_deref()),
            "owner write of cache.bin must succeed before borrow-only setup"
        );
        *ctx.search_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);
        (canonical, cache_dir)
    }

    fn grep_count(ctx: &AppContext, pattern: &str, root: &Path) -> usize {
        let index = ctx
            .search_index()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        index
            .as_ref()
            .expect("resident search index")
            .grep(pattern, true, &[], &[], root, 10)
            .matches
            .len()
    }

    fn cache_bin_hash(path: &Path) -> blake3::Hash {
        blake3::hash(&std::fs::read(path).expect("read cache.bin"))
    }

    fn search_index_has_pending_disk_changes(ctx: &AppContext) -> bool {
        ctx.search_index()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|index| index.has_pending_disk_changes())
    }

    fn mark_borrow_only(ctx: &AppContext, canonical: &Path, cache_dir: &Path) {
        let project_key = cache_dir
            .file_name()
            .and_then(|name| name.to_str())
            .expect("cache key")
            .to_string();
        crate::root_cache::configure_artifact_access(canonical, &project_key, true);
        ctx.set_cache_role(true, None);
    }

    #[test]
    fn ram_overlay_is_off_by_default_and_transport_independent() {
        let temp = tempfile::tempdir().unwrap();
        let (ctx, _) = context_with_watcher(temp.path());
        ctx.set_cache_role(true, None);
        assert!(
            ctx.shared_artifacts_read_only(),
            "worktree role is borrow-only"
        );
        assert!(
            !ctx.ram_overlay_active(),
            "worktree.ram_overlay defaults off"
        );

        ctx.update_config(|config| config.worktree.ram_overlay = true);
        assert!(
            ctx.ram_overlay_active(),
            "overlay follows config + borrow-only, not bind/transport identity"
        );

        ctx.set_cache_role(false, None);
        assert!(
            !ctx.ram_overlay_active(),
            "owner roots stay on the normal writer path even when the gate is on"
        );
    }

    #[test]
    fn borrow_only_watcher_arms_are_noops_until_overlay_flips_on() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let (ctx, tx) = context_with_watcher(root.path());
        let file = std::fs::canonicalize(root.path())
            .expect("canonical root")
            .join("overlay.rs");
        std::fs::write(&file, "old overlay token\n").expect("write source");
        let (canonical, cache_dir) = install_search_index(
            &ctx,
            root.path(),
            storage.path(),
            &file,
            b"old overlay token\n",
        );
        mark_borrow_only(&ctx, &canonical, &cache_dir);
        assert!(ctx.shared_artifacts_read_only());
        assert!(!ctx.ram_overlay_active());
        assert!(
            !search_index_has_pending_disk_changes(&ctx),
            "precondition: compacted owner write leaves no RAM delta"
        );

        std::fs::write(&file, "new overlay token\n").expect("edit source");
        tx.send(WatcherDispatchEvent::Paths(vec![file.clone()]))
            .unwrap();
        drain_watcher_events(&ctx);
        assert!(
            !search_index_has_pending_disk_changes(&ctx),
            "gate off: borrow-only watcher must not mutate the RAM delta"
        );

        ctx.update_config(|config| config.worktree.ram_overlay = true);
        assert!(ctx.ram_overlay_active());
        tx.send(WatcherDispatchEvent::Paths(vec![file])).unwrap();
        drain_watcher_events(&ctx);
        assert!(
            search_index_has_pending_disk_changes(&ctx),
            "flipping the predicate on must let the same watcher arm apply"
        );
        assert_eq!(
            grep_count(&ctx, "new overlay token", &canonical),
            1,
            "overlay-on drain must make the disk edit searchable from RAM"
        );
    }

    #[test]
    fn ram_overlay_search_reflects_edits_without_writing_cache_bin() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let (ctx, tx) = context_with_watcher(root.path());
        let file = std::fs::canonicalize(root.path())
            .expect("canonical root")
            .join("overlay.rs");
        std::fs::write(&file, "old overlay token\n").expect("write source");
        let (canonical, cache_dir) = install_search_index(
            &ctx,
            root.path(),
            storage.path(),
            &file,
            b"old overlay token\n",
        );
        mark_borrow_only(&ctx, &canonical, &cache_dir);
        ctx.update_config(|config| config.worktree.ram_overlay = true);

        *ctx.semantic_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(
            crate::semantic_index::SemanticIndex::new(canonical.clone(), 8),
        );
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::ready();

        let cache_path = cache_dir.join("cache.bin");
        let before = cache_bin_hash(&cache_path);

        std::fs::write(&file, "new overlay token\n").expect("edit source");
        tx.send(WatcherDispatchEvent::Paths(vec![file.clone()]))
            .unwrap();
        drain_watcher_events(&ctx);

        assert_eq!(
            grep_count(&ctx, "new overlay token", &canonical),
            1,
            "overlay must serve the edit from the RAM delta"
        );
        assert_eq!(grep_count(&ctx, "old overlay token", &canonical), 0);
        assert_eq!(
            cache_bin_hash(&cache_path),
            before,
            "overlay must not change on-disk cache.bin bytes"
        );

        {
            // Family key != this checkout's path identity, matching a linked
            // worktree that borrows the owner's shared artifact key.
            let family_key = "shared-family-overlay-key";
            crate::root_cache::configure_artifact_access(&canonical, family_key, true);
            let family_dir = storage.path().join(family_key);
            let mut index = ctx
                .search_index()
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(
                !index
                    .as_mut()
                    .expect("resident search index")
                    .write_to_disk(&family_dir, None),
                "direct persist must fail-closed for a borrow-only shared key"
            );
        }
        assert!(
            !ctx.flush_search_index_on_graceful_shutdown(),
            "shutdown must not flush a borrow-only overlay root"
        );
        assert_eq!(
            cache_bin_hash(&cache_path),
            before,
            "shutdown and persist must leave cache.bin byte-identical"
        );

        let refreshing = match &*ctx
            .semantic_index_status()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            SemanticIndexStatus::Ready { refreshing, .. } => refreshing.clone(),
            other => panic!("expected Ready semantic status, got {other:?}"),
        };
        assert!(
            refreshing.is_empty(),
            "semantic arm must stay frozen under the overlay"
        );
    }

    #[test]
    fn owner_root_still_uses_normal_path_when_overlay_gate_is_on() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let (ctx, tx) = context_with_watcher(root.path());
        let file = std::fs::canonicalize(root.path())
            .expect("canonical root")
            .join("owner.rs");
        std::fs::write(&file, "old owner token\n").expect("write source");
        let (canonical, cache_dir) = install_search_index(
            &ctx,
            root.path(),
            storage.path(),
            &file,
            b"old owner token\n",
        );
        ctx.update_config(|config| config.worktree.ram_overlay = true);
        assert!(!ctx.shared_artifacts_read_only(), "owner root is a writer");
        assert!(!ctx.ram_overlay_active());

        std::fs::write(&file, "new owner token\n").expect("edit source");
        tx.send(WatcherDispatchEvent::Paths(vec![file])).unwrap();
        drain_watcher_events(&ctx);
        assert_eq!(grep_count(&ctx, "new owner token", &canonical), 1);
        assert_eq!(grep_count(&ctx, "old owner token", &canonical), 0);

        assert!(
            ctx.flush_search_index_on_graceful_shutdown(),
            "owner persist path must still flush when the overlay gate is on"
        );
        let mut restored = crate::search_index::SearchIndex::read_from_disk(&cache_dir, &canonical)
            .expect("reload owner cache.bin");
        restored.set_ready(true);
        assert_eq!(
            restored
                .grep("new owner token", true, &[], &[], &canonical, 10)
                .matches
                .len(),
            1,
            "owner shutdown flush must persist the RAM delta"
        );
    }
}

// Every test here spawns a real child process, so the module is Unix-only.
#[cfg(all(test, unix))]
mod idle_lsp_tests {
    use super::shutdown_idle_lsp_at;
    use crate::config::Config;
    use crate::context::AppContext;
    use crate::lsp::child_registry::LspChildRegistry;
    use crate::lsp::client::LspClient;
    use crate::lsp::registry::ServerKind;
    use std::collections::HashMap;
    use std::path::Path;
    use std::time::{Duration, Instant};

    #[cfg(unix)]
    fn spawn_sleep_client(ctx: &AppContext, root: std::path::PathBuf) -> u32 {
        let registry = LspChildRegistry::new();
        ctx.lsp().set_child_registry(registry.clone());
        let client = LspClient::spawn(
            ServerKind::TypeScript,
            root,
            Path::new("sh"),
            &["-c".to_string(), "exec sleep 60".to_string()],
            &HashMap::new(),
            ctx.lsp().event_sender_for_test(),
            registry,
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
            if !crate::bash_background::process::is_process_alive(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[cfg(unix)]
    #[test]
    fn idle_lsp_ttl_shuts_down_stale_client_and_keeps_recent() {
        let tmp = tempfile::tempdir().unwrap();
        let mut stale_config = Config::default();
        stale_config.idle.lsp_ttl_minutes = 1;
        let stale = AppContext::new(
            crate::context::default_language_provider_factory(),
            stale_config,
        );
        let pid = spawn_sleep_client(&stale, tmp.path().to_path_buf());
        assert_eq!(stale.lsp().server_count(), 1);
        let now = Instant::now();
        stale.set_last_request_at_for_test(now - Duration::from_secs(61));
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
            "a root idle longer than lsp_ttl_minutes must shut down its language servers"
        );
        assert!(
            wait_until_dead(pid, Duration::from_secs(6)),
            "idle-reaped child must die within SHUTDOWN_TIMEOUT + 1s"
        );

        let mut fresh_config = Config::default();
        fresh_config.idle.lsp_ttl_minutes = 1;
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
}
