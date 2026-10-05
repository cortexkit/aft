use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Condvar, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Map, Value};

use crate::alert_state::{AcceptedDiagnosticSnapshot, AcceptedObservationBatch};
use crate::config::Config;
use crate::context::AppContext;
use crate::inspect::diagnostics_category::{inspect_request_timeout, run_diagnostics_category};
#[cfg(test)]
use crate::inspect::InspectBuilderState;
use crate::inspect::{
    format_wait_text, InspectCache, InspectCategory, InspectPhaseEntry, InspectPhaseId,
    InspectPhaseLog, InspectSnapshot, JobOutcome, JobScope,
};
use crate::lsp::client::RustCheckState;
use crate::lsp::manager::{
    ApplicabilityResolutionError, ApplicableServerFailure, ApplicableServerSnapshot,
    ApplicableServerStartOutcomes, NotApplicableServer,
};
use crate::lsp::roots::ServerKey;
use crate::protocol::{RawRequest, Response};
use crate::response_finalize::{DispatchOutcome, PendingResponse};

const DEFAULT_TOP_K: usize = 20;
const MAX_TOP_K: usize = 100;
// The remaining reasons stay in the body and structured gaps. This is a
// presentation limit, not a limit on analysis or completion accounting.
const MAX_INSPECT_HEADER_PARTS: usize = 3;
// Give each waiting phase at most half the remaining work time so a slow
// producer leaves room for other categories and final freshness verification.
// The cap avoids turning large configured budgets into equally long waits.
const INSPECT_PHASE_WAIT_CAP: Duration = Duration::from_secs(60);
/// Reserve time inside the configured request budget for terminal assembly and
/// egress. The server always answers before the client gives up: server work
/// stops before `diagnostics_timeout_ms`, while the client waits for that budget
/// plus its transport headroom.
const INSPECT_TERMINAL_MARGIN: Duration = Duration::from_secs(5);

struct ParsedScope {
    job: JobScope,
    roots: Vec<PathBuf>,
}

#[derive(Clone, Copy, Debug)]
struct InspectRequestDeadline {
    budget: Duration,
    terminal_at: Instant,
    work_at: Instant,
}

impl InspectRequestDeadline {
    fn from_config(config: &crate::config::Config) -> Self {
        Self::new(inspect_request_timeout(config), INSPECT_TERMINAL_MARGIN)
    }

    fn new(budget: Duration, terminal_margin: Duration) -> Self {
        let started = Instant::now();
        let terminal_at = started + budget;
        let work_at = started + budget.saturating_sub(terminal_margin);
        Self {
            budget,
            terminal_at,
            work_at,
        }
    }

    fn work_at(self) -> Instant {
        self.work_at
    }

    fn phase_deadline(self, phase_limit: Duration) -> Instant {
        let now = Instant::now();
        now + (self.work_at.saturating_duration_since(now) / 2).min(phase_limit)
    }

    fn has_work_budget(self) -> bool {
        Instant::now() < self.work_at
    }

    fn timeout_detail(self, phase: InspectPhaseId) -> String {
        format!(
            "inspect_request_timeout: {} could not complete within the {}ms request budget ({}ms terminal reserve)",
            phase.as_str(),
            self.budget.as_millis(),
            self.terminal_at
                .saturating_duration_since(self.work_at)
                .as_millis(),
        )
    }
}

static DEFERRED_INSPECT_ROOTS: LazyLock<(Mutex<BTreeSet<PathBuf>>, Condvar)> =
    LazyLock::new(|| (Mutex::new(BTreeSet::new()), Condvar::new()));

#[cfg(test)]
struct DeferredInspectBodyGate {
    started: mpsc::SyncSender<()>,
    release: mpsc::Receiver<()>,
}

#[cfg(test)]
static DEFERRED_INSPECT_BODY_GATE: LazyLock<Mutex<Option<DeferredInspectBodyGate>>> =
    LazyLock::new(|| Mutex::new(None));
#[cfg(test)]
static DEFERRED_INSPECT_TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
#[cfg(test)]
static DEFERRED_INSPECT_SHORT_CIRCUIT_TO_STAT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
pub(crate) fn deferred_inspect_test_lock() -> std::sync::MutexGuard<'static, ()> {
    DEFERRED_INSPECT_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
pub(crate) fn install_deferred_inspect_body_gate_for_test(
) -> (mpsc::Receiver<()>, mpsc::SyncSender<()>) {
    let (started_tx, started_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    *DEFERRED_INSPECT_BODY_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(DeferredInspectBodyGate {
        started: started_tx,
        release: release_rx,
    });
    (started_rx, release_tx)
}

#[cfg(test)]
pub(crate) fn install_deferred_inspect_stat_gate_for_test(
) -> (mpsc::Receiver<()>, mpsc::SyncSender<()>) {
    DEFERRED_INSPECT_SHORT_CIRCUIT_TO_STAT.store(true, std::sync::atomic::Ordering::SeqCst);
    install_deferred_inspect_body_gate_for_test()
}

#[cfg(test)]
pub(crate) fn deferred_inspect_root_count_for_test() -> usize {
    DEFERRED_INSPECT_ROOTS
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .len()
}

#[cfg(test)]
fn wait_at_deferred_inspect_body_gate_for_test(deadline: InspectRequestDeadline) {
    let gate = DEFERRED_INSPECT_BODY_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    let Some(gate) = gate else {
        return;
    };
    let _ = gate.started.send(());
    let hang_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if inspect_cancellation_requested()
            || Instant::now() >= hang_deadline
            || !deadline.has_work_budget()
        {
            return;
        }
        match gate.release.recv_timeout(Duration::from_millis(5)) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

#[cfg(not(test))]
fn wait_at_deferred_inspect_body_gate_for_test(_deadline: InspectRequestDeadline) {}

#[cfg(test)]
fn take_deferred_inspect_stat_short_circuit_for_test() -> bool {
    DEFERRED_INSPECT_SHORT_CIRCUIT_TO_STAT.swap(false, std::sync::atomic::Ordering::SeqCst)
}

#[cfg(not(test))]
fn take_deferred_inspect_stat_short_circuit_for_test() -> bool {
    false
}

// Host abort tests need the call to remain in flight, regardless of project size.
// This environment-only seam also obeys the request budget and cancellation so
// an abort never has to wait for the artificial delay to expire.
fn delay_inspect_body_from_env_for_test(deadline: InspectRequestDeadline) {
    let Some(delay) = std::env::var("AFT_TEST_INSPECT_DELAY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
    else {
        return;
    };
    wait_inspect_test_delay(
        delay,
        deadline,
        crate::executor::current_job_cancellation().as_ref(),
    );
}

fn wait_inspect_test_delay(
    delay: Duration,
    deadline: InspectRequestDeadline,
    cancellation: Option<&crate::executor::JobCancellation>,
) {
    let started = Instant::now();
    loop {
        let remaining = delay
            .saturating_sub(started.elapsed())
            .min(deadline.work_at().saturating_duration_since(Instant::now()));
        if remaining.is_zero() {
            return;
        }
        let wait = remaining.min(Duration::from_millis(50));
        if let Some(token) = cancellation {
            if token.wait_for_cancellation(wait) {
                return;
            }
        } else {
            std::thread::sleep(wait);
        }
    }
}

struct DeferredInspectRootPermit {
    root: PathBuf,
}

impl DeferredInspectRootPermit {
    fn acquire(root: PathBuf, deadline: InspectRequestDeadline) -> Option<Self> {
        let (roots, changed) = &*DEFERRED_INSPECT_ROOTS;
        let mut active = roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while active.contains(&root) {
            if inspect_cancellation_requested() || !deadline.has_work_budget() {
                return None;
            }
            let wait = Duration::from_millis(50)
                .min(deadline.work_at().saturating_duration_since(Instant::now()));
            let (next, _) = changed
                .wait_timeout(active, wait)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            active = next;
        }
        if inspect_cancellation_requested() || !deadline.has_work_budget() {
            return None;
        }
        active.insert(root.clone());
        Some(Self { root })
    }
}

impl Drop for DeferredInspectRootPermit {
    fn drop(&mut self) {
        let (roots, changed) = &*DEFERRED_INSPECT_ROOTS;
        let mut active = roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        active.remove(&self.root);
        changed.notify_one();
    }
}

#[derive(Debug, Eq, PartialEq)]
struct InspectRootStatSnapshot(Vec<(PathBuf, u64, SystemTime)>);

fn capture_inspect_root_stats_until(
    root: &Path,
    deadline: Option<InspectRequestDeadline>,
) -> Result<InspectRootStatSnapshot, String> {
    let mut files = Vec::new();
    for file in crate::callgraph::walk_project_files(root) {
        if deadline.is_some_and(|deadline| !deadline.has_work_budget()) {
            return Err(deadline
                .expect("checked request deadline")
                .timeout_detail(InspectPhaseId::StatVerification));
        }
        match std::fs::metadata(&file) {
            Ok(metadata) => files.push((
                file,
                metadata.len(),
                metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err("a project file changed during inspect stat verification".to_string());
            }
            Err(error) => {
                return Err(format!(
                    "failed to stat {} during inspect verification: {error}",
                    file.display()
                ));
            }
        }
    }
    files.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(InspectRootStatSnapshot(files))
}

fn verify_final_root_stats(
    project_root: &Path,
    initial_stats: &InspectRootStatSnapshot,
    phase_log: &InspectPhaseLog,
    deadline: InspectRequestDeadline,
) -> Result<(), InspectTerminal> {
    let phase_entry =
        InspectPhaseEntry::category(InspectPhaseId::StatVerification, InspectCategory::Metrics)
            .with_also_satisfied(InspectCategory::active().iter().copied());
    if !deadline.has_work_budget() {
        return Err(request_deadline_terminal(Some(phase_entry), deadline));
    }
    let stat_phase = phase_log.start(phase_entry);
    match capture_inspect_root_stats_until(project_root, Some(deadline)) {
        Ok(final_stats) if final_stats == *initial_stats => {
            stat_phase.complete();
            Ok(())
        }
        Ok(_) => {
            stat_phase.fail("project files changed while inspect was running");
            Err(InspectTerminal::Interrupted)
        }
        Err(detail) if detail.contains("inspect_request_timeout") => {
            stat_phase.fail(&detail);
            Err(InspectTerminal::PhaseFailed {
                failed_phase: Some(InspectPhaseEntry::category(
                    InspectPhaseId::StatVerification,
                    InspectCategory::Metrics,
                )),
                failure_reason: "inspect_request_timeout",
                failure_detail: Some(detail),
            })
        }
        Err(detail) => {
            stat_phase.fail(&detail);
            Err(InspectTerminal::PhaseFailed {
                failed_phase: Some(InspectPhaseEntry::category(
                    InspectPhaseId::StatVerification,
                    InspectCategory::Metrics,
                )),
                failure_reason: "inspect_not_fresh",
                failure_detail: Some(detail),
            })
        }
    }
}

pub fn handle_inspect(req: &RawRequest, ctx: &AppContext) -> Response {
    handle_inspect_payload(req, ctx, false, false, &[], &[], &[], &[], None, None, None)
}

/// Resolve the language servers an inspect should start, within the request
/// deadline. A scoped inspect considers only files inside the scope: a server
/// is selected only when at least one scoped file is one it handles, so a
/// scope of `.rs` files never starts TypeScript because some `.mjs` file lives
/// elsewhere in the project, and rust-analyzer starts only for the Cargo
/// workspace that owns the scoped Rust files.
///
/// The filesystem walk runs without the language-server manager lock; only
/// the per-server classification takes it. The walk covers the whole
/// inspected tree, and while it held the lock every other manager user
/// waited, including the standalone request loop, so a sibling `read` sent
/// during an inspect was not answered until the walk finished.
fn resolve_inspect_applicability(
    ctx: &AppContext,
    project_root: &Path,
    scoped_roots: Option<&[PathBuf]>,
    config: &Config,
    deadline: Instant,
) -> Result<ApplicableServerSnapshot, ApplicabilityResolutionError> {
    let walk = crate::lsp::manager::walk_applicable_area(
        project_root,
        scoped_roots,
        config,
        Some(deadline),
    )?;
    Ok(ctx.lsp().classify_applicable_servers(walk, config))
}

/// Test-only warm-path entry that preserves nonblocking diagnostics semantics
/// while waiting on scanner completion events with the normal phase hang catch.
/// Integration fixtures use it when their assertion is unrelated to the
/// one-second interactive soft deadline.
#[doc(hidden)]
pub fn handle_inspect_warm_for_test(req: &RawRequest, ctx: &AppContext) -> Response {
    let phase_log = InspectPhaseLog::for_request(req.id.clone());
    handle_inspect_payload(
        req,
        ctx,
        false,
        false,
        &[],
        &[],
        &[],
        &[],
        Some(&phase_log),
        None,
        None,
    )
}

pub fn handle_inspect_tool_call(req: &RawRequest, ctx: &AppContext) -> Response {
    let phase_log = InspectPhaseLog::for_request(req.id.clone());
    let deadline = InspectRequestDeadline::from_config(&ctx.config());
    let snapshot = match inspect_preflight(req, ctx) {
        Ok(snapshot) => snapshot,
        Err(response) => {
            if matches!(
                response.data.get("code").and_then(Value::as_str),
                Some("path_not_found" | "path_outside_root")
            ) {
                return response;
            }
            let detail = response
                .data
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned);
            return build_inspect_terminal(
                &req.id,
                &phase_log,
                InspectTerminal::PhaseFailed {
                    failed_phase: None,
                    failure_reason: "root_resolution_failed",
                    failure_detail: detail,
                },
            );
        }
    };
    let scope = parse_scope(req, ctx, &snapshot.project_root)
        .expect("inspect preflight already validated the request scope");
    let scoped_roots = (!scope.roots.is_empty()).then_some(scope.roots.as_slice());
    let applicability = resolve_inspect_applicability(
        ctx,
        &snapshot.project_root,
        scoped_roots,
        &snapshot.config,
        deadline.work_at(),
    );
    let response = match applicability {
        Ok(applicability) => {
            run_blocking_inspect_body(req, ctx, applicability, phase_log, deadline)
        }
        Err(error) => build_inspect_terminal(
            &req.id,
            &phase_log,
            InspectTerminal::PhaseFailed {
                failed_phase: None,
                failure_reason: applicability_failure_reason(&error),
                failure_detail: Some(applicability_failure_detail(error)),
            },
        ),
    };
    let status = if response.success { "ok" } else { "error" };
    ctx.note_index_query(crate::logging::IndexPlane::Tier2, "inspect", 0, status);
    response
}

/// Diagnostics read the warm working set, with or without a request scope.
/// A blocking scoped request first has its started language servers analyze
/// the scoped files (see `scoped_diagnostics_sweep`); scope then filters the
/// rendered findings and adds per-file authority (named gaps for scoped files
/// no producer has authoritatively analyzed).
fn handle_inspect_payload(
    req: &RawRequest,
    ctx: &AppContext,
    force_root_diagnostics: bool,
    applicability_is_empty: bool,
    producer_failures: &[ApplicableServerFailure],
    not_applicable: &[NotApplicableServer],
    expected_producers: &[ServerKey],
    indexing_gaps: &[(ServerKey, String)],
    phase_log: Option<&InspectPhaseLog>,
    request_deadline: Option<InspectRequestDeadline>,
    observed_stats: Option<&InspectRootStatSnapshot>,
) -> Response {
    let top_k = match parse_top_k(&req.params) {
        Ok(top_k) => top_k,
        Err(message) => return invalid_request(&req.id, message),
    };
    let sections = match parse_sections(req.params.get("sections")) {
        Ok(sections) => sections,
        Err(message) => return invalid_request(&req.id, message),
    };

    let scope_was_provided = scope_was_provided(req.params.get("scope"));
    let snapshot = match build_snapshot(ctx) {
        Ok(snapshot) => snapshot,
        Err(response) => return response.with_id(&req.id),
    };
    let parsed_scope = match parse_scope(req, ctx, &snapshot.project_root) {
        Ok(scope) => scope,
        Err(response) => return response,
    };
    let scope_was_provided = scope_was_provided && !parsed_scope.roots.is_empty();
    let scope_roots = scope_was_provided.then_some(parsed_scope.roots.as_slice());
    let scope = parsed_scope.job;

    if inspect_cancellation_requested() {
        return inspect_interrupted_response(&req.id);
    }

    // Wait while the request's config/query guard is alive. The result is pinned
    // once and handed into workers; they must not open another generation.
    let routed_store = if ctx.checkout_query_runtime_active() {
        ctx.callgraph_store_for_ops()
    } else {
        crate::context::CallgraphStoreAccess::Unavailable
    };
    let checkout_routed = ctx.checkout_query_outcome().is_some();
    let checkout_store = if checkout_routed {
        match routed_store {
            crate::context::CallgraphStoreAccess::Ready(store) => Some(store),
            _ => None,
        }
    } else {
        None
    };
    let manager = ctx.inspect_manager();
    // A writer-backed unscoped request must retain contribution reuse. Only
    // requests the legacy scanner cannot serve need an ephemeral view scan.
    let needs_ephemeral_view = scope_was_provided || !ctx.inspect_writer();
    let checkout_store =
        if needs_ephemeral_view && snapshot.config.views.enabled && !checkout_routed {
            manager.current_checkout_view(&snapshot, observed_stats.map(|stats| stats.0.as_slice()))
        } else {
            checkout_store
        };
    let use_checkout_view = needs_ephemeral_view
        && snapshot.config.views.enabled
        && (checkout_routed || checkout_store.is_some());
    let blocking_tier1_deadline = phase_log.map(|_| {
        request_deadline.map_or_else(
            || Instant::now() + inspect_request_timeout(snapshot.config.as_ref()),
            |deadline| deadline.phase_deadline(INSPECT_PHASE_WAIT_CAP),
        )
    });
    let mut outcomes = BTreeMap::new();
    if blocking_tier1_deadline.is_none() {
        // The nonblocking path gives each Tier-1 scan a short soft deadline. Join
        // those completion events before queuing parse-heavy Tier-2 work so the
        // request cannot consume its own budget waiting behind work it enqueued.
        for category in [InspectCategory::Metrics, InspectCategory::Todos] {
            if inspect_cancellation_requested() {
                return inspect_interrupted_response(&req.id);
            }
            let outcome = manager.submit_category_with_callgraph(
                snapshot.clone(),
                category,
                scope.clone(),
                None,
            );
            outcomes.insert(category, outcome);
        }
    }
    let mut tier2_receivers = BTreeMap::new();
    for category in InspectCategory::active()
        .iter()
        .copied()
        .filter(|category| category.is_tier2())
    {
        if (scope_was_provided || !ctx.inspect_writer()) && !use_checkout_view {
            continue;
        }
        if request_deadline.is_some_and(|deadline| !deadline.has_work_budget()) {
            outcomes.insert(
                category,
                JobOutcome::Failed {
                    message: request_deadline
                        .expect("checked request deadline")
                        .timeout_detail(InspectPhaseId::Tier2Rescan),
                },
            );
            continue;
        }
        let manager = manager.clone();
        let checkout_store = checkout_store.clone();
        let snapshot = snapshot.clone();
        let scope = scope.clone();
        let callgraph_phase = phase_log.and_then(|phase_log| {
            if category != InspectCategory::DeadCode {
                return None;
            }
            // Shared with dead_code projection (stale backend rows are not
            // ready). Completion is owned by finish_tier2_phases from the
            // builder aggregate so this check cannot mark callgraph_ready
            // while the builder still reports callgraph_unavailable.
            if !manager.callgraph_ready_for_snapshot(&snapshot) {
                crate::slog_debug!("tier2 dead_code: callgraph store not ready at inspect start");
            }
            Some(phase_log.start(InspectPhaseEntry::category(
                InspectPhaseId::CallgraphReady,
                category,
            )))
        });
        let tier2_phase = phase_log.map(|phase_log| {
            phase_log.start(InspectPhaseEntry::category(
                InspectPhaseId::Tier2Rescan,
                category,
            ))
        });
        let (tx, rx) = std::sync::mpsc::channel();
        let cancellation = crate::executor::current_job_cancellation();
        std::thread::spawn(move || {
            let _cancellation = cancellation.map(crate::executor::install_job_cancellation);
            let outcome = if use_checkout_view {
                manager.tier2_run_with_pinned_view(snapshot, category, scope, checkout_store)
            } else if force_root_diagnostics {
                manager.tier2_run_with_reuse_blocking_fresh(snapshot, category, scope)
            } else {
                manager.tier2_run_with_reuse_blocking(snapshot, category, scope)
            };
            let _ = tx.send(outcome);
        });
        tier2_receivers.insert(
            category,
            (
                rx,
                request_deadline.map_or_else(
                    || std::time::Instant::now() + INSPECT_PHASE_WAIT_CAP,
                    |deadline| deadline.phase_deadline(INSPECT_PHASE_WAIT_CAP),
                ),
                callgraph_phase,
                tier2_phase,
            ),
        );
    }

    // Diagnostics goes last: a blocking scoped request spends part of the
    // budget asking language servers to analyze the scoped files, and the
    // other categories' scans must not find their shared deadline already
    // used up by that wait.
    let ordered_categories = InspectCategory::active()
        .iter()
        .filter(|category| **category != InspectCategory::Diagnostics)
        .chain(
            InspectCategory::active()
                .iter()
                .filter(|category| **category == InspectCategory::Diagnostics),
        );
    for category in ordered_categories {
        if outcomes.contains_key(category) {
            continue;
        }
        if inspect_cancellation_requested() {
            return inspect_interrupted_response(&req.id);
        }
        let outcome = if *category == InspectCategory::Diagnostics {
            // Read the warm LSP store with named gaps for producers whose wait
            // expired. A blocking scoped request first has its started servers
            // analyze the scoped files, within half the remaining budget.
            run_diagnostics_category(
                ctx,
                &snapshot,
                &scope,
                scope_was_provided,
                applicability_is_empty,
                producer_failures,
                not_applicable,
                expected_producers,
                indexing_gaps,
                request_deadline
                    .filter(|_| scope_was_provided)
                    .map(|deadline| deadline.phase_deadline(INSPECT_PHASE_WAIT_CAP)),
            )
        } else if category.is_tier2() {
            if let Some((rx, deadline, callgraph_phase, tier2_phase)) =
                tier2_receivers.remove(category)
            {
                match receive_tier2_completion_until(
                    rx,
                    manager.as_ref(),
                    *category,
                    deadline,
                    request_deadline,
                ) {
                    Some(outcome) => {
                        finish_tier2_phases(&outcome, callgraph_phase, tier2_phase);
                        outcome
                    }
                    None => return inspect_interrupted_response(&req.id),
                }
            } else {
                // A read-only daemon may serve a cached aggregate only when the
                // stat-verification path proves that artifact is still current.
                manager.tier2_read_cached_readonly(snapshot.clone(), *category, scope.clone())
            }
        } else if let Some(deadline) = blocking_tier1_deadline {
            manager.submit_category_until(snapshot.clone(), *category, scope.clone(), deadline)
        } else {
            manager.submit_category_with_callgraph(snapshot.clone(), *category, scope.clone(), None)
        };
        outcomes.insert(*category, outcome);
    }

    // Truthful fleet-status values update from whatever this collection proved,
    // even when the freshness gate below refuses the payload: a verified count
    // stays verified, and pending or failed categories remain absent rather
    // than reading as zero.
    refresh_status_bar_counts(ctx, &outcomes);

    // Scoped inspection may compute from its own immutable view, but never
    // schedules or joins a legacy project-wide build. Missing cache rows are gaps.
    if scope_was_provided {
        for (category, outcome) in &mut outcomes {
            if category.is_tier2() && !matches!(outcome, JobOutcome::Fresh { .. }) {
                let reason = match &*outcome {
                    JobOutcome::Failed { message } => message.clone(),
                    JobOutcome::Stale { .. } => "cached source contributions, file set, or analysis configuration changed; freshness could not be verified".into(),
                    _ if snapshot.config.views.enabled => "checkout call graph view is not ready; no legacy or borrowed analysis was substituted".into(),
                    _ if ctx.is_worktree_bridge() => "analysis not available in this worktree; scoped inspection does not run Tier-2".into(),
                    _ => "analysis not ready; scoped inspection does not wait for Tier-2".into(),
                };
                *outcome = JobOutcome::Fresh {
                    payload: manager.tier2_incomplete_payload(
                        &snapshot,
                        *category,
                        &scope,
                        "tier2_unavailable",
                        reason,
                    ),
                };
            }
        }
    }
    if request_deadline.is_some() {
        // Completed categories survive another scanner's budget exhaustion. Do
        // not reuse unverified stale rows as if they were current findings.
        for (category, outcome) in outcomes.iter_mut() {
            if !matches!(outcome, JobOutcome::Fresh { .. }) {
                let reason = match &*outcome {
                    JobOutcome::Failed { message } => message.clone(),
                    _ => "analysis did not finish within its wait budget".to_string(),
                };
                // Name the scanner that did not finish; the gap has no
                // language server behind it.
                let producer = if category.is_tier2() {
                    format!("{} analysis (Tier-2)", category.as_str())
                } else {
                    format!("{} scanner", category.as_str())
                };
                let payload = if category.is_tier2() {
                    manager.tier2_incomplete_payload(
                        &snapshot,
                        *category,
                        &scope,
                        "analysis_incomplete",
                        reason,
                    )
                } else {
                    serde_json::json!({
                        "unavailable": true, "complete": false,
                        "gaps": [{"kind": "analysis_incomplete", "producer": producer, "reason": format!("{reason}; retry aft_inspect") }]
                    })
                };
                *outcome = JobOutcome::Fresh { payload };
            }
        }
    }
    let payloads = match fresh_payloads(&outcomes) {
        Ok(payloads) => payloads,
        Err(message) => return Response::error(&req.id, "inspect_not_fresh", message),
    };

    let mut payload =
        build_inspect_payload(&snapshot, &payloads, &sections, top_k, ctx, scope_roots);
    // A scoped answer carries only the notes of servers for its own files; a
    // TypeScript SDK note from a server started for another request does not
    // belong in an answer about Rust files.
    let runtime_notes = if scope_was_provided {
        let producers =
            scope_producer_keys(&snapshot, &scope, expected_producers, producer_failures);
        ctx.lsp().runtime_notes_for(&producers)
    } else {
        ctx.lsp().runtime_notes()
    };
    append_inspect_runtime_notes(&mut payload, &runtime_notes);
    Response::success(&req.id, payload)
}

fn append_inspect_runtime_notes(payload: &mut Value, runtime_notes: &[String]) {
    if !runtime_notes.is_empty() {
        let rendered_notes = collapse_runtime_notes(runtime_notes);
        if let Some(text) = payload
            .get_mut("text")
            .filter(|_| !rendered_notes.is_empty())
        {
            if let Some(existing) = text.as_str() {
                *text =
                    serde_json::Value::String(format!("{existing}\n{}", rendered_notes.join("\n")));
            }
        }
        payload["lsp_runtime_notes"] = serde_json::json!(runtime_notes);
    }
}

/// Collapse runtime notes that differ only in their trailing parenthesized
/// path, such as one "TypeScript 5.9.3: project installation (<tsserver>)"
/// per TypeScript server, into one line with a count and the first path.
/// Distinct notes keep their first-seen order. A single known project SDK is
/// routine; keep SDK notes when selection is uncertain, falls back, or names
/// multiple installations (even when those installations have the same version).
fn collapse_runtime_notes(notes: &[String]) -> Vec<String> {
    let typescript_notes = notes
        .iter()
        .filter(|note| note.starts_with("TypeScript ") || note.starts_with("TypeScript:"))
        .collect::<std::collections::HashSet<_>>();
    let routine_sdk = if typescript_notes.len() == 1 {
        typescript_notes.iter().next().copied().filter(|note| {
            note.strip_prefix("TypeScript ")
                .and_then(|note| note.split_once(": "))
                .is_some_and(|(version, source)| {
                    version != "unknown"
                        && (source.starts_with("project installation (")
                            || source
                                .starts_with("native language server (project installation) ("))
                })
        })
    } else {
        None
    };
    let mut groups: Vec<(&str, Vec<&str>)> = Vec::new();
    for note in notes {
        if Some(note) == routine_sdk {
            continue;
        }
        let (head, detail) = match note
            .strip_suffix(')')
            .and_then(|rest| rest.rsplit_once(" ("))
        {
            Some((head, detail)) => (head, detail),
            None => (note.as_str(), ""),
        };
        match groups.iter_mut().find(|(known, _)| *known == head) {
            Some((_, details)) => {
                if !details.contains(&detail) {
                    details.push(detail);
                }
            }
            None => groups.push((head, vec![detail])),
        }
    }
    groups
        .into_iter()
        .map(|(head, details)| match details.as_slice() {
            [""] => head.to_string(),
            [detail] => format!("{head} ({detail})"),
            [first, ..] => format!("{head} ×{} (first: {first})", details.len()),
            [] => head.to_string(),
        })
        .collect()
}

/// The language servers a scoped inspect answers for. A blocking request
/// already resolved them from the scoped files (started or failed); the
/// nonblocking path derives them from the scoped files directly.
fn scope_producer_keys(
    snapshot: &InspectSnapshot,
    scope: &JobScope,
    expected_producers: &[ServerKey],
    producer_failures: &[ApplicableServerFailure],
) -> std::collections::HashSet<ServerKey> {
    if expected_producers.is_empty() && producer_failures.is_empty() {
        return crate::inspect::diagnostics_category::scope_producer_keys(snapshot, scope);
    }
    expected_producers
        .iter()
        .cloned()
        .chain(
            producer_failures
                .iter()
                .map(|failure| failure.server_key.clone()),
        )
        .collect()
}

/// Register one inspect completion whose poll closure only observes the result
/// channel. Keep payload construction and checks that require newly scanned data
/// in `handle_inspect_payload` and the scanners that produce those results.
pub fn handle_inspect_deferred(req: &RawRequest, ctx: Arc<AppContext>) -> DispatchOutcome {
    handle_inspect_deferred_with_restriction(req, ctx, false)
}

pub(crate) fn handle_inspect_deferred_with_restriction(
    req: &RawRequest,
    ctx: Arc<AppContext>,
    force_restrict: bool,
) -> DispatchOutcome {
    let request_id = req.id.clone();
    let phase_log = InspectPhaseLog::for_request(request_id.clone());
    let deadline = InspectRequestDeadline::from_config(&ctx.config());
    let snapshot = match inspect_preflight(req, &ctx) {
        Ok(snapshot) => snapshot,
        Err(response) => {
            let detail = response
                .data
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned);
            return deferred_response(
                request_id,
                build_inspect_terminal(
                    &req.id,
                    &phase_log,
                    InspectTerminal::PhaseFailed {
                        failed_phase: None,
                        failure_reason: "root_resolution_failed",
                        failure_detail: detail,
                    },
                ),
            );
        }
    };
    let scope = parse_scope(req, &ctx, &snapshot.project_root)
        .expect("inspect preflight already validated the request scope");
    let scoped_roots = (!scope.roots.is_empty()).then_some(scope.roots.as_slice());
    let applicability = resolve_inspect_applicability(
        &ctx,
        &snapshot.project_root,
        scoped_roots,
        &snapshot.config,
        deadline.work_at(),
    );
    let applicability = match applicability {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return deferred_response(
                request_id,
                build_inspect_terminal(
                    &req.id,
                    &phase_log,
                    InspectTerminal::PhaseFailed {
                        failed_phase: None,
                        failure_reason: applicability_failure_reason(&error),
                        failure_detail: Some(applicability_failure_detail(error)),
                    },
                ),
            );
        }
    };

    let request = RawRequest {
        id: req.id.clone(),
        command: req.command.clone(),
        lsp_hints: req.lsp_hints.clone(),
        session_id: req.session_id.clone(),
        params: req.params.clone(),
    };
    let completion_request_id = request_id.clone();
    let shutdown_log = phase_log.clone();
    let cancellation = crate::executor::current_job_cancellation()
        .unwrap_or_else(crate::executor::JobCancellation::new);
    let worker_cancellation = cancellation.clone();
    let root = snapshot.project_root.clone();
    let (tx, rx) = mpsc::sync_channel(1);
    // The request's admitted config, installed on the worker below.
    let admitted_config = ctx.config();
    std::thread::spawn(move || {
        let _config_pin = ctx.pin_config_to(admitted_config);
        let _cancellation = crate::executor::install_job_cancellation(worker_cancellation);
        let _force_restrict = force_restrict.then(|| ctx.force_restrict_guard(&request.id));
        if ctx.checkout_query_runtime_active() {
            // Pin the one final wait outcome even when diagnostics terminate
            // before Tier-2 submission. Finalization below uses this same guard.
            let _ = ctx.callgraph_store_for_ops();
        }
        // Queueing instead of sharing a response keeps request-specific scopes,
        // phase logs, and terminals independent while bounding expensive work to
        // one detached inspect body per root.
        let mut response = match DeferredInspectRootPermit::acquire(root, deadline) {
            Some(_permit) => {
                run_blocking_inspect_body(&request, &ctx, applicability, phase_log, deadline)
            }
            None if inspect_cancellation_requested() => {
                build_inspect_terminal(&request.id, &phase_log, InspectTerminal::Interrupted)
            }
            None => build_inspect_terminal(
                &request.id,
                &phase_log,
                request_deadline_terminal(next_phase(&applicability), deadline),
            ),
        };
        crate::response_finalize::attach_checkout_query_gaps(&mut response, &ctx);
        if response.success {
            if let Some(payload) = response.data.as_object_mut() {
                set_inspect_completion(payload);
            }
        }
        let _ = tx.send(response);
    });
    DispatchOutcome::Deferred(PendingResponse {
        request_id: completion_request_id,
        session_id: String::new(),
        attach_command: String::new(),
        poll: Box::new(move |_| rx.try_recv().ok()),
        cancellation: Some(cancellation),
        on_shutdown: Some(inspect_shutdown_terminal(request_id, shutdown_log)),
    })
}

fn inspect_preflight(req: &RawRequest, ctx: &AppContext) -> Result<InspectSnapshot, Response> {
    parse_top_k(&req.params).map_err(|message| invalid_request(&req.id, message))?;
    parse_sections(req.params.get("sections"))
        .map_err(|message| invalid_request(&req.id, message))?;
    let snapshot = build_snapshot(ctx).map_err(|response| response.with_id(&req.id))?;
    parse_scope(req, ctx, &snapshot.project_root)?;
    Ok(snapshot)
}

fn deferred_response(request_id: String, response: Response) -> DispatchOutcome {
    let (tx, rx) = mpsc::sync_channel(1);
    let _ = tx.send(response);
    DispatchOutcome::Deferred(PendingResponse {
        request_id,
        session_id: String::new(),
        attach_command: String::new(),
        poll: Box::new(move |_| rx.try_recv().ok()),
        cancellation: None,
        on_shutdown: None,
    })
}

fn inspect_shutdown_terminal(
    request_id: String,
    phase_log: InspectPhaseLog,
) -> crate::response_finalize::PendingResponseShutdown {
    Box::new(move |_| {
        build_inspect_terminal(
            &request_id,
            &phase_log,
            InspectTerminal::PhaseFailed {
                failed_phase: phase_log.in_flight_entry(),
                failure_reason: "daemon_shutdown",
                failure_detail: None,
            },
        )
    })
}

/// Feed fleet-status values from inspect outcomes. Only a verified payload
/// supplies a category count; pending or failed categories remain absent in the
/// truthful values state instead of being replaced with zero.
fn refresh_status_bar_counts(ctx: &AppContext, outcomes: &BTreeMap<InspectCategory, JobOutcome>) {
    // `JobOutcome::payload()` exposes only Fresh data or a stat-verified stale
    // cache, so an unavailable category cannot overwrite a proven value.
    let count_of = |category: InspectCategory| -> Option<usize> {
        outcomes
            .get(&category)
            .and_then(JobOutcome::payload)
            .and_then(|payload| available_count_from_payload(category, payload))
    };
    let any_tier2_stale = [
        InspectCategory::DeadCode,
        InspectCategory::UnusedExports,
        InspectCategory::Duplicates,
    ]
    .iter()
    .any(|category| match outcomes.get(category) {
        Some(JobOutcome::Fresh { payload }) => category_is_incomplete(payload),
        Some(JobOutcome::Stale { .. } | JobOutcome::Pending { .. } | JobOutcome::Failed { .. }) => {
            true
        }
        None => false,
    });
    let todos = count_of(InspectCategory::Todos);

    ctx.update_status_bar_tier2(
        count_of(InspectCategory::DeadCode),
        count_of(InspectCategory::UnusedExports),
        count_of(InspectCategory::Duplicates),
        todos,
        any_tier2_stale,
    );
}

/// A blocking `aft_inspect` may update alert state only from accepted snapshots
/// whose document versions were verified. Other inspect operations compute
/// payloads or fleet values and must not update alert state.
fn record_blocking_inspect_observations(
    ctx: &AppContext,
    req: &RawRequest,
    snapshot: &InspectSnapshot,
    accepted_snapshots: Vec<AcceptedDiagnosticSnapshot>,
) {
    if accepted_snapshots.is_empty() {
        return;
    }

    let batch = match AcceptedObservationBatch::from_diagnostic_snapshots(
        req.session(),
        &snapshot.project_root,
        accepted_snapshots,
    ) {
        Ok(batch) => batch,
        Err(error) => {
            crate::slog_warn!(
                "[inspect:diagnostics] omitted duplicate producer observation batch: {error}"
            );
            return;
        }
    };
    if let Err(error) = ctx.accept_alert_observation_batch(&batch) {
        crate::slog_warn!("[inspect:diagnostics] failed to accept observation batch: {error}");
    }
}

fn run_blocking_inspect_body(
    req: &RawRequest,
    ctx: &AppContext,
    applicability: ApplicableServerSnapshot,
    phase_log: InspectPhaseLog,
    deadline: InspectRequestDeadline,
) -> Response {
    if inspect_cancellation_requested() {
        return build_inspect_terminal(&req.id, &phase_log, InspectTerminal::Interrupted);
    }
    if !deadline.has_work_budget() {
        return build_inspect_terminal(
            &req.id,
            &phase_log,
            request_deadline_terminal(next_phase(&applicability), deadline),
        );
    }
    let project_root = match build_snapshot(ctx) {
        Ok(snapshot) => snapshot.project_root,
        Err(response) => {
            return build_inspect_terminal(
                &req.id,
                &phase_log,
                InspectTerminal::PhaseFailed {
                    failed_phase: None,
                    failure_reason: "root_resolution_failed",
                    failure_detail: response
                        .data
                        .get("message")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                },
            );
        }
    };
    let initial_stats = match capture_inspect_root_stats_until(&project_root, Some(deadline)) {
        Ok(snapshot) => snapshot,
        Err(detail) if detail.contains("inspect_request_timeout") => {
            return build_inspect_terminal(
                &req.id,
                &phase_log,
                request_deadline_terminal(next_phase(&applicability), deadline),
            );
        }
        Err(detail) => {
            return build_inspect_terminal(
                &req.id,
                &phase_log,
                InspectTerminal::PhaseFailed {
                    failed_phase: None,
                    failure_reason: "inspect_not_fresh",
                    failure_detail: Some(detail),
                },
            );
        }
    };
    wait_at_deferred_inspect_body_gate_for_test(deadline);
    delay_inspect_body_from_env_for_test(deadline);
    if inspect_cancellation_requested() {
        return build_inspect_terminal(&req.id, &phase_log, InspectTerminal::Interrupted);
    }
    if !deadline.has_work_budget() {
        return build_inspect_terminal(
            &req.id,
            &phase_log,
            request_deadline_terminal(next_phase(&applicability), deadline),
        );
    }
    if take_deferred_inspect_stat_short_circuit_for_test() {
        let terminal =
            match verify_final_root_stats(&project_root, &initial_stats, &phase_log, deadline) {
                Ok(()) => InspectTerminal::Fresh(serde_json::json!({})),
                Err(terminal) => terminal,
            };
        return build_inspect_terminal(&req.id, &phase_log, terminal);
    }

    let mut start_outcomes = ApplicableServerStartOutcomes::default();
    let startup_deadline = deadline.phase_deadline(INSPECT_PHASE_WAIT_CAP);
    let starts = applicability
        .server_keys
        .iter()
        .map(|server| {
            let phase = phase_log.start(InspectPhaseEntry::lsp(InspectPhaseId::LspStart, server));
            (server, phase)
        })
        .collect::<Vec<_>>();
    let outcomes = start_applicable_servers_concurrently(ctx, &applicability, startup_deadline);
    if inspect_cancellation_requested() {
        for (_, phase) in starts {
            phase.fail("inspect request cancelled");
        }
        return build_inspect_terminal(&req.id, &phase_log, InspectTerminal::Interrupted);
    }
    for ((server, phase), outcome) in starts.into_iter().zip(outcomes) {
        let deadline_exceeded = outcome.deadline_exceeded.is_some();
        finish_start_phases(vec![(server.clone(), phase)], &outcome);
        start_outcomes.successful.extend(outcome.successful);
        start_outcomes.failures.extend(outcome.failures);
        if deadline_exceeded
            && !start_outcomes
                .failures
                .iter()
                .any(|failure| failure.server_key == *server)
        {
            start_outcomes.failures.push(ApplicableServerFailure {
                server_key: server.clone(),
                result: crate::lsp::manager::ServerAttemptResult::SpawnFailed {
                    binary: String::new(),
                    reason: "startup wait budget exhausted; retry aft_inspect".into(),
                },
            });
        }
    }

    if inspect_cancellation_requested() {
        return build_inspect_terminal(&req.id, &phase_log, InspectTerminal::Interrupted);
    }
    // A rust-analyzer that was already running may hold a workspace load
    // (possibly a failed one) made from manifests that have changed since,
    // for example a stale Cargo.lock fixed with `cargo update` in a shell.
    // Ask it to reload before waiting, so the wait below observes the new
    // load instead of returning the old result.
    let reload_scope_roots = parse_scope(req, ctx, &project_root)
        .map(|scope| scope.roots)
        .unwrap_or_default();
    for server in &start_outcomes.successful {
        ctx.lsp_reload_rust_workspace_if_manifests_changed(server, &reload_scope_roots);
    }
    if !deadline.has_work_budget() {
        let failed_phase = start_outcomes
            .successful
            .first()
            .map(|server| InspectPhaseEntry::lsp(InspectPhaseId::LspQuiescence, server));
        return build_inspect_terminal(
            &req.id,
            &phase_log,
            request_deadline_terminal(
                failed_phase.or_else(|| next_phase(&applicability)),
                deadline,
            ),
        );
    }
    let quiescence = start_outcomes
        .successful
        .iter()
        .map(|server| {
            phase_log.start(InspectPhaseEntry::lsp(
                InspectPhaseId::LspQuiescence,
                server,
            ))
        })
        .collect::<Vec<_>>();
    // Give producers a bounded chance to settle before reading the warm store.
    // The wait is root-level: producers publish while events are drained. A
    // scoped request's per-file work happens later, in the diagnostics
    // category, with its own share of the budget; that step also waits for
    // rust-analyzer's `cargo check`, so only an unscoped request waits for
    // it here.
    let scoped_request =
        scope_was_provided(req.params.get("scope")) && !reload_scope_roots.is_empty();
    let wait_outcome =
        wait_for_root_quiescence(ctx, &start_outcomes.successful, deadline, !scoped_request);
    if inspect_cancellation_requested() {
        for phase in quiescence {
            phase.fail("inspect request cancelled");
        }
        return build_inspect_terminal(&req.id, &phase_log, InspectTerminal::Interrupted);
    }
    // A blocking inspection is an explicit diagnostics observation source. Keep
    // accepted producer snapshots intact until the inspect response is built;
    // flattened category payloads cannot recover producer ownership.
    let (accepted_snapshots, indexing_gaps) = match wait_outcome {
        Ok((snapshots, blocked, gaps)) => {
            if blocked {
                phase_log.note_blocking_wait();
            }
            (snapshots, gaps)
        }
        Err(message) => {
            // Name the phase that was still in flight before the handles are
            // failed, so the terminal keeps its quiescence attribution.
            let failed_phase = phase_log.in_flight_entry();
            for phase in quiescence {
                phase.fail(&message);
            }
            let failure_reason = quiescence_failure_reason(&message);
            return build_inspect_terminal(
                &req.id,
                &phase_log,
                InspectTerminal::PhaseFailed {
                    failed_phase,
                    failure_reason,
                    failure_detail: Some(message),
                },
            );
        }
    };
    for (server, phase) in start_outcomes.successful.iter().zip(quiescence) {
        if let Some((_, reason)) = indexing_gaps.iter().find(|(key, _)| key == server) {
            phase.fail(reason);
        } else {
            phase.complete();
        }
    }
    let inspect_snapshot = build_snapshot(ctx).ok();
    let mut response = handle_inspect_payload(
        req,
        ctx,
        true,
        applicability.server_keys.is_empty(),
        &start_outcomes.failures,
        &applicability.not_applicable,
        &start_outcomes.successful,
        &indexing_gaps,
        Some(&phase_log),
        Some(deadline),
        Some(&initial_stats),
    );
    if inspect_cancellation_requested() {
        return build_inspect_terminal(&req.id, &phase_log, InspectTerminal::Interrupted);
    }
    if !response.success && inspect_failure_reason(&response) == "inspect_request_timeout" {
        return build_inspect_terminal(
            &req.id,
            &phase_log,
            InspectTerminal::PhaseFailed {
                failed_phase: failed_phase_from_response(&response)
                    .or_else(|| phase_log.in_flight_entry()),
                failure_reason: "inspect_request_timeout",
                failure_detail: response
                    .data
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            },
        );
    }
    if let Err(terminal) =
        verify_final_root_stats(&project_root, &initial_stats, &phase_log, deadline)
    {
        if response.success
            && matches!(
                &terminal,
                InspectTerminal::PhaseFailed {
                    failure_reason: "inspect_request_timeout",
                    ..
                }
            )
        {
            response.data["complete"] = Value::Bool(false);
            let gap = serde_json::json!({"kind": "stat_verification_incomplete",
                "reason": "file freshness verification exceeded the request budget; retry aft_inspect"});
            if !response.data["gaps"].is_array() {
                response.data["gaps"] = serde_json::json!([]);
            }
            response.data["gaps"].as_array_mut().unwrap().push(gap);
            return build_inspect_terminal(
                &req.id,
                &phase_log,
                InspectTerminal::Fresh(response.data),
            );
        }
        return build_inspect_terminal(&req.id, &phase_log, terminal);
    }
    if let Some(inspect_snapshot) = &inspect_snapshot {
        record_blocking_inspect_observations(ctx, req, inspect_snapshot, accepted_snapshots);
    }
    if inspect_cancellation_requested() {
        return build_inspect_terminal(&req.id, &phase_log, InspectTerminal::Interrupted);
    }
    if response.success {
        build_inspect_terminal(&req.id, &phase_log, InspectTerminal::Fresh(response.data))
    } else {
        build_inspect_terminal(
            &req.id,
            &phase_log,
            InspectTerminal::PhaseFailed {
                failed_phase: failed_phase_from_response(&response)
                    .or_else(|| phase_log.in_flight_entry()),
                failure_reason: inspect_failure_reason(&response),
                failure_detail: response
                    .data
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            },
        )
    }
}

/// Wait for every successfully started producer to settle before the payload
/// reads the warm store: a producer settles once it holds a current
/// authoritative (non-stale, non-provisional) report or stops warming
/// (declares quiescence). With `wait_for_rust_check`, a rust-analyzer
/// producer must also have no `cargo check` running or just requested (an
/// edit's save starts one): until it finishes, the compiler errors in the
/// store describe the files before the edit. Events are drained with the
/// manager lock held only for the drain and the checks, so producers keep
/// publishing while the wait ticks. Cancellation and the bounded phase
/// deadline are checked at every tick. At the deadline, retain observations
/// and name only the unsettled producers.
fn wait_for_root_quiescence(
    ctx: &AppContext,
    expected: &[ServerKey],
    deadline: InspectRequestDeadline,
    wait_for_rust_check: bool,
) -> Result<
    (
        Vec<AcceptedDiagnosticSnapshot>,
        bool,
        Vec<(ServerKey, String)>,
    ),
    String,
> {
    let started = Instant::now();
    let wait_until = deadline.phase_deadline(INSPECT_PHASE_WAIT_CAP);
    let mut accepted_snapshots = Vec::new();
    let mut blocked = false;
    let rust_check_state = |lsp: &crate::lsp::manager::LspManager, server: &ServerKey| {
        if wait_for_rust_check {
            lsp.rust_check_state(server)
        } else {
            RustCheckState::Current
        }
    };
    if wait_for_rust_check {
        let mut lsp = ctx.lsp();
        for server in expected {
            lsp.rearm_unreported_rust_check(server);
        }
    }
    loop {
        if inspect_cancellation_requested() {
            return Err("inspect request cancelled during LSP quiescence".to_string());
        }
        accepted_snapshots.extend(ctx.lsp().drain_events().accepted_snapshots);
        let settled = root_producers_settled(ctx, expected);
        // A check that was expected and did not begin by its deadline is not
        // waited for: its results are unknown however long this waits.
        let checking = {
            let lsp = ctx.lsp();
            expected
                .iter()
                .any(|server| rust_check_state(&lsp, server) == RustCheckState::Running)
        };
        if (settled && !checking) || Instant::now() >= wait_until {
            let lsp = ctx.lsp();
            let gaps = expected
                .iter()
                .filter_map(|server| {
                    if !lsp.producer_has_settled(server) {
                        Some((
                            server.clone(),
                            format!(
                    "still indexing after {:.1}s; retry aft_inspect after the server settles",
                    started.elapsed().as_secs_f64()
                ),
                        ))
                    } else if rust_check_state(&lsp, server) != RustCheckState::Current {
                        Some((
                            server.clone(),
                            crate::inspect::diagnostics_category::RUST_CHECK_RUNNING_REASON
                                .to_string(),
                        ))
                    } else {
                        None
                    }
                })
                .collect();
            return Ok((accepted_snapshots, blocked, gaps));
        }
        blocked = true;
        let remaining = wait_until.saturating_duration_since(Instant::now());
        std::thread::sleep(Duration::from_millis(50).min(remaining));
    }
}

fn root_producers_settled(ctx: &AppContext, expected: &[ServerKey]) -> bool {
    // Authoritative-report predicates reject watcher-stale and provisional
    // entries, so delivered file events invalidate this wait immediately. File
    // delivery is asynchronous, however; the terminal StatVerification phase
    // compares the scanned file set directly and closes that latency window.
    // Settlement ends the producer wait, not the authority obligation. The
    // diagnostics collection rechecks reports and compiler progress and names
    // missing results as gaps instead of certifying a quiescent empty store.
    ctx.lsp().producers_settled(expected)
}

fn next_phase(applicability: &ApplicableServerSnapshot) -> Option<InspectPhaseEntry> {
    applicability
        .server_keys
        .first()
        .map(|server| InspectPhaseEntry::lsp(InspectPhaseId::LspStart, server))
        .or_else(|| {
            InspectCategory::active()
                .iter()
                .copied()
                .find(|category| category.is_tier2())
                .map(|category| InspectPhaseEntry::category(InspectPhaseId::Tier2Rescan, category))
        })
}

fn request_deadline_terminal(
    failed_phase: Option<InspectPhaseEntry>,
    deadline: InspectRequestDeadline,
) -> InspectTerminal {
    let phase = failed_phase
        .as_ref()
        .map(|entry| entry.id)
        .unwrap_or(InspectPhaseId::Tier2Rescan);
    InspectTerminal::PhaseFailed {
        failed_phase,
        failure_reason: "inspect_request_timeout",
        failure_detail: Some(deadline.timeout_detail(phase)),
    }
}

fn failed_phase_from_response(response: &Response) -> Option<InspectPhaseEntry> {
    let phase = match response.data.get("failed_phase")?.as_str()? {
        "tier2_rescan" => InspectPhaseId::Tier2Rescan,
        "callgraph_ready" => InspectPhaseId::CallgraphReady,
        "stat_verification" => InspectPhaseId::StatVerification,
        _ => return None,
    };
    let category = response.data.get("category")?.as_str()?.parse().ok()?;
    Some(InspectPhaseEntry::category(phase, category))
}

fn quiescence_failure_reason(message: &str) -> &'static str {
    if message.contains("inspect_request_timeout") {
        "inspect_request_timeout"
    } else if message.contains("lsp_quiescence_timeout") {
        "lsp_quiescence_timeout"
    } else {
        "inspect_not_fresh"
    }
}

fn inspect_failure_reason(response: &Response) -> &'static str {
    let message = response
        .data
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if message.contains("inspect_request_timeout") {
        "inspect_request_timeout"
    } else if message.contains("writer_lease_timeout") {
        "writer_lease_timeout"
    } else if message.contains("cold_build_limiter_timeout") {
        "cold_build_limiter_timeout"
    } else if message.contains("inspect_phase_timeout") {
        "inspect_phase_timeout"
    } else if message.contains("lsp_quiescence_timeout") {
        "lsp_quiescence_timeout"
    } else {
        "inspect_not_fresh"
    }
}

/// Start every applicable producer at once, each bounded by the same startup
/// deadline, and return one outcome per server in `server_keys` order.
///
/// Starting them one after another made the startup budget a sum: on a busy
/// machine a project with a dozen servers spent it on the first few
/// handshakes, and every later server was reported as "startup wait budget
/// exhausted" without ever having been tried. Each start already runs its
/// spawn and `initialize` handshake without the manager lock, so starts of
/// different servers do not wait on each other.
fn start_applicable_servers_concurrently(
    ctx: &AppContext,
    applicability: &ApplicableServerSnapshot,
    startup_deadline: Instant,
) -> Vec<ApplicableServerStartOutcomes> {
    let config = ctx.config();
    let start = |server: &crate::lsp::roots::ServerKey| {
        ctx.lsp_start_applicable_server_until(applicability, server, &config, startup_deadline)
    };
    std::thread::scope(|scope| {
        let handles = applicability
            .server_keys
            .iter()
            .map(|server| {
                let spawned = std::thread::Builder::new()
                    .name("aft-inspect-lsp-start".into())
                    .spawn_scoped(scope, move || start(server));
                (server, spawned)
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|(server, spawned)| match spawned {
                Ok(handle) => handle
                    .join()
                    .unwrap_or_else(|_| start_panicked_outcome(server, "server start panicked")),
                // If the thread cannot be spawned, start the server on this
                // thread instead: slower, because it no longer overlaps the
                // other starts, but the server is still started.
                Err(_) => start(server),
            })
            .collect()
    })
}

fn start_panicked_outcome(
    server: &crate::lsp::roots::ServerKey,
    reason: &str,
) -> ApplicableServerStartOutcomes {
    ApplicableServerStartOutcomes {
        failures: vec![ApplicableServerFailure {
            server_key: server.clone(),
            result: crate::lsp::manager::ServerAttemptResult::SpawnFailed {
                binary: String::new(),
                reason: reason.to_string(),
            },
        }],
        ..ApplicableServerStartOutcomes::default()
    }
}

fn finish_start_phases(
    starts: Vec<(
        crate::lsp::roots::ServerKey,
        crate::inspect::phase_log::InspectPhaseHandle,
    )>,
    outcomes: &ApplicableServerStartOutcomes,
) {
    for (server, phase) in starts {
        if outcomes.deadline_exceeded.as_ref() == Some(&server)
            || (!outcomes.successful.contains(&server)
                && !outcomes
                    .failures
                    .iter()
                    .any(|failure| failure.server_key == server))
        {
            phase.fail("producer was not started before the inspect request deadline");
        } else if let Some(failure) = outcomes
            .failures
            .iter()
            .find(|failure| failure.server_key == server)
        {
            // A producer failure is complete evidence about that producer, not a
            // request-wide phase failure. Other producers must still be reported.
            phase.fail(failure.reason());
        } else {
            phase.complete();
        }
    }
}

fn applicability_failure_reason(error: &ApplicabilityResolutionError) -> &'static str {
    match error {
        ApplicabilityResolutionError::RequestDeadline { .. } => "inspect_request_timeout",
        ApplicabilityResolutionError::RootUnreadable { .. } => "applicability_resolution_failed",
    }
}

fn applicability_failure_detail(error: ApplicabilityResolutionError) -> String {
    match error {
        ApplicabilityResolutionError::RootUnreadable { root, reason } => {
            format!("cannot resolve {}: {reason}", root.display())
        }
        ApplicabilityResolutionError::RequestDeadline { root } => format!(
            "inspect_request_timeout: producer discovery for {} exceeded the shared request deadline",
            root.display()
        ),
    }
}

#[allow(dead_code)]
enum InspectTerminal {
    Fresh(Value),
    Interrupted,
    PhaseFailed {
        failed_phase: Option<InspectPhaseEntry>,
        failure_reason: &'static str,
        failure_detail: Option<String>,
    },
}

/// Completion is about every reported category, not just language servers.
/// Sections and topK select detail rows; neither changes which results count.
pub(crate) fn partial_terminal_reason(payload: &Map<String, Value>) -> Option<String> {
    partial_reason_from_parts(&incomplete_result_parts(payload))
}

fn incomplete_result_parts(payload: &Map<String, Value>) -> Vec<(bool, String)> {
    let mut parts = payload
        .get("summary")
        .and_then(Value::as_object)
        .map(incomplete_summary_parts)
        .unwrap_or_default();
    for gap in payload
        .get("gaps")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        // Category gaps are already explained by their summary. Final checkout
        // and stat-verification gaps have no category and must also count.
        if gap.get("categories").is_some() {
            continue;
        }
        let label = match gap["kind"].as_str() {
            Some("view_pending") => "callgraph view pending",
            Some("stat_verification_incomplete") => "file freshness unverified",
            _ => "analysis incomplete",
        };
        let reason = gap["reason"].as_str().unwrap_or("analysis did not finish");
        let reason = format!("{label}: {}", compact_inspect_reason(reason));
        if !parts.iter().any(|(_, existing)| existing == &reason) {
            parts.push((false, reason));
        }
    }
    if parts.is_empty() && payload.get("complete").and_then(Value::as_bool) == Some(false) {
        parts.push((false, "analysis incomplete: no complete report".to_string()));
    }
    parts.sort_by_key(|(diagnostic, reason)| {
        (*diagnostic, if *diagnostic { 0 } else { reason.len() })
    });
    parts
}

/// Finalization can add freshness gaps after scanners have answered. Keep the
/// structured state and the already rendered status line in sync at that point.
fn set_inspect_completion(payload: &mut Map<String, Value>) {
    let partial = partial_terminal_reason(payload);
    payload.insert("complete".to_string(), Value::Bool(partial.is_none()));
    payload.insert(
        "inspect_terminal".to_string(),
        serde_json::json!(if partial.is_some() {
            "partial"
        } else {
            "fresh"
        }),
    );
    if let Some(text) = payload.get("text").and_then(Value::as_str) {
        let body = if text.starts_with("PARTIAL — ") || text.starts_with("FRESH\n") {
            text.split_once('\n').map_or("", |(_, body)| body)
        } else {
            text
        };
        let header = partial.as_ref().map_or_else(
            || "FRESH".to_string(),
            |reason| format!("PARTIAL — {reason}"),
        );
        let mut text = if body.is_empty() {
            header
        } else {
            format!("{header}\n{body}")
        };
        let summary_parts = payload
            .get("summary")
            .and_then(Value::as_object)
            .map(incomplete_summary_parts)
            .unwrap_or_default();
        for (diagnostic, reason) in incomplete_result_parts(payload)
            .into_iter()
            .skip(MAX_INSPECT_HEADER_PARTS)
        {
            if diagnostic && !text.contains(&reason) {
                text.push_str(&format!("\nIncomplete diagnostics: {reason}"));
            }
        }
        for (_, reason) in incomplete_result_parts(payload) {
            if !summary_parts.iter().any(|(_, part)| part == &reason) && !text.contains(&reason) {
                text.push_str(&format!("\nIncomplete analysis: {reason}"));
            }
        }
        payload.insert("text".to_string(), Value::String(text));
    }
    match partial {
        Some(reason) => {
            payload.insert("partial_reason".to_string(), Value::String(reason));
        }
        None => {
            payload.remove("partial_reason");
        }
    }
}

fn category_is_incomplete(value: &Value) -> bool {
    value.get("complete").and_then(Value::as_bool) == Some(false)
        || value.get("unavailable").and_then(Value::as_bool) == Some(true)
        || value.get("callgraph_available").and_then(Value::as_bool) == Some(false)
}

fn compact_inspect_reason(reason: &str) -> String {
    reason
        .trim_end_matches("; retry aft_inspect.")
        .trim_end_matches("; retry aft_inspect")
        .trim_end_matches("; retry")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn incomplete_analysis_reason(value: &Value) -> String {
    let mut reasons = Vec::new();
    for (key, cause) in [
        ("parse_errors", "could not be parsed"),
        ("skipped_files", "could not be analyzed"),
    ] {
        if let Some(files) = value
            .get(key)
            .and_then(Value::as_array)
            .filter(|files| !files.is_empty())
        {
            let label = if files.len() == 1 { "file" } else { "files" };
            reasons.push(format!("{} {label} {cause}", files.len()));
        }
    }
    if !reasons.is_empty() {
        return reasons.join("; ");
    }
    compact_inspect_reason(
        value
            .get("reason")
            .or_else(|| value.get("callgraph_unavailable_reason"))
            .and_then(Value::as_str)
            .unwrap_or("analysis did not finish"),
    )
}

fn incomplete_category_gaps(category: InspectCategory, value: &Value) -> Vec<Value> {
    if let Some(gaps) = value
        .get("gaps")
        .and_then(Value::as_array)
        .filter(|gaps| !gaps.is_empty())
    {
        return gaps.clone();
    }
    let reason = if category == InspectCategory::Diagnostics {
        "no authoritative report".to_string()
    } else {
        incomplete_analysis_reason(value)
    };
    vec![serde_json::json!({"kind": "analysis_incomplete", "reason": reason})]
}

/// Short scanner explanations precede verbose diagnostic producer explanations.
/// Diagnostic causes retain first-seen order, including their scoped roots.
fn incomplete_summary_parts(summary: &Map<String, Value>) -> Vec<(bool, String)> {
    let mut parts = Vec::new();
    for (category, value) in summary {
        if category == "diagnostics" || !category_is_incomplete(value) {
            continue;
        }
        let label = category.replace('_', " ");
        let reason = match value.pointer("/building/state").and_then(Value::as_str) {
            Some("building") => format!("{label} still building"),
            Some("rebuilding") => format!("{label} still rebuilding"),
            Some(state) => format!("{label} still building ({})", state.replace('_', " ")),
            None => {
                let reason = value
                    .get("gaps")
                    .and_then(Value::as_array)
                    .and_then(|gaps| gaps.first())
                    .and_then(|gap| gap["reason"].as_str())
                    .map(compact_inspect_reason)
                    .unwrap_or_else(|| incomplete_analysis_reason(value));
                let status =
                    if value["unavailable"] == true || value["callgraph_available"] == false {
                        "unavailable"
                    } else {
                        "incomplete"
                    };
                format!("{label} {status}: {reason}")
            }
        };
        parts.push((false, compact_inspect_reason(&reason)));
    }
    parts.sort_by_key(|(_, reason)| reason.len());
    if let Some(causes) = summary
        .get("diagnostics")
        .and_then(diagnostics_unknown_causes)
    {
        parts.extend(causes.into_iter().map(|cause| (true, cause)));
    }
    parts
}

fn partial_summary_reason(summary: &Map<String, Value>) -> Option<String> {
    let parts = incomplete_summary_parts(summary);
    partial_reason_from_parts(&parts)
}

fn partial_reason_from_parts(parts: &[(bool, String)]) -> Option<String> {
    if parts.is_empty() {
        return None;
    }
    let mut diagnostic_label_shown = false;
    // Headline cap, registered as an exclusion in list_surfaces.rs.
    let mut shown = parts
        .iter()
        .take(MAX_INSPECT_HEADER_PARTS)
        .map(|(diagnostic, reason)| {
            if *diagnostic && !diagnostic_label_shown {
                diagnostic_label_shown = true;
                format!("diagnostics unknown: {reason}")
            } else {
                reason.clone()
            }
        })
        .collect::<Vec<_>>();
    if parts.len() > shown.len() {
        shown.push(format!("+{} more", parts.len() - shown.len()));
    }
    Some(format!("{}; retry aft_inspect.", shown.join("; ")))
}

fn diagnostics_unknown_causes(diagnostics: &Value) -> Option<Vec<String>> {
    if diagnostics.get("complete").and_then(Value::as_bool) != Some(false) {
        return None;
    }
    let gaps = diagnostics
        .get("gaps")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut causes = Vec::<(String, String, String, usize)>::new();
    // Producer gaps are the primary explanation; uncovered files add counts
    // to that explanation rather than restating it with slightly different words.
    for uncovered in [false, true] {
        for gap in gaps {
            if (gap["kind"] == "uncovered_file") != uncovered {
                continue;
            }
            let cause = if uncovered {
                gap.get("cause").unwrap_or(gap)
            } else {
                gap
            };
            let producer = cause["producer"].as_str().unwrap_or("unknown producer");
            let root = cause["root"].as_str().unwrap_or("");
            let reason = cause["reason"]
                .as_str()
                .unwrap_or("no authoritative report");
            let files = if uncovered {
                1
            } else {
                gap.get("affected_files")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len)
            };
            if let Some(existing) = causes.iter_mut().find(|entry| {
                entry.0 == producer && (entry.1 == root || entry.1.is_empty() || root.is_empty())
            }) {
                existing.3 += files;
                if existing.1.is_empty() {
                    existing.1 = root.to_string();
                }
            } else {
                causes.push((
                    producer.to_string(),
                    root.to_string(),
                    reason.to_string(),
                    files,
                ));
            }
        }
    }
    let mut explanations = causes
        .into_iter()
        .map(|(producer, root, reason, files)| {
            let producer = if producer == "rust" {
                "rust-analyzer"
            } else {
                &producer
            };
            let reason = reason.strip_prefix("still checking: ").unwrap_or(&reason);
            let reason = reason
                .strip_prefix(&format!("{producer}: "))
                .unwrap_or(reason);
            let reason = compact_inspect_reason(reason);
            let root = if root.is_empty() {
                String::new()
            } else {
                format!(" @ {root}")
            };
            let count = match files {
                0 => String::new(),
                1 => " (1 file)".to_string(),
                n => format!(" ({n} files)"),
            };
            compact_inspect_reason(&format!("{producer}{root}: {reason}{count}"))
        })
        .collect::<Vec<_>>();
    if explanations.is_empty() {
        explanations.push("no authoritative report".to_string());
    }
    Some(explanations)
}

fn build_inspect_terminal(
    request_id: &str,
    log: &InspectPhaseLog,
    terminal: InspectTerminal,
) -> Response {
    let (phases, blocking_waited) = log.terminal_inputs();
    match terminal {
        InspectTerminal::Fresh(mut payload) => {
            let Some(payload) = payload.as_object_mut() else {
                return Response::error(
                    request_id,
                    "inspect_terminal_invalid",
                    "inspect payload was not an object",
                );
            };
            set_inspect_completion(payload);
            payload.insert(
                "wait_stamp".to_string(),
                serde_json::json!({
                    "text": format_wait_text(&phases, blocking_waited),
                    "phases": phases,
                }),
            );
            Response::success(request_id, Value::Object(payload.clone()))
        }
        InspectTerminal::Interrupted => Response {
            id: request_id.to_string(),
            success: false,
            data: serde_json::json!({"inspect_terminal": "interrupted", "completed_phases": phases}),
        },
        InspectTerminal::PhaseFailed {
            failed_phase,
            failure_reason,
            failure_detail,
        } => {
            let mut data = serde_json::json!({
                "inspect_terminal": "phase_failed",
                "completed_phases": phases,
                "failure_reason": failure_reason,
            });
            if let Some(phase) = failed_phase {
                data["failed_phase"] = serde_json::json!(phase.id);
                if let Some(producer) = phase.producer {
                    data["producer"] = Value::String(producer);
                }
                if let Some(category) = phase.category {
                    data["category"] = Value::String(category);
                }
            }
            if let Some(detail) = failure_detail {
                data["failure_detail"] = Value::String(detail);
            }
            Response {
                id: request_id.to_string(),
                success: false,
                data,
            }
        }
    }
}

pub fn handle_inspect_tier2_run(req: &RawRequest, ctx: &AppContext) -> Response {
    let categories = match parse_tier2_categories(req.params.get("categories")) {
        Ok(categories) => categories,
        Err(message) => return invalid_request(&req.id, message),
    };

    if !ctx.inspect_writer() {
        let skipped = categories
            .iter()
            .map(|category| {
                serde_json::json!({
                    "category": category.as_str(),
                    "reason": "inspect_read_only",
                })
            })
            .collect::<Vec<_>>();
        return Response::success(
            &req.id,
            serde_json::json!({
                "queued_categories": [],
                "in_flight_categories": [],
                "errors": [],
                "skipped_categories": skipped,
            }),
        );
    }

    let snapshot = match build_snapshot(ctx) {
        Ok(snapshot) => snapshot,
        Err(response) => return response.with_id(&req.id),
    };
    let manager = ctx.inspect_manager();
    let submission = manager.submit_tier2_run_with_reuse_serial_background(snapshot, categories);
    if submission.has_new_work() {
        ctx.note_tier2_refresh_started();
    }

    let queued = submission
        .queued_categories
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let errors = submission
        .errors
        .iter()
        .map(|error| {
            serde_json::json!({
                "category": error.category.as_str(),
                "message": error.message.as_str(),
            })
        })
        .collect::<Vec<_>>();

    Response::success(
        &req.id,
        serde_json::json!({
            "queued_categories": queued.clone(),
            "in_flight_categories": queued,
            "errors": errors,
        }),
    )
}

trait ResponseIdExt {
    fn with_id(self, id: &str) -> Self;
}

impl ResponseIdExt for Response {
    fn with_id(mut self, id: &str) -> Self {
        self.id = id.to_string();
        self
    }
}

#[derive(Debug, Clone)]
struct Sections {
    detail_categories: BTreeSet<InspectCategory>,
}

impl Sections {
    fn summary_only() -> Self {
        Self {
            detail_categories: BTreeSet::new(),
        }
    }

    fn all() -> Self {
        Self {
            detail_categories: InspectCategory::active().iter().copied().collect(),
        }
    }

    fn includes(&self, category: InspectCategory) -> bool {
        self.detail_categories.contains(&category)
    }
}

fn build_snapshot(ctx: &AppContext) -> Result<InspectSnapshot, Response> {
    if ctx.harness_opt().is_none() {
        return Err(Response::error(
            "inspect",
            "not_configured",
            "inspect: configure must run before aft_inspect so the harness-scoped cache path is known",
        ));
    }

    let config = ctx.config();
    let project_root = config
        .project_root
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    // Normalized, not bare-canonical: the diagnostics collection filters
    // LSP-reported (normalized) paths against this root with starts_with,
    // so a Windows verbatim root here silently drops every diagnostic.
    let project_root = crate::inspect::job::canonicalize_normalized(&project_root);
    Ok(InspectSnapshot::new_with_capabilities(
        project_root,
        ctx.inspect_dir(),
        config,
        ctx.symbol_cache(),
        ctx.inspect_writer(),
        ctx.callgraph_writer(),
    ))
}

fn finish_tier2_phases(
    outcome: &JobOutcome,
    callgraph_phase: Option<crate::inspect::phase_log::InspectPhaseHandle>,
    tier2_phase: Option<crate::inspect::phase_log::InspectPhaseHandle>,
) {
    match outcome {
        JobOutcome::Fresh { payload } => {
            if let Some(callgraph_phase) = callgraph_phase {
                if payload.get("callgraph_available").and_then(Value::as_bool) == Some(true)
                    || payload
                        .get("notes")
                        .and_then(Value::as_array)
                        .is_some_and(|notes| {
                            notes.iter().any(|note| {
                                note.as_str() == Some("callgraph_path_identity_mismatch")
                            })
                        })
                {
                    callgraph_phase.complete();
                } else {
                    callgraph_phase.fail("dead_code aggregate has no ready callgraph snapshot");
                }
            }
            if let Some(tier2_phase) = tier2_phase {
                tier2_phase.complete();
            }
        }
        JobOutcome::Failed { message } => {
            if let Some(callgraph_phase) = callgraph_phase {
                callgraph_phase.fail(message);
            }
            if let Some(tier2_phase) = tier2_phase {
                tier2_phase.fail(message);
            }
        }
        JobOutcome::Stale { .. } | JobOutcome::Pending { .. } => {
            if let Some(callgraph_phase) = callgraph_phase {
                callgraph_phase.fail("dead_code aggregate did not become fresh");
            }
            if let Some(tier2_phase) = tier2_phase {
                tier2_phase.fail("Tier-2 aggregate did not become fresh");
            }
        }
    }
}

fn receive_tier2_completion_until(
    rx: std::sync::mpsc::Receiver<JobOutcome>,
    manager: &crate::inspect::InspectManager,
    category: InspectCategory,
    deadline: std::time::Instant,
    request_deadline: Option<InspectRequestDeadline>,
) -> Option<JobOutcome> {
    loop {
        // Another category may have used this wait's budget after the worker
        // finished. Keep an already published result even at the deadline.
        if let Ok(outcome) = rx.try_recv() {
            return Some(outcome);
        }
        let now = std::time::Instant::now();
        if now >= deadline {
            if request_deadline.is_some_and(|request| now >= request.work_at()) {
                return Some(JobOutcome::Failed {
                    message: request_deadline
                        .expect("checked request deadline")
                        .timeout_detail(InspectPhaseId::Tier2Rescan),
                });
            }
            return Some(JobOutcome::Failed {
                message: format!(
                    "inspect_phase_timeout: tier2 {} aggregate did not complete within its phase wait budget; builder_state={}",
                    category.as_str(),
                    manager.tier2_builder_state_detail(category),
                ),
            });
        }
        let wait = Duration::from_millis(50).min(deadline.saturating_duration_since(now));
        match rx.recv_timeout(wait) {
            Ok(outcome) => return Some(outcome),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if inspect_cancellation_requested() {
                    return None;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Some(JobOutcome::Failed {
                    message: "inspect Tier-2 worker disconnected before completion".to_string(),
                });
            }
        }
    }
}

fn inspect_cancellation_requested() -> bool {
    crate::executor::current_job_cancellation()
        .is_some_and(|token| token.cancel_requested_before_commit())
}

fn inspect_interrupted_response(request_id: &str) -> Response {
    Response::error(
        request_id,
        "inspect_interrupted",
        "inspect request was abandoned before completion",
    )
}

fn fresh_payloads(
    outcomes: &BTreeMap<InspectCategory, JobOutcome>,
) -> Result<BTreeMap<InspectCategory, Value>, String> {
    let mut payloads = BTreeMap::new();
    for category in InspectCategory::active() {
        match outcomes.get(category) {
            Some(JobOutcome::Fresh { payload }) => {
                payloads.insert(*category, payload.clone());
            }
            Some(JobOutcome::Stale { .. }) => {
                return Err(format!("{} could not be stat-verified", category.as_str()));
            }
            Some(outcome @ JobOutcome::Pending { .. }) => {
                let mut message = format!("{} did not complete", category.as_str());
                if let Some(detail) = outcome.pending_detail() {
                    message.push_str(" (");
                    message.push_str(&detail);
                    message.push(')');
                }
                return Err(message);
            }
            Some(JobOutcome::Failed { message }) => {
                return Err(format!("{} failed: {message}", category.as_str()));
            }
            None => return Err(format!("{} did not produce an outcome", category.as_str())),
        }
    }
    Ok(payloads)
}

fn parse_top_k(params: &Value) -> Result<usize, String> {
    let Some(value) = params.get("topK").or_else(|| params.get("top_k")) else {
        return Ok(DEFAULT_TOP_K);
    };
    if value.is_null() || empty_string(value) {
        return Ok(DEFAULT_TOP_K);
    }
    let Some(top_k) = value.as_u64() else {
        return Err("inspect: topK must be a positive integer".to_string());
    };
    if top_k == 0 {
        return Err("inspect: topK must be greater than 0".to_string());
    }
    Ok((top_k as usize).min(MAX_TOP_K))
}

fn parse_sections(value: Option<&Value>) -> Result<Sections, String> {
    let Some(value) = value else {
        return Ok(Sections::summary_only());
    };
    if value.is_null() || empty_string(value) || empty_array(value) {
        return Ok(Sections::summary_only());
    }

    let mut categories = BTreeSet::new();
    match value {
        Value::String(section) => add_section(section, &mut categories)?,
        Value::Array(sections) => {
            for section in sections {
                if section.is_null() || empty_string(section) {
                    continue;
                }
                let Some(section) = section.as_str() else {
                    return Err("inspect: sections array entries must be strings".to_string());
                };
                add_section(section, &mut categories)?;
            }
        }
        _ => return Err("inspect: sections must be a string or string array".to_string()),
    }

    if categories.len() == InspectCategory::active().len() {
        Ok(Sections::all())
    } else {
        Ok(Sections {
            detail_categories: categories,
        })
    }
}

fn scope_was_provided(value: Option<&Value>) -> bool {
    let Some(value) = value else {
        return false;
    };
    !(value.is_null() || empty_string(value) || empty_array(value))
}

fn add_section(section: &str, categories: &mut BTreeSet<InspectCategory>) -> Result<(), String> {
    let section = section.trim();
    if section.is_empty() {
        return Ok(());
    }
    if section == "all" {
        categories.extend(InspectCategory::active().iter().copied());
        return Ok(());
    }
    let category = section
        .parse::<InspectCategory>()
        .map_err(|error| format!("inspect: {error}"))?;
    if !category.is_active() {
        return Err(format!(
            "inspect: category '{category}' is registered but disabled in v0.33"
        ));
    }
    categories.insert(category);
    Ok(())
}

fn parse_tier2_categories(value: Option<&Value>) -> Result<Vec<InspectCategory>, String> {
    let sections = parse_sections(value)?.detail_categories;
    let categories = if sections.is_empty() {
        InspectCategory::active()
            .iter()
            .copied()
            .filter(|category| category.is_tier2())
            .collect::<Vec<_>>()
    } else {
        sections
            .into_iter()
            .filter(|category| category.is_tier2())
            .collect::<Vec<_>>()
    };
    Ok(categories)
}

fn parse_scope(
    req: &RawRequest,
    ctx: &AppContext,
    project_root: &Path,
) -> Result<ParsedScope, Response> {
    let Some(value) = req.params.get("scope") else {
        return Ok(ParsedScope {
            job: JobScope::for_project(project_root.to_path_buf()),
            roots: Vec::new(),
        });
    };
    if value.is_null() || empty_string(value) || empty_array(value) {
        return Ok(ParsedScope {
            job: JobScope::for_project(project_root.to_path_buf()),
            roots: Vec::new(),
        });
    }

    let raw_scopes = match value {
        Value::String(scope) => vec![scope.clone()],
        Value::Array(scopes) => {
            let mut values = Vec::new();
            for scope in scopes {
                if scope.is_null() || empty_string(scope) {
                    continue;
                }
                let Some(scope) = scope.as_str() else {
                    return Err(Response::error(
                        &req.id,
                        "invalid_request",
                        "inspect: scope array entries must be strings",
                    ));
                };
                values.push(scope.to_string());
            }
            values
        }
        _ => {
            return Err(Response::error(
                &req.id,
                "invalid_request",
                "inspect: scope must be a string or string array",
            ));
        }
    };

    let mut roots = Vec::new();
    let mut missing = Vec::new();
    for scope in raw_scopes {
        let raw_path = PathBuf::from(&scope);
        let candidate = if raw_path.is_absolute() {
            raw_path
        } else {
            project_root.join(raw_path)
        };
        let validated = ctx.validate_path(&req.id, &candidate)?;
        if !validated.exists() {
            missing.push(scope);
            continue;
        }
        // Never `fs::canonicalize` directly: its Windows result is a verbatim
        // path, and every downstream comparison (workspace ownership, job
        // scope, analyzed-file membership) is against non-verbatim roots.
        roots.push(crate::inspect::job::canonicalize_normalized(&validated));
    }

    if !missing.is_empty() {
        let paths = missing
            .iter()
            .map(|path| format!("'{path}'"))
            .collect::<Vec<_>>()
            .join(", ");
        let (noun, verb) = if missing.len() == 1 {
            ("path", "does")
        } else {
            ("paths", "do")
        };
        return Err(Response::error(
            &req.id,
            "path_not_found",
            format!(
                "inspect: scope {noun} {paths} {verb} not exist (scope accepts one path string or an array of paths)"
            ),
        ));
    }

    roots.sort();
    roots.dedup();
    Ok(ParsedScope {
        job: JobScope::from_roots(project_root.to_path_buf(), roots.clone()),
        roots,
    })
}

fn scope_root_display(project_root: &Path, root: &Path) -> String {
    // Compare normalized copies: on Windows the canonical root carries the
    // verbatim prefix while a scope root may not, and mixed separators would
    // defeat a byte-wise strip and leak the absolute spelling into the reply.
    #[cfg(windows)]
    let (project_root, root) = (
        crate::windows_path::normalize_windows_path(project_root),
        crate::windows_path::normalize_windows_path(root),
    );
    #[cfg(windows)]
    let (project_root, root) = (project_root.as_path(), root.as_path());
    let relative = root.strip_prefix(project_root).unwrap_or(root);
    if relative.as_os_str().is_empty() {
        return ".".to_string();
    }
    relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn build_inspect_payload(
    snapshot: &InspectSnapshot,
    payloads: &BTreeMap<InspectCategory, Value>,
    sections: &Sections,
    top_k: usize,
    ctx: &AppContext,
    scope_roots: Option<&[PathBuf]>,
) -> Value {
    let scope_files = scope_roots.map(|_| {
        payloads
            .get(&InspectCategory::Diagnostics)
            .and_then(|payload| payload.get("coverage"))
            .and_then(|coverage| coverage.get("files"))
            .and_then(Value::as_u64)
            .or_else(|| {
                payloads
                    .get(&InspectCategory::Metrics)
                    .and_then(|payload| payload.get("files"))
                    .and_then(Value::as_u64)
            })
            .unwrap_or(0)
    });
    let no_files_matched_scope = scope_files == Some(0);
    let mut summary = Map::new();
    let mut details = Map::new();
    let mut gaps = Vec::new();

    for category in InspectCategory::active() {
        // `fresh_payloads` established this invariant before this emitter runs.
        // Keeping the fresh payload separate from JobOutcome prevents accidental
        // reintroduction of a stale or pending branch into a successful response.
        let payload = payloads
            .get(category)
            .expect("all active categories have a fresh inspect payload");
        if payload.get("unavailable").and_then(Value::as_bool) == Some(true) {
            let category_gaps = incomplete_category_gaps(*category, payload);
            gaps.extend(category_gaps.iter().cloned().map(|mut gap| {
                gap["categories"] = serde_json::json!([category.as_str()]);
                gap
            }));
            summary.insert(
                category.as_str().to_string(),
                serde_json::json!({
                    "unavailable": true, "complete": false, "gaps": category_gaps,
                }),
            );
            for key in ["building", "last_complete"] {
                if let Some(value) = payload.get(key) {
                    summary.get_mut(category.as_str()).unwrap()[key] = value.clone();
                }
            }
            if sections.includes(*category) {
                details.insert(category.as_str().to_string(), Value::Null);
            }
            continue;
        }
        let mut category_summary = summary_for(*category, payload);
        let dead_code_unavailable =
            *category == InspectCategory::DeadCode && dead_code_callgraph_unavailable(payload);
        if dead_code_unavailable {
            annotate_dead_code_unavailable(
                &mut category_summary,
                payload,
                crate::feature_status::observed_index_status(
                    ctx,
                    crate::feature_status::IndexPlane::Callgraph,
                ),
            );
        }
        if *category == InspectCategory::Duplicates && no_files_matched_scope {
            category_summary["total_analyzed_lines"] = serde_json::json!(0);
            category_summary["duplicated_percent"] = serde_json::json!(0.0);
        }
        if category_is_incomplete(payload) {
            category_summary["complete"] = Value::Bool(false);
            let category_gaps = incomplete_category_gaps(*category, payload);
            category_summary["gaps"] = Value::Array(category_gaps.clone());
            gaps.extend(category_gaps.into_iter().map(|mut gap| {
                gap["categories"] = serde_json::json!([category.as_str()]);
                gap
            }));
            for key in ["parse_errors", "skipped_files", "building"] {
                if let Some(value) = payload.get(key) {
                    category_summary[key] = value.clone();
                }
            }
        }
        if *category == InspectCategory::Diagnostics {
            attach_uncovered_file_rollup(&mut category_summary, &mut details, payload, top_k);
        }
        summary.insert(category.as_str().to_string(), category_summary);
        if dead_code_unavailable {
            // No analysis ran, so there is no findings list to show; an empty
            // list would read as "no dead code".
            if sections.includes(*category) {
                details.insert(category.as_str().to_string(), Value::Null);
            }
            continue;
        }
        if sections.includes(*category) {
            let detail = details_for(*category, payload, top_k);
            let total_count = payload
                .get("items")
                .or_else(|| payload.get("groups"))
                .and_then(Value::as_array)
                .map_or(0, |a| a.len());
            let shown = detail.as_array().map_or(0, |a| a.len());
            details.insert(category.as_str().to_string(), detail);
            if *category != InspectCategory::Metrics {
                crate::list_surfaces::inspect::attach_inspect_envelope(
                    &mut details,
                    category.as_str(),
                    shown,
                    total_count,
                );
            }
            if matches!(
                *category,
                InspectCategory::DeadCode | InspectCategory::UnusedExports
            ) {
                let test_only_detail = test_only_details_for(payload, top_k);
                let test_only_total = payload
                    .get("test_only_items")
                    .and_then(Value::as_array)
                    .map_or(0, |a| a.len());
                let test_only_shown = test_only_detail.as_array().map_or(0, |a| a.len());
                if test_only_shown > 0 || (top_k == 0 && test_only_total > 0) {
                    let key = format!("{}_test_only", category.as_str());
                    details.insert(key.clone(), test_only_detail);
                    crate::list_surfaces::inspect::attach_inspect_envelope(
                        &mut details,
                        &key,
                        test_only_shown,
                        test_only_total,
                    );
                }
            }
            if matches!(
                *category,
                InspectCategory::DeadCode
                    | InspectCategory::UnusedExports
                    | InspectCategory::Duplicates
            ) {
                let generated_detail = generated_details_for(payload, top_k);
                let generated_total = payload
                    .get("generated_items")
                    .and_then(Value::as_array)
                    .map_or(0, |a| a.len());
                let generated_shown = generated_detail.as_array().map_or(0, |a| a.len());
                if generated_shown > 0 || (top_k == 0 && generated_total > 0) {
                    let key = format!("{}_generated", category.as_str());
                    details.insert(key.clone(), generated_detail);
                    crate::list_surfaces::inspect::attach_inspect_envelope(
                        &mut details,
                        &key,
                        generated_shown,
                        generated_total,
                    );
                }
            }
        } else if *category == InspectCategory::Diagnostics {
            // Diagnostics detail is actionable even without an explicit section.
            // `top_k` limits rows only; summaries are always computed in full.
            let detail = details_for(*category, payload, top_k);
            let diag_total = payload
                .get("items")
                .and_then(Value::as_array)
                .map_or(0, |a| a.len());
            let diag_shown = detail.as_array().map_or(0, |a| a.len());
            if diag_shown > 0 || (top_k == 0 && diag_total > 0) {
                details.insert(category.as_str().to_string(), detail);
                crate::list_surfaces::inspect::attach_inspect_envelope(
                    &mut details,
                    category.as_str(),
                    diag_shown,
                    diag_total,
                );
            }
        }
    }

    let complete = partial_summary_reason(&summary).is_none();
    let text = render_inspect_text(
        &summary,
        &details,
        scope_roots.map(|roots| (roots.len(), scope_files.unwrap_or(0))),
    );
    let mut payload = serde_json::json!({
        "summary": Value::Object(summary),
        "text": text,
        "complete": complete,
        "scanner_state": {
            "tier2_last_run": tier2_last_run(snapshot),
            "tier2_trigger_reason": ctx.tier2_trigger_reason(),
            "disabled_categories": InspectCategory::disabled()
                .iter()
                .map(|category| category.as_str())
                .collect::<Vec<_>>(),
        }
    });
    if let Some(roots) = scope_roots {
        payload["scope_roots"] = Value::Array(
            roots
                .iter()
                .map(|root| Value::String(scope_root_display(&snapshot.project_root, root)))
                .collect(),
        );
        payload["scope_files"] = serde_json::json!(scope_files.unwrap_or(0));
        if no_files_matched_scope {
            payload["no_files_matched_scope"] = Value::Bool(true);
        }
    }
    if !details.is_empty() {
        payload["details"] = Value::Object(details);
    }
    if !gaps.is_empty() {
        payload["gaps"] = Value::Array(gaps);
    }
    payload
}

/// Render the compact agent-facing body. One source of truth for OpenCode + Pi.
fn render_inspect_text(
    summary: &Map<String, Value>,
    details: &Map<String, Value>,
    scope: Option<(usize, u64)>,
) -> String {
    let mut lines: Vec<String> = Vec::new();

    if let Some(reason) = partial_summary_reason(summary) {
        lines.push(format!("PARTIAL — {reason}"));
    }

    if let Some((root_count, file_count)) = scope {
        let root_label = if root_count == 1 { "root" } else { "roots" };
        let file_label = if file_count == 1 { "file" } else { "files" };
        let suffix = if file_count == 0 {
            " (no analyzed files under this scope)"
        } else {
            ""
        };
        lines.push(format!(
            "scope: {root_count} {root_label}, {file_count} {file_label}{suffix}"
        ));
    }

    // Counts are emitted only from verified producer results. A failed producer
    // is rendered separately so the remaining findings cannot read as all-clear.
    render_incomplete_categories(&mut lines, summary, details);
    // Uncomputed categories have no counts, so the incomplete-category notice
    // is their only output.
    let available_summary = summary
        .iter()
        .filter(|(_, value)| value.get("unavailable").and_then(Value::as_bool) != Some(true))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<Map<String, Value>>();
    let summary = &available_summary;
    render_not_applicable_producers(&mut lines, summary);
    render_scoped_diagnostics_coverage(&mut lines, summary);
    if let Some(notes) = summary
        .get("diagnostics")
        .and_then(|diagnostics| diagnostics.get("notes"))
        .and_then(Value::as_array)
    {
        lines.extend(notes.iter().filter_map(Value::as_str).map(str::to_string));
    }
    render_group_category(
        &mut lines,
        "Duplicates",
        summary,
        details,
        "duplicates",
        scope.is_some_and(|(_, files)| files == 0),
    );
    render_complexity_category(&mut lines, summary, details);
    render_cycles_category(&mut lines, summary, details);
    render_symbol_category(&mut lines, "Dead code", summary, details, "dead_code");
    render_symbol_category(
        &mut lines,
        "Unused exports",
        summary,
        details,
        "unused_exports",
    );
    render_todos(&mut lines, summary, details);
    render_diagnostics_category(&mut lines, summary, details);

    lines.join("\n")
}

/// Say how many scoped files a blocking scoped inspect obtained authoritative
/// diagnostics for, and how many it left out because of the file cap. The
/// count is of files with authoritative diagnostics, not of files handed to a
/// server: a file a server was asked about but never certified (its workspace
/// failed to load, say) is a gap, and counting it here would read as success
/// next to the gap line.
fn render_scoped_diagnostics_coverage(lines: &mut Vec<String>, summary: &Map<String, Value>) {
    if summary
        .get("diagnostics")
        .is_some_and(|section| section["complete"] == false)
    {
        return;
    }
    let Some(coverage) = summary
        .get("diagnostics")
        .and_then(|section| section.get("coverage"))
    else {
        return;
    };
    let files = coverage.get("files").and_then(Value::as_u64).unwrap_or(0);
    if files == 0 {
        return;
    }
    let authoritative = coverage
        .get("authoritative")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let not_examined = coverage
        .get("not_examined")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let noun = if files == 1 { "file" } else { "files" };
    let mut line =
        format!("diagnostics: authoritative results for {authoritative} of {files} scoped {noun}");
    if not_examined > 0 {
        let cap = coverage
            .get("file_cap")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        line.push_str(&format!(
            " ({not_examined} not examined: at most {cap} files per scoped inspect; narrow the scope)"
        ));
    }
    lines.push(line);
}

/// Name servers that were deliberately not started because the inspected area
/// has none of their files. Without this line a Rust repository whose
/// `package.json` exists only to install a tool gives no hint why TypeScript
/// produced nothing; it is not a failure and does not make the result partial.
fn render_not_applicable_producers(lines: &mut Vec<String>, summary: &Map<String, Value>) {
    for (category, value) in summary {
        for entry in value
            .get("not_applicable")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let producer = entry
                .get("producer")
                .and_then(Value::as_str)
                .unwrap_or("unknown producer");
            let reason = entry
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("no files to analyze");
            lines.push(format!(
                "{category}: producer {producer} not applicable ({reason})"
            ));
        }
    }
}

fn render_incomplete_categories(
    lines: &mut Vec<String>,
    summary: &Map<String, Value>,
    details: &Map<String, Value>,
) {
    // Most diagnostic reasons live only in the header. Overflow must remain
    // visible in the body, even for non-file producer failures.
    for (_, reason) in incomplete_summary_parts(summary)
        .into_iter()
        .skip(MAX_INSPECT_HEADER_PARTS)
        .filter(|(diagnostic, _)| *diagnostic)
    {
        lines.push(format!("Incomplete diagnostics: {reason}"));
    }
    for (category, value) in summary {
        if value.get("complete").and_then(Value::as_bool) != Some(false) {
            continue;
        }
        if category == "diagnostics" {
            render_uncovered_file_groups(lines, category, value, details);
            continue;
        }
        for gap in value
            .get("gaps")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let reason = gap
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("unavailable");
            if gap.get("kind").and_then(Value::as_str) == Some("tier2_unavailable") {
                lines.push(format!(
                    "Incomplete {category}: Tier-2 unavailable ({reason})"
                ));
                continue;
            }
            if gap.get("kind").and_then(Value::as_str) == Some("uncovered_file") {
                // Rendered below as one line per cause, not one line per file.
                continue;
            }
            if gap.get("kind").and_then(Value::as_str) == Some("checking_producer") {
                // Not a failure: the producer is still checking, and the
                // reason names it.
                match gap.get("root").and_then(Value::as_str) {
                    Some(root) => {
                        lines.push(format!("Incomplete {category}: {reason} (root {root})"))
                    }
                    None => lines.push(format!("Incomplete {category}: {reason}")),
                }
                continue;
            }
            if gap.get("kind").and_then(Value::as_str) == Some("analysis_incomplete") {
                // A scanner, not a language server, did not finish. An
                // aggregate gap that names no scanner keeps just its category
                // and reason rather than an invented producer name.
                match gap.get("producer").and_then(Value::as_str) {
                    Some(producer) => lines.push(format!(
                        "Incomplete {category}: {producer} did not finish ({reason})"
                    )),
                    None => lines.push(format!("Incomplete {category}: {reason}")),
                }
                continue;
            }
            if let Some(producer) = gap.get("producer").and_then(Value::as_str) {
                let producer = match gap.get("root").and_then(Value::as_str) {
                    Some(root) => format!("{producer} @ {root}"),
                    None => producer.to_string(),
                };
                if gap.get("kind").and_then(Value::as_str) == Some("unreported_producer") {
                    lines.push(format!("Incomplete {category}: {producer}: {reason}"));
                } else {
                    lines.push(format!(
                        "Incomplete {category}: producer {producer} failed ({reason})"
                    ));
                }
            } else {
                lines.push(format!("Incomplete {category}: {reason}"));
            }
        }
        render_uncovered_file_groups(lines, category, value, details);
    }
}

/// List at most `topK` affected paths, without repeating the header's causes
/// and file counts. Full per-file authority gaps stay in structured data.
fn render_uncovered_file_groups(
    lines: &mut Vec<String>,
    category: &str,
    section: &Value,
    details: &Map<String, Value>,
) {
    if section
        .get("uncovered_file_groups")
        .and_then(Value::as_array)
        .filter(|groups| !groups.is_empty())
        .is_none()
    {
        return;
    }
    // Causes and file counts are already in the PARTIAL header.
    let list_key = format!("{category}_uncovered_files");
    lines.push("Affected files:".to_string());
    for file in details
        .get(&list_key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        lines.push(format!("  {file}"));
    }
    if let Some(trailer) = crate::list_surfaces::inspect::trailer_from_details(details, &list_key) {
        lines.push(trailer);
    }
}

fn render_complexity_category(
    lines: &mut Vec<String>,
    summary: &Map<String, Value>,
    details: &Map<String, Value>,
) {
    let Some(section) = summary.get("complexity") else {
        return;
    };
    let count = section.get("count").and_then(Value::as_u64).unwrap_or(0);
    let threshold = section
        .get("threshold")
        .and_then(Value::as_u64)
        .unwrap_or(10);
    if count == 0 {
        lines.push(format!("Cyclomatic complexity: 0 functions >= {threshold}"));
        return;
    }
    let worst = section.get("worst").and_then(Value::as_object);
    let worst_text = worst.map_or_else(String::new, |item| {
        let file = item.get("file").and_then(Value::as_str).unwrap_or("?");
        let function = item.get("function").and_then(Value::as_str).unwrap_or("?");
        let complexity = item.get("complexity").and_then(Value::as_u64).unwrap_or(0);
        format!(" (worst: {file}::{function} {complexity})")
    });
    lines.push(format!(
        "Cyclomatic complexity: {count} functions >= {threshold}{worst_text}"
    ));
    let Some(items) = details.get("complexity").and_then(Value::as_array) else {
        return;
    };
    for item in items {
        let file = item.get("file").and_then(Value::as_str).unwrap_or("?");
        let function = item.get("function").and_then(Value::as_str).unwrap_or("?");
        let line = item.get("line").and_then(Value::as_u64).unwrap_or(0);
        let complexity = item.get("complexity").and_then(Value::as_u64).unwrap_or(0);
        lines.push(format!("  {file}:{line} {function} ({complexity})"));
    }
    if let Some(trailer) =
        crate::list_surfaces::inspect::trailer_from_details(details, "complexity")
    {
        lines.push(trailer);
    }
}

fn render_cycles_category(
    lines: &mut Vec<String>,
    summary: &Map<String, Value>,
    details: &Map<String, Value>,
) {
    if !details.contains_key("cycles") {
        return;
    }
    let Some(section) = summary.get("cycles") else {
        return;
    };
    if let Some(status) = section.get("status").and_then(Value::as_str) {
        lines.push(format!("Import cycles: {status}"));
        return;
    }
    let count = section.get("count").and_then(Value::as_u64).unwrap_or(0);
    if count == 0 {
        lines.push("Import cycles: 0".to_string());
        return;
    }
    let largest = section.get("largest").and_then(Value::as_u64).unwrap_or(0);
    let cycle_word = if count == 1 { "cycle" } else { "cycles" };
    let file_word = if largest == 1 { "file" } else { "files" };
    if section["scope_relation"] == "touches" {
        lines.push(format!("Import cycles: {count} {cycle_word} touch the scope (largest: {largest} scoped {file_word})"));
    } else {
        lines.push(format!(
            "Import cycles: {count} import {cycle_word} (largest: {largest} {file_word})"
        ));
    }
    let Some(items) = details.get("cycles").and_then(Value::as_array) else {
        return;
    };
    for item in items {
        let cycle = item.get("cycle").and_then(Value::as_str).unwrap_or("?");
        let edge_kind = item
            .get("edge_kind")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        lines.push(format!("  {cycle} [{edge_kind}]"));
        if let Some(edges) = item.get("edges").and_then(Value::as_array) {
            for edge in edges {
                let from = edge.get("from").and_then(Value::as_str).unwrap_or("?");
                let to = edge.get("to").and_then(Value::as_str).unwrap_or("?");
                let imports = edge
                    .get("imports")
                    .and_then(Value::as_array)
                    .map(|imports| {
                        imports
                            .iter()
                            .map(render_cycle_import)
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                if imports.is_empty() {
                    lines.push(format!("    {from} -> {to}"));
                } else {
                    lines.push(format!("    {from} -> {to} via {imports}"));
                }
            }
        }
    }
    if let Some(trailer) = crate::list_surfaces::inspect::trailer_from_details(details, "cycles") {
        lines.push(trailer);
    }
}

fn render_cycle_import(import: &Value) -> String {
    let specifier = import
        .get("specifier")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let kind = import
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("import");
    let line = import.get("line").and_then(Value::as_u64).unwrap_or(0);
    if line == 0 {
        format!("{kind} '{specifier}'")
    } else {
        format!("{kind} '{specifier}' line {line}")
    }
}

/// Pick the fuller drill-down list when present (sections requested), else the
/// summary's ranked `top` preview.
fn category_items<'a>(
    summary: &'a Map<String, Value>,
    details: &'a Map<String, Value>,
    key: &str,
) -> Option<&'a Vec<Value>> {
    if let Some(items) = details.get(key).and_then(Value::as_array) {
        return Some(items);
    }
    summary
        .get(key)
        .and_then(|s| s.get("top"))
        .and_then(Value::as_array)
}

/// Categories whose findings are `{file, symbol}` (dead_code, unused_exports).
fn render_symbol_category(
    lines: &mut Vec<String>,
    label: &str,
    summary: &Map<String, Value>,
    details: &Map<String, Value>,
    key: &str,
) {
    let Some(section) = summary.get(key) else {
        return;
    };
    if key == "dead_code"
        && section.get("callgraph_available").and_then(Value::as_bool) == Some(false)
    {
        let reason = section
            .get("callgraph_unavailable_reason")
            .and_then(Value::as_str)
            .unwrap_or("no callgraph");
        match (
            section.get("code").and_then(Value::as_str),
            section.pointer("/index/status").and_then(Value::as_str),
        ) {
            (Some(code), Some(status)) => {
                let cause = section
                    .pointer("/index/reason")
                    .and_then(Value::as_str)
                    .map(|cause| format!(": {cause}"))
                    .unwrap_or_default();
                lines.push(format!(
                    "Dead code analysis unavailable ({code}; callgraph index {status}{cause})"
                ));
            }
            _ => lines.push(format!("Dead code analysis unavailable ({reason})")),
        }
        return;
    }
    if let Some(status) = section.get("status").and_then(Value::as_str) {
        if let Some(reason) = section.get("reason").and_then(Value::as_str) {
            lines.push(format!("{label}: {status} ({reason})"));
        } else {
            lines.push(format!("{label}: {status}"));
        }
        return;
    }
    let count = section.get("count").and_then(Value::as_u64).unwrap_or(0);
    let suffix = dead_code_language_suffix(section);
    let skipped_suffix = dead_code_skipped_language_suffix(section);
    let generated_suffix = generated_count_suffix(section);
    let excluded = excluded_test_clause(section);
    if count == 0 {
        lines.push(format!(
            "{label}: 0{generated_suffix}{skipped_suffix}{excluded}"
        ));
    } else {
        lines.push(format!(
            "{label}: {count}{suffix}{generated_suffix}{skipped_suffix}{excluded}:"
        ));
        if let Some(items) = category_items(summary, details, key) {
            for item in items.iter().filter(|item| !item_is_generated(item)) {
                let file = item.get("file").and_then(Value::as_str).unwrap_or("?");
                let symbol = item.get("symbol").and_then(Value::as_str).unwrap_or("?");
                lines.push(format!("  {file}::{symbol}"));
            }
        }
        if let Some(trailer) = crate::list_surfaces::inspect::trailer_from_details(details, key) {
            lines.push(trailer);
        }
    }
    render_generated_symbol_usage(lines, summary, details, key);
    render_test_only_usage(lines, summary, details, key);
}

/// The headline clause for findings a category withheld because they live in
/// test trees or fixtures (`excluded_test_count` in `excluded_test_files`
/// files). Empty when nothing was withheld, so product-only counts never hide
/// that more exists.
fn excluded_test_clause(section: &Value) -> String {
    let count = section
        .get("excluded_test_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if count == 0 {
        return String::new();
    }
    let files = section
        .get("excluded_test_files")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let file_label = if files == 1 { "file" } else { "files" };
    format!(
        " · excluded {} in {} test/fixture {file_label} (pass includeTests to see them)",
        thousands(count),
        thousands(files)
    )
}

/// `2940` as `2,940`.
fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

fn generated_count_suffix(section: &Value) -> String {
    let generated_count = section
        .get("generated_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if generated_count == 0 {
        String::new()
    } else {
        format!(" (generated: {generated_count})")
    }
}

fn item_is_generated(item: &Value) -> bool {
    item.get("generated").and_then(Value::as_bool) == Some(true)
}

fn render_generated_symbol_usage(
    lines: &mut Vec<String>,
    summary: &Map<String, Value>,
    details: &Map<String, Value>,
    key: &str,
) {
    let generated_count = summary
        .get(key)
        .and_then(|section| section.get("generated_count"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if generated_count == 0 {
        return;
    }
    lines.push(format!("  generated: {generated_count}:"));
    if let Some(items) = generated_items(summary, details, key) {
        for item in items {
            let file = item.get("file").and_then(Value::as_str).unwrap_or("?");
            let symbol = item.get("symbol").and_then(Value::as_str).unwrap_or("?");
            lines.push(format!("    {file}::{symbol}"));
        }
    }
    if let Some(trailer) =
        crate::list_surfaces::inspect::trailer_from_details(details, &format!("{key}_generated"))
    {
        lines.push(trailer);
    }
}

fn render_test_only_usage(
    lines: &mut Vec<String>,
    summary: &Map<String, Value>,
    details: &Map<String, Value>,
    key: &str,
) {
    let test_only_count = summary
        .get(key)
        .and_then(|section| section.get("test_only_count"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if test_only_count == 0 {
        return;
    }
    lines.push(format!("  test-only usage: {test_only_count}:"));
    if let Some(items) = test_only_items(summary, details, key) {
        for item in items {
            let file = item.get("file").and_then(Value::as_str).unwrap_or("?");
            let symbol = item.get("symbol").and_then(Value::as_str).unwrap_or("?");
            let used_by = format_used_by_tests(item.get("used_by"));
            lines.push(format!("    {file}::{symbol} — used by {used_by}"));
        }
    }
    if let Some(trailer) =
        crate::list_surfaces::inspect::trailer_from_details(details, &format!("{key}_test_only"))
    {
        lines.push(trailer);
    }
}

fn test_only_items<'a>(
    summary: &'a Map<String, Value>,
    details: &'a Map<String, Value>,
    key: &str,
) -> Option<&'a Vec<Value>> {
    let detail_key = format!("{key}_test_only");
    if let Some(items) = details.get(&detail_key).and_then(Value::as_array) {
        return Some(items);
    }
    summary
        .get(key)
        .and_then(|s| s.get("test_only_top"))
        .and_then(Value::as_array)
}

fn generated_items<'a>(
    summary: &'a Map<String, Value>,
    details: &'a Map<String, Value>,
    key: &str,
) -> Option<&'a Vec<Value>> {
    let detail_key = format!("{key}_generated");
    if let Some(items) = details.get(&detail_key).and_then(Value::as_array) {
        return Some(items);
    }
    summary
        .get(key)
        .and_then(|s| s.get("generated_top"))
        .and_then(Value::as_array)
}

fn format_used_by_tests(value: Option<&Value>) -> String {
    let names = value
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    if names.is_empty() {
        "test file".to_string()
    } else {
        names.join(", ")
    }
}

/// `(rust 214, ts 143)` language breakdown for dead_code; empty for others.
fn dead_code_language_suffix(section: &Value) -> String {
    let Some(by_lang) = section.get("by_language").and_then(Value::as_object) else {
        return String::new();
    };
    if by_lang.is_empty() {
        return String::new();
    }
    let mut pairs: Vec<(&String, u64)> = by_lang
        .iter()
        .map(|(k, v)| (k, v.as_u64().unwrap_or(0)))
        .collect();
    pairs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    let rendered = pairs
        .iter()
        .map(|(lang, n)| format!("{} {n}", short_lang(lang)))
        .collect::<Vec<_>>()
        .join(", ");
    format!(" ({rendered})")
}

fn dead_code_skipped_language_suffix(section: &Value) -> String {
    let Some(languages) = section.get("languages_skipped").and_then(Value::as_array) else {
        return String::new();
    };
    if languages.is_empty() {
        return String::new();
    }
    let mut languages = languages
        .iter()
        .filter_map(Value::as_str)
        .map(short_lang)
        .collect::<Vec<_>>();
    languages.sort_unstable();
    languages.dedup();
    if languages.is_empty() {
        String::new()
    } else {
        format!(" ({} not analyzed)", languages.join(", "))
    }
}

fn short_lang(lang: &str) -> &str {
    match lang {
        "typescript" => "ts",
        "javascript" => "js",
        "python" => "py",
        other => other,
    }
}

/// Duplicates: `{cost, files: [a, b, ...]}`.
fn render_group_category(
    lines: &mut Vec<String>,
    label: &str,
    summary: &Map<String, Value>,
    details: &Map<String, Value>,
    key: &str,
    show_zero_denominator: bool,
) {
    if key == "duplicates" {
        render_duplicates_category(lines, label, summary, details, key, show_zero_denominator);
        return;
    }

    let Some(section) = summary.get(key) else {
        return;
    };
    if let Some(status) = section.get("status").and_then(Value::as_str) {
        lines.push(format!("{label}: {status}"));
        return;
    }
    let count = section.get("count").and_then(Value::as_u64).unwrap_or(0);
    let excluded = excluded_test_clause(section);
    if count == 0 {
        lines.push(format!("{label}: 0{excluded}"));
        return;
    }
    lines.push(format!("{label}: {count}{excluded} (top by cost):"));
    if let Some(items) = category_items(summary, details, key) {
        for item in items.iter().filter(|item| !item_is_generated(item)) {
            let cost = item.get("cost").and_then(Value::as_u64).unwrap_or(0);
            let files: Vec<&str> = item
                .get("files")
                .and_then(Value::as_array)
                .map(|arr| arr.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            lines.push(format!("  {cost}  {}", files.join(" == ")));
        }
    }
    if let Some(trailer) = crate::list_surfaces::inspect::trailer_from_details(details, key) {
        lines.push(trailer);
    }
}

fn render_duplicates_category(
    lines: &mut Vec<String>,
    label: &str,
    summary: &Map<String, Value>,
    details: &Map<String, Value>,
    key: &str,
    show_zero_denominator: bool,
) {
    let Some(section) = summary.get(key) else {
        return;
    };
    if let Some(status) = section.get("status").and_then(Value::as_str) {
        lines.push(format!("{label}: {status}"));
        return;
    }

    let count = section.get("count").and_then(Value::as_u64).unwrap_or(0);
    let generated_count = section
        .get("generated_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let generated_suffix = if generated_count == 0 {
        String::new()
    } else {
        format!(" (generated: {generated_count})")
    };
    let excluded = excluded_test_clause(section);
    if section["scope_relation"] == "touches" && !show_zero_denominator {
        lines.push(format!(
            "{label}: {count} groups touch the scope{generated_suffix}"
        ));
        if count > 0 {
            render_duplicate_rows(lines, summary, details, key);
        }
        if let Some(trailer) = crate::list_surfaces::inspect::trailer_from_details(details, key) {
            lines.push(trailer);
        }
        render_generated_duplicate_usage(lines, summary, details, key);
        return;
    }
    let Some(duplicated_lines) = section.get("duplicated_lines").and_then(Value::as_u64) else {
        if count == 0 {
            lines.push(format!("{label}: 0{generated_suffix}{excluded}"));
            render_generated_duplicate_usage(lines, summary, details, key);
            return;
        }
        lines.push(format!(
            "{label}: {count}{}{generated_suffix}{excluded} (top by cost):",
            duplicate_suppression_clause(section)
        ));
        render_duplicate_rows(lines, summary, details, key);
        if let Some(trailer) = crate::list_surfaces::inspect::trailer_from_details(details, key) {
            lines.push(trailer);
        }
        render_generated_duplicate_usage(lines, summary, details, key);
        return;
    };

    let total_lines = section
        .get("total_analyzed_lines")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let percent = section
        .get("duplicated_percent")
        .and_then(Value::as_f64)
        .unwrap_or_else(|| duplicate_percent(duplicated_lines, total_lines));
    let file_count = section
        .get("duplicated_file_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let group_count = count;
    let suppression_clause = duplicate_suppression_clause(section);
    let suffix = if count > 0 { " (top by cost):" } else { "" };
    // Old cached contributions can lack line counts. Suppress their unknown
    // denominator, but print an explicit zero when a resolved scope contains
    // no files in the inspect corpus.
    let percent_clause = if total_lines > 0 || show_zero_denominator {
        format!(
            " ({}% of {total_lines} analyzed lines)",
            format_percent(percent)
        )
    } else {
        String::new()
    };
    lines.push(format!(
        "{label}: {duplicated_lines} duplicated lines{percent_clause} across {file_count} files, {group_count} {}{suppression_clause}{generated_suffix}{excluded}{suffix}",
        plural_group(group_count),
    ));
    if count > 0 {
        render_duplicate_rows(lines, summary, details, key);
    }
    if let Some(trailer) = crate::list_surfaces::inspect::trailer_from_details(details, key) {
        lines.push(trailer);
    }
    render_generated_duplicate_usage(lines, summary, details, key);
}

fn render_duplicate_rows(
    lines: &mut Vec<String>,
    summary: &Map<String, Value>,
    details: &Map<String, Value>,
    key: &str,
) {
    if let Some(items) = category_items(summary, details, key) {
        for item in items.iter().filter(|item| !item_is_generated(item)) {
            let cost = item.get("cost").and_then(Value::as_u64).unwrap_or(0);
            let files: Vec<&str> = item
                .get("files")
                .and_then(Value::as_array)
                .map(|arr| arr.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            lines.push(format!("  {cost}  {}", files.join(" == ")));
            if duplicate_group_file_count(&files) >= 3 {
                lines
                    .push("      suggestion: consider extracting into a shared module".to_string());
            }
        }
    }
}

fn render_generated_duplicate_usage(
    lines: &mut Vec<String>,
    summary: &Map<String, Value>,
    details: &Map<String, Value>,
    key: &str,
) {
    let generated_count = summary
        .get(key)
        .and_then(|section| section.get("generated_count"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if generated_count == 0 {
        return;
    }
    lines.push(format!("  generated: {generated_count}:"));
    if let Some(items) = generated_items(summary, details, key) {
        for item in items {
            let cost = item.get("cost").and_then(Value::as_u64).unwrap_or(0);
            let files: Vec<&str> = item
                .get("files")
                .and_then(Value::as_array)
                .map(|arr| arr.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            lines.push(format!("    {cost}  {}", files.join(" == ")));
        }
    }
    if let Some(trailer) =
        crate::list_surfaces::inspect::trailer_from_details(details, &format!("{key}_generated"))
    {
        lines.push(trailer);
    }
}

/// Suppression counts as a headline clause (e.g. " (238 suppressed by
/// expected_mirrors, 8 by aft:expected-duplicate)"), so they read as summary
/// stats instead of items inside the top-groups list.
fn duplicate_suppression_clause(section: &Value) -> String {
    let mirror = section
        .get("mirror_suppressed_groups")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let marker = section
        .get("marker_suppressed_groups")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let mut parts = Vec::new();
    if mirror > 0 {
        parts.push(format!("{mirror} suppressed by expected_mirrors"));
    }
    if marker > 0 {
        parts.push(format!("{marker} by aft:expected-duplicate"));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", parts.join(", "))
    }
}

fn plural_group(count: u64) -> &'static str {
    if count == 1 {
        "group"
    } else {
        "groups"
    }
}

fn duplicate_group_file_count(files: &[&str]) -> usize {
    files
        .iter()
        .map(|file| display_file_from_duplicate_occurrence(file))
        .collect::<std::collections::BTreeSet<_>>()
        .len()
}

fn display_file_from_duplicate_occurrence(value: &str) -> &str {
    let Some((file, range)) = value.rsplit_once(':') else {
        return value;
    };
    let Some((start, end)) = range.split_once('-') else {
        return value;
    };
    if start.chars().all(|char| char.is_ascii_digit())
        && end.chars().all(|char| char.is_ascii_digit())
    {
        file
    } else {
        value
    }
}

fn duplicate_percent(duplicated_lines: u64, total_lines: u64) -> f64 {
    if total_lines == 0 {
        0.0
    } else {
        (duplicated_lines as f64 * 100.0) / total_lines as f64
    }
}

fn format_percent(percent: f64) -> String {
    format!("{percent:.1}")
}

fn render_todos(
    lines: &mut Vec<String>,
    summary: &Map<String, Value>,
    details: &Map<String, Value>,
) {
    let Some(section) = summary.get("todos") else {
        return;
    };
    let count = section.get("count").and_then(Value::as_u64).unwrap_or(0);
    let excluded = excluded_test_clause(section);
    if count == 0 {
        // Zero product TODOs prints nothing, unless some were withheld.
        if !excluded.is_empty() {
            lines.push(format!("TODOs: 0{excluded}"));
        }
        return;
    }
    let by_kind = section
        .get("by_kind")
        .and_then(Value::as_object)
        .map(|map| {
            let mut pairs: Vec<(&String, u64)> = map
                .iter()
                .map(|(k, v)| (k, v.as_u64().unwrap_or(0)))
                .collect();
            pairs.sort_by(|a, b| a.0.cmp(b.0));
            pairs
                .iter()
                .map(|(kind, n)| format!("{kind} {n}"))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    if by_kind.is_empty() {
        lines.push(format!("TODOs: {count}{excluded}"));
    } else {
        lines.push(format!("TODOs: {count} ({by_kind}){excluded}"));
    }
    // Detail rows only when explicitly drilled into (sections: ["todos"]) — the
    // scanner populates details["todos"] only then, keeping the default summary
    // compact while honoring an explicit request for the items.
    if let Some(items) = details.get("todos").and_then(Value::as_array) {
        for item in items {
            let file = item.get("file").and_then(Value::as_str).unwrap_or("?");
            let line = item.get("line").and_then(Value::as_u64).unwrap_or(0);
            let marker = item.get("marker").and_then(Value::as_str).unwrap_or("?");
            let text = item.get("text").and_then(Value::as_str).unwrap_or("");
            lines.push(format!("  {file}:{line} {marker} {text}"));
        }
    }
    if let Some(trailer) = crate::list_surfaces::inspect::trailer_from_details(details, "todos") {
        lines.push(trailer);
    }
}

fn render_diagnostics_category(
    lines: &mut Vec<String>,
    summary: &Map<String, Value>,
    details: &Map<String, Value>,
) {
    // Unknown totals belong only in the status header. Counts from producers
    // that answered remain useful and must not imply an all-clear total.
    if let Some(producers) = summary
        .get("diagnostics")
        .filter(|section| section["complete"] == false)
        .and_then(|section| section["by_producer"].as_object())
    {
        for (producer, counts) in producers {
            if let (Some(e), Some(w), Some(i), Some(h)) = (
                counts["errors"].as_u64(),
                counts["warnings"].as_u64(),
                counts["info"].as_u64(),
                counts["hints"].as_u64(),
            ) {
                lines.push(format!(
                    "diagnostics: {e} errors, {w} warnings, {i} info, {h} hints from {producer}"
                ));
            }
        }
    } else if !summary
        .get("diagnostics")
        .is_some_and(|section| section["complete"] == false)
    {
        if let Some(line) = crate::subc_format::format_diagnostics_summary_with(
            Some(&Value::Object(summary.clone())),
            true,
        ) {
            lines.push(line);
        }
    }
    let trailer = crate::list_surfaces::inspect::trailer_from_details(details, "diagnostics");
    if trailer.is_none()
        && details
            .get("diagnostics")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty)
    {
        return;
    }

    if !lines.is_empty() {
        lines.push(String::new());
    }

    let provisional = summary.get("diagnostics").is_some_and(|section| {
        section.get("status").and_then(Value::as_str) == Some("pending")
            || section.get("status").and_then(Value::as_str) == Some("incomplete")
            || section.get("provisional_counts").is_some()
    });
    lines.push(if provisional {
        "diagnostics details (provisional — analyzer not ready; counts excluded from E/W):"
            .to_string()
    } else {
        "diagnostics details:".to_string()
    });

    if let Some(items) = details.get("diagnostics").and_then(Value::as_array) {
        for item in items {
            if let Some(d) = item.as_object() {
                let severity = d
                    .get("severity")
                    .and_then(Value::as_str)
                    .unwrap_or("information");
                let message = d
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("(no message)");
                let source = d.get("source").and_then(Value::as_str);
                let suffix = source.map(|s| format!(" [{s}]")).unwrap_or_default();
                lines.push(format!(
                    "- {} {} {}{}",
                    format_diagnostic_location(d),
                    severity,
                    message,
                    suffix
                ));
            }
        }
    }
    if let Some(trailer) = trailer {
        lines.push(trailer);
    }
}

fn format_diagnostic_location(d: &Map<String, Value>) -> String {
    let file = d
        .get("file")
        .and_then(Value::as_str)
        .unwrap_or("(unknown file)");
    let line = d.get("line").and_then(Value::as_u64);
    let column = d.get("column").and_then(Value::as_u64);
    match (line, column) {
        (None, _) => file.to_string(),
        (Some(line), None) => format!("{file}:{line}"),
        (Some(line), Some(col)) => format!("{file}:{line}:{col}"),
    }
}

/// True when the dead-code aggregate could not run because the callgraph was
/// not usable.
fn dead_code_callgraph_unavailable(payload: &Value) -> bool {
    payload.get("callgraph_available").and_then(Value::as_bool) == Some(false)
}

/// Mark an unavailable dead-code summary with the analysis code, the callgraph
/// index status and cause, and null findings, so it can never read as zero
/// findings. Other analyses in the same inspect response are unaffected.
///
/// `observation` is the callgraph plane's current state. When the plane now
/// reports ready but this aggregate was computed without a usable graph, the
/// analysis is still unavailable; its cause comes from the aggregate.
fn annotate_dead_code_unavailable(
    summary: &mut Value,
    payload: &Value,
    observation: crate::feature_status::IndexObservation,
) {
    use crate::feature_status::{cause, IndexEffective, IndexObservation};
    let observation = if observation.is_ready() {
        IndexObservation::unavailable(
            payload
                .get("callgraph_unavailable_reason")
                .and_then(Value::as_str)
                .unwrap_or(cause::RUNTIME_NOT_OBSERVED),
        )
    } else {
        observation
    };
    let code = match observation.effective {
        IndexEffective::Off => "callgraph_off",
        IndexEffective::Building => "callgraph_building",
        IndexEffective::Ready | IndexEffective::Unavailable => "callgraph_unavailable",
    };
    summary["status"] = serde_json::json!("unavailable");
    summary["code"] = serde_json::json!(code);
    summary["index"] = observation.consumer_json();
    summary["findings"] = Value::Null;
}

#[cfg(test)]
pub(crate) fn summary_for_test(category: InspectCategory, payload: &Value) -> Value {
    summary_for(category, payload)
}

#[cfg(test)]
pub(crate) fn annotate_dead_code_unavailable_for_test(
    summary: &mut Value,
    payload: &Value,
    observation: crate::feature_status::IndexObservation,
) {
    annotate_dead_code_unavailable(summary, payload, observation);
}

fn summary_for(category: InspectCategory, payload: &Value) -> Value {
    computed_summary_for(category, payload)
}

fn computed_summary_for(category: InspectCategory, payload: &Value) -> Value {
    let mut summary = match category {
        InspectCategory::Diagnostics => diagnostics_summary_for(payload),
        InspectCategory::Metrics => serde_json::json!({
            "files": payload.get("files").or_else(|| payload.pointer("/totals/file_count")).and_then(Value::as_u64).unwrap_or(0),
            "symbols": payload.get("symbols").or_else(|| payload.pointer("/totals/symbol_count")).and_then(Value::as_u64).unwrap_or(0),
            "loc": payload.get("loc").or_else(|| payload.pointer("/totals/loc")).and_then(Value::as_u64).unwrap_or(0),
        }),
        InspectCategory::Todos => serde_json::json!({
            "count": count_from_payload(Some(payload)),
            "by_kind": payload.get("by_kind").or_else(|| payload.get("by_marker")).cloned().unwrap_or_else(|| serde_json::json!({})),
        }),
        InspectCategory::DeadCode
            if payload.get("callgraph_available").and_then(Value::as_bool) == Some(false) =>
        {
            // No dead-code analysis ran without the graph. Report the capability
            // failure without inventing a zero or claiming a complete result.
            serde_json::json!({
                "callgraph_available": false,
                "reason": payload.get("callgraph_unavailable_reason"),
            })
        }
        InspectCategory::DeadCode => serde_json::json!({
            "count": count_from_payload(Some(payload)),
            "generated_count": generated_count_from_payload(Some(payload)),
            "total_count": total_count_from_payload(Some(payload)),
            "test_only_count": test_only_count_from_payload(Some(payload)),
            "by_language": payload.get("by_language").cloned().unwrap_or_else(|| serde_json::json!({})),
            "languages_skipped": payload.get("languages_skipped").cloned().unwrap_or_else(|| serde_json::json!([])),
            "top": top_preview_from_payload(Some(payload)),
            "generated_top": generated_top_from_payload(Some(payload)),
            "test_only_top": test_only_top_from_payload(Some(payload)),
        }),
        InspectCategory::UnusedExports => serde_json::json!({
            "count": count_from_payload(Some(payload)),
            "generated_count": generated_count_from_payload(Some(payload)),
            "total_count": total_count_from_payload(Some(payload)),
            "test_only_count": test_only_count_from_payload(Some(payload)),
            "top": top_preview_from_payload(Some(payload)),
            "generated_top": generated_top_from_payload(Some(payload)),
            "test_only_top": test_only_top_from_payload(Some(payload)),
        }),
        InspectCategory::Duplicates => {
            let mut section = Map::new();
            section.insert(
                "count".to_string(),
                serde_json::json!(count_from_payload(Some(payload))),
            );
            section.insert(
                "total_groups".to_string(),
                serde_json::json!(payload
                    .get("total_groups")
                    .or_else(|| payload.get("groups_count"))
                    .and_then(Value::as_u64)
                    .unwrap_or_else(|| count_from_payload(Some(payload)))),
            );
            for key in [
                "scope_relation",
                "generated_count",
                "total_count",
                "duplicated_lines",
                "duplicated_percent",
                "duplicated_file_count",
                "generated_duplicated_lines",
                "generated_duplicated_file_count",
                "total_duplicated_lines",
                "total_duplicated_file_count",
                "total_analyzed_lines",
                "suppressed_groups",
                "mirror_suppressed_groups",
                "marker_suppressed_groups",
            ] {
                if let Some(value) = payload.get(key).cloned() {
                    section.insert(key.to_string(), value);
                }
            }
            section.insert("top".to_string(), top_preview_from_payload(Some(payload)));
            section.insert(
                "generated_top".to_string(),
                generated_top_from_payload(Some(payload)),
            );
            Value::Object(section)
        }
        InspectCategory::Cycles => serde_json::json!({
            "count": count_from_payload(Some(payload)),
            "largest": payload.get("largest").and_then(Value::as_u64).unwrap_or(0),
        }),
        InspectCategory::Complexity => serde_json::json!({
            "count": count_from_payload(Some(payload)),
            "threshold": payload.get("threshold").and_then(Value::as_u64).unwrap_or(10),
            "worst": payload.get("worst").cloned().unwrap_or(Value::Null),
        }),
        _ => serde_json::json!({ "count": count_from_payload(Some(payload)) }),
    };
    if category == InspectCategory::Cycles {
        if let Some(relation) = payload.get("scope_relation") {
            summary["scope_relation"] = relation.clone();
        }
    }
    // Findings withheld from `count` because they live in test files or
    // fixtures; carried through so a reader can see what was left out.
    if matches!(
        category,
        InspectCategory::Todos
            | InspectCategory::DeadCode
            | InspectCategory::UnusedExports
            | InspectCategory::Duplicates
    ) && summary.is_object()
    {
        for key in ["excluded_test_count", "excluded_test_files"] {
            if let Some(value) = payload.get(key) {
                summary[key] = value.clone();
            }
        }
    }
    summary
}

fn diagnostics_summary_for(payload: &Value) -> Value {
    let mut summary = serde_json::json!({
        "errors": payload.get("errors"),
        "warnings": payload.get("warnings"),
        "info": payload.get("info"),
        "hints": payload.get("hints"),
    });
    if let Some(by_producer) = payload.get("by_producer") {
        summary["by_producer"] = by_producer.clone();
    }
    if let Some(notes) = payload.get("notes") {
        summary["notes"] = notes.clone();
    }
    if let Some(not_applicable) = payload.get("not_applicable") {
        summary["not_applicable"] = not_applicable.clone();
    }
    if let Some(coverage) = payload.get("coverage") {
        summary["coverage"] = coverage.clone();
    }
    summary
}

/// Scoped files sharing one cause for their missing diagnostics: the
/// producer that should have analyzed them, that producer's workspace root,
/// and why it has no report. Both `producer` and `root` are absent when no
/// producer applies to the files at all.
struct UncoveredFileGroup<'a> {
    producer: Option<&'a str>,
    root: Option<&'a str>,
    reason: &'a str,
    files: Vec<&'a str>,
}

/// Group `uncovered_file` gaps by (root, producer, reason). Largest group
/// first so a cut path list shows the dominant cause; ties and files within a
/// group are ordered by name so the output is stable.
fn uncovered_file_groups(gaps: &[Value]) -> Vec<UncoveredFileGroup<'_>> {
    let mut groups: BTreeMap<(Option<&str>, Option<&str>, &str), Vec<&str>> = BTreeMap::new();
    for gap in gaps {
        // Unscoped failures carry the applicable paths as display metadata,
        // without introducing scoped per-file authority claims.
        if let Some(files) = gap.get("affected_files").and_then(Value::as_array) {
            groups
                .entry((
                    gap["root"].as_str(),
                    gap["producer"].as_str(),
                    gap["reason"].as_str().unwrap_or("unavailable"),
                ))
                .or_default()
                .extend(files.iter().filter_map(Value::as_str));
        }
        if gap.get("kind").and_then(Value::as_str) != Some("uncovered_file") {
            continue;
        }
        let Some(file) = gap.get("file").and_then(Value::as_str) else {
            continue;
        };
        // Gaps without a `cause` still group, under their generic reason.
        let cause = gap.get("cause");
        let producer = cause
            .and_then(|cause| cause.get("producer"))
            .and_then(Value::as_str);
        let root = cause
            .and_then(|cause| cause.get("root"))
            .and_then(Value::as_str);
        let reason = cause
            .and_then(|cause| cause.get("reason"))
            .or_else(|| gap.get("reason"))
            .and_then(Value::as_str)
            .unwrap_or("unavailable");
        groups
            .entry((root, producer, reason))
            .or_default()
            .push(file);
    }
    let mut groups = groups
        .into_iter()
        .map(|((root, producer, reason), mut files)| {
            files.sort_unstable();
            UncoveredFileGroup {
                producer,
                root,
                reason,
                files,
            }
        })
        .collect::<Vec<_>>();
    // The map yields groups in (root, producer, reason) order; the stable
    // size sort keeps that order when group sizes tie.
    groups.sort_by(|left, right| right.files.len().cmp(&left.files.len()));
    groups
}

/// Add the per-cause rollup of uncovered files to the diagnostics summary and
/// the first `top_k` affected paths to `details`, with a list envelope when
/// the path list is cut. The full per-file gap rows stay in `gaps` for
/// machine consumers; only the rendered path list is bounded.
fn attach_uncovered_file_rollup(
    category_summary: &mut Value,
    details: &mut Map<String, Value>,
    payload: &Value,
    top_k: usize,
) {
    let Some(gaps) = payload.get("gaps").and_then(Value::as_array) else {
        return;
    };
    let groups = uncovered_file_groups(gaps);
    if groups.is_empty() {
        return;
    }
    category_summary["uncovered_file_groups"] = Value::Array(
        groups
            .iter()
            .map(|group| {
                serde_json::json!({
                    "producer": group.producer,
                    "root": group.root,
                    "reason": group.reason,
                    "files": group.files.len(),
                })
            })
            .collect(),
    );
    let (listed, total) = uncovered_files_details_for(&groups, top_k);
    let shown = listed.len();
    let key = format!("{}_uncovered_files", InspectCategory::Diagnostics.as_str());
    details.insert(
        key.clone(),
        Value::Array(listed.into_iter().map(Value::from).collect()),
    );
    crate::list_surfaces::inspect::attach_inspect_envelope(details, &key, shown, total);
}

/// The first `top_k` uncovered paths in group order, and the total count.
fn uncovered_files_details_for<'a>(
    groups: &[UncoveredFileGroup<'a>],
    top_k: usize,
) -> (Vec<&'a str>, usize) {
    let total = groups.iter().map(|group| group.files.len()).sum();
    let listed = groups
        .iter()
        .flat_map(|group| group.files.iter().copied())
        .take(top_k)
        .collect();
    (listed, total)
}

fn details_for(category: InspectCategory, payload: &Value, top_k: usize) -> Value {
    if category == InspectCategory::Metrics {
        return computed_summary_for(category, payload);
    }
    let items = payload
        .get("items")
        .or_else(|| payload.get("groups"))
        .and_then(Value::as_array);
    match items {
        Some(items) => Value::Array(items.iter().take(top_k).cloned().collect()),
        None => serde_json::json!([]),
    }
}

fn test_only_details_for(payload: &Value, top_k: usize) -> Value {
    match payload.get("test_only_items").and_then(Value::as_array) {
        Some(items) => Value::Array(items.iter().take(top_k).cloned().collect()),
        None => serde_json::json!([]),
    }
}

fn generated_details_for(payload: &Value, top_k: usize) -> Value {
    match payload.get("generated_items").and_then(Value::as_array) {
        Some(items) => Value::Array(items.iter().take(top_k).cloned().collect()),
        None => serde_json::json!([]),
    }
}

fn available_count_from_payload(_category: InspectCategory, payload: &Value) -> Option<usize> {
    if category_is_incomplete(payload) {
        return None;
    }
    payload
        .get("count")
        .and_then(Value::as_u64)
        .map(|count| count as usize)
}

fn count_from_payload(payload: Option<&Value>) -> u64 {
    payload
        .and_then(|payload| payload.get("count"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

/// Pass through the scanner's already-ranked `top` preview (highest-signal
/// findings) into the summary view. Omitted (empty array) when absent so the
/// summary stays compact for empty/legacy payloads.
fn top_preview_from_payload(payload: Option<&Value>) -> Value {
    payload
        .and_then(|payload| payload.get("top"))
        .filter(|top| top.is_array())
        .cloned()
        .unwrap_or_else(|| serde_json::json!([]))
}

fn test_only_count_from_payload(payload: Option<&Value>) -> u64 {
    payload
        .and_then(|payload| payload.get("test_only_count"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

fn generated_count_from_payload(payload: Option<&Value>) -> u64 {
    payload
        .and_then(|payload| payload.get("generated_count"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

fn total_count_from_payload(payload: Option<&Value>) -> u64 {
    payload
        .and_then(|payload| payload.get("total_count"))
        .and_then(Value::as_u64)
        .unwrap_or_else(|| {
            count_from_payload(payload)
                + test_only_count_from_payload(payload)
                + generated_count_from_payload(payload)
        })
}

fn test_only_top_from_payload(payload: Option<&Value>) -> Value {
    payload
        .and_then(|payload| payload.get("test_only_top"))
        .filter(|top| top.is_array())
        .cloned()
        .unwrap_or_else(|| serde_json::json!([]))
}

fn generated_top_from_payload(payload: Option<&Value>) -> Value {
    payload
        .and_then(|payload| payload.get("generated_top"))
        .filter(|top| top.is_array())
        .cloned()
        .unwrap_or_else(|| serde_json::json!([]))
}

fn tier2_last_run(snapshot: &InspectSnapshot) -> Option<i64> {
    let cache =
        InspectCache::open_readonly(snapshot.inspect_dir.clone(), snapshot.project_root.clone())
            .ok()
            .flatten()?;
    InspectCategory::active()
        .iter()
        .copied()
        .filter(|category| category.is_tier2())
        .filter_map(|category| cache.last_full_run(category).ok().flatten())
        .max()
}

fn empty_string(value: &Value) -> bool {
    value.as_str().is_some_and(|value| value.trim().is_empty())
}

fn empty_array(value: &Value) -> bool {
    value.as_array().is_some_and(|value| value.is_empty())
}

fn invalid_request(id: &str, message: String) -> Response {
    Response::error(id, "invalid_request", message)
}

#[cfg(test)]
mod status_bar_refresh_tests {
    use super::*;
    use crate::parser::TreeSitterProvider;

    fn ctx() -> AppContext {
        AppContext::new(Box::new(TreeSitterProvider::new()), Default::default())
    }

    fn outcomes(
        entries: Vec<(InspectCategory, JobOutcome)>,
    ) -> BTreeMap<InspectCategory, JobOutcome> {
        entries.into_iter().collect()
    }

    // #1: a Pending-only Tier-2 (no scan has ever produced counts) must NOT
    // populate the status bar — otherwise it renders fabricated `~D0 U0 C0`
    // zeros that lie about project health.
    #[test]
    fn pending_tier2_does_not_populate_status_bar() {
        let ctx = ctx();
        assert!(ctx.status_bar_counts().is_none());

        refresh_status_bar_counts(
            &ctx,
            &outcomes(vec![
                (InspectCategory::DeadCode, JobOutcome::pending(true)),
                (InspectCategory::UnusedExports, JobOutcome::pending(true)),
                (InspectCategory::Duplicates, JobOutcome::pending(true)),
            ]),
        );

        assert!(
            ctx.status_bar_counts().is_none(),
            "Pending Tier-2 must leave the bar unpopulated (no fabricated zeros)"
        );
    }

    // Stale-without-cache is equally untrustworthy — also must not populate.
    #[test]
    fn stale_without_cache_does_not_populate_status_bar() {
        let ctx = ctx();
        refresh_status_bar_counts(
            &ctx,
            &outcomes(vec![(
                InspectCategory::DeadCode,
                JobOutcome::Stale {
                    cached: None,
                    in_flight: true,
                },
            )]),
        );
        assert!(ctx.status_bar_counts().is_none());
    }

    // A real Fresh outcome populates the bar with the actual counts.
    #[test]
    fn fresh_tier2_populates_status_bar() {
        let ctx = ctx();
        refresh_status_bar_counts(
            &ctx,
            &outcomes(vec![
                (
                    InspectCategory::DeadCode,
                    JobOutcome::Fresh {
                        payload: serde_json::json!({ "count": 7 }),
                    },
                ),
                (
                    InspectCategory::UnusedExports,
                    JobOutcome::Fresh {
                        payload: serde_json::json!({ "count": 3 }),
                    },
                ),
                (
                    InspectCategory::Duplicates,
                    JobOutcome::Fresh {
                        payload: serde_json::json!({ "count": 1 }),
                    },
                ),
            ]),
        );
        let counts = ctx.status_bar_count_values();
        assert_eq!(counts.errors, None);
        assert_eq!(counts.warnings, None);
        assert_eq!(counts.dead_code, Some(7));
        assert_eq!(counts.unused_exports, Some(3));
        assert_eq!(counts.duplicates, Some(1));
        assert!(!counts.tier2_stale);
    }

    // Stale-WITH-cache populates (last-known counts) and marks the bar stale.
    // All three categories must carry a cached value — the bar stays suppressed
    // until every Tier-2 category is real, never fabricating a 0 (#1).
    #[test]
    fn stale_with_cache_populates_and_marks_stale() {
        let ctx = ctx();
        let stale_cache = |count: i64| JobOutcome::Stale {
            cached: Some(serde_json::json!({ "count": count })),
            in_flight: true,
        };
        refresh_status_bar_counts(
            &ctx,
            &outcomes(vec![
                (InspectCategory::DeadCode, stale_cache(12)),
                (InspectCategory::UnusedExports, stale_cache(4)),
                (InspectCategory::Duplicates, stale_cache(2)),
            ]),
        );
        let counts = ctx.status_bar_count_values();
        assert_eq!(counts.errors, None);
        assert_eq!(counts.warnings, None);
        assert_eq!(counts.dead_code, Some(12));
        assert_eq!(counts.unused_exports, Some(4));
        assert_eq!(counts.duplicates, Some(2));
        assert!(counts.tier2_stale);
    }

    // A single category (others Pending) must NOT surface the bar — the core
    // partial-completion fabrication guard at the sync refresh path (#1).
    #[test]
    fn single_category_does_not_populate_status_bar() {
        let ctx = ctx();
        refresh_status_bar_counts(
            &ctx,
            &outcomes(vec![(
                InspectCategory::DeadCode,
                JobOutcome::Fresh {
                    payload: serde_json::json!({ "count": 9 }),
                },
            )]),
        );
        assert!(
            ctx.status_bar_counts().is_none(),
            "one real category must not surface a bar with fabricated U0 C0"
        );
    }

    #[test]
    fn incomplete_fresh_payload_keeps_status_bar_counts_stale_not_current() {
        let ctx = ctx();
        ctx.update_status_bar_tier2(Some(7), Some(3), Some(1), None, false);
        refresh_status_bar_counts(
            &ctx,
            &outcomes(vec![
                (
                    InspectCategory::DeadCode,
                    JobOutcome::Fresh {
                        payload: serde_json::json!({
                            "unavailable": true, "complete": false,
                            "building": {"state": "building"},
                            "gaps": [{"reason": "still building"}]
                        }),
                    },
                ),
                (
                    InspectCategory::UnusedExports,
                    JobOutcome::Fresh {
                        payload: serde_json::json!({
                            "count": 0, "complete": false, "parse_errors": [{"file": "a.ts"}]
                        }),
                    },
                ),
                (
                    InspectCategory::Duplicates,
                    JobOutcome::Fresh {
                        payload: serde_json::json!({"count": 1}),
                    },
                ),
            ]),
        );
        let counts = ctx.status_bar_count_values();
        assert!(counts.tier2_stale);
        assert_eq!(counts.dead_code, Some(7));
        assert_eq!(counts.unused_exports, Some(3));
        assert_eq!(counts.duplicates, Some(1));
    }
}

#[cfg(test)]
mod render_text_tests {
    use super::*;

    fn summary_map(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    fn render(summary: Value) -> String {
        render_inspect_text(&summary_map(summary), &Map::new(), None)
    }

    fn render_with_details(summary: Value, details: Value) -> String {
        render_inspect_text(&summary_map(summary), &summary_map(details), None)
    }

    #[test]
    fn scoped_unfinished_producer_has_one_status_line_golden() {
        let summary = summary_map(serde_json::json!({
            "diagnostics": {"complete": false, "by_producer": {}, "gaps": [
                {"kind": "checking_producer", "producer": "rust", "root": ".",
                 "reason": "rust-analyzer: cargo check still running; retry"},
                {"kind": "uncovered_file", "file": "src/lib.rs",
                 "cause": {"producer": "rust", "root": ".",
                           "reason": "still checking: rust-analyzer: cargo check still running; retry"}}
            ], "coverage": {"files": 1, "authoritative": 0},
            "uncovered_file_groups": [{"producer": "rust", "root": ".", "files": 1,
                "reason": "still checking: rust-analyzer: cargo check still running; retry"}]},
            "complexity": {"count": 0, "threshold": 10},
            "todos": {"count": 0}
        }));
        let details =
            summary_map(serde_json::json!({"diagnostics_uncovered_files": ["src/lib.rs"]}));
        let text = render_inspect_text(&summary, &details, Some((1, 1)));
        assert_eq!(text, "PARTIAL — diagnostics unknown: rust-analyzer @ .: cargo check still running (1 file); retry aft_inspect.\nscope: 1 root, 1 file\nAffected files:\n  src/lib.rs\nCyclomatic complexity: 0 functions >= 10");
        let response = Response::success(
            "scoped-inspect",
            serde_json::json!({
                "inspect_terminal": "partial", "partial_reason": partial_summary_reason(&summary),
                "text": text, "summary": summary, "details": details,
                "wait_stamp": {"text": "waited: yes; completed: lsp_start, lsp_quiescence",
                    "phases": [{"id": "lsp_start", "producer": "rust"}]}
            }),
        );
        assert_eq!(crate::subc_format::format_inspect_for_test(&response), text);
    }

    #[test]
    fn unscoped_fresh_body_is_byte_identical_golden() {
        assert_eq!(render(serde_json::json!({
            "duplicates": {"count": 0, "duplicated_lines": 0, "total_analyzed_lines": 968281},
            "complexity": {"count": 1, "threshold": 10,
                "worst": {"file": "outside.ts", "function": "perform", "complexity": 215}},
            "dead_code": {"count": 0, "test_only_count": 479},
            "unused_exports": {"count": 0, "test_only_count": 35},
            "todos": {"count": 0},
            "diagnostics": {"errors": 0, "warnings": 0, "info": 0, "hints": 0}
        })), "Duplicates: 0 duplicated lines (0.0% of 968281 analyzed lines) across 0 files, 0 groups\nCyclomatic complexity: 1 functions >= 10 (worst: outside.ts::perform 215)\nDead code: 0\n  test-only usage: 479:\nUnused exports: 0\n  test-only usage: 35:\ndiagnostics: 0 errors, 0 warnings, 0 info, 0 hints");
    }

    #[test]
    fn scoped_fixture_output_excludes_outside_duplicates_complexity_and_test_only_findings() {
        let scope = JobScope::from_roots("/repo", vec![PathBuf::from("/repo/src/in.rs")]);
        let payloads = [
            (
                InspectCategory::Duplicates,
                serde_json::json!({
                    "count": 1, "duplicated_lines": 40, "total_analyzed_lines": 968281,
                    "items": [{"files": ["src/out.rs:1-20", "src/other.rs:1-20"]}]
                }),
            ),
            (
                InspectCategory::Complexity,
                serde_json::json!({
                    "count": 1, "threshold": 10,
                    "worst": {"file": "src/out.rs", "function": "perform", "complexity": 215},
                    "items": [{"file": "src/out.rs", "function": "perform", "complexity": 215}]
                }),
            ),
            (
                InspectCategory::DeadCode,
                serde_json::json!({
                    "count": 0, "test_only_count": 479, "items": [],
                    "test_only_items": [{"file": "src/out.rs", "symbol": "outside"}],
                    "test_only_top": [{"file": "src/out.rs", "symbol": "outside"}]
                }),
            ),
            (
                InspectCategory::UnusedExports,
                serde_json::json!({
                    "count": 0, "test_only_count": 35, "items": [],
                    "test_only_items": [{"file": "src/out.rs", "symbol": "outside"}],
                    "test_only_top": [{"file": "src/out.rs", "symbol": "outside"}]
                }),
            ),
        ];
        let summary = payloads
            .into_iter()
            .map(|(category, payload)| {
                let filtered = crate::inspect::filter_payload_for_scope_for_test(payload, &scope);
                (
                    category.as_str().to_string(),
                    summary_for(category, &filtered),
                )
            })
            .collect();
        assert_eq!(render_inspect_text(&summary, &Map::new(), Some((1, 1))),
            "scope: 1 root, 1 file\nDuplicates: 0 groups touch the scope\nCyclomatic complexity: 0 functions >= 10\nDead code: 0\nUnused exports: 0");
    }

    #[test]
    fn aggregate_gap_renders_category_and_reason_without_a_producer() {
        let text = render(serde_json::json!({
            "dead_code": {
                "unavailable": true,
                "complete": false,
                "gaps": [{
                    "kind": "analysis_incomplete",
                    "reason": "tier2 dead_code aggregate did not complete; retry aft_inspect"
                }]
            }
        }));
        assert!(text.contains("Incomplete dead_code: tier2 dead_code aggregate did not complete; retry aft_inspect"), "{text}");
        assert!(!text.contains("producer"), "{text}");
    }

    #[test]
    fn renders_unavailable_dead_code_without_a_zero_count() {
        let text = render(serde_json::json!({
            "dead_code": { "callgraph_available": false }
        }));

        assert_eq!(text, "PARTIAL — dead code unavailable: analysis did not finish; retry aft_inspect.\nDead code analysis unavailable (no callgraph)");
        assert!(!text.contains("Dead code: 0"));
    }

    #[test]
    fn renders_complexity_summary_and_drill_down() {
        let text = render_with_details(
            serde_json::json!({
                "complexity": {
                    "count": 2,
                    "threshold": 10,
                    "worst": {
                        "file": "tests/hot.rs",
                        "function": "test_hotspot",
                        "line": 9,
                        "complexity": 99,
                    },
                },
            }),
            serde_json::json!({
                "complexity": [{
                    "file": "src/product.rs",
                    "function": "product_hotspot",
                    "line": 5,
                    "complexity": 10,
                    "language": "rust",
                }],
            }),
        );

        assert!(
            text.contains(
                "Cyclomatic complexity: 2 functions >= 10 (worst: tests/hot.rs::test_hotspot 99)"
            ),
            "{text}"
        );
        assert!(
            text.contains("  src/product.rs:5 product_hotspot (10)"),
            "{text}"
        );
    }

    #[test]
    fn renders_todo_detail_rows_when_drilled_into() {
        let text = render_with_details(
            serde_json::json!({ "todos": { "count": 2, "by_kind": { "BUG": 1, "TODO": 1 } } }),
            serde_json::json!({
                "todos": [
                    { "file": "src/a.ts", "line": 10, "marker": "BUG", "text": "leak here" },
                    { "file": "src/b.ts", "line": 4, "marker": "TODO", "text": "wire it" },
                ]
            }),
        );
        // Summary line still present, plus per-item rows.
        assert!(
            text.contains("TODOs: 2 (BUG 1, TODO 1)"),
            "summary:\n{text}"
        );
        assert!(
            text.contains("  src/a.ts:10 BUG leak here"),
            "row a:\n{text}"
        );
        assert!(text.contains("  src/b.ts:4 TODO wire it"), "row b:\n{text}");
    }

    #[test]
    fn omits_todo_detail_rows_without_drill_in() {
        // No details → count/by_kind only, no per-item rows (default compact).
        let text = render(serde_json::json!({
            "todos": { "count": 2, "by_kind": { "BUG": 1, "TODO": 1 } }
        }));
        assert!(
            text.contains("TODOs: 2 (BUG 1, TODO 1)"),
            "summary:\n{text}"
        );
        assert!(!text.contains("\n  "), "no detail rows expected:\n{text}");
    }

    #[test]
    fn renders_populated_categories_highest_signal_first() {
        let text = render(serde_json::json!({
            "duplicates": {
                "count": 2,
                "top": [
                    { "cost": 1083, "files": ["a/x.ts:1-9", "b/x.ts:1-9"] },
                    { "cost": 500, "files": ["a/y.ts:1-3", "b/y.ts:1-3"] },
                ],
            },
            "dead_code": {
                "count": 357,
                "by_language": { "rust": 214, "typescript": 143 },
                "top": [ { "file": "crates/aft/src/x.rs", "symbol": "foo" } ],
            },
            "unused_exports": {
                "count": 1,
                "top": [ { "file": "packages/aft-bridge/src/log.ts", "symbol": "sessionLog" } ],
            },
            "todos": { "count": 8, "by_kind": { "BUG": 2, "TODO": 3 } },
        }));

        // Order: duplicates → dead_code → unused_exports → todos.
        let dup = text.find("Duplicates:").expect("duplicates");
        let dead = text.find("Dead code:").expect("dead code");
        let unused = text.find("Unused exports:").expect("unused");
        let todos = text.find("TODOs:").expect("todos");
        assert!(
            dup < dead && dead < unused && unused < todos,
            "wrong order:\n{text}"
        );

        // Cost-ranked duplicate rows with `==` separator between the file pair.
        assert!(
            text.contains("1083  a/x.ts:1-9 == b/x.ts:1-9"),
            "dup row:\n{text}"
        );
        // dead_code language breakdown uses short names, count-desc.
        assert!(
            text.contains("Dead code: 357 (rust 214, ts 143):"),
            "dead head:\n{text}"
        );
        assert!(
            text.contains("  crates/aft/src/x.rs::foo"),
            "dead row:\n{text}"
        );
        assert!(
            text.contains("  packages/aft-bridge/src/log.ts::sessionLog"),
            "unused row:\n{text}"
        );
        assert!(text.contains("TODOs: 8 (BUG 2, TODO 3)"), "todos:\n{text}");

        // Metrics + scanner_state are NOT in the agent text.
        assert!(!text.contains("loc"), "metrics leaked into text:\n{text}");
        assert!(
            !text.contains("scanner_state"),
            "scanner_state leaked:\n{text}"
        );
        // Diagnostics + status bar are appended by the plugin layer, not here.
        assert!(
            !text.contains("diagnostics"),
            "diagnostics must be plugin-rendered:\n{text}"
        );
        assert!(
            !text.contains("[AFT"),
            "status bar must be plugin-appended:\n{text}"
        );
    }

    #[test]
    fn renders_test_only_usage_after_headline_items() {
        let text = render_with_details(
            serde_json::json!({
                "dead_code": {
                    "count": 1,
                    "top": [ { "file": "src/api.ts", "symbol": "plantedDead" } ],
                    "test_only_count": 2,
                    "test_only_top": [
                        { "file": "src/api.ts", "symbol": "testOnly", "used_by": ["api.test.ts"] },
                    ],
                },
                "unused_exports": {
                    "count": 0,
                    "top": [],
                    "test_only_count": 1,
                    "test_only_top": [
                        { "file": "src/barrel-target.ts", "symbol": "throughBarrel", "used_by": ["barrel.test.ts"] },
                    ],
                }
            }),
            serde_json::json!({
                "dead_code": [ { "file": "src/api.ts", "symbol": "plantedDead" } ],
                "dead_code_test_only": [
                    { "file": "src/api.ts", "symbol": "testOnly", "used_by": ["api.test.ts"] },
                    { "file": "src/barrel-target.ts", "symbol": "throughBarrel", "used_by": ["barrel.test.ts"] },
                ],
            }),
        );

        assert!(text.contains("Dead code: 1:"), "{text}");
        assert!(text.contains("  src/api.ts::plantedDead"), "{text}");
        assert!(text.contains("  test-only usage: 2:"), "{text}");
        assert!(
            text.contains("    src/api.ts::testOnly — used by api.test.ts"),
            "{text}"
        );
        assert!(
            text.contains("    src/barrel-target.ts::throughBarrel — used by barrel.test.ts"),
            "{text}"
        );
        assert!(text.contains("Unused exports: 0"), "{text}");
        assert!(
            text.contains("    src/barrel-target.ts::throughBarrel — used by barrel.test.ts"),
            "{text}"
        );
    }

    #[test]
    fn renders_dead_code_skipped_languages_as_not_analyzed() {
        let text = render(serde_json::json!({
            "dead_code": {
                "count": 0,
                "by_language": {},
                "languages_skipped": ["kotlin", "java"],
                "top": [],
            }
        }));

        assert!(
            text.contains("Dead code: 0 (java, kotlin not analyzed)"),
            "dead-code skipped language note missing:\n{text}"
        );
    }

    #[test]
    fn renders_generated_usage_after_headline_items() {
        let text = render_with_details(
            serde_json::json!({
                "duplicates": {
                    "count": 1,
                    "generated_count": 1,
                    "total_groups": 2,
                    "duplicated_lines": 6,
                    "duplicated_percent": 3.0,
                    "duplicated_file_count": 2,
                    "total_analyzed_lines": 200,
                    "top": [
                        { "cost": 10, "files": ["src/a.ts:1-3", "src/b.ts:1-3"] },
                    ],
                    "generated_top": [
                        { "cost": 100, "files": ["gen/a.ts:1-9", "gen/b.ts:1-9"], "generated": true },
                    ],
                },
                "dead_code": {
                    "count": 1,
                    "generated_count": 2,
                    "total_count": 3,
                    "top": [ { "file": "src/hand.ts", "symbol": "handDead" } ],
                    "generated_top": [
                        { "file": "gen/schema_pb.ts", "symbol": "generatedPathDead", "generated": true },
                    ],
                },
                "unused_exports": {
                    "count": 0,
                    "generated_count": 1,
                    "total_count": 1,
                    "top": [],
                    "generated_top": [
                        { "file": "src/banner.ts", "symbol": "bannerUnused", "generated": true },
                    ],
                }
            }),
            serde_json::json!({
                "duplicates": [
                    { "cost": 10, "files": ["src/a.ts:1-3", "src/b.ts:1-3"] },
                    { "cost": 100, "files": ["gen/a.ts:1-9", "gen/b.ts:1-9"], "generated": true },
                ],
                "duplicates_generated": [
                    { "cost": 100, "files": ["gen/a.ts:1-9", "gen/b.ts:1-9"], "generated": true },
                ],
                "dead_code": [
                    { "file": "src/hand.ts", "symbol": "handDead" },
                    { "file": "gen/schema_pb.ts", "symbol": "generatedPathDead", "generated": true },
                ],
                "dead_code_generated": [
                    { "file": "gen/schema_pb.ts", "symbol": "generatedPathDead", "generated": true },
                    { "file": "src/banner.ts", "symbol": "bannerDead", "generated": true },
                ],
            }),
        );

        assert!(
            text.contains("Duplicates: 6 duplicated lines (3.0% of 200 analyzed lines) across 2 files, 1 group (generated: 1) (top by cost):"),
            "{text}"
        );
        assert!(
            text.contains("  10  src/a.ts:1-3 == src/b.ts:1-3"),
            "{text}"
        );
        assert!(text.contains("  generated: 1:"), "{text}");
        assert!(
            text.contains("    100  gen/a.ts:1-9 == gen/b.ts:1-9"),
            "{text}"
        );

        assert!(text.contains("Dead code: 1 (generated: 2):"), "{text}");
        assert!(text.contains("  src/hand.ts::handDead"), "{text}");
        assert!(
            text.contains("    gen/schema_pb.ts::generatedPathDead"),
            "{text}"
        );
        assert!(text.contains("    src/banner.ts::bannerDead"), "{text}");

        assert!(text.contains("Unused exports: 0 (generated: 1)"), "{text}");
        assert!(text.contains("    src/banner.ts::bannerUnused"), "{text}");
    }

    #[test]
    fn renders_duplicate_framing_suppression_and_extraction_suggestions() {
        let text = render(serde_json::json!({
            "duplicates": {
                "count": 1,
                "total_groups": 1,
                "duplicated_lines": 42,
                "duplicated_percent": 10.4,
                "duplicated_file_count": 3,
                "total_analyzed_lines": 404,
                "mirror_suppressed_groups": 2,
                "marker_suppressed_groups": 1,
                "top": [
                    { "cost": 1083, "files": ["a/x.ts:1-9", "b/x.ts:1-9", "c/x.ts:1-9"] }
                ]
            }
        }));

        assert!(
            text.contains(
                "Duplicates: 42 duplicated lines (10.4% of 404 analyzed lines) across 3 files, 1 group (2 suppressed by expected_mirrors, 1 by aft:expected-duplicate) (top by cost):"
            ),
            "{text}"
        );
        assert!(
            !text.contains("  2 mirror groups suppressed"),
            "suppression stats must not render as list items: {text}"
        );
        assert!(
            text.contains("suggestion: consider extracting into a shared module"),
            "{text}"
        );
    }

    #[test]
    fn zero_counts_render_as_clean_zero() {
        let text = render(serde_json::json!({
            "duplicates": { "count": 0 },
            "dead_code": { "count": 0, "by_language": {} },
            "unused_exports": { "count": 0 },
            "todos": { "count": 0 },
        }));
        assert!(text.contains("Duplicates: 0"), "{text}");
        assert!(text.contains("Dead code: 0"), "{text}");
        assert!(text.contains("Unused exports: 0"), "{text}");
        // Zero todos are omitted entirely (no noise).
        assert!(
            !text.contains("TODOs:"),
            "zero todos should be omitted:\n{text}"
        );
    }

    #[test]
    fn fresh_text_never_renders_status_sentinels() {
        let text = render(serde_json::json!({
            "duplicates": { "count": 1, "top": [] },
            "dead_code": { "count": 1, "top": [] },
        }));
        assert!(!text.contains("pending"), "{text}");
        assert!(!text.contains("stale"), "{text}");
    }

    #[test]
    fn fresh_text_has_no_cache_state_note() {
        let text = render_inspect_text(&Map::new(), &Map::new(), None);
        assert!(
            !text.contains("note:"),
            "fresh text must not describe partial state: {text}"
        );
    }

    // Fresh summaries are derived only from verified category payloads.
    #[test]
    fn fresh_summary_has_no_stale_flag() {
        let payload = serde_json::json!({ "count": 357, "by_language": { "rust": 214 } });
        let summary = summary_for(InspectCategory::DeadCode, &payload);
        assert_eq!(summary.get("count").and_then(Value::as_u64), Some(357));
        assert!(summary.get("stale").is_none(), "{summary}");
        assert!(summary.get("status").is_none(), "{summary}");
    }

    // Diagnostics summaries retain only verified severity totals.
    #[test]
    fn diagnostics_summary_has_only_verified_counts() {
        let summary = diagnostics_summary_for(&serde_json::json!({
            "errors": 1,
            "warnings": 2,
            "info": 3,
            "hints": 4,
        }));
        assert_eq!(
            summary,
            serde_json::json!({
                "errors": 1,
                "warnings": 2,
                "info": 3,
                "hints": 4,
            })
        );
    }
}

#[cfg(test)]
mod fresh_payload_tests {
    use std::path::PathBuf;
    use std::sync::{Arc, RwLock};

    use super::*;
    use crate::config::Config;
    use crate::parser::SymbolCache;

    fn snapshot() -> InspectSnapshot {
        InspectSnapshot::new(
            PathBuf::from("/repo"),
            PathBuf::from("/repo/.aft"),
            Arc::new(Config::default()),
            Arc::new(RwLock::new(SymbolCache::new())),
        )
    }

    fn fresh_payloads_for_all_categories() -> BTreeMap<InspectCategory, Value> {
        InspectCategory::active()
            .iter()
            .copied()
            .map(|category| {
                let payload = match category {
                    InspectCategory::Diagnostics => serde_json::json!({
                        "errors": 2,
                        "warnings": 0,
                        "info": 0,
                        "hints": 0,
                        "items": [
                            { "file": "src/a.rs", "line": 1, "severity": "error" },
                            { "file": "src/b.rs", "line": 2, "severity": "error" },
                        ],
                    }),
                    InspectCategory::Metrics => serde_json::json!({
                        "files": 2,
                        "symbols": 3,
                        "loc": 10,
                    }),
                    InspectCategory::Todos => serde_json::json!({ "count": 1, "by_kind": {} }),
                    InspectCategory::DeadCode | InspectCategory::UnusedExports => {
                        serde_json::json!({ "count": 1, "items": [] })
                    }
                    InspectCategory::Duplicates => serde_json::json!({ "count": 1, "groups": [] }),
                    InspectCategory::Cycles => serde_json::json!({ "count": 0, "largest": 0 }),
                    InspectCategory::Complexity => serde_json::json!({
                        "count": 1,
                        "threshold": 10,
                        "worst": { "file": "src/complex.rs", "function": "hot", "line": 3, "complexity": 12 },
                        "items": [{ "file": "src/complex.rs", "function": "hot", "line": 3, "complexity": 12, "language": "rust" }],
                    }),
                    _ => unreachable!("only active categories are emitted"),
                };
                (category, payload)
            })
            .collect()
    }

    fn header_response(
        payloads: &BTreeMap<InspectCategory, Value>,
        sections: &Sections,
    ) -> Response {
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            Default::default(),
        );
        let payload = build_inspect_payload(&snapshot(), payloads, sections, 1, &ctx, None);
        build_inspect_terminal(
            "inspect-category-header",
            &InspectPhaseLog::for_request("inspect-category-header"),
            InspectTerminal::Fresh(payload),
        )
    }

    fn assert_header(response: &Response, expected: &str, complete: bool) {
        assert_eq!(response.data["complete"], complete, "{}", response.data);
        assert_eq!(
            response.data["inspect_terminal"],
            if complete { "fresh" } else { "partial" }
        );
        let text = crate::subc_format::format_inspect_for_test(response);
        assert_eq!(text.lines().next(), Some(expected), "{text}");
        assert_eq!(
            text.lines()
                .filter(|line| line.starts_with("PARTIAL — ") || *line == "FRESH")
                .count(),
            1
        );
    }

    #[test]
    fn header_dead_code_pending_with_complete_diagnostics_is_partial() {
        let mut payloads = fresh_payloads_for_all_categories();
        payloads.insert(InspectCategory::DeadCode, serde_json::json!({
            "unavailable": true, "complete": false,
            "building": {"state": "building", "progress": "projection"},
            "gaps": [{"kind": "analysis_incomplete", "reason": "building projection; estimated remaining 10ms"}]
        }));
        for sections in [Sections::summary_only(), Sections::all()] {
            let response = header_response(&payloads, &sections);
            assert_header(
                &response,
                "PARTIAL — dead code still building; retry aft_inspect.",
                false,
            );
            assert!(response.data["text"]
                .as_str()
                .unwrap()
                .contains("building projection"));
        }
    }

    #[test]
    fn header_every_category_complete_is_fresh() {
        assert_header(
            &header_response(
                &fresh_payloads_for_all_categories(),
                &Sections::summary_only(),
            ),
            "FRESH",
            true,
        );
    }

    #[test]
    fn header_complete_but_truncated_stays_fresh() {
        let response = header_response(&fresh_payloads_for_all_categories(), &Sections::all());
        assert!(response.data["details"]["diagnostics_list_envelope"].is_object());
        assert_eq!(
            response.data["details"]["diagnostics"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(response.data["summary"]["diagnostics"]["errors"], 2);
        assert_header(&response, "FRESH", true);
    }

    #[test]
    fn header_not_analyzed_languages_stay_fresh() {
        let mut payloads = fresh_payloads_for_all_categories();
        payloads.get_mut(&InspectCategory::DeadCode).unwrap()["languages_skipped"] =
            serde_json::json!(["bash", "toml"]);
        let response = header_response(&payloads, &Sections::all());
        assert!(response.data["text"]
            .as_str()
            .unwrap()
            .contains("bash, toml not analyzed"));
        assert_header(&response, "FRESH", true);
    }

    #[test]
    fn header_scoped_policy_skips_are_complete_and_disclosed_once() {
        let mut payloads = fresh_payloads_for_all_categories();
        for category in InspectCategory::active()
            .iter()
            .filter(|category| category.is_tier2())
        {
            payloads.insert(*category, serde_json::json!({
                "not_computed": true, "unavailable": true, "complete": false,
                "gaps": [{"kind": "tier2_unavailable", "reason": "analysis not ready; scoped inspection does not wait for Tier-2"}]
            }));
        }
        let response = header_response(&payloads, &Sections::all());
        assert_header(&response, "FRESH", true);
        assert!(response.data.get("gaps").is_none(), "{}", response.data);
        let text = response.data["text"].as_str().unwrap();
        let notice = "dead code, unused exports, duplicates, cycles, complexity: not computed for scoped inspects; run aft_inspect without scope";
        assert_eq!(text.matches(notice).count(), 1, "{text}");
        assert!(
            !text.contains("Dead code: 0") && !text.contains("Duplicates: 0"),
            "{text}"
        );
        assert_eq!(response.data["summary"]["dead_code"]["complete"], true);
    }

    #[test]
    fn header_scoped_cached_incomplete_analysis_is_still_partial() {
        let mut payloads = fresh_payloads_for_all_categories();
        payloads.insert(InspectCategory::DeadCode, serde_json::json!({
            "callgraph_available": false, "callgraph_unavailable_reason": "cached callgraph unavailable"
        }));
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            Default::default(),
        );
        let payload = build_inspect_payload(
            &snapshot(),
            &payloads,
            &Sections::all(),
            1,
            &ctx,
            Some(&[PathBuf::from("/repo/src")]),
        );
        let response = build_inspect_terminal(
            "scoped-cached-gap",
            &InspectPhaseLog::for_request("scoped-cached-gap"),
            InspectTerminal::Fresh(payload),
        );
        assert_header(
            &response,
            "PARTIAL — dead code unavailable: cached callgraph unavailable; retry aft_inspect.",
            false,
        );
        assert!(response.data["text"]
            .as_str()
            .unwrap()
            .contains("scope: 1 root, 2 files"));
    }

    #[test]
    fn header_groups_categories_with_the_same_reason() {
        let mut payloads = fresh_payloads_for_all_categories();
        for category in [InspectCategory::DeadCode, InspectCategory::Duplicates] {
            payloads.insert(
                category,
                serde_json::json!({
                    "unavailable": true, "complete": false, "building": {"state": "building"},
                    "gaps": [{"kind": "analysis_incomplete", "reason": "cold build still running"}]
                }),
            );
        }
        let response = header_response(&payloads, &Sections::all());
        assert_header(
            &response,
            "PARTIAL — dead code, duplicates still building; retry aft_inspect.",
            false,
        );
        assert_eq!(response.data["gaps"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn header_several_gaps_are_one_line_shortest_first_with_overflow_in_body() {
        let mut payloads = fresh_payloads_for_all_categories();
        payloads.insert(InspectCategory::DeadCode, serde_json::json!({
            "unavailable": true, "complete": false, "building": {"state": "building"},
            "gaps": [{"kind": "analysis_incomplete", "reason": "dead code projection still building"}]
        }));
        payloads.get_mut(&InspectCategory::Diagnostics).unwrap()["complete"] =
            serde_json::json!(false);
        payloads.get_mut(&InspectCategory::Diagnostics).unwrap()["gaps"] = serde_json::json!([
            {"producer": "dockerfile", "root": ".", "reason": "docker-langserver\nis unavailable"},
            {"producer": "rust", "root": ".", "reason": "cargo check still running"},
            {"producer": "typescript", "root": ".", "reason": "initialize crashed"},
            {"producer": "python", "root": ".", "reason": "pyright is unavailable"}
        ]);
        let response = header_response(&payloads, &Sections::all());
        assert_header(&response, "PARTIAL — dead code still building; diagnostics unknown: dockerfile @ .: docker-langserver is unavailable; rust-analyzer @ .: cargo check still running; +2 more; retry aft_inspect.", false);
        let text = response.data["text"].as_str().unwrap();
        assert!(
            text.contains("Incomplete diagnostics: typescript @ .: initialize crashed"),
            "{text}"
        );
        assert!(
            text.contains("Incomplete diagnostics: python @ .: pyright is unavailable"),
            "{text}"
        );
    }

    #[test]
    fn header_each_incomplete_category_sets_complete_false_without_gaps() {
        for category in InspectCategory::active() {
            let mut payloads = fresh_payloads_for_all_categories();
            payloads.get_mut(category).unwrap()["complete"] = serde_json::json!(false);
            let response = header_response(&payloads, &Sections::summary_only());
            let text = crate::subc_format::format_inspect_for_test(&response);
            assert_eq!(response.data["complete"], false, "{category:?}: {text}");
            assert_eq!(
                response.data["inspect_terminal"], "partial",
                "{category:?}: {text}"
            );
            assert!(text.starts_with("PARTIAL — "), "{category:?}: {text}");
            assert!(
                text.lines()
                    .next()
                    .unwrap()
                    .contains(&category.as_str().replace('_', " ")),
                "{category:?}: {text}"
            );
            assert!(!response.data["gaps"].as_array().unwrap().is_empty());
        }
    }

    #[test]
    fn header_unavailable_callgraph_and_parse_failures_are_partial() {
        let mut payloads = fresh_payloads_for_all_categories();
        payloads.insert(
            InspectCategory::DeadCode,
            serde_json::json!({
                "callgraph_available": false, "callgraph_unavailable_reason": "store not ready"
            }),
        );
        let response = header_response(&payloads, &Sections::summary_only());
        assert_header(
            &response,
            "PARTIAL — dead code unavailable: store not ready; retry aft_inspect.",
            false,
        );
        payloads = fresh_payloads_for_all_categories();
        payloads.get_mut(&InspectCategory::UnusedExports).unwrap()["complete"] =
            serde_json::json!(false);
        payloads.get_mut(&InspectCategory::UnusedExports).unwrap()["parse_errors"] =
            serde_json::json!([{"file": "a.ts", "reason": "parse failed"}]);
        let response = header_response(&payloads, &Sections::summary_only());
        assert_header(
            &response,
            "PARTIAL — unused exports incomplete: 1 file could not be parsed; retry aft_inspect.",
            false,
        );
        assert!(response.data["text"]
            .as_str()
            .unwrap()
            .contains("1 file could not be parsed"));
    }

    #[test]
    fn header_final_freshness_gap_is_partial_even_with_complete_categories() {
        let response = header_response(&fresh_payloads_for_all_categories(), &Sections::all());
        let mut payload = response.data;
        payload["complete"] = serde_json::json!(false);
        payload["gaps"] = serde_json::json!([{
            "kind": "stat_verification_incomplete", "reason": "file freshness verification exceeded the request budget; retry aft_inspect"
        }]);
        let response = build_inspect_terminal(
            "freshness-gap",
            &InspectPhaseLog::for_request("freshness-gap"),
            InspectTerminal::Fresh(payload),
        );
        assert_header(&response, "PARTIAL — file freshness unverified: file freshness verification exceeded the request budget; retry aft_inspect.", false);
    }

    fn assert_no_banned_field(value: &Value) {
        // These sentinels must never substitute for explicit completion and gaps.
        const BANNED_KEYS: &[&str] = &[
            "provisional",
            "provisional_counts",
            "pending_categories",
            "stale_categories",
            "incomplete_categories",
            "scope_truncated",
            "servers_pending",
            "servers_not_installed",
            "files_without_server",
            "failed_categories",
        ];

        match value {
            Value::Array(values) => {
                for value in values {
                    assert_no_banned_field(value);
                }
            }
            Value::Object(fields) => {
                for (key, value) in fields {
                    assert!(
                        !BANNED_KEYS.contains(&key.as_str()),
                        "banned inspect field {key} leaked into {value}"
                    );
                    assert!(key != "stale", "stale sentinel leaked into {value}");
                    if key == "server_ran" {
                        assert_ne!(
                            value.as_bool(),
                            Some(false),
                            "unrun server leaked into payload"
                        );
                    }
                    if key == "status" {
                        assert!(
                            !matches!(value.as_str(), Some("pending" | "stale" | "failed")),
                            "partial category status leaked into payload: {value}"
                        );
                    }
                    if key == "complete" {
                        assert!(
                            value.is_boolean(),
                            "completion must be an explicit boolean: {value}"
                        );
                    }
                    assert_no_banned_field(value);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn fresh_payload_is_recursive_banned_field_free_and_top_k_only_caps_rows() {
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            Default::default(),
        );
        let payload = build_inspect_payload(
            &snapshot(),
            &fresh_payloads_for_all_categories(),
            &Sections::all(),
            1,
            &ctx,
            None,
        );

        // These containers are the minimum top-level fields required in the
        // payload; the recursive walk still checks every descendant.
        for container in ["scanner_state", "summary", "details"] {
            assert!(
                payload.get(container).is_some(),
                "missing {container}: {payload}"
            );
        }
        assert_no_banned_field(&payload);
        assert_eq!(payload["summary"]["diagnostics"]["errors"], 2);
        assert_eq!(
            payload["details"]["diagnostics"].as_array().map(Vec::len),
            Some(1)
        );
        assert!(payload.get("topK").is_none());
        assert!(payload.get("top_k").is_none());
    }

    #[test]
    fn scoped_inspect_file_accounting_survives_unfinished_metrics() {
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            Default::default(),
        );
        let mut payloads = fresh_payloads_for_all_categories();
        payloads.insert(InspectCategory::Metrics, serde_json::json!({
            "unavailable": true, "complete": false, "gaps": [{"kind": "analysis_incomplete", "reason": "metrics still scanning"}]
        }));
        payloads.get_mut(&InspectCategory::Diagnostics).unwrap()["coverage"] = serde_json::json!({
            "files": 606, "authoritative": 606, "examined": 606, "not_examined": 0
        });
        let payload = build_inspect_payload(
            &snapshot(),
            &payloads,
            &Sections::all(),
            20,
            &ctx,
            Some(&[PathBuf::from("/repo/packages")]),
        );
        assert_eq!(payload["scope_files"], 606, "{payload:#}");
        assert!(
            payload.get("no_files_matched_scope").is_none(),
            "{payload:#}"
        );
        let text = payload["text"].as_str().unwrap();
        assert!(text.contains("\nscope: 1 root, 606 files\n"), "{text}");
        assert!(text.contains("606 of 606 scoped files"), "{text}");
    }

    #[test]
    fn nonfresh_outcomes_cannot_reach_the_payload_emitter() {
        let outcomes = InspectCategory::active()
            .iter()
            .copied()
            .map(|category| {
                let outcome = if category == InspectCategory::Diagnostics {
                    JobOutcome::pending(true)
                } else {
                    JobOutcome::Fresh {
                        payload: serde_json::json!({}),
                    }
                };
                (category, outcome)
            })
            .collect();

        assert!(fresh_payloads(&outcomes).is_err());
    }

    #[test]
    fn pending_wait_detail_keeps_the_category_prefix_stable() {
        use crate::inspect::job::PendingWaitCause;

        let cases = [
            (
                PendingWaitCause::WaiterDropped,
                "metrics did not complete (waiter dropped without outcome after 1.8s; budget 120s)",
            ),
            (
                PendingWaitCause::ResultChannelDisconnected,
                "metrics did not complete (result channel disconnected after 1.8s; budget 120s)",
            ),
            (
                PendingWaitCause::DeadlineElapsed,
                "metrics did not complete (deadline elapsed after 1.8s; budget 120s)",
            ),
        ];

        for (cause, expected) in cases {
            let outcomes = InspectCategory::active()
                .iter()
                .copied()
                .map(|category| {
                    let outcome = if category == InspectCategory::Metrics {
                        JobOutcome::pending_wait(
                            true,
                            cause,
                            Duration::from_millis(1_800),
                            Duration::from_secs(120),
                        )
                    } else {
                        JobOutcome::Fresh {
                            payload: serde_json::json!({}),
                        }
                    };
                    (category, outcome)
                })
                .collect();

            assert_eq!(
                fresh_payloads(&outcomes).expect_err("pending metrics must fail freshness"),
                expected
            );
        }
    }

    #[test]
    fn build_inspect_payload_four_capped_lists_produces_four_sibling_envelopes_and_r10_trailers() {
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            Default::default(),
        );

        let mut payloads = fresh_payloads_for_all_categories();
        payloads.insert(
            InspectCategory::DeadCode,
            serde_json::json!({
                "count": 4,
                "generated_count": 3,
                "test_only_count": 3,
                "items": [
                    { "file": "src/a.rs", "symbol": "alpha" },
                    { "file": "src/b.rs", "symbol": "beta" },
                    { "file": "src/c.rs", "symbol": "gamma" },
                    { "file": "src/d.rs", "symbol": "delta" },
                ],
                "generated_items": [
                    { "file": "src/gen_a.rs", "symbol": "gen_one" },
                    { "file": "src/gen_b.rs", "symbol": "gen_two" },
                    { "file": "src/gen_c.rs", "symbol": "gen_three" },
                ],
                "test_only_items": [
                    { "file": "src/test_a.rs", "symbol": "t_one", "used_by": ["tests/test_a.rs"] },
                    { "file": "src/test_b.rs", "symbol": "t_two", "used_by": ["tests/test_b.rs"] },
                    { "file": "src/test_c.rs", "symbol": "t_three", "used_by": ["tests/test_c.rs"] },
                ],
            }),
        );
        payloads.insert(
            InspectCategory::Diagnostics,
            serde_json::json!({
                "errors": 4,
                "warnings": 0,
                "info": 0,
                "hints": 0,
                "items": [
                    { "file": "src/diag_a.rs", "line": 10, "column": 5, "severity": "error", "message": "syntax error", "source": "rustc" },
                    { "file": "src/diag_b.rs", "line": 20, "column": 8, "severity": "error", "message": "missing type", "source": "rustc" },
                    { "file": "src/diag_c.rs", "line": 30, "column": 2, "severity": "error", "message": "unresolved name", "source": "rustc" },
                    { "file": "src/diag_d.rs", "line": 40, "column": 1, "severity": "error", "message": "borrow error", "source": "rustc" },
                ],
            }),
        );

        let sections = Sections {
            detail_categories: [InspectCategory::DeadCode].into_iter().collect(),
        };
        let payload = build_inspect_payload(&snapshot(), &payloads, &sections, 2, &ctx, None);

        let details = payload["details"].as_object().expect("details object");
        assert_eq!(details["dead_code"].as_array().map(Vec::len), Some(2));
        assert_eq!(
            details["dead_code_generated"].as_array().map(Vec::len),
            Some(2)
        );
        assert_eq!(
            details["dead_code_test_only"].as_array().map(Vec::len),
            Some(2)
        );
        assert_eq!(details["diagnostics"].as_array().map(Vec::len), Some(2));

        // Exactly four sibling envelopes with derivable keys
        let envelope_keys: Vec<&String> = details
            .keys()
            .filter(|k| k.ends_with("_list_envelope"))
            .collect();
        assert_eq!(
            envelope_keys.len(),
            4,
            "must have 4 envelopes: {envelope_keys:?}"
        );
        assert!(details.contains_key("dead_code_list_envelope"));
        assert!(details.contains_key("dead_code_generated_list_envelope"));
        assert!(details.contains_key("dead_code_test_only_list_envelope"));
        assert!(details.contains_key("diagnostics_list_envelope"));

        assert_eq!(details["dead_code_list_envelope"]["shown"], 2);
        assert_eq!(details["dead_code_list_envelope"]["total"]["value"], 4);
        assert_eq!(details["dead_code_generated_list_envelope"]["shown"], 2);
        assert_eq!(
            details["dead_code_generated_list_envelope"]["total"]["value"],
            3
        );
        assert_eq!(details["dead_code_test_only_list_envelope"]["shown"], 2);
        assert_eq!(
            details["dead_code_test_only_list_envelope"]["total"]["value"],
            3
        );
        assert_eq!(details["diagnostics_list_envelope"]["shown"], 2);
        assert_eq!(details["diagnostics_list_envelope"]["total"]["value"], 4);

        let text = payload["text"].as_str().expect("text");
        let trailers: Vec<&str> = text
            .lines()
            .filter(|line| line.starts_with("shown ") && line.contains(" items (cap)"))
            .collect();
        assert_eq!(
            trailers.len(),
            4,
            "must render exactly four trailers: {text}"
        );

        // Verify that trailers are placed directly after their corresponding truncated sections in formatted output.
        assert!(text.contains("  src/b.rs::beta\nshown 2 of 4 items (cap) · narrow: topK, scope, sections\n  generated: 3:"));
        assert!(text.contains("    src/gen_b.rs::gen_two\nshown 2 of 3 items (cap) · narrow: topK, scope, sections\n  test-only usage: 3:"));
        assert!(text.contains("    src/test_b.rs::t_two — used by tests/test_b.rs\nshown 2 of 3 items (cap) · narrow: topK, scope, sections"));
        assert!(text.contains("- src/diag_b.rs:20:8 error missing type [rustc]\nshown 2 of 4 items (cap) · narrow: topK, scope, sections"));
        assert!(text.ends_with("shown 2 of 4 items (cap) · narrow: topK, scope, sections"));
    }

    #[test]
    fn build_inspect_payload_empty_capped_list_renders_heading_then_trailer() {
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            Default::default(),
        );

        let mut payloads = fresh_payloads_for_all_categories();
        payloads.insert(
            InspectCategory::DeadCode,
            serde_json::json!({
                "count": 3,
                "generated_count": 2,
                "test_only_count": 2,
                "items": [
                    { "file": "src/a.rs", "symbol": "alpha" },
                    { "file": "src/b.rs", "symbol": "beta" },
                    { "file": "src/c.rs", "symbol": "gamma" },
                ],
                "generated_items": [
                    { "file": "src/gen_a.rs", "symbol": "gen_one" },
                    { "file": "src/gen_b.rs", "symbol": "gen_two" },
                ],
                "test_only_items": [
                    { "file": "src/test_a.rs", "symbol": "t_one", "used_by": ["tests/test_a.rs"] },
                    { "file": "src/test_b.rs", "symbol": "t_two", "used_by": ["tests/test_b.rs"] },
                ],
            }),
        );
        payloads.insert(
            InspectCategory::Diagnostics,
            serde_json::json!({
                "errors": 2,
                "warnings": 0,
                "info": 0,
                "hints": 0,
                "items": [
                    { "file": "src/diag_a.rs", "line": 10, "column": 5, "severity": "error", "message": "syntax error", "source": "rustc" },
                    { "file": "src/diag_b.rs", "line": 20, "column": 8, "severity": "error", "message": "missing type", "source": "rustc" },
                ],
            }),
        );

        let sections = Sections {
            detail_categories: [InspectCategory::DeadCode].into_iter().collect(),
        };
        let payload = build_inspect_payload(&snapshot(), &payloads, &sections, 0, &ctx, None);

        let details = payload["details"].as_object().expect("details object");
        assert_eq!(details["dead_code"].as_array().map(Vec::len), Some(0));
        assert_eq!(
            details["dead_code_generated"].as_array().map(Vec::len),
            Some(0)
        );
        assert_eq!(
            details["dead_code_test_only"].as_array().map(Vec::len),
            Some(0)
        );
        assert_eq!(details["diagnostics"].as_array().map(Vec::len), Some(0));

        let text = payload["text"].as_str().expect("text");
        assert!(text.contains("Dead code: 3 (generated: 2):\nshown 0 of 3 items (cap) · narrow: topK, scope, sections"));
        assert!(text
            .contains("  generated: 2:\nshown 0 of 2 items (cap) · narrow: topK, scope, sections"));
        assert!(text.contains(
            "  test-only usage: 2:\nshown 0 of 2 items (cap) · narrow: topK, scope, sections"
        ));
        assert!(text.contains(
            "diagnostics details:\nshown 0 of 2 items (cap) · narrow: topK, scope, sections"
        ));
    }

    /// Diagnostics payload for a scoped request where no producer analyzed any
    /// of the scoped files: eight TypeScript files whose server binary is
    /// missing and four Biome files the running server never reported on.
    fn uncovered_diagnostics_payload() -> Value {
        let missing = "typescript-language-server is unavailable; no node_modules in web: \
                       the project's dependencies are not installed; run your package \
                       manager's install";
        let unreported = "running, but has not reported on these files";
        let mut gaps = vec![serde_json::json!({
            "kind": "failed_producer",
            "producer": "typescript",
            "reason": missing,
        })];
        for index in 0..8 {
            gaps.push(serde_json::json!({
                "kind": "uncovered_file",
                "file": format!("web/src/file_{index:02}.ts"),
                "reason": "no LSP producer has a current diagnostic report for this file",
                "cause": { "producer": "typescript", "root": "web", "reason": missing },
            }));
        }
        for index in 0..4 {
            gaps.push(serde_json::json!({
                "kind": "uncovered_file",
                "file": format!("tools/lint_{index}.ts"),
                "reason": "no LSP producer has a current diagnostic report for this file",
                "cause": { "producer": "biome", "root": "tools", "reason": unreported },
            }));
        }
        serde_json::json!({
            "errors": null,
            "warnings": null,
            "info": null,
            "hints": null,
            "items": [],
            "by_producer": {},
            "complete": false,
            "gaps": gaps,
        })
    }

    #[test]
    fn uncovered_diagnostic_files_roll_up_by_cause_and_list_at_most_top_k_paths() {
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            Default::default(),
        );
        let mut payloads = fresh_payloads_for_all_categories();
        payloads.insert(
            InspectCategory::Diagnostics,
            uncovered_diagnostics_payload(),
        );
        let roots = [PathBuf::from("/repo/web"), PathBuf::from("/repo/tools")];
        let payload = build_inspect_payload(
            &snapshot(),
            &payloads,
            &Sections::summary_only(),
            5,
            &ctx,
            Some(&roots),
        );
        let text = payload["text"].as_str().expect("text");

        // Each cause and file count appears once, together in the status header.
        let group_lines = text
            .lines()
            .filter(|line| line.starts_with("PARTIAL — diagnostics unknown:"))
            .collect::<Vec<_>>();
        assert_eq!(
            group_lines,
            vec!["PARTIAL — diagnostics unknown: typescript @ web: typescript-language-server is unavailable; no node_modules in web: the project's dependencies are not installed; run your package manager's install (8 files); biome @ tools: running, but has not reported on these files (4 files); retry aft_inspect."],
            "{text}"
        );
        // At most topK (5 here) affected paths, largest group first, then the
        // list trailer.
        let listed = text
            .lines()
            .filter(|line| line.starts_with("  web/") || line.starts_with("  tools/"))
            .count();
        assert_eq!(listed, 5, "{text}");
        assert!(
            text.contains(
                "  web/src/file_04.ts\nshown 5 of 12 items (cap) · narrow: topK, scope, sections"
            ),
            "{text}"
        );
        // The one-line diagnostics status stays bounded too: it counts the
        // uncovered files instead of naming each one.
        let status = text
            .lines()
            .find(|line| line.starts_with("PARTIAL — diagnostics unknown:"))
            .expect("diagnostics status line");
        assert!(!status.contains("file_0"), "{status}");
        assert!(
            status.contains("(8 files)") && status.contains("(4 files)"),
            "{status}"
        );

        // The payload keeps every structured gap row; the path list in
        // `details` is the bounded one, with its truncation envelope.
        let uncovered = payload["gaps"]
            .as_array()
            .expect("gaps")
            .iter()
            .filter(|gap| gap["kind"] == "uncovered_file")
            .count();
        assert_eq!(uncovered, 12);
        assert_eq!(payload["complete"], false);
        assert_eq!(payload["summary"]["diagnostics"]["complete"], false);
        assert_eq!(
            payload["details"]["diagnostics_uncovered_files"]
                .as_array()
                .map(Vec::len),
            Some(5)
        );
        assert_eq!(
            payload["details"]["diagnostics_uncovered_files_list_envelope"]["total"]["value"],
            12
        );
        assert_eq!(
            payload["summary"]["diagnostics"]["uncovered_file_groups"],
            serde_json::json!([
                {
                    "producer": "typescript",
                    "root": "web",
                    "reason": "typescript-language-server is unavailable; no node_modules in web: the project's dependencies are not installed; run your package manager's install",
                    "files": 8,
                },
                {
                    "producer": "biome",
                    "root": "tools",
                    "reason": "running, but has not reported on these files",
                    "files": 4,
                },
            ])
        );
    }

    /// Findings withheld from a category's count because they live in test
    /// trees or fixtures are still reported, as a clause on that category's
    /// summary line.
    #[test]
    fn withheld_test_findings_are_counted_on_each_category_headline() {
        let summary = serde_json::json!({
            "dead_code": {"count": 865, "excluded_test_count": 2940, "excluded_test_files": 21},
            "unused_exports": {"count": 0, "excluded_test_count": 0, "excluded_test_files": 0},
            "duplicates": {"count": 0, "excluded_test_count": 3, "excluded_test_files": 1},
            "todos": {"count": 0, "excluded_test_count": 4, "excluded_test_files": 2},
        });
        let text = render_inspect_text(summary.as_object().unwrap(), &Map::new(), None);
        let lines = text.lines().collect::<Vec<_>>();
        assert!(
            lines.contains(&"Dead code: 865 · excluded 2,940 in 21 test/fixture files (pass includeTests to see them):"),
            "{text}"
        );
        assert!(lines.contains(&"Unused exports: 0"), "{text}");
        assert!(
            lines.contains(&"Duplicates: 0 · excluded 3 in 1 test/fixture file (pass includeTests to see them)"),
            "{text}"
        );
        assert!(
            lines.contains(
                &"TODOs: 0 · excluded 4 in 2 test/fixture files (pass includeTests to see them)"
            ),
            "{text}"
        );
    }

    /// A scanner that did not finish within the inspect wait budget is named
    /// on its incomplete line, instead of the generic "unknown producer".
    #[test]
    fn unfinished_scanner_gap_names_its_scanner() {
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            Default::default(),
        );
        let mut payloads = fresh_payloads_for_all_categories();
        payloads.insert(
            InspectCategory::Todos,
            serde_json::json!({
                "unavailable": true, "complete": false,
                "gaps": [{"kind": "analysis_incomplete", "producer": "todos scanner",
                    "reason": "analysis did not finish within its wait budget; retry aft_inspect"}]
            }),
        );
        let payload = build_inspect_payload(
            &snapshot(),
            &payloads,
            &Sections::summary_only(),
            5,
            &ctx,
            None,
        );
        let text = payload["text"].as_str().expect("text");
        assert!(
            text.contains("Incomplete todos: todos scanner did not finish (analysis did not finish within its wait budget; retry aft_inspect)"),
            "{text}"
        );
        assert!(!text.contains("unknown producer"), "{text}");
    }

    #[test]
    fn runtime_notes_that_differ_only_by_path_collapse_to_one_line() {
        let notes = [
            "TypeScript 5.9.3: project installation (/r/a/node_modules/typescript/lib/tsserver.js)",
            "TypeScript 5.9.3: project installation (/r/b/node_modules/typescript/lib/tsserver.js)",
            "TypeScript 5.9.3: project installation (/r/c/node_modules/typescript/lib/tsserver.js)",
            "TypeScript 5.4.0: project installation (/r/d/node_modules/typescript/lib/tsserver.js)",
            "rust-analyzer warning: proc macros unavailable",
        ]
        .map(String::from);
        assert_eq!(
            collapse_runtime_notes(&notes),
            vec![
                "TypeScript 5.9.3: project installation ×3 (first: /r/a/node_modules/typescript/lib/tsserver.js)",
                "TypeScript 5.4.0: project installation (/r/d/node_modules/typescript/lib/tsserver.js)",
                "rust-analyzer warning: proc macros unavailable",
            ]
        );
    }

    #[test]
    fn inspect_noise_single_project_typescript_note_is_omitted() {
        let ordinary =
            "TypeScript 5.9.3: project installation (/r/node_modules/typescript/lib/tsserver.js)"
                .to_string();
        let mut payload = serde_json::json!({"text": "FRESH\ndiagnostics: 0 errors, 0 warnings, 0 info, 0 hints"});
        append_inspect_runtime_notes(&mut payload, std::slice::from_ref(&ordinary));
        assert_eq!(
            payload["text"],
            "FRESH\ndiagnostics: 0 errors, 0 warnings, 0 info, 0 hints"
        );
        assert!(collapse_runtime_notes(&[ordinary.clone(), ordinary]).is_empty());
        let native = "TypeScript 7.0.2: native language server (project installation) (/r/node_modules/@typescript/typescript-darwin-arm64/lib/tsc)".to_string();
        assert!(collapse_runtime_notes(&[native]).is_empty());
    }

    #[test]
    fn inspect_noise_actionable_typescript_notes_are_retained() {
        let notes = [
            "TypeScript 5.9.3: project installation (/r/a/lib/tsserver.js)",
            "TypeScript 5.9.3: project installation (/r/b/lib/tsserver.js)",
        ]
        .map(String::from);
        assert_eq!(
            collapse_runtime_notes(&notes),
            vec!["TypeScript 5.9.3: project installation ×2 (first: /r/a/lib/tsserver.js)"]
        );
        for note in [
            "TypeScript 5.9.3: AFT cache fallback; not the project's pinned TypeScript (/cache/lib/tsserver.js)",
            "TypeScript: server-managed SDK resolution (version not reported by AFT)",
            "TypeScript: explicit tsserver.path override (version managed by configuration)",
            "TypeScript unknown: project installation (/r/lib/tsserver.js)",
            "TypeScript 5.9.3: version mismatch (expected 5.4.0)",
            "rust-analyzer warning: proc macros unavailable",
        ] {
            assert_eq!(collapse_runtime_notes(&[note.to_string()]), vec![note]);
        }
    }

    #[test]
    fn daemon_noise_renderer_golden() {
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            Default::default(),
        );
        let clean =
            serde_json::json!({"errors": 0, "warnings": 0, "info": 0, "hints": 0, "items": []});
        let ordinary = "TypeScript 5.9.3: project installation (/repo/node_modules/typescript/lib/tsserver.js)";
        let scenarios = [
            ("single-project-sdk", clean.clone(), vec![ordinary]),
            ("multiple-sdks", clean.clone(), vec![ordinary, "TypeScript 5.4.0: project installation (/repo/web/node_modules/typescript/lib/tsserver.js)"]),
            ("fallback-sdk", clean, vec!["TypeScript 5.9.3: AFT cache fallback; not the project's pinned TypeScript (/cache/typescript/lib/tsserver.js)"]),
            ("missing-docker-server", serde_json::json!({
                "complete": false, "errors": null, "warnings": null, "info": null, "hints": null,
                "items": [], "by_producer": {}, "gaps": [{"kind": "failed_producer", "producer": "dockerfile", "root": ".",
                    "reason": "docker-langserver is unavailable; AFT plugins auto-install dockerfile-language-server-nodejs with lsp.auto_install enabled (user config) into <AFT cache>/lsp-packages/dockerfile-language-server-nodejs/node_modules/.bin; diagnose with `npx @cortexkit/aft doctor lsp <file>`",
                    "affected_files": ["images/app.dockerfile", "images/base.dockerfile"]
                }]
            }), vec![]),
        ];
        let rendered = scenarios.into_iter().map(|(name, diagnostics, notes)| {
            let mut payloads = fresh_payloads_for_all_categories();
            payloads.insert(InspectCategory::Diagnostics, diagnostics);
            let mut payload = build_inspect_payload(&snapshot(), &payloads, &Sections::summary_only(), 1, &ctx, None);
            append_inspect_runtime_notes(&mut payload, &notes.into_iter().map(String::from).collect::<Vec<_>>());
            let log = InspectPhaseLog::default();
            let response = build_inspect_terminal(name, &log, InspectTerminal::Fresh(payload));
            serde_json::json!({"name": name, "inspect_terminal": response.data["inspect_terminal"],
                "text": crate::subc_format::format_inspect_for_test(&response)})
        }).collect::<Vec<_>>();
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/inspect/noise-renderer.json");
        let actual = serde_json::to_string_pretty(&rendered).unwrap() + "\n";
        if std::env::var_os("UPDATE_INSPECT_NOISE_GOLDEN").is_some() {
            std::fs::write(&path, &actual).unwrap();
        }
        assert_eq!(actual, std::fs::read_to_string(path).unwrap());
    }

    /// A producer whose workspace failed to load (rust-analyzer run with
    /// `--locked` over a stale Cargo.lock) leaves the one scoped file without
    /// diagnostics. The cargo error is compacted into one status line, not
    /// repeated in file groups and a footer; coverage cannot read as success.
    #[test]
    fn failed_producer_error_renders_once_and_coverage_counts_authoritative_files() {
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            Default::default(),
        );
        let cargo_error =
            "Failed to read Cargo metadata with dependencies for `/repo/Cargo.toml`: \
            `cargo metadata` exited with an error:\n\nerror: cannot update the lock file \
            /tmp/rust-analyzer1-0/Cargo.lock because --locked was passed to prevent this";
        let mut payloads = fresh_payloads_for_all_categories();
        payloads.insert(
            InspectCategory::Diagnostics,
            serde_json::json!({
                "errors": null,
                "warnings": null,
                "info": null,
                "hints": null,
                "items": [],
                "by_producer": {},
                "complete": false,
                "gaps": [
                    { "kind": "failed_producer", "producer": "rust", "root": ".", "reason": cargo_error },
                    {
                        "kind": "uncovered_file",
                        "file": "src/lib.rs",
                        "reason": "no LSP producer has a current diagnostic report for this file",
                        "cause": { "producer": "rust", "root": ".", "reason": cargo_error },
                    },
                ],
                "coverage": {
                    "files": 1,
                    "examined": 1,
                    "authoritative": 0,
                    "not_examined": 0,
                    "file_cap": 200,
                },
            }),
        );
        let roots = [PathBuf::from("/repo/src/lib.rs")];
        let payload = build_inspect_payload(
            &snapshot(),
            &payloads,
            &Sections::summary_only(),
            5,
            &ctx,
            Some(&roots),
        );
        let text = payload["text"].as_str().expect("text");

        assert_eq!(
            text.matches("--locked was passed").count(),
            1,
            "the producer error must be printed once: {text}"
        );
        assert!(
            text.contains(&format!(
                "PARTIAL — diagnostics unknown: rust-analyzer @ .: {} (1 file); retry aft_inspect.",
                cargo_error.split_whitespace().collect::<Vec<_>>().join(" ")
            )),
            "{text}"
        );
        assert!(!text.contains("Incomplete diagnostics:"), "{text}");
        assert!(!text.contains("\ndiagnostics: unknown"), "{text}");
        assert!(
            !text.contains("diagnostics: authoritative results"),
            "{text}"
        );
        assert!(!text.contains("analyzed 1 of 1"), "{text}");
        // The structured gap keeps the full reason for programmatic readers.
        assert_eq!(
            payload["summary"]["diagnostics"]["uncovered_file_groups"][0]["reason"],
            cargo_error
        );
    }

    #[test]
    fn uncovered_diagnostic_files_within_top_k_render_without_a_trailer() {
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            Default::default(),
        );
        let mut payloads = fresh_payloads_for_all_categories();
        payloads.insert(
            InspectCategory::Diagnostics,
            uncovered_diagnostics_payload(),
        );
        let payload = build_inspect_payload(
            &snapshot(),
            &payloads,
            &Sections::summary_only(),
            20,
            &ctx,
            None,
        );
        let text = payload["text"].as_str().expect("text");
        assert!(!text.contains("(cap)"), "{text}");
        assert!(
            text.ends_with("  tools/lint_3.ts") || text.contains("  tools/lint_3.ts\n"),
            "{text}"
        );
        assert!(payload["details"]
            .get("diagnostics_uncovered_files_list_envelope")
            .is_none());
    }

    #[test]
    fn build_inspect_payload_uncapped_renders_no_trailers_and_serializes_no_envelope() {
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            Default::default(),
        );

        let mut payloads = fresh_payloads_for_all_categories();
        payloads.insert(
            InspectCategory::DeadCode,
            serde_json::json!({
                "count": 1,
                "items": [{ "file": "src/a.rs", "symbol": "alpha" }],
            }),
        );
        payloads.insert(
            InspectCategory::Diagnostics,
            serde_json::json!({
                "errors": 1,
                "warnings": 0,
                "info": 0,
                "hints": 0,
                "items": [
                    { "file": "src/diag_a.rs", "line": 10, "column": 5, "severity": "error", "message": "syntax error", "source": "rustc" },
                ],
            }),
        );

        let sections = Sections {
            detail_categories: [InspectCategory::DeadCode].into_iter().collect(),
        };
        let payload = build_inspect_payload(&snapshot(), &payloads, &sections, 10, &ctx, None);

        let details = payload["details"].as_object().expect("details object");
        let envelope_keys: Vec<&String> = details
            .keys()
            .filter(|k| k.ends_with("_list_envelope"))
            .collect();
        assert!(
            envelope_keys.is_empty(),
            "uncapped must have no envelopes: {envelope_keys:?}"
        );

        let text = payload["text"].as_str().expect("text");
        assert!(
            !text.contains("(cap)"),
            "uncapped must have no trailer: {text}"
        );
    }
}

#[cfg(test)]
mod deferred_terminal_tests {
    use super::*;

    #[test]
    fn inspect_test_delay_holds_until_elapsed() {
        let started = Instant::now();
        wait_inspect_test_delay(
            Duration::from_millis(60),
            InspectRequestDeadline::new(Duration::from_secs(5), Duration::ZERO),
            None,
        );
        assert!(started.elapsed() >= Duration::from_millis(60));
    }

    #[test]
    fn inspect_test_delay_wakes_on_cancellation() {
        let token = crate::executor::JobCancellation::new();
        let worker_token = token.clone();
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            wait_inspect_test_delay(
                Duration::from_secs(30),
                InspectRequestDeadline::new(Duration::from_secs(60), Duration::ZERO),
                Some(&worker_token),
            );
            done_tx.send(()).unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(done_rx.recv_timeout(Duration::from_millis(60)).is_err());
        token.request_cancel();
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("cancellation must wake the hold");
        worker.join().unwrap();
    }

    #[test]
    fn inspect_test_delay_obeys_request_budget() {
        let started = Instant::now();
        wait_inspect_test_delay(
            Duration::from_secs(30),
            InspectRequestDeadline::new(Duration::from_millis(60), Duration::ZERO),
            None,
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn deferred_preflight_uses_one_terminal_poll_response() {
        let ctx = Arc::new(AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config::default(),
        ));
        let request: RawRequest = serde_json::from_value(serde_json::json!({
            "id": "inspect-preflight",
            "command": "inspect"
        }))
        .expect("request parses");
        let mut deferred = match handle_inspect_deferred(&request, Arc::clone(&ctx)) {
            DispatchOutcome::Deferred(pending) => pending,
            DispatchOutcome::Immediate(_) => panic!("inspect must use the deferred seam"),
        };
        let response = (deferred.poll)(&ctx).expect("preflight terminal response");
        assert!(!response.success);
        assert!(response.data.get("failed_phase").is_none());
        assert_eq!(response.data["failure_reason"], "root_resolution_failed");
        assert!(
            (deferred.poll)(&ctx).is_none(),
            "terminal response must be emitted once"
        );
    }

    fn deferred_test_context(root: &Path) -> Arc<AppContext> {
        let mut config = crate::config::Config::default();
        config.project_root = Some(root.to_path_buf());
        let ctx = Arc::new(AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            config,
        ));
        ctx.set_harness(crate::harness::Harness::Opencode);
        ctx
    }

    fn inspect_request(id: &str) -> RawRequest {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "command": "inspect"
        }))
        .expect("request parses")
    }

    fn wait_for_pending_terminal(pending: &mut PendingResponse, ctx: &AppContext) -> Response {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(response) = (pending.poll)(ctx) {
                return response;
            }
            assert!(Instant::now() < deadline, "inspect terminal timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn standalone_shutdown_cancels_detached_inspect_before_phase_deadline() {
        let _serial = deferred_inspect_test_lock();
        let root = tempfile::tempdir().expect("temp root");
        std::fs::write(root.path().join("main.rs"), "fn main() {}\n").expect("fixture");
        let ctx = deferred_test_context(root.path());
        let (started_rx, _release_tx) = install_deferred_inspect_body_gate_for_test();
        let pending = match handle_inspect_deferred(
            &inspect_request("inspect-standalone-cancel"),
            Arc::clone(&ctx),
        ) {
            DispatchOutcome::Deferred(pending) => pending,
            DispatchOutcome::Immediate(_) => panic!("inspect must defer"),
        };
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("detached inspect reaches body gate");

        let mut registry = crate::response_finalize::PendingResponses::default();
        registry.register(pending);
        let shutdown = registry.drain_on_shutdown_with(&ctx);
        assert_eq!(shutdown.len(), 1);
        assert_eq!(
            shutdown[0].response.data["failure_reason"],
            "daemon_shutdown"
        );
        let deadline = Instant::now() + Duration::from_secs(1);
        while deferred_inspect_root_count_for_test() != 0 {
            assert!(
                Instant::now() < deadline,
                "detached inspect ignored shutdown cancellation"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn final_stat_verification_rejects_mid_wait_mutation_without_watcher_delivery() {
        let _serial = deferred_inspect_test_lock();
        let root = tempfile::tempdir().expect("temp root");
        let file = root.path().join("README.md");
        std::fs::write(&file, "# Before\n").expect("fixture");
        let ctx = deferred_test_context(root.path());
        let (started_rx, release_tx) = install_deferred_inspect_stat_gate_for_test();
        let mut pending = match handle_inspect_deferred(
            &inspect_request("inspect-mid-wait-mutation"),
            Arc::clone(&ctx),
        ) {
            DispatchOutcome::Deferred(pending) => pending,
            DispatchOutcome::Immediate(_) => panic!("inspect must defer"),
        };
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("inspect captured its pre-wait stat snapshot");
        // No watcher drain runs in this test. The direct terminal stat proof must
        // therefore detect the mutation even though diagnostic reports were not
        // marked stale by watcher delivery.
        std::fs::write(&file, "# Changed while waiting\n").expect("mutate fixture");
        release_tx.send(()).expect("release inspect body");

        let response = wait_for_pending_terminal(&mut pending, &ctx);
        assert_eq!(response.data["inspect_terminal"], "interrupted");
        assert!(response.data["completed_phases"]
            .as_array()
            .is_some_and(|phases| phases
                .iter()
                .all(|phase| phase["id"] != "stat_verification")));
    }

    #[test]
    fn terminal_builder_uses_one_phase_shape_for_all_outcomes() {
        let log = InspectPhaseLog::for_request("inspect-terminal-shapes");
        log.start(InspectPhaseEntry::category(
            InspectPhaseId::StatVerification,
            InspectCategory::DeadCode,
        ))
        .complete();
        let fresh = build_inspect_terminal(
            "inspect-terminal-shapes",
            &log,
            InspectTerminal::Fresh(serde_json::json!({})),
        );
        assert_eq!(
            fresh.data["wait_stamp"]["phases"][0]["id"],
            "stat_verification"
        );
        let interrupted = build_inspect_terminal(
            "inspect-terminal-shapes",
            &log,
            InspectTerminal::Interrupted,
        );
        assert_eq!(
            interrupted.data["completed_phases"][0]["category"],
            "dead_code"
        );
        let failed = build_inspect_terminal(
            "inspect-terminal-shapes",
            &log,
            InspectTerminal::PhaseFailed {
                failed_phase: None,
                failure_reason: "missing_executable",
                failure_detail: None,
            },
        );
        assert_eq!(
            failed.data["completed_phases"][0]["id"],
            "stat_verification"
        );
        assert!(failed.data.get("failed_phase").is_none());
    }

    fn diagnostics_payload(diagnostics: Value) -> Value {
        serde_json::json!({"summary": {"diagnostics": diagnostics}, "text": "body"})
    }

    /// A completed request whose diagnostics are unknown for any producer is
    /// PARTIAL, never FRESH, and its reason names every producer the gaps
    /// attribute, from failure rows and from uncovered scoped files alike.
    #[test]
    fn unknown_diagnostics_make_the_terminal_partial_and_name_producers() {
        let log = InspectPhaseLog::for_request("inspect-partial-header");
        let payload = diagnostics_payload(serde_json::json!({
            "complete": false,
            "gaps": [
                {"kind": "failed_producer", "producer": "rust", "root": "spikes/x", "reason": "Failed to load workspaces."},
                {"kind": "failed_producer", "producer": "rust", "root": ".", "reason": "still indexing"},
                {"kind": "uncovered_file", "file": "a.ts", "reason": "no report", "cause": {"producer": "typescript", "root": ".", "reason": "x"}}
            ]
        }));
        let response = build_inspect_terminal(
            "inspect-partial-header",
            &log,
            InspectTerminal::Fresh(payload),
        );
        assert!(response.success);
        assert_eq!(response.data["inspect_terminal"], "partial");
        assert_eq!(
            response.data["partial_reason"],
            "diagnostics unknown: rust-analyzer @ spikes/x: Failed to load workspaces.; rust-analyzer @ .: still indexing; typescript @ .: x (1 file); retry aft_inspect."
        );
        assert!(response.data["wait_stamp"]["text"].is_string());
        let rendered = crate::subc_format::format_inspect_for_test(&response);
        assert_eq!(
            rendered.lines().next(),
            Some("PARTIAL — diagnostics unknown: rust-analyzer @ spikes/x: Failed to load workspaces.; rust-analyzer @ .: still indexing; typescript @ .: x (1 file); retry aft_inspect.")
        );
    }

    #[test]
    fn authoritative_diagnostics_keep_the_terminal_fresh() {
        let log = InspectPhaseLog::for_request("inspect-fresh-header");
        let payload = diagnostics_payload(serde_json::json!({
            "errors": 0, "warnings": 0, "info": 0, "hints": 0, "items": []
        }));
        let response = build_inspect_terminal(
            "inspect-fresh-header",
            &log,
            InspectTerminal::Fresh(payload),
        );
        assert_eq!(response.data["inspect_terminal"], "fresh");
        assert!(response.data.get("partial_reason").is_none());
        let rendered = crate::subc_format::format_inspect_for_test(&response);
        assert!(!rendered.contains("PARTIAL"), "{rendered}");
    }

    #[test]
    fn writer_lease_deadline_has_a_named_terminal_reason() {
        let response = Response::error(
            "inspect-writer-timeout",
            "inspect_not_fresh",
            "dead_code failed: writer_lease_timeout: inspect writer lease deadline elapsed",
        );
        assert_eq!(inspect_failure_reason(&response), "writer_lease_timeout");
    }

    #[test]
    fn completed_tier2_result_survives_an_expired_wait_budget() {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(JobOutcome::Fresh {
            payload: serde_json::json!({"count": 3}),
        })
        .unwrap();
        let manager = crate::inspect::InspectManager::new();
        let outcome = receive_tier2_completion_until(
            rx,
            &manager,
            InspectCategory::DeadCode,
            Instant::now(),
            None,
        )
        .unwrap();
        assert_eq!(outcome.payload().unwrap()["count"], 3);
    }

    #[test]
    fn blocking_tier2_wait_has_a_hard_phase_deadline() {
        let (_tx, rx) = std::sync::mpsc::channel();
        let manager = crate::inspect::InspectManager::new();
        let outcome = receive_tier2_completion_until(
            rx,
            &manager,
            InspectCategory::DeadCode,
            std::time::Instant::now() + Duration::from_millis(20),
            None,
        )
        .expect("deadline produces an honest failure");
        assert!(matches!(
            outcome,
            JobOutcome::Failed { message }
                if message.contains("inspect_phase_timeout")
                    && message.contains("tier2 dead_code aggregate")
                    && message.contains("builder_state=absent")
        ));
    }

    #[test]
    fn shared_request_deadline_returns_a_named_tier2_terminal_before_slow_work() {
        // The slow producer never completes on its own: it only sends after the
        // test releases it, so "returned before the slow work finished" is an
        // ordering proof rather than a wall-clock race on a loaded runner.
        let (tx, rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            let _ = release_rx.recv();
            let _ = tx.send(JobOutcome::Fresh {
                payload: serde_json::json!({}),
            });
        });
        let manager = crate::inspect::InspectManager::new();
        // The phase wait takes half the budget left after the reserve (450 ms
        // here), so the budget assertion below tolerates up to 450 ms of
        // scheduling overshoot. A 120 ms budget left only 40 ms, which a loaded
        // macOS runner exceeded.
        let deadline =
            InspectRequestDeadline::new(Duration::from_millis(1_000), Duration::from_millis(100));
        let outcome = receive_tier2_completion_until(
            rx,
            &manager,
            InspectCategory::DeadCode,
            deadline.phase_deadline(INSPECT_PHASE_WAIT_CAP),
            Some(deadline),
        )
        .expect("deadline produces an honest failure");
        assert!(
            deadline.has_work_budget(),
            "the phase wait must leave budget for other categories and verification"
        );
        assert!(matches!(
            outcome,
            JobOutcome::Failed { message } if message.contains("inspect_phase_timeout")
        ));
        let _ = release_tx.send(());

        let log = InspectPhaseLog::for_request("inspect-slow-tier2");
        let response = build_inspect_terminal(
            "inspect-slow-tier2",
            &log,
            request_deadline_terminal(
                Some(InspectPhaseEntry::category(
                    InspectPhaseId::Tier2Rescan,
                    InspectCategory::DeadCode,
                )),
                deadline,
            ),
        );
        assert_eq!(response.data["inspect_terminal"], "phase_failed");
        assert_eq!(response.data["failed_phase"], "tier2_rescan");
        assert_eq!(response.data["category"], "dead_code");
    }

    #[test]
    fn expired_request_does_not_start_tier2_rescan() {
        let root = tempfile::tempdir().expect("temp root");
        std::fs::write(root.path().join("main.rs"), "fn main() {}\n").expect("fixture");
        let ctx = deferred_test_context(root.path());
        let request = inspect_request("inspect-no-tier2-start");
        let phase_log = InspectPhaseLog::for_request(request.id.clone());
        let manager = ctx.inspect_manager();
        let starts_before = manager.reuse_start_count_for_test();
        let response = handle_inspect_payload(
            &request,
            &ctx,
            true,
            true,
            &[],
            &[],
            &[],
            &[],
            Some(&phase_log),
            Some(InspectRequestDeadline::new(Duration::ZERO, Duration::ZERO)),
            None,
        );

        assert!(response.success);
        assert_eq!(response.data["complete"], false);
        assert!(response.data["gaps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|gap| gap["kind"] == "analysis_incomplete"));
        assert_eq!(manager.reuse_start_count_for_test(), starts_before);
    }

    #[test]
    fn inspect_builder_state_refusal_includes_start_timestamp() {
        let (_tx, rx) = std::sync::mpsc::channel();
        let manager = crate::inspect::InspectManager::new();
        manager.set_tier2_in_flight_for_test(InspectCategory::DeadCode, true);
        let outcome = receive_tier2_completion_until(
            rx,
            &manager,
            InspectCategory::DeadCode,
            std::time::Instant::now() + Duration::from_millis(20),
            None,
        )
        .expect("deadline produces an honest failure");
        assert!(matches!(
            outcome,
            JobOutcome::Failed { message }
                if message.contains("inspect_phase_timeout")
                    && message.contains("builder_state=building since ")
                    && message.contains("age_s=")
        ));
    }

    #[test]
    fn inspect_builder_state_refusal_uses_locked_failed_attempt_history() {
        let manager = crate::inspect::InspectManager::new();
        let unavailable = JobOutcome::Fresh {
            payload: crate::inspect::scanners::dead_code::callgraph_unavailable_aggregate(0),
        };
        manager
            .record_tier2_attempt_outcome_for_test(InspectCategory::DeadCode, unavailable.clone());
        let first = manager.tier2_builder_state_detail(InspectCategory::DeadCode);
        let first_at = first
            .rsplit("first at ")
            .next()
            .and_then(|tail| tail.strip_suffix(')'))
            .expect("failed-attempt detail includes first-at unix time");
        for _ in 1..7 {
            manager.record_tier2_attempt_outcome_for_test(
                InspectCategory::DeadCode,
                unavailable.clone(),
            );
        }
        assert_eq!(
            manager.tier2_builder_state_detail(InspectCategory::DeadCode),
            format!("last attempt failed: callgraph_unavailable (attempt 7, first at {first_at})")
        );
    }

    #[test]
    fn callgraph_ready_phase_does_not_complete_when_builder_reports_unavailable() {
        let log = InspectPhaseLog::for_request("inspect-callgraph-ready-honesty");
        let callgraph_phase = log.start(InspectPhaseEntry::category(
            InspectPhaseId::CallgraphReady,
            InspectCategory::DeadCode,
        ));
        let tier2_phase = log.start(InspectPhaseEntry::category(
            InspectPhaseId::Tier2Rescan,
            InspectCategory::DeadCode,
        ));
        finish_tier2_phases(
            &JobOutcome::Fresh {
                payload: crate::inspect::scanners::dead_code::callgraph_unavailable_aggregate(0),
            },
            Some(callgraph_phase),
            Some(tier2_phase),
        );
        let (entries, _) = log.terminal_inputs();
        assert!(
            entries
                .iter()
                .all(|entry| entry.id != InspectPhaseId::CallgraphReady),
            "callgraph_ready must not complete when the builder aggregate is callgraph_unavailable: {entries:?}"
        );
    }

    #[test]
    fn inspect_builder_state_detail_uses_stable_honest_names() {
        assert_eq!(InspectBuilderState::Building.as_str(), "building");
        assert_eq!(
            InspectBuilderState::QueuedBehindColdBuilds.as_str(),
            "queued_behind_cold_builds"
        );
        assert_eq!(
            InspectBuilderState::GatedBySemanticSeed.as_str(),
            "gated_by_semantic_seed"
        );
        assert_eq!(InspectBuilderState::Suspended.as_str(), "suspended");
        assert_eq!(
            InspectBuilderState::BuildDenied.as_str(),
            "build_denied (borrow-only)"
        );
        assert_eq!(InspectBuilderState::Absent.as_str(), "absent");
    }
}

#[cfg(test)]
mod checkout_deferred_tests {
    use super::*;
    use crate::views::contracts::{QueryWait, ViewAccess, WaitOutcome};
    use crate::views::manifest_v2::{ManifestHeader, ManifestV2, Producers};
    use crate::views::snapshot::{LiveDelta, OpenGeneration, Snapshot};

    struct Timeout {
        snapshot: Snapshot,
        gap: PathBuf,
        calls: std::sync::atomic::AtomicUsize,
    }
    impl QueryWait for Timeout {
        fn wait_for(
            &self,
            _: &ViewAccess,
            _: crate::blob_store::v2::FamilyPlane,
            _: Duration,
        ) -> WaitOutcome {
            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            WaitOutcome::TimedOut {
                snapshot: self.snapshot.clone(),
                unreflected: vec![self.gap.clone()],
            }
        }
    }
    fn run(active: bool) -> (Response, usize) {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("README.md"), "# Fixture\n").unwrap();
        let mut config = crate::config::Config::default();
        config.project_root = Some(root.path().to_path_buf());
        let ctx = Arc::new(AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            config,
        ));
        ctx.set_harness(crate::harness::Harness::Opencode);
        let manifest = ManifestV2::new(ManifestHeader {
            producers: Producers {
                trigram: "test".into(),
                semantic: None,
                callgraph: crate::views::callgraph::PRODUCER.into(),
            },
            head_tree: None,
            ignore_fingerprint: None,
            segment: None,
        });
        let snapshot =
            LiveDelta::new(Arc::new(OpenGeneration::new("test", manifest, None))).snapshot();
        let waiter = Arc::new(Timeout {
            snapshot,
            gap: root.path().join("pending.ts"),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        if active {
            let registry =
                crate::views::registry::FamilyRegistry::open(storage.path(), "inspect-test")
                    .unwrap();
            let access = ViewAccess::Owner(registry.register_view("scope", root.path()).unwrap());
            ctx.install_checkout_query_runtime(Arc::new(
                crate::views::query_wait::CheckoutQueryRuntime {
                    access,
                    waiter: waiter.clone(),
                    callgraph: Arc::new(crate::views::callgraph::CallgraphPlane::default()),
                },
            ));
        }
        let request: RawRequest = serde_json::from_value(
            serde_json::json!({"id":"deferred-checkout", "command":"inspect"}),
        )
        .unwrap();
        let (started, release) = install_deferred_inspect_stat_gate_for_test();
        let DispatchOutcome::Deferred(mut pending) = handle_inspect_deferred(&request, ctx.clone())
        else {
            panic!("expected deferred inspect");
        };
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        release.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let response = loop {
            if let Some(response) = (pending.poll)(&ctx) {
                break response;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        };
        (
            response,
            waiter.calls.load(std::sync::atomic::Ordering::Relaxed),
        )
    }
    #[test]
    fn deferred_checkout_timeout_attaches_named_gaps_before_guard_drops() {
        let _serial = deferred_inspect_test_lock();
        let (response, calls) = run(true);
        assert_eq!(calls, 1, "one wait budget per worker request");
        assert_eq!(response.data["complete"], false);
        assert_eq!(response.data["inspect_terminal"], "partial");
        assert!(crate::subc_format::format_inspect_for_test(&response)
            .starts_with("PARTIAL — callgraph view pending:"));
        let gaps = response.data["gaps"].as_array().unwrap();
        assert!(gaps.iter().any(|gap| gap["kind"] == "view_pending"
            && gap["path"]
                .as_str()
                .is_some_and(|p| p.ends_with("pending.ts"))));
    }
    #[test]
    fn default_deferred_inspect_has_no_checkout_wait_or_gap_fields() {
        let _serial = deferred_inspect_test_lock();
        let (response, calls) = run(false);
        assert_eq!(calls, 0);
        assert_eq!(response.data["inspect_terminal"], "fresh");
        assert_eq!(response.data["complete"], true);
        assert!(response.data.get("gaps").is_none());
    }
}
