//! `module.draining` support: the drain window, the registry of held bash
//! calls, and the census of every request the module still holds open.
//!
//! The daemon releases a request's in-flight credit only when the module sends a
//! terminal frame (StreamEnd, Response or Error) for that correlation id on the
//! route. A drain therefore quiesces only when every held request has answered.
//! The census names each kind of held request so a drain that runs to its
//! forced-teardown ceiling shows in the log exactly what held it open.
//!
//! Kinds of held request, and what `module.draining` does with each:
//!
//! - `bg_events`: a held background-event stream subscription. Ended with a
//!   StreamEnd at once; completions stay in the per-session registry.
//! - `bash:<phase>`: a foreground bash call from spawn to its terminal
//!   response (`spawning`, `foreground` wait window, `block` to completion,
//!   `wait` for `wait: true`). Detached into a background task, exactly as a
//!   user-message detach does: the caller gets the task id and the task keeps
//!   running. Never killed. Each call detaches itself on its next poll; one
//!   still held when the backstop comes due (see [`bash_drain_backstop_at`])
//!   is answered by the module loop directly, because that poll runs through
//!   the root's executor actor and can sit queued behind other work there. A
//!   client Cancel for a held bash call is answered the same way, at once.
//! - `permission_ask`: an untrusted bash call waiting on the host's
//!   elicitation answer. Answered at once with the retryable
//!   `module_reloading` error; the command never ran, so a retry is safe.
//! - `deferred:<tool>`: a deferred `inspect` or LSP navigation response still
//!   running off the executor (each may legitimately take up to two minutes).
//!   Answered at once with its own shutdown terminal, or a retryable
//!   `module_reloading` error when it has none. Both are read-only, so the
//!   caller retries on the restarted module.
//! - `tool:<name>`: an ordinary executor tool call. Left to finish: these
//!   complete well inside the drain deadline. A call stays counted until its
//!   response frame is in the writer queue, not merely until it has run.
//! - `route_bind`: a RouteBind awaiting its configure job. Left to finish; it
//!   is already bounded by the route-bind deadline.
//!
//! `control_requests` (the module's own channel-0 requests to the daemon) are
//! reported alongside but are not route-held requests.
//!
//! The census only sees requests the module still tracks. Every path that stops
//! tracking a request other than by answering it (a client Cancel, reclaiming a
//! deleted project root) must itself send the request's terminal frame, or the
//! daemon keeps counting a request this census no longer shows.

use std::collections::BTreeMap;
use std::sync::atomic::AtomicU8;

use super::*;

/// Upper bound on how long one `module.draining` notice keeps the module in
/// drain mode. The daemon's own drain ceiling is 30 s; the cap only guards
/// against a nonsensical deadline leaving drain behaviour switched on for good
/// if the daemon abandons the drain and keeps this module.
pub(super) const MODULE_DRAINING_WINDOW_CAP: Duration = Duration::from_secs(120);

/// Error code for requests answered early because the module is draining. The
/// daemon uses the same code for route opens refused while a module reloads,
/// and subc clients already treat it as retryable.
pub(super) const MODULE_DRAINING_ERROR_CODE: &str = "module_reloading";

/// Converts the daemon's wall-clock drain deadline (Unix milliseconds) into a
/// local monotonic instant, capped by [`MODULE_DRAINING_WINDOW_CAP`]. A deadline
/// already in the past yields `now`, which leaves no drain window open.
pub(super) fn draining_window_end(deadline_ms: u64) -> Instant {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    let remaining = Duration::from_millis(deadline_ms.saturating_sub(now_ms));
    Instant::now() + remaining.min(MODULE_DRAINING_WINDOW_CAP)
}

/// The window opened by the daemon's `module.draining` notice, shared between
/// the module loop and the detached bash wait tasks.
#[derive(Clone, Default)]
pub(super) struct ModuleDrainWindow {
    until: Arc<StdMutex<Option<Instant>>>,
}

impl ModuleDrainWindow {
    pub(super) fn begin(&self, until: Instant) {
        *self.lock() = Some(until);
    }

    /// True while the drain window is open.
    pub(super) fn is_active(&self) -> bool {
        self.lock().is_some_and(|until| Instant::now() < until)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Instant>> {
        self.until
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Where a foreground bash call currently is between its request and its
/// terminal response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BashHoldPhase {
    /// The spawn job has not reported back yet.
    Spawning,
    /// Plain foreground call inside its wait window; promoted at the deadline.
    Foreground,
    /// `block_to_completion` without `wait`: waits for the command to exit.
    Block,
    /// `wait: true`: waits for the command to exit or a detach signal.
    Wait,
}

impl BashHoldPhase {
    /// True for the phases that sit in the bash wait loop, which detaches the
    /// call into a background task on its own next poll once the drain window
    /// opens. A call in one of these phases will answer shortly after the
    /// release step without anything else acting on it. A call still spawning
    /// has not reached that loop yet, so it is not counted as detaching.
    fn detaches_on_drain(self) -> bool {
        matches!(self, Self::Foreground | Self::Block | Self::Wait)
    }

    fn label(self) -> &'static str {
        match self {
            Self::Spawning => "bash:spawning",
            Self::Foreground => "bash:foreground",
            Self::Block => "bash:block",
            Self::Wait => "bash:wait",
        }
    }
}

/// Who sends a held bash call's terminal frame. Exactly one side may: the
/// call's own wait task, or the module loop answering for it.
const CLAIM_UNCLAIMED: u8 = 0;
const CLAIM_WAIT_TASK: u8 = 1;
const CLAIM_MODULE_LOOP: u8 = 2;

/// Decides, exactly once, who answers one held bash call.
///
/// The wait task normally answers its own call after polling the command on the
/// executor. The module loop answers instead when that answer cannot be relied
/// on to arrive in time: a drain whose deadline is near, or a client Cancel
/// (the caller is gone). Whichever side claims first owns the answer and the
/// task's wait registration; the other side must neither send a frame nor touch
/// that registration.
#[derive(Default)]
pub(super) struct BashCallClaim {
    state: AtomicU8,
    cancel_requested: AtomicBool,
    changed: Notify,
}

impl BashCallClaim {
    /// Claims the call for its wait task. True when the wait task owns the
    /// answer (it just claimed it, or had already); false when the module loop
    /// answered, in which case the wait task must stop without a frame.
    pub(super) fn claim_for_wait_task(&self) -> bool {
        match self.state.compare_exchange(
            CLAIM_UNCLAIMED,
            CLAIM_WAIT_TASK,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => true,
            Err(current) => current == CLAIM_WAIT_TASK,
        }
    }

    fn claim_for_module_loop(&self) -> bool {
        let claimed = self
            .state
            .compare_exchange(
                CLAIM_UNCLAIMED,
                CLAIM_MODULE_LOOP,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok();
        if claimed {
            self.changed.notify_one();
        }
        claimed
    }

    pub(super) fn claimed_by_module_loop(&self) -> bool {
        self.state.load(Ordering::SeqCst) == CLAIM_MODULE_LOOP
    }

    fn request_cancel(&self) {
        self.cancel_requested.store(true, Ordering::SeqCst);
        self.changed.notify_one();
    }

    fn cancel_requested(&self) -> bool {
        self.cancel_requested.load(Ordering::SeqCst)
    }

    /// Resolves after the module loop claimed the call. The wait task selects
    /// on this so it stops even while it is parked on an executor poll that
    /// may not run for a long time.
    pub(super) async fn module_loop_claimed(&self) {
        loop {
            let notified = self.changed.notified();
            if self.claimed_by_module_loop() {
                return;
            }
            notified.await;
        }
    }
}

/// What the module loop needs to answer a held bash call and hand its command
/// to the background without the executor: the call's own wait loop runs every
/// poll through the root's executor actor, and a busy or wedged actor would
/// otherwise keep the call open for as long as it stays busy.
#[derive(Clone)]
pub(super) struct BashDetachTarget {
    pub(super) task_id: String,
    pub(super) session_id: String,
    /// `wait: true`: the session's wait-mode registration is ended rather than
    /// only the foreground-task registration.
    pub(super) wait_mode: bool,
    pub(super) registry: crate::bash_background::BgTaskRegistry,
    pub(super) request_id: String,
    pub(super) ver: u8,
    pub(super) flags: Flags,
    pub(super) format_context: crate::subc_format::FormatContext,
}

/// Why the module loop answered a held bash call itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BashLoopAnswer {
    /// The drain deadline is near and the call had not detached on its own.
    Drain,
    /// The client cancelled the call.
    Cancel,
}

#[derive(Clone)]
struct HeldBashCall {
    since: Instant,
    phase: BashHoldPhase,
    claim: Arc<BashCallClaim>,
    /// Set once the call reaches its foreground wait; until then the module
    /// loop cannot detach the command because it does not exist yet.
    detach: Option<BashDetachTarget>,
}

/// Every bash call whose route request has not yet received a terminal frame.
/// Entries are added when the call is submitted and removed when its completion
/// reaches the module loop, or when the module loop answers the call itself.
#[derive(Default)]
pub(super) struct HeldBashCalls {
    calls: StdMutex<HashMap<(RouteChannel, u64), HeldBashCall>>,
}

impl HeldBashCalls {
    /// Starts tracking a call and returns the claim its wait task must honour.
    pub(super) fn insert(&self, route: RouteChannel, corr: u64) -> Arc<BashCallClaim> {
        let claim = Arc::new(BashCallClaim::default());
        self.lock().insert(
            (route, corr),
            HeldBashCall {
                since: Instant::now(),
                phase: BashHoldPhase::Spawning,
                claim: Arc::clone(&claim),
                detach: None,
            },
        );
        claim
    }

    pub(super) fn set_phase(&self, route: RouteChannel, corr: u64, phase: BashHoldPhase) {
        if let Some(call) = self.lock().get_mut(&(route, corr)) {
            call.phase = phase;
        }
    }

    pub(super) fn set_detach_target(
        &self,
        route: RouteChannel,
        corr: u64,
        target: BashDetachTarget,
    ) {
        if let Some(call) = self.lock().get_mut(&(route, corr)) {
            call.detach = Some(target);
        }
    }

    pub(super) fn remove(&self, route: RouteChannel, corr: u64) {
        self.lock().remove(&(route, corr));
    }

    /// Records a client Cancel for a held call. Returns false when no bash
    /// call is held under that id.
    pub(super) fn request_cancel(&self, route: RouteChannel, corr: u64) -> bool {
        match self.lock().get(&(route, corr)) {
            Some(call) => {
                call.claim.request_cancel();
                true
            }
            None => false,
        }
    }

    /// Claims, for the module loop, every held call it should answer now: each
    /// call whose client cancelled it, and, when `drain_due`, each call in a
    /// phase that detaches on drain. Only calls that already reached their
    /// foreground wait qualify. Claimed calls stop being tracked here; the
    /// caller owes each one its terminal frame.
    pub(super) fn claim_for_module_loop(
        &self,
        drain_due: bool,
    ) -> Vec<(RouteChannel, u64, BashDetachTarget, BashLoopAnswer)> {
        let mut calls = self.lock();
        let mut claimed = Vec::new();
        for (&(route, corr), call) in calls.iter() {
            let Some(target) = call.detach.as_ref() else {
                continue;
            };
            let reason = if call.claim.cancel_requested() {
                BashLoopAnswer::Cancel
            } else if drain_due && call.phase.detaches_on_drain() {
                BashLoopAnswer::Drain
            } else {
                continue;
            };
            if call.claim.claim_for_module_loop() {
                claimed.push((route, corr, target.clone(), reason));
            }
        }
        for (route, corr, _, _) in &claimed {
            calls.remove(&(*route, *corr));
        }
        claimed
    }

    fn snapshot(&self) -> Vec<(Instant, BashHoldPhase)> {
        self.lock()
            .values()
            .map(|call| (call.since, call.phase))
            .collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(RouteChannel, u64), HeldBashCall>> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct HeldKind {
    count: usize,
    oldest: Option<Duration>,
    /// Requests of this kind release themselves asynchronously once the drain
    /// window is open (see [`BashHoldPhase::detaches_on_drain`]).
    detaching: bool,
}

/// Count of held requests by kind, with the oldest age per kind where the
/// module records when the request started.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct HeldRequestCensus {
    kinds: BTreeMap<String, HeldKind>,
    control_requests: usize,
}

impl HeldRequestCensus {
    fn add(&mut self, kind: impl Into<String>, age: Option<Duration>) {
        self.add_kind(kind, age, false);
    }

    fn add_kind(&mut self, kind: impl Into<String>, age: Option<Duration>, detaching: bool) {
        let entry = self.kinds.entry(kind.into()).or_default();
        entry.count += 1;
        entry.detaching = detaching;
        entry.oldest = match (entry.oldest, age) {
            (Some(current), Some(age)) => Some(current.max(age)),
            (current, age) => current.or(age),
        };
    }

    /// Number of route-held requests (excludes the module's own control requests).
    pub(super) fn total(&self) -> usize {
        self.kinds.values().map(|kind| kind.count).sum()
    }

    /// `(kind, count)` pairs in kind order.
    pub(super) fn counts(&self) -> Vec<(String, usize)> {
        self.kinds
            .iter()
            .map(|(kind, held)| (kind.clone(), held.count))
            .collect()
    }

    pub(super) fn oldest(&self) -> Option<Duration> {
        self.kinds.values().filter_map(|kind| kind.oldest).max()
    }

    /// Route-held requests that will release themselves without further action
    /// now that the drain window is open.
    pub(super) fn detaching(&self) -> usize {
        self.kinds
            .values()
            .filter(|kind| kind.detaching)
            .map(|kind| kind.count)
            .sum()
    }

    /// The `held` figure the line for `phase` reports. On the `released` line
    /// the self-releasing requests are reported as `detaching` instead, so
    /// `held` there counts only requests nothing is about to answer. The other
    /// phases report every route-held request: before the release step nothing
    /// is detaching yet, and a bash wait still held at the deadline is stuck.
    pub(super) fn held(&self, phase: &str) -> usize {
        if phase == DRAIN_PHASE_RELEASED {
            self.total() - self.detaching()
        } else {
            self.total()
        }
    }

    /// The single log line for this census, e.g.
    /// `subc attach: drain census phase=start held=3 oldest=12.3s kinds=[bash:wait=1 (12.3s), bg_events=2] control_requests=0`.
    /// The `released` line adds `detaching=<n>` after `held` (see [`Self::held`]).
    pub(super) fn line(&self, phase: &str) -> String {
        let kinds = self
            .kinds
            .iter()
            .map(|(kind, held)| match held.oldest {
                Some(age) => format!("{kind}={} ({})", held.count, format_age(age)),
                None => format!("{kind}={}", held.count),
            })
            .collect::<Vec<_>>()
            .join(", ");
        let oldest = self
            .oldest()
            .map(format_age)
            .unwrap_or_else(|| "-".to_string());
        let detaching = if phase == DRAIN_PHASE_RELEASED {
            format!(" detaching={}", self.detaching())
        } else {
            String::new()
        };
        format!(
            "subc attach: drain census phase={phase} held={}{detaching} oldest={oldest} kinds=[{kinds}] control_requests={}",
            self.held(phase),
            self.control_requests
        )
    }
}

fn format_age(age: Duration) -> String {
    format!("{:.1}s", age.as_secs_f64())
}

/// Collects every request the module still holds open on any route.
///
/// A deferred `inspect` or navigation call sits in both `active_tool_calls` and
/// `pending_responses` until it answers; it is counted once, as `deferred:*`.
#[allow(clippy::too_many_arguments)]
pub(super) fn held_request_census(
    active_tool_calls: &ActiveToolCalls,
    pending_responses: &PendingSubcResponses,
    metrics: &DispatchPathMetrics,
    pending_bash_asks: &HashMap<ReverseCorrKey, PendingBashAsk>,
    bg_subs: &HashMap<RouteChannel, BgSub>,
    pending_binds: &HashMap<RouteChannel, PendingBind>,
    control_requests: usize,
) -> HeldRequestCensus {
    let now = Instant::now();
    let age = |since: Instant| Some(now.saturating_duration_since(since));
    let mut census = HeldRequestCensus {
        control_requests,
        ..HeldRequestCensus::default()
    };
    let mut deferred_keys = HashSet::new();
    for entry in &pending_responses.entries {
        deferred_keys.insert((entry.route, entry.corr));
        census.add(
            format!("deferred:{}", entry.bare_name),
            age(entry.held_since),
        );
    }
    {
        let calls = active_tool_calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (key, call) in calls.iter() {
            if !deferred_keys.contains(key) {
                census.add(format!("tool:{}", call.tool), age(call.started_at));
            }
        }
    }
    for (since, phase) in metrics.held_bash_calls.snapshot() {
        census.add_kind(phase.label(), age(since), phase.detaches_on_drain());
    }
    for ask in pending_bash_asks.values() {
        census.add("permission_ask", age(ask.asked_at));
    }
    for _ in bg_subs.values() {
        census.add("bg_events", None);
    }
    for bind in pending_binds.values() {
        census.add("route_bind", age(bind.started_at));
    }
    census
}

/// Logs one census line and hands the same census to the test probe.
pub(super) fn report_drain_census(
    phase: &str,
    census: &HeldRequestCensus,
    lifecycle_probe: Option<&SubcTestLifecycleProbe>,
) {
    emit_drain_census(phase, census, census.line(phase), lifecycle_probe);
}

/// Logs the `connection-end` census. `quiesced_before_close` says whether a
/// drain tick saw nothing held before the connection ended; without it a
/// `held=0` here cannot tell a drain that quiesced from requests that were
/// still open when the daemon closed the connection and were dropped with it.
pub(super) fn report_connection_end_census(
    census: &HeldRequestCensus,
    quiesced_before_close: bool,
    lifecycle_probe: Option<&SubcTestLifecycleProbe>,
) {
    let line = connection_end_line(census, quiesced_before_close);
    emit_drain_census(DRAIN_PHASE_CONNECTION_END, census, line, lifecycle_probe);
}

fn connection_end_line(census: &HeldRequestCensus, quiesced_before_close: bool) -> String {
    format!(
        "{} quiesced_before_close={quiesced_before_close}",
        census.line(DRAIN_PHASE_CONNECTION_END)
    )
}

fn emit_drain_census(
    phase: &str,
    census: &HeldRequestCensus,
    line: String,
    lifecycle_probe: Option<&SubcTestLifecycleProbe>,
) {
    if phase == DRAIN_PHASE_DEADLINE && census.total() > 0 {
        log::warn!("{line}");
    } else {
        log::info!("{line}");
    }
    if let Some(probe) = lifecycle_probe {
        probe.drain_census(phase, census, &line);
    }
}

/// Census phases, in the order they can appear for one drain.
/// `start`: on the `module.draining` notice, before anything is released.
pub(super) const DRAIN_PHASE_START: &str = "start";
/// `released`: right after the notice's release actions ran. Bash waits are
/// still held at that instant but detach on their own next poll; the line
/// reports them as `detaching` rather than `held`.
pub(super) const DRAIN_PHASE_RELEASED: &str = "released";
/// `quiesced`: the first drain tick that finds nothing held.
pub(super) const DRAIN_PHASE_QUIESCED: &str = "quiesced";
/// `deadline`: the drain deadline passed with the connection still up.
pub(super) const DRAIN_PHASE_DEADLINE: &str = "deadline";
/// `connection-end`: the daemon connection ended during or after a drain. The
/// line says whether `quiesced` was observed before the close.
pub(super) const DRAIN_PHASE_CONNECTION_END: &str = "connection-end";

/// Which one-time census lines one drain has already written.
pub(super) struct DrainProgress {
    pub(super) until: Instant,
    pub(super) quiesced_reported: bool,
    pub(super) deadline_reported: bool,
    /// From this instant the module loop answers every held bash call that has
    /// not detached on its own (see [`bash_drain_backstop_at`]).
    pub(super) bash_backstop_at: Instant,
}

/// Longest a drain waits for held bash calls to detach on their own before the
/// module loop answers them itself. A call normally detaches on its next
/// 100 ms poll, but that poll runs through the root's executor actor, and a
/// busy actor can hold it past the whole drain deadline.
pub(super) const BASH_DRAIN_BACKSTOP_MAX: Duration = Duration::from_millis(500);

/// When the module loop starts answering held bash calls for a drain that began
/// at `now` and ends at `until`: [`BASH_DRAIN_BACKSTOP_MAX`] after the notice,
/// or a quarter of the way to the deadline when the drain is shorter, so the
/// answers still land well inside it.
pub(super) fn bash_drain_backstop_at(now: Instant, until: Instant) -> Instant {
    let window = until.saturating_duration_since(now);
    now + BASH_DRAIN_BACKSTOP_MAX.min(window / 4)
}

/// Response for a request answered early because the module is draining.
pub(super) fn module_draining_response(request_id: &str, what: &str) -> Response {
    Response::error_with_data(
        request_id,
        MODULE_DRAINING_ERROR_CODE,
        format!("{what} was not completed because AFT is restarting; retry the call"),
        json!({ "retryable": true }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn census_line_lists_counts_and_oldest_age_per_kind() {
        let mut census = HeldRequestCensus {
            control_requests: 1,
            ..HeldRequestCensus::default()
        };
        census.add("bash:wait", Some(Duration::from_millis(12_300)));
        census.add("bash:wait", Some(Duration::from_millis(400)));
        census.add("bg_events", None);
        census.add("tool:grep", Some(Duration::from_millis(100)));

        assert_eq!(census.total(), 4);
        assert_eq!(
            census.counts(),
            vec![
                ("bash:wait".to_string(), 2),
                ("bg_events".to_string(), 1),
                ("tool:grep".to_string(), 1),
            ]
        );
        assert_eq!(census.oldest(), Some(Duration::from_millis(12_300)));
        assert_eq!(
            census.line("start"),
            "subc attach: drain census phase=start held=4 oldest=12.3s kinds=[bash:wait=2 (12.3s), bg_events=1, tool:grep=1 (0.1s)] control_requests=1"
        );
    }

    #[test]
    fn released_line_reports_self_detaching_bash_waits_apart_from_held() {
        let mut census = HeldRequestCensus::default();
        census.add_kind(
            BashHoldPhase::Foreground.label(),
            Some(Duration::from_millis(1_500)),
            BashHoldPhase::Foreground.detaches_on_drain(),
        );
        census.add_kind(
            BashHoldPhase::Wait.label(),
            Some(Duration::from_millis(45_200)),
            BashHoldPhase::Wait.detaches_on_drain(),
        );
        census.add_kind(
            BashHoldPhase::Spawning.label(),
            Some(Duration::from_millis(200)),
            BashHoldPhase::Spawning.detaches_on_drain(),
        );
        census.add("tool:grep", Some(Duration::from_millis(100)));

        assert_eq!(census.total(), 4);
        assert_eq!(census.detaching(), 2);
        assert_eq!(census.held(DRAIN_PHASE_RELEASED), 2);
        assert_eq!(
            census.line(DRAIN_PHASE_RELEASED),
            "subc attach: drain census phase=released held=2 detaching=2 oldest=45.2s kinds=[bash:foreground=1 (1.5s), bash:spawning=1 (0.2s), bash:wait=1 (45.2s), tool:grep=1 (0.1s)] control_requests=0"
        );
        // Before the release step and at the deadline every held request counts.
        assert_eq!(census.held(DRAIN_PHASE_START), 4);
        assert!(!census.line(DRAIN_PHASE_DEADLINE).contains("detaching="));
    }

    #[test]
    fn connection_end_line_states_whether_quiescence_was_seen() {
        let census = HeldRequestCensus::default();
        assert_eq!(
            connection_end_line(&census, true),
            "subc attach: drain census phase=connection-end held=0 oldest=- kinds=[] control_requests=0 quiesced_before_close=true"
        );
        assert!(connection_end_line(&census, false).ends_with(" quiesced_before_close=false"));
    }

    #[test]
    fn empty_census_reads_as_quiesced() {
        let census = HeldRequestCensus::default();
        assert_eq!(census.total(), 0);
        assert_eq!(
            census.line("quiesced"),
            "subc attach: drain census phase=quiesced held=0 oldest=- kinds=[] control_requests=0"
        );
    }

    #[test]
    fn drain_window_is_active_only_until_its_end() {
        let window = ModuleDrainWindow::default();
        assert!(!window.is_active());
        window.begin(Instant::now() + Duration::from_secs(5));
        assert!(window.is_active());
        window.begin(Instant::now());
        assert!(!window.is_active());
    }

    #[test]
    fn held_bash_calls_track_phase_until_removed() {
        let calls = HeldBashCalls::default();
        let route = RouteChannel {
            channel: 3,
            epoch: 1,
        };
        calls.insert(route, 7);
        calls.set_phase(route, 7, BashHoldPhase::Wait);
        let snapshot = calls.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].1, BashHoldPhase::Wait);
        calls.remove(route, 7);
        assert!(calls.snapshot().is_empty());
    }
}
