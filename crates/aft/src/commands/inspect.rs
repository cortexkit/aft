use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Condvar, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Map, Value};

use crate::alert_state::{AcceptedDiagnosticSnapshot, AcceptedObservationBatch};
use crate::context::AppContext;
use crate::inspect::diagnostics_category::{inspect_request_timeout, run_diagnostics_category};
#[cfg(test)]
use crate::inspect::InspectBuilderState;
use crate::inspect::{
    format_wait_text, InspectCache, InspectCategory, InspectPhaseEntry, InspectPhaseId,
    InspectPhaseLog, InspectSnapshot, JobOutcome, JobScope,
};
use crate::lsp::manager::{
    ApplicabilityResolutionError, ApplicableServerFailure, ApplicableServerSnapshot,
    ApplicableServerStartOutcomes,
};
use crate::lsp::roots::ServerKey;
use crate::protocol::{RawRequest, Response};
use crate::response_finalize::{DispatchOutcome, PendingResponse};

const DEFAULT_TOP_K: usize = 20;
const MAX_TOP_K: usize = 100;
const BLOCKING_TIER2_PHASE_TIMEOUT: Duration = Duration::from_secs(120);
/// Reserve time inside the configured request budget for terminal assembly and
/// egress. The server always answers before the client gives up: server work
/// stops before `diagnostics_timeout_ms`, while the client waits for that budget
/// plus its transport headroom.
const INSPECT_TERMINAL_MARGIN: Duration = Duration::from_secs(5);

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
        (Instant::now() + phase_limit).min(self.work_at)
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
    handle_inspect_payload(req, ctx, false, false, &[], &[], None, None)
}

/// Test-only warm-path entry that preserves nonblocking diagnostics semantics
/// while waiting on scanner completion events with the normal phase hang catch.
/// Integration fixtures use it when their assertion is unrelated to the
/// one-second interactive soft deadline.
#[doc(hidden)]
pub fn handle_inspect_warm_for_test(req: &RawRequest, ctx: &AppContext) -> Response {
    let phase_log = InspectPhaseLog::for_request(req.id.clone());
    handle_inspect_payload(req, ctx, false, false, &[], &[], Some(&phase_log), None)
}

pub fn handle_inspect_tool_call(req: &RawRequest, ctx: &AppContext) -> Response {
    let phase_log = InspectPhaseLog::for_request(req.id.clone());
    let deadline = InspectRequestDeadline::from_config(&ctx.config());
    let snapshot = match inspect_preflight(req, ctx) {
        Ok(snapshot) => snapshot,
        Err(response) => {
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
    let scoped_roots = scope_was_provided(req.params.get("scope")).then_some(scope.roots());
    let applicability = {
        let lsp = ctx.lsp();
        lsp.resolve_applicable_servers_for_inspect(
            &snapshot.project_root,
            scoped_roots,
            &snapshot.config,
            deadline.work_at(),
        )
    };
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

/// Diagnostics collection is always the warm working set, with or without a
/// request scope: scope filters rendered findings and adds per-file authority
/// (named gaps for scoped files no producer has authoritatively analyzed),
/// never extra collection work.
fn handle_inspect_payload(
    req: &RawRequest,
    ctx: &AppContext,
    force_root_diagnostics: bool,
    applicability_is_empty: bool,
    producer_failures: &[ApplicableServerFailure],
    expected_producers: &[ServerKey],
    phase_log: Option<&InspectPhaseLog>,
    request_deadline: Option<InspectRequestDeadline>,
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
    let scope = match parse_scope(req, ctx, &snapshot.project_root) {
        Ok(scope) => scope,
        Err(response) => return response,
    };

    if inspect_cancellation_requested() {
        return inspect_interrupted_response(&req.id);
    }

    let manager = ctx.inspect_manager();
    let blocking_tier1_deadline = phase_log.map(|_| {
        request_deadline.map_or_else(
            || Instant::now() + inspect_request_timeout(snapshot.config.as_ref()),
            InspectRequestDeadline::work_at,
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
        if !ctx.inspect_writer() {
            continue;
        }
        let phase_entry = InspectPhaseEntry::category(InspectPhaseId::Tier2Rescan, category);
        if request_deadline.is_some_and(|deadline| !deadline.has_work_budget()) {
            return phase_failure_response(
                &req.id,
                &phase_entry,
                "inspect_request_timeout",
                request_deadline
                    .expect("checked request deadline")
                    .timeout_detail(InspectPhaseId::Tier2Rescan),
            );
        }
        let manager = manager.clone();
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
            let outcome = if force_root_diagnostics {
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
                    || std::time::Instant::now() + BLOCKING_TIER2_PHASE_TIMEOUT,
                    |deadline| deadline.phase_deadline(BLOCKING_TIER2_PHASE_TIMEOUT),
                ),
                callgraph_phase,
                tier2_phase,
            ),
        );
    }

    for category in InspectCategory::active() {
        if outcomes.contains_key(category) {
            continue;
        }
        if inspect_cancellation_requested() {
            return inspect_interrupted_response(&req.id);
        }
        let outcome = if *category == InspectCategory::Diagnostics {
            // Diagnostics use the serial LSP lane rather than the inspect worker
            // pool. A non-authoritative collection remains a non-fresh outcome;
            // it is never converted into a partial inspect payload below.
            run_diagnostics_category(
                ctx,
                &snapshot,
                &scope,
                scope_was_provided,
                applicability_is_empty,
                producer_failures,
                expected_producers,
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
                        let request_timed_out = outcome.payload().is_none()
                            && matches!(
                                &outcome,
                                JobOutcome::Failed { message }
                                    if message.contains("inspect_request_timeout")
                            );
                        finish_tier2_phases(&outcome, callgraph_phase, tier2_phase);
                        if request_timed_out {
                            return phase_failure_response(
                                &req.id,
                                &InspectPhaseEntry::category(
                                    InspectPhaseId::Tier2Rescan,
                                    *category,
                                ),
                                "inspect_request_timeout",
                                request_deadline
                                    .expect("request timeout requires a shared deadline")
                                    .timeout_detail(InspectPhaseId::Tier2Rescan),
                            );
                        }
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

    let payloads = match fresh_payloads(&outcomes) {
        Ok(payloads) => payloads,
        Err(message) => return Response::error(&req.id, "inspect_not_fresh", message),
    };

    let payload = build_inspect_payload(&snapshot, &payloads, &sections, top_k, ctx);
    Response::success(&req.id, payload)
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
    let scoped_roots = scope_was_provided(req.params.get("scope")).then_some(scope.roots());
    let applicability = {
        let lsp = ctx.lsp();
        lsp.resolve_applicable_servers_for_inspect(
            &snapshot.project_root,
            scoped_roots,
            &snapshot.config,
            deadline.work_at(),
        )
    };
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
    std::thread::spawn(move || {
        let _cancellation = crate::executor::install_job_cancellation(worker_cancellation);
        let _force_restrict = force_restrict.then(|| ctx.force_restrict_guard(&request.id));
        // Queueing instead of sharing a response keeps request-specific scopes,
        // phase logs, and terminals independent while bounding expensive work to
        // one detached inspect body per root.
        let response = match DeferredInspectRootPermit::acquire(root, deadline) {
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
    .any(|category| {
        matches!(
            outcomes.get(category),
            Some(JobOutcome::Stale { .. } | JobOutcome::Pending { .. })
        )
    });
    let todos = outcomes
        .get(&InspectCategory::Todos)
        .and_then(JobOutcome::payload)
        .and_then(|payload| payload.get("count"))
        .and_then(Value::as_u64)
        .map(|count| count as usize);

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
    for server in &applicability.server_keys {
        let phase_entry = InspectPhaseEntry::lsp(InspectPhaseId::LspStart, server);
        if !deadline.has_work_budget() {
            return build_inspect_terminal(
                &req.id,
                &phase_log,
                request_deadline_terminal(Some(phase_entry), deadline),
            );
        }
        let phase = phase_log.start(phase_entry.clone());
        let outcome = {
            let mut lsp = ctx.lsp();
            lsp.start_applicable_server_until(
                &applicability,
                server,
                &ctx.config(),
                deadline.work_at(),
            )
        };
        if inspect_cancellation_requested() {
            phase.fail("inspect request cancelled");
            return build_inspect_terminal(&req.id, &phase_log, InspectTerminal::Interrupted);
        }
        let deadline_exceeded = outcome.deadline_exceeded.is_some();
        finish_start_phases(vec![(server.clone(), phase)], &outcome);
        start_outcomes.successful.extend(outcome.successful);
        start_outcomes.failures.extend(outcome.failures);
        if deadline_exceeded {
            return build_inspect_terminal(
                &req.id,
                &phase_log,
                request_deadline_terminal(Some(phase_entry), deadline),
            );
        }
    }

    if inspect_cancellation_requested() {
        return build_inspect_terminal(&req.id, &phase_log, InspectTerminal::Interrupted);
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
    // A blocking inspection waits for the producers it started so the warm
    // store holds their settled view before the payload reads it. The wait is
    // root-level — producers fill the warm store by publishing while events
    // are drained — and never per-file: a request scope cannot change it.
    let wait_outcome = wait_for_root_quiescence(ctx, &start_outcomes.successful, deadline);
    if inspect_cancellation_requested() {
        for phase in quiescence {
            phase.fail("inspect request cancelled");
        }
        return build_inspect_terminal(&req.id, &phase_log, InspectTerminal::Interrupted);
    }
    // A blocking inspection is an explicit diagnostics observation source. Keep
    // accepted producer snapshots intact until the inspect response is built;
    // flattened category payloads cannot recover producer ownership.
    let accepted_snapshots = match wait_outcome {
        Ok((snapshots, blocked)) => {
            if blocked {
                phase_log.note_blocking_wait();
            }
            snapshots
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
    for phase in quiescence {
        phase.complete();
    }
    let inspect_snapshot = build_snapshot(ctx).ok();
    let response = handle_inspect_payload(
        req,
        ctx,
        true,
        applicability.server_keys.is_empty(),
        &start_outcomes.failures,
        &start_outcomes.successful,
        Some(&phase_log),
        Some(deadline),
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
    if !deadline.has_work_budget() {
        let failed_phase =
            InspectPhaseEntry::category(InspectPhaseId::StatVerification, InspectCategory::Metrics);
        return build_inspect_terminal(
            &req.id,
            &phase_log,
            request_deadline_terminal(Some(failed_phase), deadline),
        );
    }

    if let Err(terminal) =
        verify_final_root_stats(&project_root, &initial_stats, &phase_log, deadline)
    {
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
/// (declares quiescence). Events are drained with the manager lock held only
/// for the drain itself, so producers keep publishing while the wait ticks.
/// Cancellation and the shared request deadline are checked at every tick.
fn wait_for_root_quiescence(
    ctx: &AppContext,
    expected: &[ServerKey],
    deadline: InspectRequestDeadline,
) -> Result<(Vec<AcceptedDiagnosticSnapshot>, bool), String> {
    let mut accepted_snapshots = Vec::new();
    let mut blocked = false;
    loop {
        if inspect_cancellation_requested() {
            return Err("inspect request cancelled during LSP quiescence".to_string());
        }
        if !deadline.has_work_budget() {
            return Err(deadline.timeout_detail(InspectPhaseId::LspQuiescence));
        }
        accepted_snapshots.extend(ctx.lsp().drain_events().accepted_snapshots);
        if root_producers_settled(ctx, expected) {
            return Ok((accepted_snapshots, blocked));
        }
        blocked = true;
        let remaining = deadline.work_at().saturating_duration_since(Instant::now());
        std::thread::sleep(Duration::from_millis(50).min(remaining));
    }
}

fn root_producers_settled(ctx: &AppContext, expected: &[ServerKey]) -> bool {
    // Authoritative-report predicates reject watcher-stale and provisional
    // entries, so delivered file events invalidate this wait immediately. File
    // delivery is asynchronous, however; the terminal StatVerification phase
    // compares the scanned file set directly and closes that latency window.
    // The unscoped diagnostics gate uses this same producer-settled check, so
    // a wait that returns cannot then be judged incomplete for lack of reports.
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

fn phase_failure_response(
    request_id: &str,
    phase: &InspectPhaseEntry,
    failure_reason: &'static str,
    detail: String,
) -> Response {
    let mut data = serde_json::json!({
        "code": failure_reason,
        "message": detail,
        "failed_phase": phase.id,
    });
    if let Some(producer) = &phase.producer {
        data["producer"] = Value::String(producer.clone());
    }
    if let Some(category) = &phase.category {
        data["category"] = Value::String(category.clone());
    }
    Response {
        id: request_id.to_string(),
        success: false,
        data,
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
            payload.insert(
                "inspect_terminal".to_string(),
                Value::String("fresh".to_string()),
            );
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
                    "inspect_phase_timeout: tier2 {} aggregate did not complete within {}s; builder_state={}",
                    category.as_str(),
                    BLOCKING_TIER2_PHASE_TIMEOUT.as_secs(),
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
) -> Result<JobScope, Response> {
    let Some(value) = req.params.get("scope") else {
        return Ok(JobScope::for_project(project_root.to_path_buf()));
    };
    if value.is_null() || empty_string(value) || empty_array(value) {
        return Ok(JobScope::for_project(project_root.to_path_buf()));
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
    for scope in raw_scopes {
        let raw_path = PathBuf::from(scope);
        let candidate = if raw_path.is_absolute() {
            raw_path
        } else {
            project_root.join(raw_path)
        };
        let validated = ctx.validate_path(&req.id, &candidate)?;
        roots.push(std::fs::canonicalize(&validated).unwrap_or(validated));
    }

    Ok(JobScope::from_roots(project_root.to_path_buf(), roots))
}

fn build_inspect_payload(
    snapshot: &InspectSnapshot,
    payloads: &BTreeMap<InspectCategory, Value>,
    sections: &Sections,
    top_k: usize,
    ctx: &AppContext,
) -> Value {
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
        let mut category_summary = summary_for(*category, payload);
        if payload.get("complete").and_then(Value::as_bool) == Some(false) {
            category_summary["complete"] = Value::Bool(false);
            if let Some(category_gaps) = payload.get("gaps").and_then(Value::as_array) {
                category_summary["gaps"] = Value::Array(category_gaps.clone());
                gaps.extend(category_gaps.iter().cloned().map(|mut gap| {
                    gap["categories"] = serde_json::json!([category.as_str()]);
                    gap
                }));
            }
        }
        summary.insert(category.as_str().to_string(), category_summary);
        if sections.includes(*category) {
            details.insert(
                category.as_str().to_string(),
                details_for(*category, payload, top_k),
            );
            if matches!(
                *category,
                InspectCategory::DeadCode | InspectCategory::UnusedExports
            ) {
                let test_only_detail = test_only_details_for(payload, top_k);
                if test_only_detail
                    .as_array()
                    .is_some_and(|items| !items.is_empty())
                {
                    details.insert(format!("{}_test_only", category.as_str()), test_only_detail);
                }
            }
            if matches!(
                *category,
                InspectCategory::DeadCode
                    | InspectCategory::UnusedExports
                    | InspectCategory::Duplicates
            ) {
                let generated_detail = generated_details_for(payload, top_k);
                if generated_detail
                    .as_array()
                    .is_some_and(|items| !items.is_empty())
                {
                    details.insert(format!("{}_generated", category.as_str()), generated_detail);
                }
            }
        } else if *category == InspectCategory::Diagnostics {
            // Diagnostics detail is actionable even without an explicit section.
            // `top_k` limits rows only; summaries are always computed in full.
            let detail = details_for(*category, payload, top_k);
            if detail.as_array().is_some_and(|items| !items.is_empty()) {
                details.insert(category.as_str().to_string(), detail);
            }
        }
    }

    let text = render_inspect_text(&summary, &details);
    let mut payload = serde_json::json!({
        "summary": Value::Object(summary),
        "text": text,
        "scanner_state": {
            "tier2_last_run": tier2_last_run(snapshot),
            "tier2_trigger_reason": ctx.tier2_trigger_reason(),
            "disabled_categories": InspectCategory::disabled()
                .iter()
                .map(|category| category.as_str())
                .collect::<Vec<_>>(),
        }
    });
    if !details.is_empty() {
        payload["details"] = Value::Object(details);
    }
    if !gaps.is_empty() {
        payload["complete"] = Value::Bool(false);
        payload["gaps"] = Value::Array(gaps);
    }
    payload
}

/// Render the compact agent-facing body. One source of truth for OpenCode + Pi.
fn render_inspect_text(summary: &Map<String, Value>, details: &Map<String, Value>) -> String {
    let mut lines: Vec<String> = Vec::new();

    // Counts are emitted only from verified producer results. A failed producer
    // is rendered separately so the remaining findings cannot read as all-clear.
    render_incomplete_categories(&mut lines, summary);
    render_group_category(&mut lines, "Duplicates", summary, details, "duplicates");
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

    lines.join("\n")
}

fn render_incomplete_categories(lines: &mut Vec<String>, summary: &Map<String, Value>) {
    for (category, value) in summary {
        if value.get("complete").and_then(Value::as_bool) != Some(false) {
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
            if gap.get("kind").and_then(Value::as_str) == Some("uncovered_file") {
                let file = gap
                    .get("file")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown file");
                lines.push(format!(
                    "Incomplete {category}: no authoritative diagnostics for {file} ({reason})"
                ));
                continue;
            }
            let producer = gap
                .get("producer")
                .and_then(Value::as_str)
                .unwrap_or("unknown producer");
            lines.push(format!(
                "Incomplete {category}: producer {producer} failed ({reason})"
            ));
        }
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
    lines.push(format!(
        "Import cycles: {count} import {cycle_word} (largest: {largest} {file_word})"
    ));
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
    details
        .get(key)
        .and_then(Value::as_array)
        .filter(|items| !items.is_empty())
        .or_else(|| {
            summary
                .get(key)
                .and_then(|s| s.get("top"))
                .and_then(Value::as_array)
        })
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
        lines.push(format!("Dead code analysis unavailable ({reason})"));
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
    if count == 0 {
        lines.push(format!("{label}: 0{generated_suffix}{skipped_suffix}"));
    } else {
        lines.push(format!(
            "{label}: {count}{suffix}{generated_suffix}{skipped_suffix}:"
        ));
        if let Some(items) = category_items(summary, details, key) {
            for item in items.iter().filter(|item| !item_is_generated(item)) {
                let file = item.get("file").and_then(Value::as_str).unwrap_or("?");
                let symbol = item.get("symbol").and_then(Value::as_str).unwrap_or("?");
                lines.push(format!("  {file}::{symbol}"));
            }
        }
    }
    render_generated_symbol_usage(lines, summary, details, key);
    render_test_only_usage(lines, summary, details, key);
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
}

fn test_only_items<'a>(
    summary: &'a Map<String, Value>,
    details: &'a Map<String, Value>,
    key: &str,
) -> Option<&'a Vec<Value>> {
    details
        .get(&format!("{key}_test_only"))
        .and_then(Value::as_array)
        .filter(|items| !items.is_empty())
        .or_else(|| {
            summary
                .get(key)
                .and_then(|s| s.get("test_only_top"))
                .and_then(Value::as_array)
        })
}

fn generated_items<'a>(
    summary: &'a Map<String, Value>,
    details: &'a Map<String, Value>,
    key: &str,
) -> Option<&'a Vec<Value>> {
    details
        .get(&format!("{key}_generated"))
        .and_then(Value::as_array)
        .filter(|items| !items.is_empty())
        .or_else(|| {
            summary
                .get(key)
                .and_then(|s| s.get("generated_top"))
                .and_then(Value::as_array)
        })
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
) {
    if key == "duplicates" {
        render_duplicates_category(lines, label, summary, details, key);
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
    if count == 0 {
        lines.push(format!("{label}: 0"));
        return;
    }
    lines.push(format!("{label}: {count} (top by cost):"));
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
}

fn render_duplicates_category(
    lines: &mut Vec<String>,
    label: &str,
    summary: &Map<String, Value>,
    details: &Map<String, Value>,
    key: &str,
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
    let Some(duplicated_lines) = section.get("duplicated_lines").and_then(Value::as_u64) else {
        if count == 0 {
            lines.push(format!("{label}: 0{generated_suffix}"));
            render_generated_duplicate_usage(lines, summary, details, key);
            return;
        }
        lines.push(format!(
            "{label}: {count}{}{generated_suffix} (top by cost):",
            duplicate_suppression_clause(section)
        ));
        render_duplicate_rows(lines, summary, details, key);
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
    // A zero denominator means analyzed-line counts are missing (pre-v0.44
    // cached contributions); print no percentage rather than a false "0.0%".
    let percent_clause = if total_lines > 0 {
        format!(
            " ({}% of {total_lines} analyzed lines)",
            format_percent(percent)
        )
    } else {
        String::new()
    };
    lines.push(format!(
        "{label}: {duplicated_lines} duplicated lines{percent_clause} across {file_count} files, {group_count} {}{suppression_clause}{generated_suffix}{suffix}",
        plural_group(group_count),
    ));
    if count > 0 {
        render_duplicate_rows(lines, summary, details, key);
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
    if count == 0 {
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
        lines.push(format!("TODOs: {count}"));
    } else {
        lines.push(format!("TODOs: {count} ({by_kind})"));
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
}

fn summary_for(category: InspectCategory, payload: &Value) -> Value {
    computed_summary_for(category, payload)
}

fn computed_summary_for(category: InspectCategory, payload: &Value) -> Value {
    match category {
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
            // This is a terminal capability result, not a partial scan: dead-code
            // analysis cannot run without the callgraph, so it must not claim zero.
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
    }
}

fn diagnostics_summary_for(payload: &Value) -> Value {
    serde_json::json!({
        "errors": payload.get("errors").and_then(Value::as_u64).unwrap_or(0),
        "warnings": payload.get("warnings").and_then(Value::as_u64).unwrap_or(0),
        "info": payload.get("info").and_then(Value::as_u64).unwrap_or(0),
        "hints": payload.get("hints").and_then(Value::as_u64).unwrap_or(0),
    })
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

fn available_count_from_payload(category: InspectCategory, payload: &Value) -> Option<usize> {
    if category == InspectCategory::DeadCode
        && payload.get("callgraph_available").and_then(Value::as_bool) == Some(false)
    {
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
}

#[cfg(test)]
mod render_text_tests {
    use super::*;

    fn summary_map(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    fn render(summary: Value) -> String {
        render_inspect_text(&summary_map(summary), &Map::new())
    }

    fn render_with_details(summary: Value, details: Value) -> String {
        render_inspect_text(&summary_map(summary), &summary_map(details))
    }

    #[test]
    fn renders_unavailable_dead_code_without_a_zero_count() {
        let text = render(serde_json::json!({
            "dead_code": { "callgraph_available": false }
        }));

        assert_eq!(text, "Dead code analysis unavailable (no callgraph)");
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
        let text = render_inspect_text(&Map::new(), &Map::new());
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

    fn assert_no_banned_field(value: &Value) {
        // `callgraph_available` is capability disclosure, not partiality, so fresh
        // payloads may report terminal callgraph unavailability.
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
                        assert_eq!(
                            value.as_bool(),
                            Some(false),
                            "inspect completion disclosure must name a real gap"
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
}

#[cfg(test)]
mod deferred_terminal_tests {
    use super::*;

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
        let deadline =
            InspectRequestDeadline::new(Duration::from_millis(120), Duration::from_millis(40));
        let outcome = receive_tier2_completion_until(
            rx,
            &manager,
            InspectCategory::DeadCode,
            deadline.phase_deadline(BLOCKING_TIER2_PHASE_TIMEOUT),
            Some(deadline),
        )
        .expect("deadline produces an honest failure");
        assert!(
            !deadline.has_work_budget(),
            "the receive loop must not return before the shared work deadline"
        );
        assert!(matches!(
            outcome,
            JobOutcome::Failed { message } if message.contains("inspect_request_timeout")
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
            Some(&phase_log),
            Some(InspectRequestDeadline::new(Duration::ZERO, Duration::ZERO)),
        );

        assert!(!response.success);
        assert_eq!(response.data["failed_phase"], "tier2_rescan");
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
