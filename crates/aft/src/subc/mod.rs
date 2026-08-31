//! subc daemon attach — transport edge.
//!
//! When AFT is launched as `aft --subc <connection-file>`, it does NOT run the
//! standalone NDJSON-over-stdin loop. Instead it connects to a running subc
//! daemon over loopback TCP, authenticates with the pre-envelope HMAC handshake
//! (`subc-transport`), then speaks the subc frame protocol (`subc-protocol`):
//! ModuleHello → HelloAck (register as a tool provider), then a channel-0
//! control loop (Ping/Pong, RouteBind) plus route-channel tool calls.
//!
//! Concurrency: subc routes tool calls through the executor. The tokio
//! edge never dispatches against `AppContext` inline; per-actor executor lanes
//! own the reader/mutator epoch, while a writer task serializes outbound frames.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::Config;
use crate::context::{App, AppContext, ProgressSender, RootHealthSnapshot};
use crate::executor::{Executor, JobCancellation, Lane};
use crate::fleet_status::{spawn_fleet_status_dial, FleetStatusClient};
use crate::log_ctx;
use crate::path_identity::ProjectRootId;
use crate::protocol::{ProgressKind, PushFrame, RawRequest, Response};
use crate::response_finalize::{DispatchOutcome, PendingResponse};
use crate::run_tool_call::{
    finish_tool_call_response, prepare_tool_call, run_tool_call, strip_agent_preview_arg_owned,
    PhaseTrace, ToolCallContext, ToolCallOutcome, ToolCallResult,
};
use crate::runtime_drain;
use crate::sandbox_spawn::{AuthenticatedPrincipal, PrincipalTrust};

use subc_protocol::manifest::{
    Bindings, Concurrency, ExecutionMode, IdentityBinding, IdentityScope, ModuleManifest,
    ProviderRole, StorageBinding, StorageKind, StorageScope, Tool, TrustTier,
};
use subc_protocol::session::{
    HealthReport, HealthStatus, ModuleControlRequest, ModuleControlResponse,
    MODULE_CONTROL_OP_HEALTH_CHECK,
};
use subc_protocol::{
    ErrorBody, Flags, Frame, FrameType, ModuleHelloBody, Principal, Priority, MAX_FRAME_BODY_LEN,
    PROTOCOL_VERSION,
};
use subc_transport::{authenticate_client, connection_file, read_frame, write_frame};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, Notify};
use tokio::task::JoinHandle;

/// Per-attempt handshake deadline. The initial attach loop has a separate total
/// budget so a stalled peer cannot consume an unbounded supervisor launch window.
const AUTH_DEADLINE: Duration = Duration::from_secs(5);
const ATTACH_RETRY_BUDGET: Duration = Duration::from_secs(60);
const ATTACH_RETRY_INITIAL_BACKOFF: Duration = Duration::from_millis(250);
const ATTACH_RETRY_MAX_BACKOFF: Duration = Duration::from_secs(5);
const ATTACH_RETRY_JITTER_PERCENT: u64 = 20;

/// Correlation id for the initial ModuleHello (channel 0).
const HELLO_CORR: u64 = 1;

/// Per-session in-memory replay cap for must-deliver Push frames. This covers
/// detach/re-attach while AFT stays alive; cross-restart replay is phased later.
const PUSH_BUFFER_MAX_PER_KEY: usize = 256;

/// Bounded guard for control-frame sends. If the daemon stops reading and the
/// writer queue stays full, tear the subc edge down instead of stalling the
/// route loop indefinitely.
const CONTROL_SEND_TIMEOUT: Duration = Duration::from_millis(250);

/// Cadence for the loop's deadline-driven drain work (retry-buffer flush,
/// bg-wake emission, maintenance submission). Checked at the top of every
/// loop turn so busy select arms cannot starve it.
const DRAIN_TICK_PERIOD: Duration = Duration::from_millis(250);

/// Root-scoped stores and watcher runtimes are reopened lazily after this
/// period without tool traffic. Keeping the value fixed avoids per-client
/// eviction policies competing inside the module loop.
const IDLE_ROOT_TTL: Duration = Duration::from_secs(30 * 60);

const WRITER_QUEUE_CAPACITY: usize = 256;

/// Keep reliable Push bursts from monopolizing the current-thread subc loop;
/// any remaining must-deliver frames stay queued for the next loop turn.
const RELIABLE_PUSH_DRAIN_BUDGET: usize = 32;

/// Limit maintenance submissions per tick so background drains cannot delay
/// control-plane work such as completed RouteBind acknowledgements.
///
/// The decomposed maintenance pass charges this budget by Mutating job, not by
/// root. Size the default burst for one maintenance pass over eight live roots,
/// while follow-up batches still re-enter the capped queue instead of bypassing
/// the budget.
const MAINTENANCE_SUBMIT_BUDGET: usize = INITIAL_MAINTENANCE_DRAIN_KINDS.len() * 8;
const INITIAL_MAINTENANCE_DRAIN_KINDS: [MaintenanceDrainKind; 4] = [
    MaintenanceDrainKind::Watcher,
    MaintenanceDrainKind::Lsp,
    MaintenanceDrainKind::ConfigureTail,
    MaintenanceDrainKind::CompletionDrains,
];
#[cfg(test)]
const INITIAL_MAINTENANCE_JOB_COUNT: usize = INITIAL_MAINTENANCE_DRAIN_KINDS.len();

const RELIABLE_WRITER_RETRY_INITIAL_BACKOFF: Duration = Duration::from_millis(10);
const RELIABLE_WRITER_RETRY_MAX_BACKOFF: Duration = Duration::from_millis(250);

const DISPATCH_PATH_BIND_WARN_AFTER: Duration = Duration::from_secs(6);
const ROUTE_BIND_DEADLINE: Duration = Duration::from_secs(12);
/// Upper bound on a caller-supplied `deadline_ms_remaining`. Covers the
/// production bash maximum (30 minutes) plus its 10-second transport margin
/// without permitting `Instant` overflow.
pub(crate) const MAX_REQUEST_DEADLINE_REMAINING: Duration = Duration::from_secs(31 * 60);

/// Convert ingress transport deadline metadata into one absolute local
/// `Instant`. Zero is rejected with logical `request_deadline_exceeded`; values
/// above the cap are clamped to the cap. `None` passes through unchanged.
pub(crate) fn normalize_request_deadline(
    deadline_ms_remaining: Option<u64>,
    request_id: &str,
) -> Result<Option<Instant>, Response> {
    match deadline_ms_remaining {
        None => Ok(None),
        Some(0) => Err(Response::error_with_data(
            request_id,
            "request_deadline_exceeded",
            "request deadline already elapsed at ingress",
            serde_json::json!({
                "retryable": false,
                "phase": "queue",
            }),
        )),
        Some(ms) => Ok(Some(
            Instant::now() + Duration::from_millis(ms).min(MAX_REQUEST_DEADLINE_REMAINING),
        )),
    }
}

/// Small bounded memory of completed task ids used to suppress stale lossy
const COMPLETED_TASK_SUPPRESSION_MAX: usize = 4096;

/// Bash foreground orchestration polls detached tasks with short read-lane jobs.
/// The sleep between polls is outside the executor so no read or write worker is
/// pinned while a foreground command is still running.
const PENDING_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Host elicitation asks fail closed if the MCP facade does not answer promptly.
const BASH_ELICITATION_TIMEOUT: Duration = Duration::from_secs(60);
const BASH_ELICITATION_CREATE_METHOD: &str = "elicitation/create";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct RouteChannel {
    channel: u16,
    epoch: u32,
}

impl fmt::Display for RouteChannel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.channel, self.epoch)
    }
}

type PushEnvelope = (ProjectRootId, PushFrame);
type LossyPushEnvelope = (u64, ProjectRootId, PushFrame);
type RetryBuffer = HashMap<RouteChannel, VecDeque<(push::ReplayKey, PushFrame)>>;
mod bash;
mod health;
mod manifest;
mod push;
mod standing;
mod wire;

use self::health::{
    build_health_report, warn_slow_pending_binds, warn_slow_running_interactive_jobs,
    DispatchPathMetrics, HealthRollupCache, ReapBlockerCensus, ResponseTaskGuard,
    HEALTH_ROLLUP_TTL,
};
use self::manifest::{
    build_manifest, command_lane, control_flags, control_ops, is_bash_family_tool,
    is_subc_agent_core_tool, is_subc_native_plumbing_tool,
};
pub use self::wire::SubcError;

/// Lifecycle milestones emitted only by the dedicated subc integration-test runner.
///
/// Production entry points never install a probe, so these notifications cannot
/// affect routing or delivery behavior.
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubcLifecycleEvent {
    AttachDecision {
        attempt: u32,
        will_retry: bool,
    },
    RouteDetached {
        route_channel: u16,
        route_epoch: u32,
        session_id: String,
    },
    ReliableCompletionRetained {
        task_id: String,
        session_id: String,
    },
    ReliableCompletionReplayed {
        route_channel: u16,
        route_epoch: u32,
        task_id: String,
        session_id: String,
    },
}

/// Test-only observer for the detach/rebind lifecycle.
#[doc(hidden)]
#[derive(Clone)]
pub struct SubcTestLifecycleProbe {
    events_tx: mpsc::UnboundedSender<SubcLifecycleEvent>,
}

impl SubcTestLifecycleProbe {
    #[doc(hidden)]
    pub fn new(events_tx: mpsc::UnboundedSender<SubcLifecycleEvent>) -> Self {
        Self { events_tx }
    }

    fn attach_decision(&self, attempt: u32, will_retry: bool) {
        let _ = self.events_tx.send(SubcLifecycleEvent::AttachDecision {
            attempt,
            will_retry,
        });
    }

    fn route_detached(&self, route: RouteChannel, session_id: &str) {
        let _ = self.events_tx.send(SubcLifecycleEvent::RouteDetached {
            route_channel: route.channel,
            route_epoch: route.epoch,
            session_id: session_id.to_string(),
        });
    }

    fn reliable_completion_retained(&self, task_id: &str, session_id: &str) {
        let _ = self
            .events_tx
            .send(SubcLifecycleEvent::ReliableCompletionRetained {
                task_id: task_id.to_string(),
                session_id: session_id.to_string(),
            });
    }

    fn reliable_completion_replayed(&self, route: RouteChannel, task_id: &str, session_id: &str) {
        let _ = self
            .events_tx
            .send(SubcLifecycleEvent::ReliableCompletionReplayed {
                route_channel: route.channel,
                route_epoch: route.epoch,
                task_id: task_id.to_string(),
                session_id: session_id.to_string(),
            });
    }
}

/// Test-only view of the fail-closed tool-call gate: would `name` be admitted
/// on a bound route (as an agent tool or native plumbing)? Used by the
/// plugin-send drift guard in `subc_plumbing_drift_test.rs`.
pub fn is_tool_call_admitted_for_test(name: &str) -> bool {
    manifest::is_subc_agent_core_tool(name) || manifest::is_subc_native_plumbing_tool(name)
}
use self::wire::{
    build_error_frame, build_goodbye_frame, build_tool_response_frame,
    build_tool_response_frame_with_limit, decrement_counted_channel, response_is_fatal_panic,
    response_message, send_counted_channel, send_frame, send_reliable_writer_frame,
    send_traced_tool_response_frame, ToolResponseWriteTrace, WriterFrame, WriterSender,
};

struct DecodedFrame {
    frame: Frame,
    phase_trace: PhaseTrace,
}

struct ToolCallCompletion {
    text: String,
    phase_trace: PhaseTrace,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RouteDetachPolicy {
    RetainForReplay,
    CancelOnDetach,
}

#[derive(Clone)]
struct ActiveToolCall {
    root_id: ProjectRootId,
    cancellation: JobCancellation,
    detach_policy: RouteDetachPolicy,
}

type ActiveToolCalls = Arc<StdMutex<HashMap<(RouteChannel, u64), ActiveToolCall>>>;

struct PendingDeferredSetupGuard(Arc<AtomicUsize>);

impl PendingDeferredSetupGuard {
    fn new(count: Arc<AtomicUsize>) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Self(count)
    }
}

impl Drop for PendingDeferredSetupGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

enum DeferredSetupOutcome {
    Immediate {
        text: String,
        phase_trace: PhaseTrace,
    },
    Deferred {
        pending: PendingResponse,
        surface_downgraded: bool,
        phase_trace: PhaseTrace,
    },
}

struct PendingSubcResponse {
    route: RouteChannel,
    corr: u64,
    flags: Flags,
    ver: u8,
    root: ProjectRootId,
    session_id: String,
    bare_name: String,
    format_context: crate::subc_format::FormatContext,
    bind_trust: BindTrust,
    pending: PendingResponse,
    surface_downgraded: bool,
    phase_trace: PhaseTrace,
}

struct ResolvedSubcResponse {
    entry: PendingSubcResponse,
    response: Response,
}

#[derive(Default)]
struct PendingSubcResponses {
    entries: Vec<PendingSubcResponse>,
}

impl PendingSubcResponses {
    fn register(&mut self, pending: PendingSubcResponse) {
        self.entries.retain(|entry| {
            let keep = entry.route != pending.route || entry.corr != pending.corr;
            if !keep {
                if let Some(cancellation) = &entry.pending.cancellation {
                    cancellation.request_cancel();
                }
            }
            keep
        });
        self.entries.push(pending);
    }

    fn poll_ready(&mut self, executor: &Executor) -> Vec<ResolvedSubcResponse> {
        let mut ready = Vec::new();
        let mut waiting = Vec::with_capacity(self.entries.len());
        for mut entry in self.entries.drain(..) {
            let response = executor
                .actor_context(&entry.root)
                .and_then(|ctx| (entry.pending.poll)(&ctx));
            if let Some(response) = response {
                ready.push(ResolvedSubcResponse { entry, response });
            } else {
                waiting.push(entry);
            }
        }
        self.entries = waiting;
        ready
    }

    fn cancel_request(&mut self, route: RouteChannel, corr: u64) -> bool {
        let mut cancelled = false;
        self.entries.retain(|entry| {
            let keep = entry.route != route || entry.corr != corr;
            if !keep {
                cancelled = true;
                if let Some(cancellation) = &entry.pending.cancellation {
                    cancellation.request_cancel();
                }
            }
            keep
        });
        cancelled
    }

    fn drain_route(
        &mut self,
        route: RouteChannel,
        executor: &Executor,
    ) -> Vec<ResolvedSubcResponse> {
        self.drain_matching(executor, |entry| entry.route == route)
    }

    fn drain_on_shutdown(&mut self, executor: &Executor) -> Vec<ResolvedSubcResponse> {
        self.drain_matching(executor, |_| true)
    }

    fn drain_matching(
        &mut self,
        executor: &Executor,
        matches: impl Fn(&PendingSubcResponse) -> bool,
    ) -> Vec<ResolvedSubcResponse> {
        let mut resolved = Vec::new();
        let mut waiting = Vec::with_capacity(self.entries.len());
        for mut entry in self.entries.drain(..) {
            if !matches(&entry) {
                waiting.push(entry);
                continue;
            }
            if let Some(cancellation) = &entry.pending.cancellation {
                cancellation.request_cancel();
            }
            if let Some(ctx) = executor.actor_context(&entry.root) {
                if let Some(on_shutdown) = entry.pending.on_shutdown.as_mut() {
                    let response = on_shutdown(&ctx);
                    resolved.push(ResolvedSubcResponse { entry, response });
                }
            }
        }
        self.entries = waiting;
        resolved
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[derive(Clone)]
struct PushSenders {
    lossy_tx: mpsc::Sender<LossyPushEnvelope>,
    reliable_tx: mpsc::UnboundedSender<PushEnvelope>,
    lossy_overflow: Arc<push::LossyOverflow>,
    lossy_seq: Arc<AtomicU64>,
    fleet_status_client: FleetStatusClient,
}

#[derive(Clone)]
struct PersistentCancelSignal {
    inner: Arc<PersistentCancelInner>,
}

struct PersistentCancelInner {
    cancelled: AtomicBool,
    notify: Notify,
}

impl PersistentCancelSignal {
    fn new() -> Self {
        Self {
            inner: Arc::new(PersistentCancelInner {
                cancelled: AtomicBool::new(false),
                notify: Notify::new(),
            }),
        }
    }

    fn cancel(&self) {
        if !self.inner.cancelled.swap(true, Ordering::SeqCst) {
            self.inner.notify.notify_waiters();
        }
    }

    fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    async fn cancelled(&self) {
        // `enable()` REGISTERS this waiter before we read the flag, closing the
        // lost-wakeup window: `notify_waiters()` only wakes already-registered
        // waiters and stores no permit, so without enable() a `cancel()` firing
        // between the flag read and `.await` would be missed and the future
        // would park forever (cancel() fires only once). With enable(), a cancel
        // racing the flag read still wakes the registered waiter. The loop is a
        // belt-and-suspenders re-check on spurious wakeups.
        loop {
            let notified = self.inner.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

fn submit_active_tool_call(
    executor: &Executor,
    active: &ActiveToolCalls,
    route: RouteChannel,
    corr: u64,
    root_id: ProjectRootId,
    lane: Lane,
    request_id: String,
    detach_policy: RouteDetachPolicy,
    job: crate::executor::ExecutorJob,
    deadline: Option<Instant>,
) -> oneshot::Receiver<Response> {
    let (rx, cancellation) = executor.submit_cancellable_async_with_deadline(
        root_id.clone(),
        lane,
        request_id,
        job,
        deadline,
    );
    active
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            (route, corr),
            ActiveToolCall {
                root_id,
                cancellation,
                detach_policy,
            },
        );
    rx
}

fn finish_active_tool_call(active: &ActiveToolCalls, route: RouteChannel, corr: u64) -> bool {
    active
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&(route, corr))
        .is_some()
}

fn active_tool_call_is_registered(
    active: &ActiveToolCalls,
    route: RouteChannel,
    corr: u64,
) -> bool {
    active
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains_key(&(route, corr))
}

fn cancel_active_tool_call(
    active: &ActiveToolCalls,
    executor: &Executor,
    route: RouteChannel,
    corr: u64,
    reason: &str,
) -> bool {
    let call = active
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&(route, corr));
    let Some(call) = call else {
        return false;
    };
    let outcome = executor.cancel_job(&call.root_id, &call.cancellation);
    log::debug!(
        "subc attach: cancelled active tool call route={route} corr={corr} reason={reason} outcome={outcome:?}"
    );
    true
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RouteWorkDisposition {
    RetainForReplay,
    Abandon,
}

fn apply_route_work_disposition(
    active: &ActiveToolCalls,
    executor: &Executor,
    route: RouteChannel,
    disposition: RouteWorkDisposition,
    reason: &str,
) -> usize {
    if disposition == RouteWorkDisposition::RetainForReplay {
        let (retained, cancelled) = {
            let mut calls = active
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut retained = 0usize;
            let mut cancelled = Vec::new();
            calls.retain(|(call_route, _), call| {
                if *call_route != route {
                    return true;
                }
                match call.detach_policy {
                    RouteDetachPolicy::RetainForReplay => {
                        retained += 1;
                        true
                    }
                    RouteDetachPolicy::CancelOnDetach => {
                        cancelled.push(call.clone());
                        false
                    }
                }
            });
            (retained, cancelled)
        };
        let cancelled_count = cancelled.len();
        for call in cancelled {
            executor.cancel_job(&call.root_id, &call.cancellation);
        }
        log::debug!(
            "subc attach: retained {retained} replayable active tool call(s) and cancelled {cancelled_count} teardown-terminal call(s) route={route} reason={reason}"
        );
        return retained;
    }

    let cancelled = {
        let mut calls = active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut cancelled = Vec::new();
        calls.retain(|(call_route, _), call| {
            if *call_route == route {
                cancelled.push(call.clone());
                false
            } else {
                true
            }
        });
        cancelled
    };
    for call in &cancelled {
        let outcome = executor.cancel_job(&call.root_id, &call.cancellation);
        log::debug!(
            "subc attach: cancelled active tool call route={route} reason={reason} outcome={outcome:?}"
        );
    }
    cancelled.len()
}

fn cancel_all_active_tool_calls(
    active: &ActiveToolCalls,
    executor: &Executor,
    reason: &str,
) -> usize {
    let cancelled = {
        let mut calls = active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        calls.drain().map(|(_, call)| call).collect::<Vec<_>>()
    };
    for call in &cancelled {
        let outcome = executor.cancel_job(&call.root_id, &call.cancellation);
        log::debug!("subc attach: cancelled active tool call reason={reason} outcome={outcome:?}");
    }
    cancelled.len()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BindTrust {
    FirstParty,
    Untrusted,
}

impl BindTrust {
    fn allows_bash_observation(self) -> bool {
        matches!(self, Self::FirstParty)
    }

    fn label(self) -> &'static str {
        match self {
            Self::FirstParty => "first_party",
            Self::Untrusted => "untrusted",
        }
    }

    fn sandbox_trust(self) -> PrincipalTrust {
        match self {
            Self::FirstParty => PrincipalTrust::FirstParty,
            Self::Untrusted => PrincipalTrust::Untrusted,
        }
    }
}

pub(super) fn trust_for_principal(principal: &Option<Principal>) -> BindTrust {
    match principal {
        Some(Principal::Direct) => BindTrust::FirstParty,
        // Module renames are flag-days: the daemon registry refuses duplicate
        // active ids, so a renaming module cannot advertise both names during
        // its transition. This allowlist is DIALLED, not dialling — it must
        // accept a module's NEW name in a released binary before the module
        // starts using it, and the old name stays until the flip has settled.
        // That is why transitional pairs appear here: llm-runner/broca was the
        // previous rename, alfonso-core/prefrontal is the current one. When
        // retiring an old name, confirm the fleet no longer spawns it — a
        // stale entry here is inert, but a missing one silently downgrades a
        // first-party module to Untrusted and revokes its bash access.
        Some(Principal::Reserved { module_id })
            if module_id == "llm-runner"
                || module_id == "aft"
                || module_id == "broca"
                || module_id == "alfonso-core"
                || module_id == "prefrontal"
                || module_id == "prefrontal-core" =>
        {
            BindTrust::FirstParty
        }
        Some(Principal::Reserved { .. }) | Some(Principal::Unverified) | None => {
            BindTrust::Untrusted
        }
    }
}

fn harness_forces_untrusted(harness: &str) -> bool {
    harness.starts_with("fed:")
}

pub(super) fn trust_for_bind(harness: &str, principal: &Option<Principal>) -> BindTrust {
    if harness_forces_untrusted(harness) {
        BindTrust::Untrusted
    } else {
        trust_for_principal(principal)
    }
}

fn principal_id(principal: &Option<Principal>) -> Option<String> {
    match principal {
        Some(Principal::Direct) => Some("direct".to_string()),
        Some(Principal::Reserved { module_id }) => Some(format!("reserved:{module_id}")),
        Some(Principal::Unverified) => Some("unverified".to_string()),
        None => None,
    }
}

fn principal_label(principal: &Option<Principal>) -> String {
    principal_id(principal).unwrap_or_else(|| "absent".to_string())
}

#[derive(Debug)]
/// Per-root route metadata owned by the subc loop. The `active_bash_waits` field
/// counts detached bash processes that are still being observed for this root.
/// Any future logic that evicts roots based on idle time must not evict a root
/// while this count is greater than zero, because a foreground bash response may
/// still arrive later.
struct RootMeta {
    maintenance_pending: bool,
    maintenance_jobs_in_flight: usize,
    maintenance_queued_kinds: VecDeque<MaintenanceDrainKind>,
    maintenance_last_submitted: Option<Instant>,
    maintenance_poisoned: bool,
    last_touched: Instant,
    diagnostics_on_edit: bool,
    active_bash_waits: usize,
    idle_artifacts_evicted: bool,
    unbound_quiesced: bool,
    consecutive_missing_sweeps: u8,
}

#[derive(Debug)]
struct PendingBind {
    bind_root_id: ProjectRootId,
    inserted_new_actor: bool,
    cancelled: bool,
    configure_request_id: String,
    started_at: Instant,
    warned_half_deadline: bool,
    deadline_reported: bool,
    corr: u64,
    ver: u8,
    flags: Flags,
    /// Exact-job cancellation for the submitted configure: Goodbye and
    /// deadline expiry cancel the executor job operationally (queued jobs are
    /// removed, running configures return at their next checkpoint) instead of
    /// only marking bookkeeping.
    cancellation: crate::executor::JobCancellation,
}

struct RouteBindCompletion {
    route: RouteChannel,
    identity: RouteIdentity,
    bind_root_id: ProjectRootId,
    inserted_new_actor: bool,
    configure_response: Response,
    diagnostics_on_edit: bool,
    ver: u8,
    corr: u64,
    flags: Flags,
}

#[derive(Debug, Clone)]
struct RouteIdentity(Arc<RouteIdentityData>);

#[derive(Debug)]
struct RouteIdentityData {
    root: ProjectRootId,
    project_root: PathBuf,
    harness: String,
    session: String,
    trust: BindTrust,
    spawn_principal: AuthenticatedPrincipal,
    consumer_elicitation_capable: bool,
}

impl Deref for RouteIdentity {
    type Target = RouteIdentityData;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Debug, Clone)]
struct RetainedSessionIdentity {
    harness: String,
    trust: BindTrust,
}

#[derive(Clone)]
struct BgSub {
    corr: u64,
    ver: u8,
    flags: Flags,
    root: ProjectRootId,
    session: String,
}

// A session can be observed by multiple long-lived consumer records. Retain
// every route so each wake uses the correlation captured by that route's BgSub.
type BgSubsBySession = HashMap<(ProjectRootId, String), HashSet<RouteChannel>>;

struct MaintenanceCompletion {
    root_id: ProjectRootId,
    kind: MaintenanceDrainKind,
    response: Response,
    empty_bg_sessions: Vec<(String, u64)>,
    requeue_kind: Option<MaintenanceDrainKind>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MaintenanceDrainKind {
    Watcher,
    Lsp,
    ConfigureTail,
    CompletionDrains,
}

impl MaintenanceDrainKind {
    fn label(self) -> &'static str {
        match self {
            Self::Watcher => "watcher",
            Self::Lsp => "lsp",
            Self::ConfigureTail => "configure-tail",
            Self::CompletionDrains => "completion-drains",
        }
    }
}

#[derive(Debug, Default)]
struct MaintenanceJobOutcome {
    empty_bg_sessions: Vec<(String, u64)>,
    requeue_kind: Option<MaintenanceDrainKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ReverseCorrKey {
    route: RouteChannel,
    corr: u64,
}

struct PendingBashAsk {
    route: RouteChannel,
    tool_corr: u64,
    tool_flags: Flags,
    tool_ver: u8,
    root: ProjectRootId,
    project_root: PathBuf,
    session_id: String,
    spawn_principal: AuthenticatedPrincipal,
    edit_slot_survives: Option<bool>,
    request_id: String,
    arguments: Value,
    format_context: crate::subc_format::FormatContext,
    cancel: bash::BashWaitCancel,
    grants: Vec<String>,
    expires_at: Instant,
    /// The caller's absolute request deadline, captured at ingress before
    /// permission elicitation. Checked again on an allowed reply before any
    /// spawn bookkeeping or executor submission.
    request_deadline: Option<Instant>,
}

impl RootMeta {
    fn new(now: Instant) -> Self {
        Self {
            maintenance_pending: false,
            maintenance_jobs_in_flight: 0,
            maintenance_queued_kinds: VecDeque::new(),
            maintenance_last_submitted: None,
            maintenance_poisoned: false,
            last_touched: now,
            diagnostics_on_edit: false,
            active_bash_waits: 0,
            idle_artifacts_evicted: false,
            unbound_quiesced: false,
            consecutive_missing_sweeps: 0,
        }
    }

    fn note_activity(&mut self) {
        self.last_touched = Instant::now();
    }

    fn reactivate_bound(&mut self) {
        self.note_activity();
        self.idle_artifacts_evicted = false;
        self.unbound_quiesced = false;
    }
}

fn due_maintenance_jobs(
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    executor: Option<&Executor>,
    bg_sub_by_session: &BgSubsBySession,
    bg_wake_pending: &HashSet<RouteChannel>,
    budget: usize,
    pending_bind_roots: &HashSet<ProjectRootId>,
) -> (Vec<(ProjectRootId, MaintenanceDrainKind)>, bool) {
    let mut jobs = Vec::new();
    let mut deferred = false;
    let mut roots = live_roots.keys().cloned().collect::<Vec<_>>();
    roots.sort_by(|left, right| {
        let left_last = live_roots
            .get(left)
            .and_then(|meta| meta.maintenance_last_submitted);
        let right_last = live_roots
            .get(right)
            .and_then(|meta| meta.maintenance_last_submitted);
        left_last
            .cmp(&right_last)
            .then_with(|| left.as_path().cmp(right.as_path()))
    });

    for root_id in roots {
        let Some(meta) = live_roots.get_mut(&root_id) else {
            continue;
        };
        if meta.maintenance_poisoned {
            continue;
        }

        if pending_bind_roots.contains(&root_id) {
            if meta.maintenance_pending || !meta.maintenance_queued_kinds.is_empty() {
                deferred = true;
            }
            continue;
        }

        if !meta.maintenance_pending {
            if jobs.len() >= budget {
                deferred = true;
                continue;
            }
            // Only enqueue kinds with pending work. Probes are cheap and
            // fail-open (contended sources count as pending), so an idle root
            // costs four probes per tick instead of four dispatched jobs.
            let executor_actor_context =
                executor.and_then(|executor| executor.actor_context(&root_id));
            let root_has_pending_bg_wake =
                bg_sub_by_session.iter().any(|((sub_root, _), channels)| {
                    sub_root == &root_id
                        && channels
                            .iter()
                            .any(|channel| bg_wake_pending.contains(channel))
                });
            let kinds_with_work: Vec<MaintenanceDrainKind> = match executor_actor_context {
                Some(ctx) => INITIAL_MAINTENANCE_DRAIN_KINDS
                    .into_iter()
                    .filter(|kind| {
                        if meta.unbound_quiesced && !matches!(kind, MaintenanceDrainKind::Lsp) {
                            return false;
                        }
                        match kind {
                            MaintenanceDrainKind::Watcher => ctx.watcher_drain_has_work(),
                            MaintenanceDrainKind::Lsp => ctx.lsp_drain_has_work(),
                            MaintenanceDrainKind::ConfigureTail => ctx.configure_tail_has_work(),
                            // Every CompletionDrains source is visible at this enqueue site:
                            // AppContext probes completion queues, this loop owns bg wakes,
                            // and queued continuations bypass probing via maintenance_pending.
                            // New drain sources must expose a probe here rather than making
                            // every subscribed root fail open again.
                            MaintenanceDrainKind::CompletionDrains => {
                                root_has_pending_bg_wake || ctx.completion_drains_have_work()
                            }
                        }
                    })
                    .collect(),
                None if meta.unbound_quiesced => Vec::new(),
                // No context handle (actor gone mid-tick): enqueue everything.
                None => INITIAL_MAINTENANCE_DRAIN_KINDS.to_vec(),
            };
            if kinds_with_work.is_empty() {
                continue;
            }
            meta.maintenance_pending = true;
            meta.maintenance_queued_kinds.extend(kinds_with_work);
        }

        while let Some(kind) = meta.maintenance_queued_kinds.pop_front() {
            if jobs.len() >= budget {
                meta.maintenance_queued_kinds.push_front(kind);
                deferred = true;
                break;
            }
            meta.maintenance_jobs_in_flight += 1;
            meta.maintenance_last_submitted = Some(Instant::now());
            jobs.push((root_id.clone(), kind));
        }

        meta.maintenance_pending =
            meta.maintenance_jobs_in_flight > 0 || !meta.maintenance_queued_kinds.is_empty();
    }

    (jobs, deferred)
}

fn eviction_estimate_label(estimate: &crate::memory::MemoryEstimate) -> String {
    match estimate.estimated_bytes {
        Some(bytes) => format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0)),
        None if estimate.status == "busy" => "busy".to_string(),
        None => "not estimated".to_string(),
    }
}

fn optional_memory_label(bytes: Option<u64>) -> String {
    bytes.map_or_else(
        || "not estimated".to_string(),
        |bytes| format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0)),
    )
}

fn pressure_relief_label(relief: &crate::memory::AllocatorPressureRelief) -> String {
    format!(
        "; allocator pressure relief: RSS {} -> {}, in-use {} -> {}, allocated {} -> {}, slack {} -> {}, allocator reported {:.1} MB released",
        optional_memory_label(relief.rss_before_bytes),
        optional_memory_label(relief.rss_after_bytes),
        optional_memory_label(relief.allocator_before.bytes_in_use),
        optional_memory_label(relief.allocator_after.bytes_in_use),
        optional_memory_label(relief.allocator_before.size_allocated),
        optional_memory_label(relief.allocator_after.size_allocated),
        optional_memory_label(relief.allocator_before.retained_slack_bytes),
        optional_memory_label(relief.allocator_after.retained_slack_bytes),
        relief.bytes_released as f64 / (1024.0 * 1024.0),
    )
}

fn idle_root_eviction_message(
    root_id: &ProjectRootId,
    memory: &crate::memory::RootMemorySnapshot,
    pressure_relief: Option<&crate::memory::AllocatorPressureRelief>,
) -> String {
    // Bash, LSP, and parser state remain resident. The freed total is deliberately
    // only the known-byte portion of handles eviction actually drops.
    let freed_bytes = [
        &memory.semantic,
        &memory.trigram,
        &memory.symbols,
        &memory.callgraph,
        &memory.inspect,
    ]
    .iter()
    .filter_map(|estimate| estimate.estimated_bytes)
    .fold(0u64, u64::saturating_add);
    let mut message = format!(
        "evicted idle root {}: freed ~{:.1} MB (semantic {}, trigram {}, symbols {}, callgraph {}, inspect {}; retained: bash {}, lsp {}, parser_pool {})",
        root_id.as_path().display(),
        freed_bytes as f64 / (1024.0 * 1024.0),
        eviction_estimate_label(&memory.semantic),
        eviction_estimate_label(&memory.trigram),
        eviction_estimate_label(&memory.symbols),
        eviction_estimate_label(&memory.callgraph),
        eviction_estimate_label(&memory.inspect),
        eviction_estimate_label(&memory.bash),
        eviction_estimate_label(&memory.lsp),
        eviction_estimate_label(&memory.parser_pool),
    );
    if let Some(pressure_relief) = pressure_relief {
        message.push_str(&pressure_relief_label(pressure_relief));
    }
    message
}

fn process_has_been_idle(now: Instant, live_roots: &HashMap<ProjectRootId, RootMeta>) -> bool {
    !live_roots.is_empty()
        && live_roots.values().all(|meta| {
            now.saturating_duration_since(meta.last_touched) >= IDLE_ROOT_TTL
                && meta.active_bash_waits == 0
                && !meta.maintenance_pending
                && meta.maintenance_queued_kinds.is_empty()
        })
}

fn allocator_pressure_relief_after_idle_sweep(
    now: Instant,
    live_roots: &HashMap<ProjectRootId, RootMeta>,
    executor: &Executor,
) -> Option<crate::memory::AllocatorPressureRelief> {
    if !process_has_been_idle(now, live_roots)
        || live_roots.keys().any(|root_id| {
            executor
                .actor_context(root_id)
                .is_some_and(|ctx| ctx.artifact_eviction_blocked())
        })
    {
        return None;
    }

    #[cfg(any(target_os = "macos", all(target_os = "linux", target_env = "gnu")))]
    {
        Some(crate::memory::relieve_allocator_pressure())
    }
    #[cfg(not(any(target_os = "macos", all(target_os = "linux", target_env = "gnu"))))]
    {
        None
    }
}

fn quiesce_unbound_root(
    root_id: &ProjectRootId,
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    executor: &Arc<Executor>,
) {
    let Some(meta) = live_roots.get_mut(root_id) else {
        return;
    };

    let ctx = executor.actor_context(root_id);
    if let Some(ctx) = ctx.as_ref() {
        // Close lifecycle admission before touching scheduler queues. A running
        // ConfigureTail cannot release gates, install a watcher, or reserve a
        // callgraph build after this transition becomes visible.
        ctx.mark_subc_unbound();
    }
    let cancelled = executor.cancel_queued_maintenance(root_id);
    // Transient unbind keeps the root WARM: the watcher stays running (its
    // events accumulate and replay on rebind, so no unobserved gap exists) and
    // resident artifacts stay resident. Host restarts unbind every root and
    // rebind seconds later; stopping the watcher here would force strict
    // re-verification plus a full callgraph rebuild on every restart. The
    // expensive teardown (watcher stop + gap invalidation) belongs to the
    // idle-TTL reaper and the root-deleted path.
    let discarded = ctx
        .map(|ctx| crate::commands::configure::cancel_deferred_configure_maintenance(&ctx))
        .unwrap_or(0);
    meta.unbound_quiesced = true;
    meta.maintenance_queued_kinds.clear();
    meta.maintenance_pending = meta.maintenance_jobs_in_flight > 0;
    log::info!(
        "subc attach: quiesced unbound root {} (cancelled {} queued maintenance job(s), cancelled {} configure maintenance job(s)); cause=goodbye_unbound",
        root_id.as_path().display(),
        cancelled,
        discarded
    );
}

#[allow(clippy::too_many_arguments)]
fn quiesce_connection_roots(
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    pending_binds: &mut HashMap<RouteChannel, PendingBind>,
    routes: &mut HashMap<RouteChannel, RouteIdentity>,
    root_channels: &mut HashMap<ProjectRootId, HashSet<RouteChannel>>,
    installed_route_epochs: &mut HashMap<u16, u32>,
    route_bash_cancels: &mut HashMap<RouteChannel, bash::RouteBashCancel>,
    active_tool_calls: &ActiveToolCalls,
    executor: &Arc<Executor>,
) {
    cancel_all_active_tool_calls(active_tool_calls, executor, "connection teardown");
    for cancel in route_bash_cancels.values() {
        cancel.token.cancel();
    }
    route_bash_cancels.clear();

    let mut roots = live_roots.keys().cloned().collect::<HashSet<_>>();
    for pending in pending_binds.values_mut() {
        pending.cancelled = true;
        roots.insert(pending.bind_root_id.clone());
        let _ = executor.cancel_job(&pending.bind_root_id, &pending.cancellation);
    }

    // A connection exit abandons every installed route at once. Close lifecycle
    // admission before cancelling maintenance so no deferred worker can restore
    // root activity after the loop-owned route tables disappear.
    for root_id in roots {
        if live_roots.contains_key(&root_id) {
            quiesce_unbound_root(&root_id, live_roots, executor);
        } else if let Some(ctx) = executor.actor_context(&root_id) {
            ctx.mark_subc_unbound();
            executor.cancel_queued_maintenance(&root_id);
            crate::commands::configure::cancel_deferred_configure_maintenance(&ctx);
        }
    }

    routes.clear();
    root_channels.clear();
    installed_route_epochs.clear();
}

/// Per-channel epoch watermarks for roots reclaimed without a client Goodbye.
/// The 16-bit channel space bounds this map, and no root identity or resource
/// handle is retained. It exists only so late requests receive a typed error.
#[derive(Debug, Default)]
struct ReclaimedRoutes {
    highest_epoch_by_channel: HashMap<u16, u32>,
}

impl ReclaimedRoutes {
    fn insert(&mut self, route: RouteChannel) {
        self.highest_epoch_by_channel
            .entry(route.channel)
            .and_modify(|epoch| *epoch = (*epoch).max(route.epoch))
            .or_insert(route.epoch);
    }

    fn contains(&self, route: RouteChannel) -> bool {
        self.highest_epoch_by_channel
            .get(&route.channel)
            .is_some_and(|epoch| route.epoch <= *epoch)
    }
}

#[derive(Debug, Default)]
struct IdleReapOutcome {
    evicted: usize,
    forgotten_deleted_roots: Vec<ProjectRootId>,
}

fn reap_idle_roots(
    now: Instant,
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    pending_binds: &HashMap<RouteChannel, PendingBind>,
    root_channels: &HashMap<ProjectRootId, HashSet<RouteChannel>>,
    executor: &Arc<Executor>,
    metrics: &DispatchPathMetrics,
) -> IdleReapOutcome {
    let pending_bind_roots = pending_binds
        .values()
        .map(|pending| pending.bind_root_id.clone())
        .collect::<HashSet<_>>();
    let mut census = ReapBlockerCensus::default();
    let mut candidates = Vec::new();

    for (root_id, meta) in live_roots.iter_mut() {
        let deleted = !root_id.as_path().exists();
        if deleted {
            // A missing directory makes a bound route obsolete, but one failed
            // lookup is not enough evidence to tear down a client-visible actor.
            // Requiring two maintenance sweeps protects atomic replacement and
            // transient filesystem failures; observing the path resets the proof.
            // Absence also covers renames: a task's cwd handle can follow the
            // moved directory while the registered path disappears. The old
            // path is deliberately treated as a retired root identity, so the
            // reaper accepts killing such tasks; rename a project only with no
            // live tasks rather than relying on cwd-resolution heuristics.
            meta.consecutive_missing_sweeps = meta.consecutive_missing_sweeps.saturating_add(1);
        } else {
            meta.consecutive_missing_sweeps = 0;
        }
        let deletion_confirmed = meta.consecutive_missing_sweeps >= 2;
        let has_bound_route = root_channels
            .get(root_id)
            .is_some_and(|channels| !channels.is_empty());
        let has_pending_bind = pending_bind_roots.contains(root_id);

        if deleted {
            let mut retained = false;
            if !deletion_confirmed {
                census.absence_unconfirmed += 1;
                retained = true;
            }
            // Once absence is confirmed, the directory cannot serve this route
            // again. Neither a stale route nor the lack of normal unbind cleanup
            // justifies retaining the root; purge removes the route after retirement.
            if meta.active_bash_waits > 0 {
                census.bash_waits += 1;
                retained = true;
            }
            if meta.maintenance_pending {
                census.maintenance_pending += 1;
                retained = true;
            }
            if !meta.maintenance_queued_kinds.is_empty() {
                census.maintenance_queued += 1;
                retained = true;
            }
            if has_pending_bind {
                census.pending_binds += 1;
                retained = true;
            }
            match executor.try_actor_is_idle(root_id) {
                Some(true) => {}
                Some(false) => {
                    census.actor_busy += 1;
                    retained = true;
                }
                None => {
                    census.actor_state_busy += 1;
                    retained = true;
                }
            }
            if retained {
                census.deleted_retained += 1;
                continue;
            }
        } else {
            // Route teardown marks the lifecycle admission gate before the last
            // channel disappears. Requiring zero bound channels and a quiesced
            // lifecycle prevents a still-bound root from losing its watcher.
            if has_bound_route
                || !meta.unbound_quiesced
                || meta.idle_artifacts_evicted
                || now.saturating_duration_since(meta.last_touched) < IDLE_ROOT_TTL
                || meta.active_bash_waits > 0
                || meta.maintenance_pending
                || !meta.maintenance_queued_kinds.is_empty()
                || has_pending_bind
                || !executor.actor_is_idle(root_id)
            {
                continue;
            }
        }
        candidates.push((root_id.clone(), deleted));
    }

    let mut reaped = Vec::new();
    let mut forgotten_deleted_roots = Vec::new();
    for (root_id, deleted) in candidates {
        let Some(ctx) = executor.actor_context(&root_id) else {
            if deleted {
                census.deleted_retained += 1;
                census.actor_busy += 1;
            }
            continue;
        };
        // A TTL-aged unbound root retained its watcher-derived pending paths
        // across the transient-unbind window. Strict gap invalidation subsumes
        // them, but every abort path must restore them because a rebind can
        // still happen until eviction commits.
        //
        // After two consecutive directory-absence scans confirm that the
        // root is gone, terminate its background task before checking the
        // artifact-eviction gate. The task can otherwise keep the root's
        // artifacts in use; cleanup first lets confirmed reclamation finish
        // without weakening the gate for unrelated active work.
        if deleted {
            ctx.bash_background()
                .kill_running_tasks_for_root(root_id.as_path());
        }
        let taken_pending = Some(ctx.take_pending_reconciliation_state());
        if ctx.artifact_eviction_blocked() {
            if let Some(pending) = taken_pending {
                ctx.restore_pending_reconciliation_state(pending);
            }
            if deleted {
                census.deleted_retained += 1;
                census.artifact_eviction_blocked += 1;
            }
            continue;
        }
        let memory_before = ctx.memory_root_snapshot();
        if !ctx.evict_idle_artifacts() {
            if let Some(pending) = taken_pending {
                ctx.restore_pending_reconciliation_state(pending);
            }
            if deleted {
                census.deleted_retained += 1;
                census.artifact_eviction_failed += 1;
            }
            continue;
        }
        drop(taken_pending);
        ctx.stop_watcher_runtime_in_background();
        // Edits during watcher downtime are unobserved. Advance publication
        // epochs and force strict verification before any later warm reload.
        ctx.invalidate_artifacts_after_watcher_gap();

        if deleted {
            if executor.retire_idle_actor_in_background(&root_id) {
                live_roots.remove(&root_id);
                forgotten_deleted_roots.push(root_id.clone());
            } else {
                census.deleted_retained += 1;
                census.actor_busy += 1;
            }
        } else {
            if let Some(meta) = live_roots.get_mut(&root_id) {
                meta.idle_artifacts_evicted = true;
            }
            ctx.release_idle_reopenable_resources_in_background();
        }
        reaped.push((root_id, memory_before));
    }

    metrics.record_reap(census);
    if census.deleted_retained > 0 {
        log::info!(
            "subc attach: retained {} deleted root(s) during idle reap; blockers={}",
            census.deleted_retained,
            census.blocker_histogram()
        );
    }

    let pressure_relief = (!reaped.is_empty())
        .then(|| allocator_pressure_relief_after_idle_sweep(now, live_roots, executor))
        .flatten();
    for (root_id, memory_before) in &reaped {
        log::info!(
            "{}",
            idle_root_eviction_message(root_id, memory_before, pressure_relief.as_ref())
        );
    }
    IdleReapOutcome {
        evicted: reaped.len(),
        forgotten_deleted_roots,
    }
}

#[allow(clippy::too_many_arguments)]
fn purge_deleted_root_residents(
    root_id: &ProjectRootId,
    routes: &mut HashMap<RouteChannel, RouteIdentity>,
    root_channels: &mut HashMap<ProjectRootId, HashSet<RouteChannel>>,
    installed_route_epochs: &mut HashMap<u16, u32>,
    route_bash_cancels: &mut HashMap<RouteChannel, bash::RouteBashCancel>,
    active_tool_calls: &ActiveToolCalls,
    executor: &Executor,
    retry_buffer: &mut RetryBuffer,
    reclaimed_routes: &mut ReclaimedRoutes,
    session_identity: &mut HashMap<(ProjectRootId, String), RetainedSessionIdentity>,
    push_buffer: &mut HashMap<push::ReplayKey, VecDeque<PushFrame>>,
    bg_subs: &mut HashMap<RouteChannel, BgSub>,
    bg_sub_by_session: &mut BgSubsBySession,
    bg_wake_pending: &mut HashSet<RouteChannel>,
    bg_wake_epoch: &mut HashMap<(ProjectRootId, String), u64>,
    pending_bash_asks: &mut HashMap<ReverseCorrKey, PendingBashAsk>,
    metrics: &DispatchPathMetrics,
) {
    let mut stale_routes = root_channels.get(root_id).cloned().unwrap_or_default();
    stale_routes.extend(
        routes
            .iter()
            .filter_map(|(route, identity)| (&identity.root == root_id).then_some(*route)),
    );
    stale_routes.extend(
        bg_sub_by_session
            .iter()
            .filter(|((root, _), _)| root == root_id)
            .flat_map(|(_, routes)| routes.iter().copied()),
    );
    stale_routes.extend(
        pending_bash_asks
            .values()
            .filter_map(|ask| (&ask.root == root_id).then_some(ask.route)),
    );

    for route in stale_routes {
        reclaimed_routes.insert(route);
        remove_installed_route(installed_route_epochs, route);
        remove_route_channel(routes, root_channels, route);
        if let Some(cancel) = route_bash_cancels.remove(&route) {
            cancel.token.cancel();
        }
        apply_route_work_disposition(
            active_tool_calls,
            executor,
            route,
            RouteWorkDisposition::Abandon,
            "root reclaim",
        );
        retry_buffer.remove(&route);
        if let Some(sub) = bg_subs.remove(&route) {
            metrics.record_bg_subscription_ended(&sub.root, &sub.session, route, "root-reclaim");
        }
        bg_wake_pending.remove(&route);
    }
    root_channels.remove(root_id);
    session_identity.retain(|(root, _), _| root != root_id);
    push_buffer.retain(|key, _| &key.root != root_id);
    bg_wake_epoch.retain(|(root, _), _| root != root_id);
    pending_bash_asks.retain(|_, ask| &ask.root != root_id);
    bg_sub_by_session.retain(|(root, _), _| root != root_id);

    log::info!(
        "subc attach: fully forgot deleted root {}; cause=absence_reclaim",
        root_id.as_path().display()
    );
}

#[allow(clippy::too_many_arguments)]
fn submit_due_maintenance_jobs(
    executor: &Arc<Executor>,
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    pending_binds: &HashMap<RouteChannel, PendingBind>,
    bg_sub_by_session: &BgSubsBySession,
    bg_wake_pending: &HashSet<RouteChannel>,
    bg_wake_epoch: &HashMap<(ProjectRootId, String), u64>,
    maintenance_tx: &mpsc::Sender<MaintenanceCompletion>,
    metrics: &Arc<DispatchPathMetrics>,
) {
    let pending_bind_roots = pending_binds
        .values()
        .map(|pending| pending.bind_root_id.clone())
        .collect::<HashSet<_>>();
    let (due_jobs, deferred_jobs) = due_maintenance_jobs(
        live_roots,
        Some(executor),
        bg_sub_by_session,
        bg_wake_pending,
        MAINTENANCE_SUBMIT_BUDGET,
        &pending_bind_roots,
    );
    if deferred_jobs {
        metrics
            .maintenance_budget_deferrals
            .fetch_add(1, Ordering::Relaxed);
    }
    for (root_id, kind) in due_jobs {
        let bg_sessions_to_check = if kind == MaintenanceDrainKind::CompletionDrains {
            bg_sub_by_session
                .iter()
                .filter_map(|((root, session), _)| {
                    if root == &root_id {
                        Some((
                            session.clone(),
                            bg_wake_epoch
                                .get(&(root_id.clone(), session.clone()))
                                .copied()
                                .unwrap_or(0),
                        ))
                    } else {
                        None
                    }
                })
                .collect()
        } else {
            Vec::new()
        };
        submit_maintenance_job(
            executor,
            root_id,
            kind,
            bg_sessions_to_check,
            maintenance_tx,
            metrics,
        );
    }
}

fn should_requiesce_after_maintenance(
    meta: &RootMeta,
    completed_kind: MaintenanceDrainKind,
    bind_pending: bool,
) -> bool {
    meta.unbound_quiesced && completed_kind != MaintenanceDrainKind::Lsp && !bind_pending
}

fn note_maintenance_completion(
    meta: &mut RootMeta,
    requeue_kind: Option<MaintenanceDrainKind>,
    fatal: bool,
    defer_requeue: bool,
) {
    if fatal {
        meta.maintenance_poisoned = true;
    }

    if let Some(kind) = requeue_kind.filter(|_| !meta.maintenance_poisoned && !defer_requeue) {
        meta.maintenance_queued_kinds.push_back(kind);
    }

    meta.maintenance_jobs_in_flight = meta.maintenance_jobs_in_flight.saturating_sub(1);
    meta.maintenance_pending =
        meta.maintenance_jobs_in_flight > 0 || !meta.maintenance_queued_kinds.is_empty();
}

fn route_key(channel: u16, epoch: u32) -> RouteChannel {
    RouteChannel { channel, epoch }
}

fn remove_installed_route(installed_epochs: &mut HashMap<u16, u32>, route: RouteChannel) {
    if installed_epochs.get(&route.channel).copied() == Some(route.epoch) {
        installed_epochs.remove(&route.channel);
    }
}

fn ingress_route_should_be_processed(
    installed_epochs: &HashMap<u16, u32>,
    reclaimed_routes: &ReclaimedRoutes,
    frame: &Frame,
) -> bool {
    if frame.header.channel == 0
        || installed_epochs.get(&frame.header.channel).copied() == Some(frame.header.epoch)
    {
        return true;
    }

    // A late request for a reclaimed root reaches the normal unknown-route
    // handler, which returns the typed `route_not_bound` error. Other stale or
    // never-installed generations remain silent so they cannot affect a newer
    // route or change the protocol's rejected-bind behavior.
    frame.header.ty == FrameType::Request
        && reclaimed_routes.contains(route_key(frame.header.channel, frame.header.epoch))
}

fn bash_elicitation_timeout() -> Duration {
    if cfg!(debug_assertions) {
        if let Ok(raw) = std::env::var("AFT_TEST_SUBC_BASH_ELICITATION_TTL_MS") {
            if let Ok(ms) = raw.parse::<u64>() {
                if ms > 0 {
                    return Duration::from_millis(ms);
                }
            }
        }
    }
    BASH_ELICITATION_TIMEOUT
}

fn allocate_reverse_corr(
    pending_bash_asks: &HashMap<ReverseCorrKey, PendingBashAsk>,
    route: RouteChannel,
    next_corr: &mut u64,
) -> u64 {
    loop {
        let corr = *next_corr;
        *next_corr = (*next_corr).wrapping_add(1).max(1);
        if !pending_bash_asks.contains_key(&ReverseCorrKey { route, corr }) {
            return corr;
        }
    }
}

fn bash_permission_kind_label(kind: &crate::bash_permissions::PermissionKind) -> &'static str {
    match kind {
        crate::bash_permissions::PermissionKind::ExternalDirectory => "external directory",
        crate::bash_permissions::PermissionKind::Bash => "bash",
    }
}

fn bash_elicitation_patterns(asks: &[crate::bash_permissions::PermissionAsk]) -> Vec<String> {
    let mut patterns = Vec::new();
    let mut seen = HashSet::new();
    for ask in asks {
        for pattern in ask.patterns.iter().chain(ask.always.iter()) {
            if seen.insert(pattern.clone()) {
                patterns.push(pattern.clone());
            }
        }
    }
    patterns
}

fn bash_elicitation_message(
    command: &str,
    asks: &[crate::bash_permissions::PermissionAsk],
) -> String {
    let command = command.split_whitespace().collect::<Vec<_>>().join(" ");
    let patterns = bash_elicitation_patterns(asks);
    let pattern_text = if patterns.is_empty() {
        "no matched permission patterns".to_string()
    } else {
        patterns.join(", ")
    };
    let ask_kinds = asks
        .iter()
        .map(|ask| bash_permission_kind_label(&ask.kind))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join(", ");
    if ask_kinds.is_empty() {
        format!("Allow bash command `{command}`? Matched patterns: {pattern_text}")
    } else {
        format!("Allow bash command `{command}`? Matched {ask_kinds} patterns: {pattern_text}")
    }
}

fn bash_elicitation_request_body(
    command: &str,
    asks: &[crate::bash_permissions::PermissionAsk],
) -> Value {
    json!({
        "method": BASH_ELICITATION_CREATE_METHOD,
        "params": {
            "mode": "form",
            "message": bash_elicitation_message(command, asks),
            "requestedSchema": {
                "type": "object",
                "properties": {
                    "decision": {
                        "type": "string",
                        "enum": ["allow", "deny"],
                        "description": "Choose allow to run this bash command once, or deny to block it."
                    }
                },
                "required": ["decision"],
                "additionalProperties": false
            },
            "_meta": {
                "aft": {
                    "tool": "bash",
                    "command": command,
                    "asks": asks
                }
            }
        }
    })
}

fn build_bash_elicitation_request_frame(
    ver: u8,
    route: RouteChannel,
    corr: u64,
    flags: Flags,
    command: &str,
    asks: &[crate::bash_permissions::PermissionAsk],
) -> Result<Frame, SubcError> {
    let body = bash_elicitation_request_body(command, asks);
    Frame::build_with_version(
        ver,
        FrameType::Request,
        flags,
        route.channel,
        route.epoch,
        corr,
        serde_json::to_vec(&body).map_err(SubcError::Json)?,
    )
    .map_err(SubcError::FrameBuild)
}

fn bash_elicitation_reply_is_allow(body: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    flat_bash_elicitation_reply_is_allow(&value) || mcp_bash_elicitation_reply_is_allow(&value)
}

fn flat_bash_elicitation_reply_is_allow(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    object.len() == 1 && object.get("decision").and_then(Value::as_str) == Some("allow")
}

fn mcp_bash_elicitation_reply_is_allow(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    if object.len() != 2 || object.get("action").and_then(Value::as_str) != Some("accept") {
        return false;
    }
    let Some(content) = object.get("content").and_then(Value::as_object) else {
        return false;
    };
    content.len() == 1 && content.get("decision").and_then(Value::as_str) == Some("allow")
}

#[allow(clippy::too_many_arguments)]
async fn settle_pending_bash_ask_denied(
    tx: &WriterSender,
    pending: PendingBashAsk,
    routes: &HashMap<RouteChannel, RouteIdentity>,
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    route_bash_cancels: &mut HashMap<RouteChannel, bash::RouteBashCancel>,
    shutdown: &Arc<Notify>,
    metrics: &DispatchPathMetrics,
) -> Result<(), SubcError> {
    let completion = bash::bash_denied_untrusted_completion(
        pending.route,
        pending.tool_corr,
        pending.tool_flags,
        pending.tool_ver,
        pending.root,
        pending.request_id,
        pending.format_context,
    );
    bash::handle_bash_deferred_completion(
        tx,
        completion,
        routes,
        live_roots,
        route_bash_cancels,
        shutdown,
        metrics,
    )
    .await
}

fn take_pending_bash_asks_for_route(
    pending_bash_asks: &mut HashMap<ReverseCorrKey, PendingBashAsk>,
    route: RouteChannel,
) -> Vec<PendingBashAsk> {
    let keys = pending_bash_asks
        .keys()
        .copied()
        .filter(|key| key.route == route)
        .collect::<Vec<_>>();
    keys.into_iter()
        .filter_map(|key| pending_bash_asks.remove(&key))
        .collect()
}

#[allow(clippy::too_many_arguments)]
async fn settle_pending_bash_asks_for_route(
    tx: &WriterSender,
    pending_bash_asks: &mut HashMap<ReverseCorrKey, PendingBashAsk>,
    route: RouteChannel,
    routes: &HashMap<RouteChannel, RouteIdentity>,
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    route_bash_cancels: &mut HashMap<RouteChannel, bash::RouteBashCancel>,
    shutdown: &Arc<Notify>,
    metrics: &DispatchPathMetrics,
) -> Result<(), SubcError> {
    for pending in take_pending_bash_asks_for_route(pending_bash_asks, route) {
        settle_pending_bash_ask_denied(
            tx,
            pending,
            routes,
            live_roots,
            route_bash_cancels,
            shutdown,
            metrics,
        )
        .await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn settle_all_pending_bash_asks(
    tx: &WriterSender,
    pending_bash_asks: &mut HashMap<ReverseCorrKey, PendingBashAsk>,
    routes: &HashMap<RouteChannel, RouteIdentity>,
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    route_bash_cancels: &mut HashMap<RouteChannel, bash::RouteBashCancel>,
    shutdown: &Arc<Notify>,
    metrics: &DispatchPathMetrics,
) -> Result<(), SubcError> {
    let pending = pending_bash_asks
        .drain()
        .map(|(_, pending)| pending)
        .collect::<Vec<_>>();
    for pending in pending {
        settle_pending_bash_ask_denied(
            tx,
            pending,
            routes,
            live_roots,
            route_bash_cancels,
            shutdown,
            metrics,
        )
        .await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn expire_pending_bash_asks(
    tx: &WriterSender,
    pending_bash_asks: &mut HashMap<ReverseCorrKey, PendingBashAsk>,
    routes: &HashMap<RouteChannel, RouteIdentity>,
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    route_bash_cancels: &mut HashMap<RouteChannel, bash::RouteBashCancel>,
    shutdown: &Arc<Notify>,
    metrics: &DispatchPathMetrics,
) -> Result<(), SubcError> {
    let now = Instant::now();
    let expired = pending_bash_asks
        .iter()
        .filter_map(|(key, pending)| (pending.expires_at <= now).then_some(*key))
        .collect::<Vec<_>>();
    for key in expired {
        if let Some(pending) = pending_bash_asks.remove(&key) {
            log::debug!(
                "subc attach: bash elicitation request {} on route {} expired fail-closed",
                key.corr,
                pending.route
            );
            settle_pending_bash_ask_denied(
                tx,
                pending,
                routes,
                live_roots,
                route_bash_cancels,
                shutdown,
                metrics,
            )
            .await?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_bash_elicitation_reply(
    tx: &WriterSender,
    frame: &Frame,
    pending_bash_asks: &mut HashMap<ReverseCorrKey, PendingBashAsk>,
    routes: &HashMap<RouteChannel, RouteIdentity>,
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    executor: &Arc<Executor>,
    shutdown: &Arc<Notify>,
    bash_deferred_tx: &mpsc::Sender<bash::BashDeferredCompletion>,
    bash_poll_touch_tx: &mpsc::Sender<ProjectRootId>,
    metrics: &Arc<DispatchPathMetrics>,
    route_bash_cancels: &mut HashMap<RouteChannel, bash::RouteBashCancel>,
    dispatch: DispatchFn,
) -> Result<(), SubcError> {
    let key = ReverseCorrKey {
        route: route_key(frame.header.channel, frame.header.epoch),
        corr: frame.header.corr,
    };
    let Some(pending) = pending_bash_asks.remove(&key) else {
        return Ok(());
    };

    if frame.header.ty == FrameType::Response && bash_elicitation_reply_is_allow(&frame.body) {
        // The request deadline is checked BEFORE bash-wait bookkeeping and
        // before any executor submission: an expired permission answer must
        // prove that no bash command started.
        if let Some(deadline) = pending.request_deadline {
            if Instant::now() >= deadline {
                let response = Response::error_with_data(
                    pending.request_id.clone(),
                    "request_deadline_exceeded",
                    "request deadline elapsed during permission elicitation",
                    serde_json::json!({
                        "retryable": false,
                        "phase": "queue",
                    }),
                );
                let completion = bash::bash_deadline_exceeded_completion(
                    pending.route,
                    pending.tool_corr,
                    pending.tool_flags,
                    pending.tool_ver,
                    pending.root,
                    pending.request_id,
                    pending.format_context,
                    response,
                );
                bash::handle_bash_deferred_completion(
                    tx,
                    completion,
                    routes,
                    live_roots,
                    route_bash_cancels,
                    shutdown,
                    metrics,
                )
                .await?;
                return Ok(());
            }
        }
        if routes.contains_key(&key.route) {
            bash::submit_deferred_bash(
                executor,
                bash_deferred_tx,
                bash_poll_touch_tx,
                metrics,
                dispatch,
                pending.root,
                pending.project_root,
                pending.session_id,
                pending.request_id,
                pending.route,
                pending.tool_corr,
                pending.tool_flags,
                pending.tool_ver,
                pending.arguments,
                pending.format_context,
                pending.cancel,
                BindTrust::Untrusted,
                pending.spawn_principal,
                pending.edit_slot_survives,
                Some(pending.grants),
                pending.request_deadline,
            );
            return Ok(());
        }
        log::debug!(
            "subc attach: dropping allowed bash elicitation reply {} for unbound route {}",
            key.corr,
            pending.route
        );
    }

    settle_pending_bash_ask_denied(
        tx,
        pending,
        routes,
        live_roots,
        route_bash_cancels,
        shutdown,
        metrics,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn cancel_pending_bash_ask_for_tool_call(
    tx: &WriterSender,
    pending_bash_asks: &mut HashMap<ReverseCorrKey, PendingBashAsk>,
    route: RouteChannel,
    tool_corr: u64,
    routes: &HashMap<RouteChannel, RouteIdentity>,
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    route_bash_cancels: &mut HashMap<RouteChannel, bash::RouteBashCancel>,
    shutdown: &Arc<Notify>,
    metrics: &DispatchPathMetrics,
) -> Result<(), SubcError> {
    let keys = pending_bash_asks
        .iter()
        .filter_map(|(key, pending)| {
            (key.route == route && pending.tool_corr == tool_corr).then_some(*key)
        })
        .collect::<Vec<_>>();
    for key in keys {
        if let Some(pending) = pending_bash_asks.remove(&key) {
            settle_pending_bash_ask_denied(
                tx,
                pending,
                routes,
                live_roots,
                route_bash_cancels,
                shutdown,
                metrics,
            )
            .await?;
        }
    }
    Ok(())
}

fn remove_root_channel(
    root_channels: &mut HashMap<ProjectRootId, HashSet<RouteChannel>>,
    root: &ProjectRootId,
    channel: RouteChannel,
) {
    let remove_root = if let Some(channels) = root_channels.get_mut(root) {
        channels.remove(&channel);
        channels.is_empty()
    } else {
        false
    };
    if remove_root {
        root_channels.remove(root);
    }
}

fn remove_route_channel(
    routes: &mut HashMap<RouteChannel, RouteIdentity>,
    root_channels: &mut HashMap<ProjectRootId, HashSet<RouteChannel>>,
    channel: RouteChannel,
) -> Option<RouteIdentity> {
    let removed = routes.remove(&channel);
    if let Some(identity) = &removed {
        remove_root_channel(root_channels, &identity.root, channel);
    }
    removed
}

fn insert_route_channel(
    routes: &mut HashMap<RouteChannel, RouteIdentity>,
    root_channels: &mut HashMap<ProjectRootId, HashSet<RouteChannel>>,
    channel: RouteChannel,
    identity: RouteIdentity,
) {
    if let Some(previous) = routes.insert(channel, identity.clone()) {
        remove_root_channel(root_channels, &previous.root, channel);
    }
    root_channels
        .entry(identity.root.clone())
        .or_default()
        .insert(channel);
}

fn insert_bg_subscription_index(
    bg_sub_by_session: &mut BgSubsBySession,
    root: ProjectRootId,
    session: String,
    channel: RouteChannel,
) {
    bg_sub_by_session
        .entry((root, session))
        .or_default()
        .insert(channel);
}

fn remove_bg_subscription_index(
    bg_sub_by_session: &mut BgSubsBySession,
    channel: RouteChannel,
    identity: Option<&RouteIdentity>,
) {
    if let Some(identity) = identity {
        let key = (identity.root.clone(), identity.session.clone());
        let remove_key = bg_sub_by_session.get_mut(&key).is_some_and(|channels| {
            channels.remove(&channel);
            channels.is_empty()
        });
        if remove_key {
            bg_sub_by_session.remove(&key);
        }
    } else {
        bg_sub_by_session.retain(|_, channels| {
            channels.remove(&channel);
            !channels.is_empty()
        });
    }
}

fn route_removal_will_quiesce_root(
    root: &ProjectRootId,
    route: RouteChannel,
    root_channels: &HashMap<ProjectRootId, HashSet<RouteChannel>>,
    has_pending_bind: bool,
    replacement_root: Option<&ProjectRootId>,
) -> bool {
    let removes_last_route = root_channels
        .get(root)
        .is_some_and(|channels| channels.len() == 1 && channels.contains(&route));
    removes_last_route && !has_pending_bind && replacement_root != Some(root)
}

fn should_quiesce_removed_root(
    root: &ProjectRootId,
    root_channels: &HashMap<ProjectRootId, HashSet<RouteChannel>>,
    has_pending_bind: bool,
    replacement_root: Option<&ProjectRootId>,
) -> bool {
    !root_channels.contains_key(root) && !has_pending_bind && replacement_root != Some(root)
}

async fn end_bg_subscription(
    writer_tx: &WriterSender,
    metrics: &DispatchPathMetrics,
    bg_subs: &mut HashMap<RouteChannel, BgSub>,
    bg_sub_by_session: &mut BgSubsBySession,
    bg_wake_pending: &mut HashSet<RouteChannel>,
    channel: RouteChannel,
    identity: Option<&RouteIdentity>,
    cause: &str,
) -> Result<(), SubcError> {
    if let Some(sub) = bg_subs.remove(&channel) {
        bg_wake_pending.remove(&channel);
        remove_bg_subscription_index(bg_sub_by_session, channel, identity);
        metrics.record_bg_subscription_ended(&sub.root, &sub.session, channel, cause);
        push::send_reliable_bg_stream_end(writer_tx, metrics, channel, &sub).await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn teardown_installed_route(
    tx: &WriterSender,
    metrics: &DispatchPathMetrics,
    executor: &Arc<Executor>,
    channel: RouteChannel,
    cancellation_reason: &str,
    replacement_root: Option<&ProjectRootId>,
    installed_route_epochs: &mut HashMap<u16, u32>,
    routes: &mut HashMap<RouteChannel, RouteIdentity>,
    root_channels: &mut HashMap<ProjectRootId, HashSet<RouteChannel>>,
    bg_subs: &mut HashMap<RouteChannel, BgSub>,
    bg_sub_by_session: &mut BgSubsBySession,
    bg_wake_pending: &mut HashSet<RouteChannel>,
    pending_bash_asks: &mut HashMap<ReverseCorrKey, PendingBashAsk>,
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    route_bash_cancels: &mut HashMap<RouteChannel, bash::RouteBashCancel>,
    active_tool_calls: &ActiveToolCalls,
    pending_responses: &mut PendingSubcResponses,
    pending_binds: &mut HashMap<RouteChannel, PendingBind>,
    retry_buffer: &mut RetryBuffer,
    push_buffer: &mut HashMap<push::ReplayKey, VecDeque<PushFrame>>,
    shutdown: &Arc<Notify>,
    tool_response_body_limit: usize,
    lifecycle_probe: Option<&SubcTestLifecycleProbe>,
) -> Result<(), SubcError> {
    remove_installed_route(installed_route_epochs, channel);
    let bg_end_cause = match cancellation_reason {
        "Goodbye" => "goodbye",
        "higher-epoch RouteBind" => "higher-epoch",
        other => other,
    };
    end_bg_subscription(
        tx,
        metrics,
        bg_subs,
        bg_sub_by_session,
        bg_wake_pending,
        channel,
        routes.get(&channel),
        bg_end_cause,
    )
    .await?;
    settle_pending_bash_asks_for_route(
        tx,
        pending_bash_asks,
        channel,
        routes,
        live_roots,
        route_bash_cancels,
        shutdown,
        metrics,
    )
    .await?;
    if let Some(cancel) = route_bash_cancels.remove(&channel) {
        cancel.token.cancel();
    }
    for resolved in pending_responses.drain_route(channel, executor) {
        deliver_resolved_subc_response(
            tx,
            resolved,
            routes,
            live_roots,
            executor.as_ref(),
            active_tool_calls,
            shutdown,
            metrics,
            tool_response_body_limit,
        )
        .await?;
    }
    // Closing a route does not abandon its session: reliable responses are
    // retained for detach/rebind replay. Cancellation belongs to explicit
    // Cancel frames, whole-connection teardown, or root reclamation.
    apply_route_work_disposition(
        active_tool_calls,
        executor,
        channel,
        RouteWorkDisposition::RetainForReplay,
        cancellation_reason,
    );
    if let Some(pending) = pending_binds.get_mut(&channel) {
        pending.cancelled = true;
        let outcome = executor.cancel_job(&pending.bind_root_id, &pending.cancellation);
        log::debug!(
            "subc attach: cancelled pending RouteBind for route {} on {cancellation_reason} (configure job: {outcome:?})",
            channel.channel
        );
    }
    let migrated = push::migrate_retry_buffer_to_push_buffer(retry_buffer, channel, push_buffer);
    if let Some(identity) = routes.get(&channel) {
        let has_pending_bind = pending_binds
            .values()
            .any(|pending| pending.bind_root_id == identity.root);
        if route_removal_will_quiesce_root(
            &identity.root,
            channel,
            root_channels,
            has_pending_bind,
            replacement_root,
        ) {
            if let Some(ctx) = executor.actor_context(&identity.root) {
                // Fence deferred admissions before the final route disappears
                // from the loop-owned routing tables.
                ctx.mark_subc_unbound();
            }
        }
    }
    // This test-only delay lets the lifecycle probe verify that a queued rebind
    // cannot run before the route is removed and a completion is recorded for replay.
    delay_route_detach_for_test(lifecycle_probe).await;
    if let Some(identity) = remove_route_channel(routes, root_channels, channel) {
        if let Some(probe) = lifecycle_probe {
            probe.route_detached(channel, &identity.session);
        }
        let session_still_routed = routes
            .values()
            .any(|route| route.root == identity.root && route.session == identity.session);
        if !session_still_routed {
            if let Some(ctx) = executor.actor_context(&identity.root) {
                ctx.hashline_bindings()
                    .teardown(identity.root.as_path(), &identity.session);
            }
        }
        if migrated > 0 {
            log::debug!(
                "subc attach: migrated {migrated} retry-buffered reliable Push frame(s) from route {} into detach replay",
                channel.channel
            );
        }
        if let Some(meta) = live_roots.get_mut(&identity.root) {
            let idle_for = meta.last_touched.elapsed();
            meta.note_activity();
            log::debug!(
                "subc attach: route {} torn down for root {} harness {} session {} (last touched {:?} ago)",
                channel.channel,
                identity.root.as_path().display(),
                identity.harness,
                identity.session,
                idle_for
            );
        } else {
            log::debug!(
                "subc attach: route {} torn down for root {} harness {} session {}",
                channel.channel,
                identity.root.as_path().display(),
                identity.harness,
                identity.session
            );
        }
        let has_pending_bind = pending_binds
            .values()
            .any(|pending| pending.bind_root_id == identity.root);
        if should_quiesce_removed_root(
            &identity.root,
            root_channels,
            has_pending_bind,
            replacement_root,
        ) {
            quiesce_unbound_root(&identity.root, live_roots, executor);
        }
    } else {
        if migrated > 0 {
            log::debug!(
                "subc attach: migrated {migrated} retry-buffered reliable Push frame(s) from unbound route {} into detach replay",
                channel.channel
            );
        }
        log::debug!("subc attach: unbound route {} torn down", channel.channel);
    }
    Ok(())
}

async fn delay_route_detach_for_test(lifecycle_probe: Option<&SubcTestLifecycleProbe>) {
    if lifecycle_probe.is_none() {
        return;
    }
    let Some(delay) = std::env::var("AFT_TEST_SUBC_ROUTE_DETACH_DELAY_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
    else {
        return;
    };
    tokio::time::sleep(Duration::from_millis(delay)).await;
}

fn remember_session_identity(
    session_identity: &mut HashMap<(ProjectRootId, String), RetainedSessionIdentity>,
    identity: &RouteIdentity,
) {
    let key = (identity.root.clone(), identity.session.clone());
    if matches!(identity.trust, BindTrust::Untrusted)
        && session_identity
            .get(&key)
            .is_some_and(|retained| matches!(retained.trust, BindTrust::FirstParty))
    {
        return;
    }

    // Retained after route Goodbye so reliable session-scoped frames emitted while
    // the session is detached can still be keyed by the full (root,harness,session)
    // replay triple. Untrusted binds never overwrite a retained first-party
    // session identity, because bash completion replay is an observation channel.
    session_identity.insert(
        key,
        RetainedSessionIdentity {
            harness: identity.harness.clone(),
            trust: identity.trust,
        },
    );
}

fn replay_key_for_session(
    session_identity: &HashMap<(ProjectRootId, String), RetainedSessionIdentity>,
    root: &ProjectRootId,
    session: &str,
) -> Option<(push::ReplayKey, BindTrust)> {
    let retained = session_identity.get(&(root.clone(), session.to_string()))?;
    Some((
        push::ReplayKey {
            root: root.clone(),
            harness: retained.harness.clone(),
            session: session.to_string(),
        },
        retained.trust,
    ))
}
/// Sync command dispatch, passed in from `main` (the binary owns the command
/// table). Invoked only inside executor jobs in subc mode.
pub type DispatchFn = fn(RawRequest, &AppContext) -> Response;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ModuleLoopExit {
    Graceful,
    SkipSearchFlush,
}

/// Entry point for `aft --subc <connection-file>`. Synchronous on the outside;
/// owns an isolated current-thread tokio runtime for the async transport.
/// Returns `Err` (fail-loud) on any connect/auth/protocol failure — we never
/// fall back to the standalone loop, to avoid split-brain index state.
pub fn run_subc_mode(
    connection_file_path: &Path,
    ctx: Arc<AppContext>,
    executor: Arc<Executor>,
    dispatch: DispatchFn,
    user_config_path: Option<PathBuf>,
) -> Result<(), SubcError> {
    // Production NEVER allows non-manifest tool names on route channels: AFT
    // fails closed and does not trust subc to enforce the manifest. The
    // test-only harness sets this through `run_subc_mode_for_test`.
    run_subc_mode_inner(
        connection_file_path,
        ctx,
        executor,
        dispatch,
        user_config_path,
        false,
        MAX_FRAME_BODY_LEN as usize,
        None,
    )
}

fn run_subc_mode_inner(
    connection_file_path: &Path,
    ctx: Arc<AppContext>,
    executor: Arc<Executor>,
    dispatch: DispatchFn,
    user_config_path: Option<PathBuf>,
    allow_native_passthrough: bool,
    tool_response_body_limit: usize,
    lifecycle_probe: Option<SubcTestLifecycleProbe>,
) -> Result<(), SubcError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(SubcError::Runtime)?;

    let executor_for_loop = Arc::clone(&executor);
    let loop_result = runtime.block_on(async move {
        let shared_app = ctx.app();
        drop(ctx);
        let stream =
            connect_and_authenticate(connection_file_path, lifecycle_probe.as_ref()).await?;
        log::info!(
            "subc attach: authenticated to daemon via {}",
            connection_file_path.display()
        );
        let (read_half, write_half) = tokio::io::split(stream);
        run_module_loop(
            read_half,
            write_half,
            connection_file_path,
            shared_app,
            executor_for_loop,
            dispatch,
            user_config_path,
            allow_native_passthrough,
            tool_response_body_limit,
            lifecycle_probe,
        )
        .await
    });

    let actor_contexts = executor.actor_contexts();
    if matches!(loop_result, Ok(ModuleLoopExit::Graceful)) {
        // EOF/Goodbye teardown flushes each root's index deltas and queued
        // callgraph refreshes. Fatal/panic teardown skips this best-effort work.
        flush_actor_indexes_on_graceful_shutdown(&actor_contexts);
    }
    for actor_ctx in &actor_contexts {
        actor_ctx.lsp().shutdown_all();
        actor_ctx.bash_background().detach();
    }

    loop_result.map(|_| ())
}

fn flush_actor_indexes_on_graceful_shutdown(actor_contexts: &[Arc<AppContext>]) {
    for actor_ctx in actor_contexts {
        let _ = actor_ctx.flush_search_index_on_graceful_shutdown();
    }
    let _ = crate::callgraph_store::flush_callgraph_store_refreshes_on_graceful_shutdown();
}

/// Test-only entry that enables the non-manifest native-command passthrough on
/// route channels. Integration tests drive synthetic native commands (`glob`,
/// `callers`, `subc_test_echo_session`, …) through the executor to exercise
/// mechanics; production callers use [`run_subc_mode`], which fails closed.
#[doc(hidden)]
pub fn run_subc_mode_for_test(
    connection_file_path: &Path,
    ctx: Arc<AppContext>,
    executor: Arc<Executor>,
    dispatch: DispatchFn,
    user_config_path: Option<PathBuf>,
) -> Result<(), SubcError> {
    run_subc_mode_inner(
        connection_file_path,
        ctx,
        executor,
        dispatch,
        user_config_path,
        true,
        MAX_FRAME_BODY_LEN as usize,
        None,
    )
}

/// Test-only entry that observes detach/rebind lifecycle milestones.
#[doc(hidden)]
pub fn run_subc_mode_for_test_with_lifecycle_probe(
    connection_file_path: &Path,
    ctx: Arc<AppContext>,
    executor: Arc<Executor>,
    dispatch: DispatchFn,
    user_config_path: Option<PathBuf>,
    lifecycle_probe: SubcTestLifecycleProbe,
) -> Result<(), SubcError> {
    run_subc_mode_inner(
        connection_file_path,
        ctx,
        executor,
        dispatch,
        user_config_path,
        true,
        MAX_FRAME_BODY_LEN as usize,
        Some(lifecycle_probe),
    )
}

/// Test-only entry that lowers the effective tool-response body limit without
/// allocating a 64 MiB fixture. The fixed fallback envelope needs 4 KiB of room.
#[doc(hidden)]
pub fn run_subc_mode_for_test_with_response_body_limit(
    connection_file_path: &Path,
    ctx: Arc<AppContext>,
    executor: Arc<Executor>,
    dispatch: DispatchFn,
    user_config_path: Option<PathBuf>,
    tool_response_body_limit: usize,
) -> Result<(), SubcError> {
    assert!((4 * 1_024..=MAX_FRAME_BODY_LEN as usize).contains(&tool_response_body_limit));
    run_subc_mode_inner(
        connection_file_path,
        ctx,
        executor,
        dispatch,
        user_config_path,
        true,
        tool_response_body_limit,
        None,
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AttachErrorClass {
    Transient,
    Permanent,
}

impl fmt::Display for AttachErrorClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transient => f.write_str("transient"),
            Self::Permanent => f.write_str("permanent"),
        }
    }
}

#[derive(Clone, Copy)]
struct AttachRetryPolicy {
    budget: Duration,
    initial_backoff: Duration,
    max_backoff: Duration,
    jitter_percent: u64,
}

const ATTACH_RETRY_POLICY: AttachRetryPolicy = AttachRetryPolicy {
    budget: ATTACH_RETRY_BUDGET,
    initial_backoff: ATTACH_RETRY_INITIAL_BACKOFF,
    max_backoff: ATTACH_RETRY_MAX_BACKOFF,
    jitter_percent: ATTACH_RETRY_JITTER_PERCENT,
};

/// Retry only failures that can be caused by a daemon bounce or an interrupted
/// handshake. Protocol and credential failures are permanent for this process.
fn classify_attach_error(error: &SubcError) -> AttachErrorClass {
    let transient = match error {
        SubcError::Connect { source, .. } => is_transient_attach_io(source.kind()),
        SubcError::Auth { source, .. } => match source {
            subc_transport::AuthError::Timeout { .. }
            | subc_transport::AuthError::UnexpectedEof { .. } => true,
            subc_transport::AuthError::Io { source, .. } => is_transient_attach_io(source.kind()),
            _ => false,
        },
        _ => false,
    };
    if transient {
        AttachErrorClass::Transient
    } else {
        AttachErrorClass::Permanent
    }
}

fn is_transient_attach_io(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::TimedOut
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::UnexpectedEof
    )
}

/// Read the connection file → resolve the first endpoint → TCP connect → HMAC
/// handshake. Transient initial-attach failures retry on fresh sockets and reread
/// the file so a daemon bounce can publish a new endpoint or authentication key.
async fn connect_and_authenticate(
    connection_file_path: &Path,
    lifecycle_probe: Option<&SubcTestLifecycleProbe>,
) -> Result<TcpStream, SubcError> {
    connect_and_authenticate_with_policy(connection_file_path, ATTACH_RETRY_POLICY, lifecycle_probe)
        .await
}

async fn connect_and_authenticate_with_policy(
    connection_file_path: &Path,
    policy: AttachRetryPolicy,
    lifecycle_probe: Option<&SubcTestLifecycleProbe>,
) -> Result<TcpStream, SubcError> {
    let started_at = Instant::now();
    let deadline = started_at + policy.budget;
    let mut attempt = 0_u32;
    let mut backoff = policy.initial_backoff;
    let mut history = Vec::new();

    loop {
        attempt = attempt.saturating_add(1);
        let error = match connect_and_authenticate_once(connection_file_path, deadline).await {
            Ok(stream) => return Ok(stream),
            Err(error) => error,
        };
        let class = classify_attach_error(&error);
        let will_retry = class != AttachErrorClass::Permanent;
        if let Some(probe) = lifecycle_probe {
            probe.attach_decision(attempt, will_retry);
        }
        let error_text = error.to_string().lines().collect::<Vec<_>>().join(" ");
        history.push(format!("attempt {attempt} [{class}]: {error_text}"));

        if !will_retry {
            log_attach_final_failure(started_at.elapsed(), &history);
            return Err(error);
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            log_attach_final_failure(started_at.elapsed(), &history);
            return Err(error);
        }

        let delay = jittered_attach_delay(backoff, policy.jitter_percent, attempt).min(remaining);
        log::info!(
            "subc attach retry: attempt {attempt} failed; error_class={class}; error={error_text}; next_delay={delay:?}"
        );
        tokio::time::sleep(delay).await;

        if Instant::now() >= deadline {
            log_attach_final_failure(started_at.elapsed(), &history);
            return Err(error);
        }
        backoff = backoff.saturating_mul(2).min(policy.max_backoff);
    }
}

fn jittered_attach_delay(base: Duration, jitter_percent: u64, attempt: u32) -> Duration {
    let jitter_percent = jitter_percent.min(100);
    if jitter_percent == 0 {
        return base;
    }

    let mut random_bytes = [0_u8; 8];
    let random = if getrandom::fill(&mut random_bytes).is_ok() {
        u64::from_le_bytes(random_bytes)
    } else {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        u64::from(timestamp) ^ u64::from(attempt)
    };
    let span = jitter_percent.saturating_mul(2).saturating_add(1);
    let multiplier_percent = 100 - jitter_percent + random % span;
    let base_millis = u64::try_from(base.as_millis()).unwrap_or(u64::MAX);
    Duration::from_millis(base_millis.saturating_mul(multiplier_percent) / 100)
}

fn log_attach_final_failure(elapsed: Duration, history: &[String]) {
    log::error!(
        "subc initial attach failed after {} attempt(s) in {elapsed:?}; attempt history: {}",
        history.len(),
        history.join(" | ")
    );
}

async fn connect_and_authenticate_once(
    connection_file_path: &Path,
    deadline: Instant,
) -> Result<TcpStream, SubcError> {
    // This read intentionally lives inside the per-attempt function. The daemon
    // publishes connection files atomically and may change both port and key.
    let conn = connection_file::read_for_client(connection_file_path).map_err(|source| {
        SubcError::ConnectionFile {
            path: connection_file_path.to_path_buf(),
            source,
        }
    })?;

    let endpoint = conn
        .endpoints
        .first()
        .ok_or_else(|| SubcError::NoEndpoint {
            path: connection_file_path.to_path_buf(),
        })?;
    let endpoint_label = format!("{}:{}", endpoint.host, endpoint.port);
    let ip = endpoint
        .host
        .parse::<IpAddr>()
        .map_err(|_| SubcError::InvalidEndpoint {
            path: connection_file_path.to_path_buf(),
            endpoint: endpoint_label.clone(),
        })?;
    let addr = SocketAddr::new(ip, endpoint.port);

    let connect_budget = deadline.saturating_duration_since(Instant::now());
    let mut stream = tokio::time::timeout(connect_budget, TcpStream::connect(addr))
        .await
        .map_err(|_| SubcError::Connect {
            endpoint: endpoint_label.clone(),
            source: io::Error::new(
                io::ErrorKind::TimedOut,
                "initial subc attach retry budget elapsed during TCP connect",
            ),
        })?
        .map_err(|source| SubcError::Connect {
            endpoint: endpoint_label.clone(),
            source,
        })?;
    stream
        .set_nodelay(true)
        .map_err(|source| SubcError::Connect {
            endpoint: endpoint_label.clone(),
            source,
        })?;

    let auth_budget = AUTH_DEADLINE.min(deadline.saturating_duration_since(Instant::now()));
    authenticate_client(&mut stream, &conn, auth_budget)
        .await
        .map_err(|source| SubcError::Auth {
            endpoint: endpoint_label,
            source,
        })?;

    Ok(stream)
}

#[allow(clippy::too_many_arguments)]
async fn process_route_bind_completion(
    writer_tx: &WriterSender,
    completion: RouteBindCompletion,
    routes: &mut HashMap<RouteChannel, RouteIdentity>,
    root_channels: &mut HashMap<ProjectRootId, HashSet<RouteChannel>>,
    session_identity: &mut HashMap<(ProjectRootId, String), RetainedSessionIdentity>,
    push_buffer: &mut HashMap<push::ReplayKey, VecDeque<PushFrame>>,
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    pending_binds: &mut HashMap<RouteChannel, PendingBind>,
    installed_route_epochs: &mut HashMap<u16, u32>,
    executor: &Arc<Executor>,
    standing_actor: &standing::StandingActor,
    shutdown: &Arc<Notify>,
    metrics: &Arc<DispatchPathMetrics>,
    lifecycle_probe: Option<&SubcTestLifecycleProbe>,
) -> Result<(), SubcError> {
    decrement_counted_channel(&metrics.control_completion_queued);
    handle_route_bind_completion(
        writer_tx,
        completion,
        routes,
        root_channels,
        session_identity,
        push_buffer,
        live_roots,
        pending_binds,
        installed_route_epochs,
        executor,
        standing_actor,
        shutdown,
        metrics,
        lifecycle_probe,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn drain_pending_route_bind_completions(
    control_completion_rx: &mut mpsc::Receiver<RouteBindCompletion>,
    writer_tx: &WriterSender,
    routes: &mut HashMap<RouteChannel, RouteIdentity>,
    root_channels: &mut HashMap<ProjectRootId, HashSet<RouteChannel>>,
    session_identity: &mut HashMap<(ProjectRootId, String), RetainedSessionIdentity>,
    push_buffer: &mut HashMap<push::ReplayKey, VecDeque<PushFrame>>,
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    pending_binds: &mut HashMap<RouteChannel, PendingBind>,
    installed_route_epochs: &mut HashMap<u16, u32>,
    executor: &Arc<Executor>,
    standing_actor: &standing::StandingActor,
    shutdown: &Arc<Notify>,
    metrics: &Arc<DispatchPathMetrics>,
    lifecycle_probe: Option<&SubcTestLifecycleProbe>,
) -> Result<usize, SubcError> {
    let mut drained = 0;
    while let Ok(completion) = control_completion_rx.try_recv() {
        process_route_bind_completion(
            writer_tx,
            completion,
            routes,
            root_channels,
            session_identity,
            push_buffer,
            live_roots,
            pending_binds,
            installed_route_epochs,
            executor,
            standing_actor,
            shutdown,
            metrics,
            lifecycle_probe,
        )
        .await?;
        drained += 1;
    }
    Ok(drained)
}

/// ModuleHello → HelloAck → control/route loop. Runs until the daemon closes
/// the connection (EOF), sends channel-0 Goodbye, or a fatal mutating executor
/// response requests whole-connection teardown.
async fn run_module_loop<R, W>(
    mut read: R,
    mut write: W,
    connection_file_path: &Path,
    shared_app: Arc<App>,
    executor: Arc<Executor>,
    dispatch: DispatchFn,
    user_config_path: Option<PathBuf>,
    allow_native_passthrough: bool,
    tool_response_body_limit: usize,
    lifecycle_probe: Option<SubcTestLifecycleProbe>,
) -> Result<ModuleLoopExit, SubcError>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    // ModuleHello: register as a tool provider and advertise the supported control-plane operations.
    // Echo the one-time launch nonce the daemon injected via SUBC_LAUNCH_NONCE so a
    // reserved module_id's HELLO is accepted; absent for non-reserved/self-connect.
    let hello = ModuleHelloBody {
        manifest: build_manifest(),
        protocol_ver: PROTOCOL_VERSION,
        control_ops: control_ops(),
        launch_nonce: std::env::var("SUBC_LAUNCH_NONCE").ok(),
    };
    let hello_frame = Frame::build(
        FrameType::Hello,
        control_flags(),
        0,
        0,
        HELLO_CORR,
        serde_json::to_vec(&hello).map_err(SubcError::Json)?,
    )
    .map_err(SubcError::FrameBuild)?;
    write_frame(&mut write, &hello_frame)
        .await
        .map_err(SubcError::FrameIo)?;

    // Expect HelloAck (registered) or a channel-0 Error (manifest/version reject).
    match read_frame(&mut read).await.map_err(SubcError::FrameIo)? {
        None => return Err(SubcError::ClosedBeforeHelloAck),
        Some(frame) => match frame.header.ty {
            FrameType::HelloAck => {
                log::info!("subc attach: registered (HelloAck received)");
            }
            FrameType::Error => {
                let body = serde_json::from_slice::<ErrorBody>(&frame.body).ok();
                return Err(SubcError::HelloRejected { body });
            }
            other => return Err(SubcError::UnexpectedFrame { ty: other }),
        },
    }

    let dispatch_path_metrics = Arc::new(DispatchPathMetrics::new());
    let (writer_tx, writer_rx) = mpsc::channel::<WriterFrame>(WRITER_QUEUE_CAPACITY);
    let writer_task = spawn_writer_task(write, writer_rx, Arc::clone(&dispatch_path_metrics));
    // `read_frame` is NOT cancellation-safe, so it must never sit directly inside
    // the `select!` below: a drain-interval tick (or shutdown) firing while a
    // frame is mid-transit would drop the partially-consumed bytes and desync the
    // stream (the next read would parse a body byte as a frame header). A
    // dedicated reader task owns the socket, reads whole frames sequentially, and
    // forwards them over a channel; the loop selects on the cancel-safe `recv()`.
    let (control_reader_tx, mut control_reader_rx) =
        mpsc::channel::<Result<DecodedFrame, SubcError>>(32);
    let (data_reader_tx, mut data_reader_rx) =
        mpsc::channel::<Result<DecodedFrame, SubcError>>(256);
    let reader_task = spawn_reader_task(read, control_reader_tx, data_reader_tx);
    let mut reader_lane = PrioritizedFrameLane::default();
    let shutdown = Arc::new(Notify::new());
    // Drain-tick deadline is tracked manually and checked at the TOP of every
    // loop turn rather than as an Interval select arm: the select below is
    // `biased` (bind completions first), and biased polling means a saturated
    // higher arm (sustained lossy push traffic keeps lossy_rx always-ready)
    // would starve every arm below it, including a timer arm — leaving
    // backpressured reliable frames parked in the retry buffer past their
    // delivery deadline. The pre-turn check cannot be starved by arm order;
    // the sleep_until arm below only exists to wake an otherwise-idle loop.
    let mut next_drain_at = tokio::time::Instant::now() + DRAIN_TICK_PERIOD;
    let mut next_maintenance_at = next_drain_at;
    let standing_actor =
        standing::StandingActor::new(Arc::clone(&shared_app), Arc::clone(&executor));
    // Startup reconciliation is intentionally direct; subsequent passes use
    // this existing maintenance timer arm and never create a standing timer.
    standing_actor.reconcile_at_startup();
    let mut next_standing_pass_at = tokio::time::Instant::now();
    // Rate-limit stamp for detached allocator slack scans. The maintenance
    // tick performs only a cheap cadence comparison on the transport thread.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let mut last_slack_scan: Option<std::time::Instant> = None;
    let (maintenance_tx, mut maintenance_rx) = mpsc::channel::<MaintenanceCompletion>(256);
    let (bash_deferred_tx, mut bash_deferred_rx) =
        mpsc::channel::<bash::BashDeferredCompletion>(256);
    let (deferred_response_tx, mut deferred_response_rx) =
        mpsc::unbounded_channel::<PendingSubcResponse>();
    let (bash_poll_touch_tx, mut bash_poll_touch_rx) = mpsc::channel::<ProjectRootId>(256);
    let (control_completion_tx, mut control_completion_rx) =
        mpsc::channel::<RouteBindCompletion>(256);
    let (lossy_tx, mut lossy_rx) = mpsc::channel::<LossyPushEnvelope>(1024);
    let lossy_overflow = Arc::new(push::LossyOverflow::default());
    let lossy_seq = Arc::new(AtomicU64::new(0));
    let (reliable_tx, mut reliable_rx) = mpsc::unbounded_channel::<PushEnvelope>();
    let (fleet_status_client, fleet_status_task) =
        spawn_fleet_status_dial(connection_file_path, 64);
    let push_senders = PushSenders {
        lossy_tx,
        reliable_tx,
        lossy_overflow: Arc::clone(&lossy_overflow),
        lossy_seq,
        fleet_status_client: fleet_status_client.clone(),
    };
    let connection_cancel = PersistentCancelSignal::new();
    let mut installed_route_epochs: HashMap<u16, u32> = HashMap::new();
    let mut routes: HashMap<RouteChannel, RouteIdentity> = HashMap::new();
    let mut bg_subs: HashMap<RouteChannel, BgSub> = HashMap::new();
    let mut bg_sub_by_session: BgSubsBySession = HashMap::new();
    let mut bg_wake_pending: HashSet<RouteChannel> = HashSet::new();
    let mut bg_wake_epoch: HashMap<(ProjectRootId, String), u64> = HashMap::new();
    let mut root_channels: HashMap<ProjectRootId, HashSet<RouteChannel>> = HashMap::new();
    let mut session_identity: HashMap<(ProjectRootId, String), RetainedSessionIdentity> =
        HashMap::new();
    let mut push_buffer: HashMap<push::ReplayKey, VecDeque<PushFrame>> = HashMap::new();
    let mut retry_buffer: RetryBuffer = HashMap::new();
    let mut reclaimed_routes = ReclaimedRoutes::default();
    let mut completed_tasks = push::CompletedTaskIds::default();
    let mut live_roots: HashMap<ProjectRootId, RootMeta> = HashMap::new();
    let mut pending_binds: HashMap<RouteChannel, PendingBind> = HashMap::new();
    let mut pending_bash_asks: HashMap<ReverseCorrKey, PendingBashAsk> = HashMap::new();
    let mut next_bash_ask_corr: u64 = 1;
    let mut route_bash_cancels: HashMap<RouteChannel, bash::RouteBashCancel> = HashMap::new();
    let active_tool_calls: ActiveToolCalls = Arc::new(StdMutex::new(HashMap::new()));
    let pending_deferred_setups = Arc::new(AtomicUsize::new(0));
    let mut pending_responses = PendingSubcResponses::default();
    let health_rollup_cache = HealthRollupCache::new();
    health_rollup_cache.refresh(&executor, &shared_app);
    let mut next_health_rollup_at = tokio::time::Instant::now() + HEALTH_ROLLUP_TTL;

    let loop_result: Result<ModuleLoopExit, SubcError> = 'module_loop: loop {
        shared_app.set_open_route_count(routes.len());
        if tokio::time::Instant::now() >= next_health_rollup_at {
            health_rollup_cache.refresh(&executor, &shared_app);
            next_health_rollup_at = tokio::time::Instant::now() + HEALTH_ROLLUP_TTL;
        }
        crate::logging::perf_tick(Some(&executor));
        dispatch_path_metrics.mark_frame_loop_tick();
        let ready_inspects = pending_responses.poll_ready(executor.as_ref());
        for resolved in ready_inspects {
            if let Err(error) = deliver_resolved_subc_response(
                &writer_tx,
                resolved,
                &routes,
                &mut live_roots,
                executor.as_ref(),
                &active_tool_calls,
                &shutdown,
                &dispatch_path_metrics,
                tool_response_body_limit,
            )
            .await
            {
                break 'module_loop Err(error);
            }
        }
        if let Err(error) = expire_pending_bash_asks(
            &writer_tx,
            &mut pending_bash_asks,
            &routes,
            &mut live_roots,
            &mut route_bash_cancels,
            &shutdown,
            &dispatch_path_metrics,
        )
        .await
        {
            break Err(error);
        }

        // RouteBind completions are control-plane unblockers. Drain any completed
        // binds before entering other branch work so Push and maintenance bursts
        // can only add one loop-turn of latency.
        match drain_pending_route_bind_completions(
            &mut control_completion_rx,
            &writer_tx,
            &mut routes,
            &mut root_channels,
            &mut session_identity,
            &mut push_buffer,
            &mut live_roots,
            &mut pending_binds,
            &mut installed_route_epochs,
            &executor,
            &standing_actor,
            &shutdown,
            &dispatch_path_metrics,
            lifecycle_probe.as_ref(),
        )
        .await
        {
            Ok(drained) => {
                if drained > 0 {
                    next_maintenance_at = tokio::time::Instant::now() + DRAIN_TICK_PERIOD;
                    health_rollup_cache.refresh(&executor, &shared_app);
                    next_health_rollup_at = tokio::time::Instant::now() + HEALTH_ROLLUP_TTL;
                }
            }
            Err(error) => break Err(error),
        }

        if tokio::time::Instant::now() >= next_drain_at {
            push::emit_bg_event_wakes(
                &writer_tx,
                &dispatch_path_metrics,
                &bg_subs,
                &mut bg_wake_pending,
            );
            warn_slow_pending_binds(&mut pending_binds, &executor);
            warn_slow_running_interactive_jobs(&executor);
            if let Err(error) = expire_overdue_route_binds(
                &writer_tx,
                &executor,
                &mut pending_binds,
                &mut installed_route_epochs,
                &dispatch_path_metrics,
            )
            .await
            {
                break Err(error);
            }

            let retried = push::drain_retry_buffers_for_bound_routes(
                &writer_tx,
                &dispatch_path_metrics,
                &routes,
                &mut retry_buffer,
            );
            if retried > 0 {
                log::debug!(
                    "subc attach: retried {retried} reliable Push frame(s) after writer backpressure"
                );
            }

            next_drain_at = tokio::time::Instant::now() + DRAIN_TICK_PERIOD;
        }

        // A lossy emitter may place its newest update in the overflow buffer
        // when the bounded channel is full, while this receive loop is draining
        // the channel. Drain overflow before selecting again so that raced
        // update is delivered on the next timer tick instead of waiting for
        // another lossy enqueue.
        let overflow_batch = lossy_overflow.drain();
        if !overflow_batch.is_empty() {
            let (_, deferred) = push::drain_reliable_push_turn(
                &writer_tx,
                &dispatch_path_metrics,
                &routes,
                &root_channels,
                &session_identity,
                &mut retry_buffer,
                &mut push_buffer,
                &mut completed_tasks,
                &bg_sub_by_session,
                &mut bg_wake_pending,
                &mut bg_wake_epoch,
                &mut reliable_rx,
                None,
                lifecycle_probe.as_ref(),
            );
            if deferred {
                tokio::task::yield_now().await;
            }

            let mut batch = Vec::new();
            while let Ok(item) = lossy_rx.try_recv() {
                batch.push(item);
            }
            batch.extend(overflow_batch);
            push::process_lossy_push_envelope_batch(
                &writer_tx,
                &dispatch_path_metrics,
                &routes,
                &root_channels,
                &completed_tasks,
                batch,
            );
        }

        tokio::select! {
            biased;
            Some(completion) = control_completion_rx.recv() => {
                if let Err(error) = process_route_bind_completion(
                    &writer_tx,
                    completion,
                    &mut routes,
                    &mut root_channels,
                    &mut session_identity,
                    &mut push_buffer,
                    &mut live_roots,
                    &mut pending_binds,
                    &mut installed_route_epochs,
                    &executor,
                    &standing_actor,
                    &shutdown,
                    &dispatch_path_metrics,
                    lifecycle_probe.as_ref(),
                )
                .await
                {
                    break Err(error);
                }
                next_maintenance_at = tokio::time::Instant::now() + DRAIN_TICK_PERIOD;
                next_health_rollup_at = tokio::time::Instant::now();
            }
            _ = shutdown.notified() => {
                log::warn!("subc attach: fatal executor response requested teardown");
                break Ok(ModuleLoopExit::SkipSearchFlush);
            }
            maybe_frame = recv_prioritized_frame(
                &mut control_reader_rx,
                &mut data_reader_rx,
                &mut reader_lane,
            ) => {
                let frame = match maybe_frame {
                    None => {
                        log::info!("subc attach: daemon closed connection");
                        break Ok(ModuleLoopExit::Graceful);
                    }
                    Some(Err(error)) => break Err(error),
                    Some(Ok(frame)) => frame,
                };
                let phase_trace = frame.phase_trace;
                let frame = frame.frame;

                if !ingress_route_should_be_processed(
                    &installed_route_epochs,
                    &reclaimed_routes,
                    &frame,
                ) {
                    log::debug!(
                        "subc attach: silently dropping {:?} for uninstalled route {}@{}",
                        frame.header.ty,
                        frame.header.channel,
                        frame.header.epoch
                    );
                    continue;
                }

                match frame.header.ty {
                    FrameType::Ping if frame.header.channel == 0 => {
                        let pong = match Frame::build_with_version(
                            frame.header.ver,
                            FrameType::Pong,
                            frame.header.flags,
                            0,
                            0,
                            frame.header.corr,
                            Vec::new(),
                        ) {
                            Ok(pong) => pong,
                            Err(error) => break Err(SubcError::FrameBuild(error)),
                        };
                        if let Err(error) = send_frame(&writer_tx, &dispatch_path_metrics, pong).await {
                            break Err(error);
                        }
                    }
                    FrameType::Goodbye if frame.header.channel == 0 => {
                        log::info!("subc attach: received channel-0 Goodbye");
                        break Ok(ModuleLoopExit::Graceful);
                    }
                    FrameType::Goodbye => {
                        let channel = route_key(frame.header.channel, frame.header.epoch);
                        if let Err(error) = teardown_installed_route(
                            &writer_tx,
                            &dispatch_path_metrics,
                            &executor,
                            channel,
                            "Goodbye",
                            None,
                            &mut installed_route_epochs,
                            &mut routes,
                            &mut root_channels,
                            &mut bg_subs,
                            &mut bg_sub_by_session,
                            &mut bg_wake_pending,
                            &mut pending_bash_asks,
                            &mut live_roots,
                            &mut route_bash_cancels,
                            &active_tool_calls,
                            &mut pending_responses,
                            &mut pending_binds,
                            &mut retry_buffer,
                            &mut push_buffer,
                            &shutdown,
                            tool_response_body_limit,
                            lifecycle_probe.as_ref(),
                        )
                        .await
                        {
                            break Err(error);
                        }
                    }
                    FrameType::Response | FrameType::Error if frame.header.channel != 0 => {
                        if let Err(error) = handle_bash_elicitation_reply(
                            &writer_tx,
                            &frame,
                            &mut pending_bash_asks,
                            &routes,
                            &mut live_roots,
                            &executor,
                            &shutdown,
                            &bash_deferred_tx,
                            &bash_poll_touch_tx,
                            &dispatch_path_metrics,
                            &mut route_bash_cancels,
                            dispatch,
                        )
                        .await
                        {
                            break Err(error);
                        }
                    }
                    FrameType::Request if frame.header.channel == 0 => {
                        if let Err(error) = handle_control_request(
                            &writer_tx,
                            &frame,
                            &shared_app,
                            &executor,
                            &mut live_roots,
                            &mut pending_binds,
                            &mut installed_route_epochs,
                            &mut routes,
                            &mut root_channels,
                            &mut bg_subs,
                            &mut bg_sub_by_session,
                            &mut bg_wake_pending,
                            &mut pending_bash_asks,
                            &mut route_bash_cancels,
                            &active_tool_calls,
                            &mut pending_responses,
                            &mut retry_buffer,
                            &mut push_buffer,
                            &shutdown,
                            &control_completion_tx,
                            &dispatch_path_metrics,
                            lifecycle_probe.as_ref(),
                            &health_rollup_cache,
                            &push_senders,
                            dispatch,
                            user_config_path.as_deref(),
                            tool_response_body_limit,
                        )
                        .await
                        {
                            break Err(error);
                        }
                    }
                    FrameType::Request => {
                        if let Err(error) = handle_tool_call(
                            &writer_tx,
                            &frame,
                            phase_trace,
                            &routes,
                            &pending_binds,
                            &mut live_roots,
                            &executor,
                            &active_tool_calls,
                            &pending_deferred_setups,
                            &shutdown,
                            &connection_cancel,
                            &bash_deferred_tx,
                            &bash_poll_touch_tx,
                            &dispatch_path_metrics,
                            &mut route_bash_cancels,
                            &mut pending_bash_asks,
                            &mut next_bash_ask_corr,
                            &mut bg_subs,
                            &mut bg_sub_by_session,
                            &mut bg_wake_pending,
                            &mut bg_wake_epoch,
                            dispatch,
                            &deferred_response_tx,
                            allow_native_passthrough,
                            tool_response_body_limit,
                        )
                        .await
                        {
                            break Err(error);
                        }
                    }
                    FrameType::Cancel => {
                        let channel = route_key(frame.header.channel, frame.header.epoch);
                        cancel_active_tool_call(
                            &active_tool_calls,
                            executor.as_ref(),
                            channel,
                            frame.header.corr,
                            "Cancel frame",
                        );
                        pending_responses.cancel_request(channel, frame.header.corr);
                        if bg_subs.contains_key(&channel) {
                            if let Err(error) = end_bg_subscription(
                                &writer_tx,
                                &dispatch_path_metrics,
                                &mut bg_subs,
                                &mut bg_sub_by_session,
                                &mut bg_wake_pending,
                                channel,
                                routes.get(&channel),
                                "cancel",
                            )
                            .await
                            {
                                break Err(error);
                            }
                        }
                        if let Err(error) = cancel_pending_bash_ask_for_tool_call(
                            &writer_tx,
                            &mut pending_bash_asks,
                            channel,
                            frame.header.corr,
                            &routes,
                            &mut live_roots,
                            &mut route_bash_cancels,
                            &shutdown,
                            &dispatch_path_metrics,
                        )
                        .await
                        {
                            break Err(error);
                        }
                    }
                    // Incoming push messages are ignored here. Cancel frames are
                    // handled above for active and deferred tool calls plus pending
                    // bash elicitation requests.
                    _ => {}
                }
            }
            Some(pending) = deferred_response_rx.recv() => {
                if routes.contains_key(&pending.route)
                    && active_tool_call_is_registered(
                        &active_tool_calls,
                        pending.route,
                        pending.corr,
                    )
                {
                    pending_responses.register(pending);
                } else {
                    if let Some(cancellation) = &pending.pending.cancellation {
                        cancellation.request_cancel();
                    }
                    finish_active_tool_call(&active_tool_calls, pending.route, pending.corr);
                }
            }
            Some((root_id, frame)) = reliable_rx.recv() => {
                // Reliable Push frames are FIFO and must-deliver, but draining an
                // unbounded burst in one current-thread turn can starve RouteBind
                // completions. The budget defers excess frames, never drops them.
                let (_, deferred) = push::drain_reliable_push_turn(
                    &writer_tx,
                    &dispatch_path_metrics,
                    &routes,
                    &root_channels,
                    &session_identity,
                    &mut retry_buffer,
                    &mut push_buffer,
                    &mut completed_tasks,
                    &bg_sub_by_session,
                    &mut bg_wake_pending,
                    &mut bg_wake_epoch,
                    &mut reliable_rx,
                    Some((root_id, frame)),
                    lifecycle_probe.as_ref(),
                );
                if deferred {
                    tokio::task::yield_now().await;
                }
            }
            Some((order, root_id, frame)) = lossy_rx.recv() => {
                // When both push lanes have work, handle a small reliable slice before lossy work.
                // That ordering lets completed task ids suppress stale BashLongRunning frames.
                // The slice stays bounded so reliable bursts cannot monopolize this loop turn.
                let (_, deferred) = push::drain_reliable_push_turn(
                    &writer_tx,
                    &dispatch_path_metrics,
                    &routes,
                    &root_channels,
                    &session_identity,
                    &mut retry_buffer,
                    &mut push_buffer,
                    &mut completed_tasks,
                    &bg_sub_by_session,
                    &mut bg_wake_pending,
                    &mut bg_wake_epoch,
                    &mut reliable_rx,
                    None,
                    lifecycle_probe.as_ref(),
                );
                if deferred {
                    tokio::task::yield_now().await;
                }

                // Drain the currently queued burst in one loop turn so lossy
                // status/progress updates can be merged before reaching subc's
                // shared egress queue. Each lossy frame gets a sequence number
                // before it goes to the channel or overflow buffer, so the
                // combined batch is sorted back into producer order before
                // coalescing drops stale updates for the same key.
                let mut batch = vec![(order, root_id, frame)];
                while let Ok(item) = lossy_rx.try_recv() {
                    batch.push(item);
                }
                batch.extend(lossy_overflow.drain());
                push::process_lossy_push_envelope_batch(
                    &writer_tx,
                    &dispatch_path_metrics,
                    &routes,
                    &root_channels,
                    &completed_tasks,
                    batch,
                );
            }
            Some(done) = bash_deferred_rx.recv() => {
                decrement_counted_channel(&dispatch_path_metrics.bash_deferred_queued);
                if let Err(error) = bash::handle_bash_deferred_completion(
                    &writer_tx,
                    done,
                    &routes,
                    &mut live_roots,
                    &mut route_bash_cancels,
                    &shutdown,
                    &dispatch_path_metrics,
                )
                .await
                {
                    break Err(error);
                }
            }
            Some(root_id) = bash_poll_touch_rx.recv() => {
                decrement_counted_channel(&dispatch_path_metrics.bash_poll_touch_queued);
                if let Some(meta) = live_roots.get_mut(&root_id) {
                    meta.note_activity();
                }
            }
            Some(completion) = maintenance_rx.recv() => {
                decrement_counted_channel(&dispatch_path_metrics.maintenance_queued);
                let root_id = completion.root_id.clone();
                let response = completion.response;
                let response_is_fatal = response_is_fatal_panic(&response);
                let bind_pending = pending_binds
                    .values()
                    .any(|pending| pending.bind_root_id == root_id);
                let requiesce = if let Some(meta) = live_roots.get_mut(&root_id) {
                    let defer_requeue = meta.unbound_quiesced || bind_pending;
                    note_maintenance_completion(
                        meta,
                        completion.requeue_kind,
                        response_is_fatal,
                        defer_requeue,
                    );
                    should_requiesce_after_maintenance(meta, completion.kind, bind_pending)
                } else {
                    false
                };
                if requiesce {
                    quiesce_unbound_root(&root_id, &mut live_roots, &executor);
                }
                push::clear_stale_bg_wakes_for_empty_sessions(
                    &root_id,
                    &completion.empty_bg_sessions,
                    &bg_sub_by_session,
                    &mut bg_wake_pending,
                    &bg_wake_epoch,
                );
                if response_is_fatal {
                    if let Some(meta) = live_roots.get_mut(&root_id) {
                        meta.maintenance_poisoned = true;
                    }
                    log::warn!(
                        "subc attach: maintenance drain observed a fatal actor; deferring teardown until a route request can receive actor_fatal"
                    );
                }
            }
            _ = tokio::time::sleep(PENDING_POLL_INTERVAL), if !pending_responses.is_empty() => {
                // The next loop turn polls detached inspect completions. Keeping
                // the timer here lets already-ready control frames run first.
            }
            _ = tokio::time::sleep_until(next_drain_at) => {
                // Wakes an otherwise-idle loop so the pre-turn drain check
                // above runs on schedule; the drain work itself lives there.
            }
            _ = tokio::time::sleep_until(next_maintenance_at) => {
                // Delay cache-draining maintenance until any already-ready
                // inbound route/control messages and push completions have run,
                // so maintenance does not block the actor from handling the
                // first request that arrives after a route bind is acknowledged.
                crate::logging::maybe_sweep_logs();
                let reaped_lsp_children = shared_app
                    .lsp_child_registry()
                    .reap_children_with_gone_cwd_or_reclaimed_root();
                if reaped_lsp_children > 0 {
                    log::warn!(
                        "subc attach: reaped {reaped_lsp_children} LSP child process group(s) with a deleted cwd or reclaimed root"
                    );
                }
                let reap = reap_idle_roots(
                    Instant::now(),
                    &mut live_roots,
                    &pending_binds,
                    &root_channels,
                    &executor,
                    &dispatch_path_metrics,
                );
                for root_id in &reap.forgotten_deleted_roots {
                    purge_deleted_root_residents(
                        root_id,
                        &mut routes,
                        &mut root_channels,
                        &mut installed_route_epochs,
                        &mut route_bash_cancels,
                        &active_tool_calls,
                        executor.as_ref(),
                        &mut retry_buffer,
                        &mut reclaimed_routes,
                        &mut session_identity,
                        &mut push_buffer,
                        &mut bg_subs,
                        &mut bg_sub_by_session,
                        &mut bg_wake_pending,
                        &mut bg_wake_epoch,
                        &mut pending_bash_asks,
                        &dispatch_path_metrics,
                    );
                }
                if reap.evicted > 0 {
                    log::debug!("subc attach: reaped {} idle root(s)", reap.evicted);
                }
                submit_due_maintenance_jobs(
                    &executor,
                    &mut live_roots,
                    &pending_binds,
                    &bg_sub_by_session,
                    &bg_wake_pending,
                    &bg_wake_epoch,
                    &maintenance_tx,
                    &dispatch_path_metrics,
                );
                if tokio::time::Instant::now() >= next_standing_pass_at {
                    standing_actor.tick();
                    next_standing_pass_at = tokio::time::Instant::now()
                        + standing::STANDING_MAINTENANCE_INTERVAL;
                }
                // Scan and trim allocator arenas on a detached thread. On glibc,
                // mallinfo2() walks every arena under allocator locks, so the
                // transport thread must only evaluate the scan cadence here.
                #[cfg(any(target_os = "macos", target_os = "linux"))]
                {
                    let now_std = std::time::Instant::now();
                    if crate::memory::spawn_allocator_slack_scan_if_due(
                        last_slack_scan,
                        now_std,
                    ) {
                        last_slack_scan = Some(now_std);
                    }
                }
                next_maintenance_at = tokio::time::Instant::now() + DRAIN_TICK_PERIOD;
            }
        }
    };

    shared_app.set_open_route_count(0);

    connection_cancel.cancel();
    cancel_all_active_tool_calls(&active_tool_calls, executor.as_ref(), "connection teardown");
    let setup_drain_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while pending_deferred_setups.load(Ordering::SeqCst) != 0
        && tokio::time::Instant::now() < setup_drain_deadline
    {
        tokio::select! {
            biased;
            Some(pending) = deferred_response_rx.recv() => pending_responses.register(pending),
            _ = tokio::time::sleep(Duration::from_millis(5)) => {}
        }
    }
    if pending_deferred_setups.load(Ordering::SeqCst) != 0 {
        log::warn!(
            "subc attach: timed out waiting for deferred response setup registration during shutdown"
        );
    }
    while let Ok(pending) = deferred_response_rx.try_recv() {
        pending_responses.register(pending);
    }
    for resolved in pending_responses.drain_on_shutdown(executor.as_ref()) {
        if let Err(error) = deliver_resolved_subc_response(
            &writer_tx,
            resolved,
            &routes,
            &mut live_roots,
            executor.as_ref(),
            &active_tool_calls,
            &shutdown,
            &dispatch_path_metrics,
            tool_response_body_limit,
        )
        .await
        {
            log::warn!("subc attach: failed to emit deferred shutdown terminal: {error}");
        }
    }
    // Channel-0 Goodbye, EOF, and fatal exits bypass per-route Goodbye. Settle
    // their root lifecycle state before loop-owned routing metadata is dropped.
    quiesce_connection_roots(
        &mut live_roots,
        &mut pending_binds,
        &mut routes,
        &mut root_channels,
        &mut installed_route_epochs,
        &mut route_bash_cancels,
        &active_tool_calls,
        &executor,
    );

    fleet_status_client.set_route_live(false);
    fleet_status_task.abort();
    let _ = fleet_status_task.await;

    let mut loop_result = loop_result;
    if !pending_bash_asks.is_empty() {
        let no_routes: HashMap<RouteChannel, RouteIdentity> = HashMap::new();
        if let Err(error) = settle_all_pending_bash_asks(
            &writer_tx,
            &mut pending_bash_asks,
            &no_routes,
            &mut live_roots,
            &mut route_bash_cancels,
            &shutdown,
            &dispatch_path_metrics,
        )
        .await
        {
            loop_result = loop_result.and(Err(error));
        }
    }

    // The reader task may be parked on `read_frame`; abort it (we are done with
    // the connection) and flush the writer.
    reader_task.abort();
    drop(writer_tx);
    let writer_result = finish_writer_task(writer_task).await;
    loop_result.and_then(|exit| writer_result.map(|_| exit))
}

fn spawn_writer_task<W>(
    mut write: W,
    mut rx: mpsc::Receiver<WriterFrame>,
    metrics: Arc<DispatchPathMetrics>,
) -> JoinHandle<Result<(), subc_transport::FrameIoError>>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut write_buffer = Vec::new();
        while let Some(mut queued) = rx.recv().await {
            let measure = queued.tool_response_trace.is_some();
            let dequeued = measure.then(Instant::now);
            metrics.writer_active.store(true, Ordering::Relaxed);
            decrement_counted_channel(&metrics.writer_queued);
            let write_timing = write_frame_contiguous(
                &mut write,
                queued.frame(),
                queued.body(),
                &mut write_buffer,
                measure,
            )
            .await;
            metrics.writer_active.store(false, Ordering::Relaxed);
            let write_timing = write_timing?;

            if let (Some(trace), Some(dequeued), Some(write_timing)) =
                (queued.tool_response_trace.take(), dequeued, write_timing)
            {
                if let Some(completed) = trace.finish(
                    dequeued,
                    write_timing.write_started,
                    write_timing.write_finished,
                    write_timing.frame_bytes,
                ) {
                    log_ctx::with_session(Some(completed.session), || {
                        crate::logging::note_tool_call_trace(
                            &completed.name,
                            &completed.root,
                            completed.channel,
                            completed.corr,
                            completed.phases,
                        );
                    });
                }
            }
        }
        Ok(())
    })
}

struct FrameWriteTiming {
    write_started: Instant,
    write_finished: Instant,
    frame_bytes: usize,
}

/// Encode one complete frame into the existing reusable buffer and write it
/// without interleaving bytes from another channel. Timing is collected only
/// for tool responses, so Push and control frames add no clock reads.
async fn write_frame_contiguous<W>(
    writer: &mut W,
    frame: &Frame,
    body: &[u8],
    buffer: &mut Vec<u8>,
    measure: bool,
) -> Result<Option<FrameWriteTiming>, subc_transport::FrameIoError>
where
    W: AsyncWrite + Unpin,
{
    if frame.header.len as usize != body.len() {
        return Err(subc_transport::FrameIoError::BodyLengthMismatch {
            header_len: frame.header.len,
            body_len: body.len(),
        });
    }

    let header = frame.header.encode();
    buffer.clear();
    buffer.reserve(header.len() + body.len());
    buffer.extend_from_slice(&header);
    buffer.extend_from_slice(body);
    let write_started = measure.then(Instant::now);
    writer
        .write_all(buffer)
        .await
        .map_err(subc_transport::FrameIoError::Io)?;
    Ok(write_started.map(|write_started| FrameWriteTiming {
        write_started,
        write_finished: Instant::now(),
        frame_bytes: buffer.len(),
    }))
}

fn spawn_reader_task<R>(
    mut read: R,
    control_tx: mpsc::Sender<Result<DecodedFrame, SubcError>>,
    data_tx: mpsc::Sender<Result<DecodedFrame, SubcError>>,
) -> JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            match read_frame(&mut read).await {
                Ok(Some(frame)) => {
                    let is_control = frame.header.channel == 0;
                    let decoded = DecodedFrame {
                        frame,
                        phase_trace: PhaseTrace::new(Instant::now()),
                    };
                    let tx = if is_control { &control_tx } else { &data_tx };
                    if tx.send(Ok(decoded)).await.is_err() {
                        return;
                    }
                }
                Ok(None) => {
                    // EOF: let the loop observe both channel closures as "daemon closed".
                    return;
                }
                Err(error) => {
                    // A killed daemon surfaces as ConnectionReset (RST) on
                    // Windows where Unix delivers a clean EOF (FIN); a
                    // mid-teardown daemon can also abort the socket. Both mean
                    // "daemon went away", not a wire fault — normalize them to
                    // the clean-close path so module exit behavior matches
                    // across platforms (same class subc-core fixed in d33d9a71).
                    if let subc_transport::FrameIoError::Io(io_error) = &error {
                        if matches!(
                            io_error.kind(),
                            std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::ConnectionAborted
                        ) {
                            log::info!(
                                "subc attach: connection reset by daemon; treating as close"
                            );
                            return;
                        }
                    }
                    let _ = control_tx.send(Err(SubcError::FrameIo(error))).await;
                    return;
                }
            }
        }
    })
}

/// Cap on consecutive control frames before the reader lane checks data, so
/// sustained control traffic cannot starve data frames under the biased
/// select.
const CONTROL_BURST_LIMIT: usize = 8;

#[derive(Default)]
struct PrioritizedFrameLane {
    control_closed: bool,
    data_closed: bool,
    consecutive_control: usize,
}

async fn recv_prioritized_frame(
    control_rx: &mut mpsc::Receiver<Result<DecodedFrame, SubcError>>,
    data_rx: &mut mpsc::Receiver<Result<DecodedFrame, SubcError>>,
    lane: &mut PrioritizedFrameLane,
) -> Option<Result<DecodedFrame, SubcError>> {
    loop {
        // Once a lane is closed it can never produce again; drain the other
        // lane before declaring EOF so buffered frames are not dropped.
        if lane.control_closed {
            return match data_rx.recv().await {
                Some(frame) => Some(frame),
                None => {
                    lane.data_closed = true;
                    None
                }
            };
        }
        if lane.data_closed {
            return match control_rx.recv().await {
                Some(frame) => Some(frame),
                None => {
                    lane.control_closed = true;
                    None
                }
            };
        }
        if lane.consecutive_control >= CONTROL_BURST_LIMIT {
            // Prefer one ready data frame after a control burst, but keep
            // polling control so an idle data lane cannot block heartbeats.
            lane.consecutive_control = 0;
            tokio::select! {
                biased;
                frame = data_rx.recv() => match frame {
                    Some(frame) => return Some(frame),
                    None => {
                        lane.data_closed = true;
                        continue;
                    }
                },
                frame = control_rx.recv() => match frame {
                    Some(frame) => {
                        lane.consecutive_control = 1;
                        return Some(frame);
                    }
                    None => {
                        lane.control_closed = true;
                        continue;
                    }
                }
            }
        }
        tokio::select! {
            biased;
            frame = control_rx.recv() => match frame {
                Some(frame) => {
                    lane.consecutive_control += 1;
                    return Some(frame);
                }
                None => {
                    lane.control_closed = true;
                    continue;
                }
            },
            frame = data_rx.recv() => {
                lane.consecutive_control = 0;
                return frame;
            }
        }
    }
}

async fn finish_writer_task(
    mut writer_task: JoinHandle<Result<(), subc_transport::FrameIoError>>,
) -> Result<(), SubcError> {
    match tokio::time::timeout(Duration::from_millis(100), &mut writer_task).await {
        Ok(Ok(Ok(()))) => Ok(()),
        Ok(Ok(Err(error))) => Err(SubcError::FrameIo(error)),
        Ok(Err(error)) => Err(SubcError::WriterJoin(error)),
        Err(_) => {
            writer_task.abort();
            Ok(())
        }
    }
}

fn register_actor_for_bind(
    shared_app: &Arc<App>,
    executor: &Arc<Executor>,
    push_senders: &PushSenders,
    bind_root_id: &ProjectRootId,
    route_channel: u16,
    root_was_live: bool,
) -> bool {
    if executor.actor_registered(bind_root_id) {
        log::debug!(
            "subc attach: reusing actor for route {} root {}",
            route_channel,
            bind_root_id.as_path().display()
        );
        return false;
    }

    if root_was_live {
        log::warn!(
            "subc attach: recreating missing actor for live root {} on route {}",
            bind_root_id.as_path().display(),
            route_channel
        );
    }

    let actor_ctx = Arc::new(AppContext::from_app(
        Arc::clone(shared_app),
        Config::default(),
    ));
    install_bash_compressor(&actor_ctx);
    actor_ctx.install_fleet_status_client(Some(push_senders.fleet_status_client.clone()));
    actor_ctx.set_progress_sender(Some(push::progress_sender_for_root(
        push_senders.clone(),
        bind_root_id.clone(),
    )));
    let inserted = executor.register_actor(bind_root_id.clone(), Arc::clone(&actor_ctx));
    drop(actor_ctx);
    if inserted {
        // Do not insert into live_roots until configure succeeds: live_roots
        // drives maintenance, and a half-configured new actor must not be
        // maintenance-eligible before its route/session identity exists.
        log::debug!(
            "subc attach: registered actor for route {} root {}",
            route_channel,
            bind_root_id.as_path().display()
        );
    } else {
        log::debug!(
            "subc attach: actor appeared while binding route {} root {}; reusing it",
            route_channel,
            bind_root_id.as_path().display()
        );
    }
    inserted
}

fn rollback_pending_bind_actor(
    executor: &Arc<Executor>,
    live_roots: &HashMap<ProjectRootId, RootMeta>,
    pending_binds: &mut HashMap<RouteChannel, PendingBind>,
    root_id: &ProjectRootId,
    inserted_new_actor: bool,
) {
    if !inserted_new_actor || live_roots.contains_key(root_id) {
        return;
    }

    if let Some((route, pending)) = pending_binds
        .iter_mut()
        .find(|(_, pending)| &pending.bind_root_id == root_id)
    {
        pending.inserted_new_actor = true;
        log::debug!(
            "subc attach: transferred rollback ownership for root {} to pending route {}",
            root_id.as_path().display(),
            route
        );
        return;
    }

    executor.remove_actor(root_id);
}

fn route_bind_error_code_for_configure_response(response: &Response) -> &'static str {
    match response.data.get("code").and_then(|code| code.as_str()) {
        // Preserve typed configure rejections across the bind boundary: a
        // malformed fed fingerprint means a federation-module bug or
        // fingerprint-format drift, and the fed side matches on the code rather
        // than parsing prose.
        Some("bad_harness_fingerprint") => "bad_harness_fingerprint",
        // Cache-key probe failures are transient (fd pressure, git spawn
        // contention); the client retries the bind rather than treating the
        // root as permanently divergent.
        Some("cache_key_probe_failed") => "cache_key_probe_failed",
        // Actor lifecycle gaps are transient from the daemon/client viewpoint:
        // a fresh bind can create or join a healthy actor, so do not classify
        // them as permanent config divergence.
        Some("actor_not_registered" | "actor_fatal") => "actor_not_ready",
        _ => "config_divergence",
    }
}

fn queue_post_bind_configure_and_completion_maintenance(
    root_id: &ProjectRootId,
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
) {
    let Some(meta) = live_roots.get_mut(root_id) else {
        return;
    };
    if meta.maintenance_poisoned || meta.maintenance_pending {
        return;
    }

    meta.maintenance_pending = true;
    meta.maintenance_queued_kinds
        .push_back(MaintenanceDrainKind::ConfigureTail);
    meta.maintenance_queued_kinds
        .push_back(MaintenanceDrainKind::CompletionDrains);
}

#[allow(clippy::too_many_arguments)]
async fn handle_route_bind_completion(
    tx: &WriterSender,
    completion: RouteBindCompletion,
    routes: &mut HashMap<RouteChannel, RouteIdentity>,
    root_channels: &mut HashMap<ProjectRootId, HashSet<RouteChannel>>,
    session_identity: &mut HashMap<(ProjectRootId, String), RetainedSessionIdentity>,
    push_buffer: &mut HashMap<push::ReplayKey, VecDeque<PushFrame>>,
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    pending_binds: &mut HashMap<RouteChannel, PendingBind>,
    installed_route_epochs: &mut HashMap<u16, u32>,
    executor: &Arc<Executor>,
    standing_actor: &standing::StandingActor,
    shutdown: &Arc<Notify>,
    metrics: &Arc<DispatchPathMetrics>,
    lifecycle_probe: Option<&SubcTestLifecycleProbe>,
) -> Result<(), SubcError> {
    let route_id = completion.route;
    let Some(pending) = pending_binds.remove(&route_id) else {
        log::warn!(
            "subc attach: dropping RouteBind completion for non-pending route {}",
            completion.route
        );
        rollback_pending_bind_actor(
            executor,
            live_roots,
            pending_binds,
            &completion.bind_root_id,
            completion.inserted_new_actor,
        );
        let has_pending_bind = pending_binds
            .values()
            .any(|pending| pending.bind_root_id == completion.bind_root_id);
        if !root_channels
            .get(&completion.bind_root_id)
            .is_some_and(|channels| !channels.is_empty())
            && !has_pending_bind
        {
            quiesce_unbound_root(&completion.bind_root_id, live_roots, executor);
        }
        remove_installed_route(installed_route_epochs, route_id);
        return Ok(());
    };

    if pending.bind_root_id != completion.bind_root_id {
        log::warn!(
            "subc attach: pending RouteBind root mismatch for route {} (pending {} completion {})",
            completion.route,
            pending.bind_root_id.as_path().display(),
            completion.bind_root_id.as_path().display()
        );
    }

    let inserted_new_actor = pending.inserted_new_actor || completion.inserted_new_actor;
    if pending.cancelled {
        rollback_pending_bind_actor(
            executor,
            live_roots,
            pending_binds,
            &completion.bind_root_id,
            inserted_new_actor,
        );
        let has_pending_bind = pending_binds
            .values()
            .any(|pending| pending.bind_root_id == completion.bind_root_id);
        if !root_channels
            .get(&completion.bind_root_id)
            .is_some_and(|channels| !channels.is_empty())
            && !has_pending_bind
        {
            quiesce_unbound_root(&completion.bind_root_id, live_roots, executor);
        }
        log::debug!(
            "subc attach: discarded completed RouteBind for cancelled route {} root {}",
            completion.route,
            completion.bind_root_id.as_path().display()
        );
        remove_installed_route(installed_route_epochs, route_id);
        return Ok(());
    }

    let failure = if !completion.configure_response.success {
        Some((
            &completion.configure_response,
            "configure failed during route bind",
        ))
    } else {
        None
    };

    if let Some((response, fallback)) = failure {
        rollback_pending_bind_actor(
            executor,
            live_roots,
            pending_binds,
            &completion.bind_root_id,
            inserted_new_actor,
        );
        let has_pending_bind = pending_binds
            .values()
            .any(|pending| pending.bind_root_id == completion.bind_root_id);
        if !root_channels
            .get(&completion.bind_root_id)
            .is_some_and(|channels| !channels.is_empty())
            && !has_pending_bind
        {
            quiesce_unbound_root(&completion.bind_root_id, live_roots, executor);
        }
        let message = response_message(response, fallback);
        let fatal = response_is_fatal_panic(response);
        let error_code = route_bind_error_code_for_configure_response(response);
        send_route_bind_error_parts(
            tx,
            completion.ver,
            completion.corr,
            completion.flags,
            error_code,
            &message,
            metrics,
        )
        .await?;
        remove_installed_route(installed_route_epochs, route_id);
        if fatal {
            signal_fatal_teardown(
                tx,
                Some(completion.route),
                completion.ver,
                completion.corr,
                shutdown,
                metrics,
            )
            .await;
        }
        return Ok(());
    }

    remember_session_identity(session_identity, &completion.identity);
    let replay_key = push::ReplayKey::from_identity(&completion.identity);
    let bind_trust = completion.identity.trust;
    insert_route_channel(routes, root_channels, route_id, completion.identity);
    let restore_watcher = live_roots
        .get(&completion.bind_root_id)
        .is_some_and(|meta| meta.idle_artifacts_evicted || meta.unbound_quiesced);
    live_roots
        .entry(completion.bind_root_id.clone())
        .and_modify(|meta| {
            meta.reactivate_bound();
            meta.diagnostics_on_edit = completion.diagnostics_on_edit;
            meta.maintenance_poisoned = false;
        })
        .or_insert_with(|| RootMeta::new(Instant::now()));
    if let Some(meta) = live_roots.get_mut(&completion.bind_root_id) {
        meta.diagnostics_on_edit = completion.diagnostics_on_edit;
        meta.maintenance_poisoned = false;
    }
    if let Some(ctx) = executor.actor_context(&completion.bind_root_id) {
        // The bind transition revokes any matching unbound standing admission
        // before this session can select the shared artifact family.
        standing_actor.begin_session_bind(&ctx);
        ctx.mark_subc_bound();
        if restore_watcher {
            crate::commands::configure::ensure_project_watcher(&ctx);
        }
    }

    let ack =
        serde_json::to_vec(&ModuleControlResponse::RouteBindAck {}).map_err(SubcError::Json)?;
    let response = Frame::build_with_version(
        completion.ver,
        FrameType::Response,
        control_flags(),
        0,
        0,
        completion.corr,
        ack,
    )
    .map_err(SubcError::FrameBuild)?;
    send_reliable_writer_frame(tx, metrics, response, "RouteBindAck").await?;
    queue_post_bind_configure_and_completion_maintenance(&completion.bind_root_id, live_roots);
    let replayed = push::replay_buffered_push_frames(
        tx,
        metrics,
        route_id,
        push_buffer,
        &replay_key,
        bind_trust,
        lifecycle_probe,
    );
    if replayed > 0 {
        log::debug!(
            "subc attach: replayed {} buffered Push frame(s) to route {} root {} harness {} session {}",
            replayed,
            completion.route,
            replay_key.root.as_path().display(),
            replay_key.harness,
            replay_key.session
        );
    }
    log::info!(
        "subc attach: route {} bound to root {}",
        completion.route,
        completion.bind_root_id.as_path().display()
    );
    Ok(())
}

async fn expire_overdue_route_binds(
    tx: &WriterSender,
    executor: &Arc<Executor>,
    pending_binds: &mut HashMap<RouteChannel, PendingBind>,
    installed_route_epochs: &mut HashMap<u16, u32>,
    metrics: &DispatchPathMetrics,
) -> Result<(), SubcError> {
    let now = Instant::now();
    let expired: Vec<_> = pending_binds
        .iter()
        .filter_map(|(route, pending)| {
            let age = now.saturating_duration_since(pending.started_at);
            (!pending.deadline_reported && age >= ROUTE_BIND_DEADLINE).then(|| {
                (
                    *route,
                    pending.corr,
                    pending.ver,
                    pending.flags,
                    pending.bind_root_id.clone(),
                    pending.configure_request_id.clone(),
                    age,
                )
            })
        })
        .collect();

    for (route, corr, ver, flags, root_id, configure_request_id, age) in expired {
        if let Some(pending) = pending_binds.get_mut(&route) {
            pending.cancelled = true;
            pending.deadline_reported = true;
            let outcome = executor.cancel_job(&pending.bind_root_id, &pending.cancellation);
            log::debug!(
                "subc attach: cancelled overdue RouteBind configure for route {route} ({outcome:?})"
            );
        }
        remove_installed_route(installed_route_epochs, route);
        let age_ms = age.as_millis().min(u128::from(u64::MAX)) as u64;
        let deadline_ms = ROUTE_BIND_DEADLINE.as_millis();
        send_route_bind_error_parts(
            tx,
            ver,
            corr,
            flags,
            "actor_not_ready",
            &format!("route bind deadline exceeded after {age_ms}ms (deadline {deadline_ms}ms)"),
            metrics,
        )
        .await?;
        log::warn!(
            "subc attach: route {} bind for root {} exceeded {}ms deadline (configure_request_id={})",
            route,
            root_id.as_path().display(),
            deadline_ms,
            configure_request_id
        );
    }

    Ok(())
}

/// channel-0 control requests: RouteBind plus the cached health probe. RouteBind
/// still reconciles the route's RootConfig through the executor's Mutating lane
/// and resolves completion on a loop-owned control-completion channel so slow
/// configure jobs do not block the transport loop.
#[allow(clippy::too_many_arguments)]
async fn handle_control_request(
    tx: &WriterSender,
    frame: &Frame,
    shared_app: &Arc<App>,
    executor: &Arc<Executor>,
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    pending_binds: &mut HashMap<RouteChannel, PendingBind>,
    installed_route_epochs: &mut HashMap<u16, u32>,
    routes: &mut HashMap<RouteChannel, RouteIdentity>,
    root_channels: &mut HashMap<ProjectRootId, HashSet<RouteChannel>>,
    bg_subs: &mut HashMap<RouteChannel, BgSub>,
    bg_sub_by_session: &mut BgSubsBySession,
    bg_wake_pending: &mut HashSet<RouteChannel>,
    pending_bash_asks: &mut HashMap<ReverseCorrKey, PendingBashAsk>,
    route_bash_cancels: &mut HashMap<RouteChannel, bash::RouteBashCancel>,
    active_tool_calls: &ActiveToolCalls,
    pending_responses: &mut PendingSubcResponses,
    retry_buffer: &mut RetryBuffer,
    push_buffer: &mut HashMap<push::ReplayKey, VecDeque<PushFrame>>,
    shutdown: &Arc<Notify>,
    control_completion_tx: &mpsc::Sender<RouteBindCompletion>,
    metrics: &Arc<DispatchPathMetrics>,
    lifecycle_probe: Option<&SubcTestLifecycleProbe>,
    health_rollup_cache: &HealthRollupCache,
    push_senders: &PushSenders,
    dispatch: DispatchFn,
    user_config_path: Option<&Path>,
    tool_response_body_limit: usize,
) -> Result<(), SubcError> {
    let request =
        serde_json::from_slice::<ModuleControlRequest>(&frame.body).map_err(SubcError::Json)?;
    match request {
        ModuleControlRequest::RouteBind {
            route_channel,
            epoch,
            target: _,
            identity,
            principal,
            consumer_capabilities,
            admission_facts: _,
        } => {
            let route_id = route_key(route_channel, epoch);
            if epoch == 0 {
                return send_route_bind_error(
                    tx,
                    frame,
                    "config_divergence",
                    "route bind uses an invalid channel generation",
                    metrics,
                )
                .await;
            }
            let mut bind_root_id = None;
            if let Some(installed_epoch) = installed_route_epochs.get(&route_channel).copied() {
                if installed_epoch >= epoch {
                    return send_route_bind_error(
                        tx,
                        frame,
                        "config_divergence",
                        "route bind generation is not newer than the installed generation",
                        metrics,
                    )
                    .await;
                }

                let replacement_root = match ProjectRootId::from_path(&identity.project_root) {
                    Ok(root_id) => root_id,
                    Err(error) => {
                        return send_route_bind_error(
                            tx,
                            frame,
                            "config_divergence",
                            &format!("invalid route project root: {error}"),
                            metrics,
                        )
                        .await;
                    }
                };
                teardown_installed_route(
                    tx,
                    metrics,
                    executor,
                    route_key(route_channel, installed_epoch),
                    "higher-epoch RouteBind",
                    Some(&replacement_root),
                    installed_route_epochs,
                    routes,
                    root_channels,
                    bg_subs,
                    bg_sub_by_session,
                    bg_wake_pending,
                    pending_bash_asks,
                    live_roots,
                    route_bash_cancels,
                    active_tool_calls,
                    pending_responses,
                    pending_binds,
                    retry_buffer,
                    push_buffer,
                    shutdown,
                    tool_response_body_limit,
                    lifecycle_probe,
                )
                .await?;
                bind_root_id = Some(replacement_root);
            }
            if pending_binds.contains_key(&route_id) {
                return send_route_bind_error(
                    tx,
                    frame,
                    "config_divergence",
                    "route bind is already pending for channel",
                    metrics,
                )
                .await;
            }
            let bind_root_id = match bind_root_id {
                Some(root_id) => root_id,
                None => match ProjectRootId::from_path(&identity.project_root) {
                    Ok(root_id) => root_id,
                    Err(error) => {
                        return send_route_bind_error(
                            tx,
                            frame,
                            "config_divergence",
                            &format!("invalid route project root: {error}"),
                            metrics,
                        )
                        .await;
                    }
                },
            };

            // Reconcile RootConfig: build a configure request from the bind
            // identity + forwarded config tiers and run it through the executor.
            let request_id = format!("subc-bind-{route_channel}");
            let bind_project_root = identity.project_root.clone();
            let bind_harness = identity.harness.clone();
            let bind_session = identity.session.clone();
            let bind_trust = trust_for_bind(&bind_harness, &principal);
            let bind_principal_id = principal_id(&principal);
            // Typed capability declaration from the consumer: the facade stamps it
            // from the MCP host's initialize-advertised capabilities. Absent
            // means no reverse-request capability — flat deny, fail-closed. A
            // consumer over-declaring only earns asks that TTL-deny.
            let consumer_elicitation_capable = consumer_capabilities
                .as_ref()
                .is_some_and(|capabilities| capabilities.iter().any(|c| c == "elicitation"));
            log::info!(
                "subc attach: route {} harness={} principal={} trust={} elicitation={}",
                route_channel,
                bind_harness,
                principal_label(&principal),
                bind_trust.label(),
                consumer_elicitation_capable
            );

            // Config is read directly from the CortexKit user and project files;
            // wire-relayed tiers are ignored so a front cannot inject settings.
            // The resolver selects only this bind's harness override before it
            // applies the unchanged user/project trust boundary.
            let local_tiers = crate::subc_config::read_local_cortexkit_config_tiers(
                user_config_path,
                Path::new(&bind_project_root),
            );
            let config_tiers: Vec<Value> = local_tiers
                .iter()
                .map(|t| json!({ "tier": t.tier, "source": t.source, "doc": t.doc }))
                .collect();
            // Let configure return its structured invalid-harness error rather
            // than panicking while computing this optional registration setting.
            let active_harness = bind_harness.parse::<crate::harness::Harness>().ok();
            let diagnostics_on_edit = crate::config_resolve::resolve_config_for_harness(
                &local_tiers,
                active_harness.as_ref(),
            )
            .config
            .diagnostics_on_edit;
            let configure_json = json!({
                "id": request_id,
                "command": "configure",
                "project_root": bind_project_root,
                "harness": bind_harness,
                "session_id": bind_session.clone(),
                "config": config_tiers,
            });
            let configure_req = match serde_json::from_value::<RawRequest>(configure_json) {
                Ok(req) => req,
                Err(error) => {
                    return send_route_bind_error(
                        tx,
                        frame,
                        "config_divergence",
                        &format!("failed to build configure request: {error}"),
                        metrics,
                    )
                    .await;
                }
            };

            let route_identity = RouteIdentity(Arc::new(RouteIdentityData {
                root: bind_root_id.clone(),
                project_root: PathBuf::from(&bind_project_root),
                harness: bind_harness.clone(),
                session: bind_session.clone(),
                trust: bind_trust,
                spawn_principal: AuthenticatedPrincipal::RouteBind {
                    trust: bind_trust.sandbox_trust(),
                    route_channel,
                    route_epoch: epoch,
                    project_root: PathBuf::from(&bind_project_root),
                    harness: bind_harness.clone(),
                    session_id: bind_session.clone(),
                    principal_id: bind_principal_id,
                },
                consumer_elicitation_capable,
            }));
            let configure_session = route_identity.session.clone();
            let root_was_live = live_roots.contains_key(&bind_root_id);
            let inserted_new_actor = register_actor_for_bind(
                shared_app,
                executor,
                push_senders,
                &bind_root_id,
                route_channel,
                root_was_live,
            );

            let configure_request_id = configure_req.id.clone();
            installed_route_epochs.insert(route_channel, epoch);
            if let Some(meta) = live_roots.get_mut(&bind_root_id) {
                meta.maintenance_queued_kinds.clear();
                meta.maintenance_pending = meta.maintenance_jobs_in_flight > 0;
            }
            // One bind timestamp feeds both the 12-second expiry contract and
            // the queue deadline: constructing the pending bind and its
            // started_at once keeps the two clocks identical.
            let bind_started_at = Instant::now();
            let (configure_rx, configure_cancellation) = executor
                .submit_cancellable_async_with_deadline(
                    bind_root_id.clone(),
                    Lane::Mutating,
                    configure_request_id.clone(),
                    Box::new(move |ctx| {
                        log_ctx::with_session(Some(configure_session.clone()), || {
                            dispatch(configure_req, ctx)
                        })
                    }),
                    Some(bind_started_at + ROUTE_BIND_DEADLINE),
                );
            pending_binds.insert(
                route_id,
                PendingBind {
                    bind_root_id: bind_root_id.clone(),
                    inserted_new_actor,
                    cancelled: false,
                    configure_request_id: configure_request_id.clone(),
                    started_at: bind_started_at,
                    warned_half_deadline: false,
                    deadline_reported: false,
                    corr: frame.header.corr,
                    ver: frame.header.ver,
                    flags: frame.header.flags,
                    cancellation: configure_cancellation,
                },
            );

            let completion_tx = control_completion_tx.clone();
            let completion_identity = route_identity;
            let completion_root = bind_root_id.clone();
            let completion_route_channel = route_channel;
            let completion_ver = frame.header.ver;
            let completion_corr = frame.header.corr;
            let completion_flags = frame.header.flags;
            let completion_metrics = Arc::clone(metrics);
            tokio::spawn(async move {
                let _response_task = ResponseTaskGuard::new(&completion_metrics);
                let configure_response =
                    await_executor_response(configure_rx, configure_request_id.clone()).await;
                // Send the route-bind acknowledgment as soon as configure succeeds.
                // Installing completed search or callgraph builds only refreshes cached
                // read data, so a later maintenance pass can do it without delaying the
                // daemon's confirmation that the route is usable.
                let completion = RouteBindCompletion {
                    route: route_key(completion_route_channel, epoch),
                    identity: completion_identity,
                    bind_root_id: completion_root,
                    inserted_new_actor,
                    configure_response,
                    diagnostics_on_edit,
                    ver: completion_ver,
                    corr: completion_corr,
                    flags: completion_flags,
                };
                if send_counted_channel(
                    &completion_tx,
                    &completion_metrics.control_completion_queued,
                    completion,
                )
                .await
                .is_err()
                {
                    log::debug!(
                        "subc attach: dropped RouteBind completion for route {} after loop exit",
                        completion_route_channel
                    );
                }
            });

            health_rollup_cache.refresh(executor, shared_app);
            Ok(())
        }
        ModuleControlRequest::HealthCheck {} => {
            metrics.record_bg_runtime(bg_subs.len(), bg_wake_pending.len());
            let report = build_health_report(
                health_rollup_cache,
                executor,
                pending_binds,
                metrics,
                shared_app,
            );
            let body = serde_json::to_vec(&ModuleControlResponse::from(report))
                .map_err(SubcError::Json)?;
            let response = Frame::build_with_version(
                frame.header.ver,
                FrameType::Response,
                frame.header.flags,
                0,
                0,
                frame.header.corr,
                body,
            )
            .map_err(SubcError::FrameBuild)?;
            send_frame(tx, metrics, response).await
        }
    }
}

fn install_bash_compressor(ctx: &AppContext) {
    // Mirrors main.rs per-actor compressor installation for subc-created actors.
    let filter_registry_handle = ctx.shared_filter_registry();
    let compress_flag = ctx.bash_compress_flag();
    ctx.bash_background().set_compressor_with_exit_code(
        move |command: &str, output: String, exit_code: Option<i32>| {
            if !compress_flag.load(std::sync::atomic::Ordering::Relaxed) {
                return crate::compress::CompressionResult::new(output);
            }
            let registry_guard = match filter_registry_handle.read() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            crate::compress::compress_with_registry_exit_code(
                command,
                &output,
                exit_code,
                &registry_guard,
            )
        },
    );
}

async fn send_route_bind_error(
    tx: &WriterSender,
    frame: &Frame,
    code: &str,
    message: &str,
    metrics: &DispatchPathMetrics,
) -> Result<(), SubcError> {
    send_route_bind_error_parts(
        tx,
        frame.header.ver,
        frame.header.corr,
        frame.header.flags,
        code,
        message,
        metrics,
    )
    .await
}

async fn send_route_bind_error_parts(
    tx: &WriterSender,
    ver: u8,
    corr: u64,
    flags: Flags,
    code: &str,
    message: &str,
    metrics: &DispatchPathMetrics,
) -> Result<(), SubcError> {
    let response = build_error_frame(ver, 0, 0, corr, flags, code, message)?;
    send_reliable_writer_frame(tx, metrics, response, "RouteBind error").await?;
    log_route_bind_rejection(code, message);
    Ok(())
}

/// Per-message rate limit for the bind-rejection warn line. A caller that
/// re-attaches a dead root forever turns this line into the entire readable
/// tail of the SHARED daemon log (measured 2026-08-09: 2.08M copies, ~40/sec,
/// 936MB log — other modules' incident lines pushed out of the tail).
/// The line itself stays byte-identical so external counters keep matching;
/// repeats inside the window are summarized with a suppressed count on the
/// next emission (volume stays diagnosable, per the log-diet convention).
fn log_route_bind_rejection(code: &str, message: &str) {
    const WINDOW: Duration = Duration::from_secs(60);
    static SUPPRESSED: OnceLock<StdMutex<HashMap<String, (Instant, u64)>>> = OnceLock::new();
    let map = SUPPRESSED.get_or_init(|| StdMutex::new(HashMap::new()));
    let mut map = match map.try_lock() {
        Ok(map) => map,
        // Contended: log unsuppressed rather than blocking or dropping.
        Err(_) => {
            log::warn!("subc attach: route bind rejected ({code}): {message}");
            return;
        }
    };
    let now = Instant::now();
    // Bound the map: dead roots churn, and an unbounded suppression map is
    // its own leak. Sweep expired entries once it grows past a fleet-sized
    // number of distinct rejection messages.
    if map.len() > 512 {
        map.retain(|_, (start, _)| now.duration_since(*start) < WINDOW);
    }
    match map.get_mut(message) {
        Some((window_start, suppressed)) if now.duration_since(*window_start) < WINDOW => {
            *suppressed += 1;
        }
        Some((window_start, suppressed)) => {
            if *suppressed > 0 {
                log::warn!(
                    "subc attach: route bind rejected ({code}): {message} (repeated {}x in last 60s)",
                    *suppressed
                );
            } else {
                log::warn!("subc attach: route bind rejected ({code}): {message}");
            }
            *window_start = now;
            *suppressed = 0;
        }
        None => {
            log::warn!("subc attach: route bind rejected ({code}): {message}");
            map.insert(message.to_string(), (now, 0));
        }
    }
}

/// Route-channel tool call: `{name, arguments}` → executor lane → dispatch to
/// the sync command core → wrap the structured Response in a CallToolResult
/// `{content, isError}`. Tool-result mapping: the whole `{success, ...}` Response
/// serialized into ONE text block; `isError` carries `success == false`.
async fn handle_tool_call(
    tx: &WriterSender,
    frame: &Frame,
    mut phase_trace: PhaseTrace,
    routes: &HashMap<RouteChannel, RouteIdentity>,
    pending_binds: &HashMap<RouteChannel, PendingBind>,
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    executor: &Arc<Executor>,
    active_tool_calls: &ActiveToolCalls,
    pending_deferred_setups: &Arc<AtomicUsize>,
    shutdown: &Arc<Notify>,
    connection_cancel: &PersistentCancelSignal,
    bash_deferred_tx: &mpsc::Sender<bash::BashDeferredCompletion>,
    bash_poll_touch_tx: &mpsc::Sender<ProjectRootId>,
    metrics: &Arc<DispatchPathMetrics>,
    route_bash_cancels: &mut HashMap<RouteChannel, bash::RouteBashCancel>,
    pending_bash_asks: &mut HashMap<ReverseCorrKey, PendingBashAsk>,
    next_bash_ask_corr: &mut u64,
    bg_subs: &mut HashMap<RouteChannel, BgSub>,
    bg_sub_by_session: &mut BgSubsBySession,
    bg_wake_pending: &mut HashSet<RouteChannel>,
    bg_wake_epoch: &mut HashMap<(ProjectRootId, String), u64>,
    dispatch: DispatchFn,
    deferred_response_tx: &mpsc::UnboundedSender<PendingSubcResponse>,
    allow_native_passthrough: bool,
    tool_response_body_limit: usize,
) -> Result<(), SubcError> {
    let route_id = route_key(frame.header.channel, frame.header.epoch);
    if pending_binds.contains_key(&route_id) {
        let error = build_error_frame(
            frame.header.ver,
            frame.header.channel,
            frame.header.epoch,
            frame.header.corr,
            frame.header.flags,
            "route_not_bound",
            "route is not bound before tool call",
        )?;
        return send_reliable_writer_frame(tx, metrics, error, "route_not_bound error").await;
    }

    let Some(identity) = routes.get(&route_id).cloned() else {
        let error = build_error_frame(
            frame.header.ver,
            frame.header.channel,
            frame.header.epoch,
            frame.header.corr,
            frame.header.flags,
            "route_not_bound",
            "route is not bound before tool call",
        )?;
        return send_reliable_writer_frame(tx, metrics, error, "route_not_bound error").await;
    };
    let restore_watcher = live_roots
        .get(&identity.root)
        .is_some_and(|meta| meta.idle_artifacts_evicted);
    if let Some(meta) = live_roots.get_mut(&identity.root) {
        meta.reactivate_bound();
    }
    if restore_watcher {
        if let Some(ctx) = executor.actor_context(&identity.root) {
            crate::commands::configure::ensure_project_watcher(&ctx);
        }
    }

    let route_request =
        serde_json::from_slice::<RouteRequest>(&frame.body).map_err(SubcError::Json)?;
    if matches!(
        route_request,
        RouteRequest::BgEvents(BgEventsRequest {
            op: BgEventsOp::BgEvents
        })
    ) {
        if let Some(old_sub) = bg_subs.get(&route_id).cloned() {
            metrics.record_bg_subscription_ended(
                &old_sub.root,
                &old_sub.session,
                route_id,
                "resubscribe",
            );
            push::send_reliable_bg_stream_end(tx, metrics, route_id, &old_sub).await?;
        }
        if !identity.trust.allows_bash_observation() {
            bg_subs.remove(&route_id);
            bg_wake_pending.remove(&route_id);
            remove_bg_subscription_index(bg_sub_by_session, route_id, Some(&identity));
            let denied_sub = BgSub {
                corr: frame.header.corr,
                ver: frame.header.ver,
                flags: frame.header.flags,
                root: identity.root.clone(),
                session: identity.session.clone(),
            };
            metrics.record_bg_subscription_ended(
                &identity.root,
                &identity.session,
                route_id,
                "subscribe-denied",
            );
            push::send_reliable_bg_stream_end(tx, metrics, route_id, &denied_sub).await?;
            return Ok(());
        }
        bg_subs.insert(
            route_id,
            BgSub {
                corr: frame.header.corr,
                ver: frame.header.ver,
                flags: frame.header.flags,
                root: identity.root.clone(),
                session: identity.session.clone(),
            },
        );
        insert_bg_subscription_index(
            bg_sub_by_session,
            identity.root.clone(),
            identity.session.clone(),
            route_id,
        );
        metrics.record_bg_subscription_installed(&identity.root, &identity.session, route_id);
        push::arm_bg_wake(
            identity.root.clone(),
            identity.session.clone(),
            route_id,
            bg_wake_pending,
            bg_wake_epoch,
            metrics,
        );
        return Ok(());
    }

    let RouteRequest::ToolCall(call) = route_request else {
        unreachable!("background event subscription returned above")
    };
    let bare_name = call.name;
    let arguments = strip_agent_preview_arg_owned(call.arguments);
    let request_id = format!("subc-{}-{}", frame.header.channel, frame.header.corr);
    // Convert the caller's remaining budget into ONE absolute local deadline
    // before permission elicitation or executor admission. Zero is rejected
    // here with the logical response; values above the cap clamp to the cap.
    let request_deadline = match normalize_request_deadline(call.deadline_ms_remaining, &request_id)
    {
        Ok(deadline) => deadline,
        Err(response) => {
            let text = crate::subc_format::format_response_with_context(
                &bare_name,
                &response,
                &crate::subc_format::FormatContext::from_tool_call(
                    &bare_name,
                    &arguments,
                    identity.project_root.as_path(),
                ),
            );
            let result = ToolCallResult { text, response };
            let response_frame = build_tool_response_frame_with_limit(
                frame.header.ver,
                route_id,
                frame.header.corr,
                frame.header.flags,
                &result,
                identity.trust,
                tool_response_body_limit,
            )?;
            return send_reliable_writer_frame(tx, metrics, response_frame, "tool response").await;
        }
    };
    let format_context = crate::subc_format::FormatContext::from_tool_call(
        &bare_name,
        &arguments,
        identity.project_root.as_path(),
    );

    let bind_trust = identity.trust;
    let diagnostics_on_edit = live_roots
        .get(&identity.root)
        .map(|meta| meta.diagnostics_on_edit)
        .unwrap_or(false);

    let requests_host = matches!(bare_name.as_str(), "bash" | "powershell")
        && arguments
            .get("sandbox")
            .or_else(|| {
                arguments
                    .get("params")
                    .and_then(|params| params.get("sandbox"))
            })
            .and_then(Value::as_str)
            == Some("host");
    if matches!(bind_trust, BindTrust::Untrusted) && requests_host {
        let response = Response::error(
            request_id.clone(),
            "sandbox_escalation_denied",
            "sandbox host escalation is unavailable to untrusted principals",
        );
        let text = crate::subc_format::format_response_with_context(
            &bare_name,
            &response,
            &format_context,
        );
        let result = ToolCallResult { text, response };
        let response_frame = build_tool_response_frame_with_limit(
            frame.header.ver,
            route_id,
            frame.header.corr,
            frame.header.flags,
            &result,
            bind_trust,
            tool_response_body_limit,
        )?;
        return send_reliable_writer_frame(tx, metrics, response_frame, "tool response").await;
    }

    if matches!(bind_trust, BindTrust::Untrusted)
        && is_bash_family_tool(&bare_name)
        && (!matches!(bare_name.as_str(), "bash" | "powershell")
            || !identity.consumer_elicitation_capable)
    {
        let response = bash::bash_denied_untrusted_response(request_id.clone());
        let text = crate::subc_format::format_response_with_context(
            &bare_name,
            &response,
            &format_context,
        );
        let result = ToolCallResult { text, response };
        let response_frame = build_tool_response_frame_with_limit(
            frame.header.ver,
            route_id,
            frame.header.corr,
            frame.header.flags,
            &result,
            bind_trust,
            tool_response_body_limit,
        )?;
        return send_reliable_writer_frame(tx, metrics, response_frame, "tool response").await;
    }

    // A non-core name is NOT in the tool manifest. AFT fails closed and
    // does not trust subc to enforce the manifest: rejecting here is the
    // defense-in-depth backstop that prevents a forwarded native command
    // (e.g. `configure`, which would reach handle_configure and bypass
    // the RouteBind config-trust cap) from ever reaching dispatch. Only
    // the integration-test harness (run_subc_mode_for_test) opens this to
    // drive synthetic native commands through the executor.
    if !is_subc_agent_core_tool(&bare_name)
        && !is_subc_native_plumbing_tool(&bare_name)
        && !allow_native_passthrough
    {
        log::warn!(
            "subc tool call: rejecting non-manifest tool name {:?} on route {} (fail-closed)",
            bare_name,
            frame.header.channel
        );
        let response = Response::error(
            request_id.clone(),
            "unknown_tool",
            format!("tool {:?} is not in the AFT tool manifest", bare_name),
        );
        let text = crate::subc_format::format_response_with_context(
            &bare_name,
            &response,
            &format_context,
        );
        let result = ToolCallResult { text, response };
        let response_frame = build_tool_response_frame_with_limit(
            frame.header.ver,
            route_id,
            frame.header.corr,
            frame.header.flags,
            &result,
            bind_trust,
            tool_response_body_limit,
        )?;
        return send_reliable_writer_frame(tx, metrics, response_frame, "tool response").await;
    }

    if matches!(bare_name.as_str(), "bash" | "powershell") {
        if matches!(bind_trust, BindTrust::Untrusted) {
            let plan = match bash::prepare_bash_elicitation_plan(
                &arguments,
                identity.project_root.as_path(),
            ) {
                Ok(plan) => plan,
                Err(error) => {
                    let response = Response::error(request_id.clone(), error.code, error.message);
                    let text = crate::subc_format::format_response_with_context(
                        &bare_name,
                        &response,
                        &format_context,
                    );
                    let result = ToolCallResult { text, response };
                    let response_frame = build_tool_response_frame_with_limit(
                        frame.header.ver,
                        route_id,
                        frame.header.corr,
                        frame.header.flags,
                        &result,
                        bind_trust,
                        tool_response_body_limit,
                    )?;
                    return send_reliable_writer_frame(
                        tx,
                        metrics,
                        response_frame,
                        "tool response",
                    )
                    .await;
                }
            };

            let reverse_corr =
                allocate_reverse_corr(pending_bash_asks, route_id, next_bash_ask_corr);
            let ask_frame = build_bash_elicitation_request_frame(
                frame.header.ver,
                route_id,
                reverse_corr,
                frame.header.flags,
                &plan.command,
                &plan.asks,
            )?;

            let meta = live_roots
                .entry(identity.root.clone())
                .or_insert_with(|| RootMeta::new(Instant::now()));
            meta.active_bash_waits = meta.active_bash_waits.saturating_add(1);
            meta.reactivate_bound();

            let route_cancel =
                route_bash_cancels
                    .entry(route_id)
                    .or_insert_with(|| bash::RouteBashCancel {
                        token: PersistentCancelSignal::new(),
                        active_waits: 0,
                    });
            route_cancel.active_waits = route_cancel.active_waits.saturating_add(1);
            let cancel = bash::BashWaitCancel {
                connection: connection_cancel.clone(),
                route: route_cancel.token.clone(),
            };
            pending_bash_asks.insert(
                ReverseCorrKey {
                    route: route_id,
                    corr: reverse_corr,
                },
                PendingBashAsk {
                    route: route_id,
                    tool_corr: frame.header.corr,
                    tool_flags: frame.header.flags,
                    tool_ver: frame.header.ver,
                    root: identity.root.clone(),
                    project_root: identity.project_root.clone(),
                    session_id: identity.session.clone(),
                    spawn_principal: identity.spawn_principal.clone(),
                    edit_slot_survives: call.edit_slot_survives,
                    request_id,
                    arguments,
                    format_context,
                    cancel,
                    grants: plan.grants,
                    expires_at: Instant::now() + bash_elicitation_timeout(),
                    request_deadline,
                },
            );
            return send_reliable_writer_frame(tx, metrics, ask_frame, "bash elicitation request")
                .await;
        }

        let meta = live_roots
            .entry(identity.root.clone())
            .or_insert_with(|| RootMeta::new(Instant::now()));
        meta.active_bash_waits = meta.active_bash_waits.saturating_add(1);
        meta.reactivate_bound();

        let route_cancel =
            route_bash_cancels
                .entry(route_id)
                .or_insert_with(|| bash::RouteBashCancel {
                    token: PersistentCancelSignal::new(),
                    active_waits: 0,
                });
        route_cancel.active_waits = route_cancel.active_waits.saturating_add(1);
        let cancel = bash::BashWaitCancel {
            connection: connection_cancel.clone(),
            route: route_cancel.token.clone(),
        };

        bash::submit_deferred_bash(
            executor,
            bash_deferred_tx,
            bash_poll_touch_tx,
            metrics,
            dispatch,
            identity.root.clone(),
            identity.project_root.clone(),
            identity.session.clone(),
            request_id,
            route_id,
            frame.header.corr,
            frame.header.flags,
            frame.header.ver,
            arguments,
            format_context,
            cancel,
            bind_trust,
            identity.spawn_principal.clone(),
            call.edit_slot_survives,
            None,
            request_deadline,
        );
        return Ok(());
    }

    let lane = command_lane(&bare_name);
    let tool_call_context = ToolCallContext {
        project_root: identity.project_root.clone(),
        session_id: Some(identity.session.clone()),
        request_id: request_id.clone(),
        diagnostics_on_edit,
        preview: call.preview,
        edit_slot_survives: call.edit_slot_survives,
        report_registration_downgrade: true,
    };

    let uses_deferred_response_seam = bare_name == "inspect"
        || crate::commands::lsp_navigation::is_lsp_navigation_command(&bare_name);
    if uses_deferred_response_seam {
        let Some(deferred_ctx) = executor.actor_context(&identity.root) else {
            let response = Response::error(
                &request_id,
                "actor_not_registered",
                "executor actor is not registered",
            );
            let text = crate::subc_format::format_response_with_context(
                &bare_name,
                &response,
                &format_context,
            );
            let result = ToolCallResult { text, response };
            let response_frame = build_tool_response_frame_with_limit(
                frame.header.ver,
                route_id,
                frame.header.corr,
                frame.header.flags,
                &result,
                bind_trust,
                tool_response_body_limit,
            )?;
            return send_reliable_writer_frame(tx, metrics, response_frame, "tool response").await;
        };
        let identity_for_run = identity.clone();
        let request_id_for_force = request_id.clone();
        let format_context_for_run = format_context.clone();
        let bare_name_for_run = bare_name.clone();
        let (setup_tx, setup_rx) = oneshot::channel::<DeferredSetupOutcome>();
        phase_trace.mark_executor_submitted();
        let job: crate::executor::ExecutorJob = Box::new(move |ctx| {
            phase_trace.mark_job_admitted();
            log_ctx::with_session(Some(identity_for_run.session.clone()), || {
                let run = || match prepare_tool_call(
                    &bare_name_for_run,
                    arguments,
                    &format_context_for_run,
                    &tool_call_context,
                    ctx,
                    Some(&mut phase_trace),
                ) {
                    Err(result) => {
                        let response = result.response;
                        let _ = setup_tx.send(DeferredSetupOutcome::Immediate {
                            text: result.text,
                            phase_trace,
                        });
                        response
                    }
                    Ok(prepared) => {
                        let outcome = if bare_name_for_run == "inspect" {
                            crate::commands::inspect::handle_inspect_deferred_with_restriction(
                                &prepared.request,
                                Arc::clone(&deferred_ctx),
                                matches!(bind_trust, BindTrust::Untrusted),
                            )
                        } else {
                            crate::commands::lsp_navigation::handle_lsp_navigation_deferred_with_restriction(
                                &prepared.request,
                                Arc::clone(&deferred_ctx),
                                matches!(bind_trust, BindTrust::Untrusted),
                            )
                        };
                        match outcome {
                            DispatchOutcome::Deferred(pending) => {
                                let _ = setup_tx.send(DeferredSetupOutcome::Deferred {
                                    pending,
                                    surface_downgraded: prepared.surface_downgraded,
                                    phase_trace,
                                });
                                Response::success(
                                    request_id_for_force.clone(),
                                    json!({ "response_deferred": true }),
                                )
                            }
                            DispatchOutcome::Immediate(response) => {
                                phase_trace.mark_execute_done();
                                let finalizer = |response: &mut Response| {
                                    crate::response_finalize::finalize_response_with_bg_completions(
                                        response,
                                        ctx,
                                        &identity_for_run.session,
                                        &bare_name_for_run,
                                        bind_trust.allows_bash_observation(),
                                    );
                                };
                                let result = finish_tool_call_response(
                                    &bare_name_for_run,
                                    &format_context_for_run,
                                    response,
                                    prepared.surface_downgraded,
                                    Some(&finalizer),
                                    Some(&mut phase_trace),
                                );
                                let response = result.response;
                                let _ = setup_tx.send(DeferredSetupOutcome::Immediate {
                                    text: result.text,
                                    phase_trace,
                                });
                                response
                            }
                        }
                    }
                };
                if matches!(bind_trust, BindTrust::Untrusted) {
                    ctx.with_force_restrict(&request_id_for_force, run)
                } else {
                    run()
                }
            })
        });
        let deferred_setup_guard =
            PendingDeferredSetupGuard::new(Arc::clone(pending_deferred_setups));
        let rx = submit_active_tool_call(
            executor.as_ref(),
            active_tool_calls,
            route_id,
            frame.header.corr,
            identity.root.clone(),
            lane,
            request_id.clone(),
            RouteDetachPolicy::CancelOnDetach,
            job,
            request_deadline,
        );
        let completion_tx = tx.clone();
        let completion_shutdown = Arc::clone(shutdown);
        let completion_metrics = Arc::clone(metrics);
        let active_tool_calls = Arc::clone(active_tool_calls);
        let deferred_response_tx = deferred_response_tx.clone();
        let route = route_id;
        let corr = frame.header.corr;
        let flags = frame.header.flags;
        let ver = frame.header.ver;
        let root = identity.root.clone();
        let session_id = identity.session.clone();
        tokio::spawn(async move {
            let _response_task = ResponseTaskGuard::new(&completion_metrics);
            let _deferred_setup = deferred_setup_guard;
            let response = await_executor_response(rx, request_id.clone()).await;
            match setup_rx.await {
                Ok(DeferredSetupOutcome::Deferred {
                    pending,
                    surface_downgraded,
                    phase_trace,
                }) => {
                    let pending = PendingSubcResponse {
                        route,
                        corr,
                        flags,
                        ver,
                        root,
                        session_id,
                        bare_name,
                        format_context,
                        bind_trust,
                        pending,
                        surface_downgraded,
                        phase_trace,
                    };
                    if let Err(error) = deferred_response_tx.send(pending) {
                        if let Some(cancellation) = &error.0.pending.cancellation {
                            cancellation.request_cancel();
                        }
                        finish_active_tool_call(&active_tool_calls, route, corr);
                    }
                }
                Ok(DeferredSetupOutcome::Immediate { text, phase_trace }) => {
                    if !finish_active_tool_call(&active_tool_calls, route, corr) {
                        return;
                    }
                    let result = ToolCallResult { text, response };
                    let fatal = response_is_fatal_panic(&result.response);
                    match build_tool_response_frame_with_limit(
                        ver,
                        route,
                        corr,
                        flags,
                        &result,
                        bind_trust,
                        tool_response_body_limit,
                    ) {
                        Ok(response_frame) => {
                            let trace = ToolResponseWriteTrace::new(
                                phase_trace,
                                bare_name.clone(),
                                identity.project_root.clone(),
                                identity.session.clone(),
                                route.channel,
                                corr,
                            );
                            if let Err(error) = send_traced_tool_response_frame(
                                &completion_tx,
                                &completion_metrics,
                                response_frame,
                                trace,
                            )
                            .await
                            {
                                log::warn!(
                                    "subc attach: failed to queue deferred-seam setup response: {error}"
                                );
                            }
                        }
                        Err(error) => {
                            log::error!(
                                "subc attach: failed to build deferred-seam setup response: {error}"
                            );
                        }
                    }
                    if fatal {
                        signal_fatal_teardown(
                            &completion_tx,
                            Some(route),
                            ver,
                            corr,
                            &completion_shutdown,
                            &completion_metrics,
                        )
                        .await;
                    }
                }
                Err(_) => {
                    if !finish_active_tool_call(&active_tool_calls, route, corr) {
                        return;
                    }
                    let text = crate::subc_format::format_response_with_context(
                        &bare_name,
                        &response,
                        &format_context,
                    );
                    let result = ToolCallResult { text, response };
                    if let Ok(response_frame) = build_tool_response_frame_with_limit(
                        ver,
                        route,
                        corr,
                        flags,
                        &result,
                        bind_trust,
                        tool_response_body_limit,
                    ) {
                        let _ = send_reliable_writer_frame(
                            &completion_tx,
                            &completion_metrics,
                            response_frame,
                            "deferred setup failure",
                        )
                        .await;
                    }
                }
            }
        });
        return Ok(());
    }

    let bare_name_for_frame = bare_name.clone();
    let identity_for_run = identity.clone();
    let completion_session = identity.session.clone();
    let completion_root = identity.project_root.clone();
    let request_id_for_force = request_id.clone();
    let format_context_for_frame = format_context.clone();
    let (tool_call_tx, tool_call_rx) = oneshot::channel::<ToolCallCompletion>();
    phase_trace.mark_executor_submitted();
    let job: crate::executor::ExecutorJob = Box::new(move |ctx| {
        phase_trace.mark_job_admitted();
        log_ctx::with_session(Some(identity_for_run.session.clone()), || {
            let run = || {
                let finalizer = |response: &mut Response| {
                    crate::response_finalize::finalize_response_with_bg_completions(
                        response,
                        ctx,
                        &identity_for_run.session,
                        &bare_name,
                        bind_trust.allows_bash_observation(),
                    );
                };
                match run_tool_call(
                    &bare_name,
                    arguments,
                    &format_context,
                    &tool_call_context,
                    ctx,
                    &dispatch,
                    Some(&finalizer),
                    Some(&mut phase_trace),
                ) {
                    ToolCallOutcome::Unary(result) => {
                        let response = result.response;
                        let _ = tool_call_tx.send(ToolCallCompletion {
                            text: result.text,
                            phase_trace,
                        });
                        response
                    }
                }
            };
            if matches!(bind_trust, BindTrust::Untrusted) {
                ctx.with_force_restrict(&request_id_for_force, run)
            } else {
                run()
            }
        })
    });
    let rx = submit_active_tool_call(
        executor.as_ref(),
        active_tool_calls,
        route_id,
        frame.header.corr,
        identity.root.clone(),
        lane,
        request_id.clone(),
        RouteDetachPolicy::RetainForReplay,
        job,
        request_deadline,
    );
    let completion_tx = tx.clone();
    let completion_shutdown = Arc::clone(shutdown);
    let route = route_id;
    let corr = frame.header.corr;
    let flags = frame.header.flags;
    let ver = frame.header.ver;
    let completion_metrics = Arc::clone(metrics);
    let active_tool_calls = Arc::clone(active_tool_calls);
    tokio::spawn(async move {
        let _response_task = ResponseTaskGuard::new(&completion_metrics);
        let response = await_executor_response(rx, request_id.clone()).await;
        let (text, phase_trace) = match tool_call_rx.await {
            Ok(completion) => (completion.text, Some(completion.phase_trace)),
            Err(_) => (
                crate::subc_format::format_response_with_context(
                    &bare_name_for_frame,
                    &response,
                    &format_context_for_frame,
                ),
                None,
            ),
        };
        if !finish_active_tool_call(&active_tool_calls, route, corr) {
            return;
        }
        let result = ToolCallResult { text, response };
        let fatal = response_is_fatal_panic(&result.response);
        match build_tool_response_frame_with_limit(
            ver,
            route,
            corr,
            flags,
            &result,
            bind_trust,
            tool_response_body_limit,
        ) {
            Ok(response_frame) => {
                let send_result = if let Some(phase_trace) = phase_trace {
                    let trace = ToolResponseWriteTrace::new(
                        phase_trace,
                        bare_name_for_frame,
                        completion_root,
                        completion_session,
                        route.channel,
                        corr,
                    );
                    send_traced_tool_response_frame(
                        &completion_tx,
                        &completion_metrics,
                        response_frame,
                        trace,
                    )
                    .await
                } else {
                    send_reliable_writer_frame(
                        &completion_tx,
                        &completion_metrics,
                        response_frame,
                        "tool response",
                    )
                    .await
                };
                if let Err(error) = send_result {
                    log::warn!("subc attach: failed to queue tool response frame: {error}");
                }
            }
            Err(error) => {
                log::error!("subc attach: failed to build tool response frame: {error}");
            }
        }
        if fatal {
            signal_fatal_teardown(
                &completion_tx,
                Some(route),
                ver,
                corr,
                &completion_shutdown,
                &completion_metrics,
            )
            .await;
        }
    });
    Ok(())
}

fn submit_maintenance_job(
    executor: &Arc<Executor>,
    root_id: ProjectRootId,
    kind: MaintenanceDrainKind,
    bg_sessions_to_check: Vec<(String, u64)>,
    completion_tx: &mpsc::Sender<MaintenanceCompletion>,
    metrics: &Arc<DispatchPathMetrics>,
) {
    let request_id = format!(
        "subc-maintenance-drain-{}-{}",
        kind.label(),
        root_id.as_path().to_string_lossy()
    );
    let response_id = request_id.clone();
    let completion_root_id = root_id.clone();
    let maintenance_generation = executor
        .actor_context(&root_id)
        .map(|ctx| ctx.configure_generation())
        .unwrap_or(0);
    let (outcome_tx, outcome_rx) = oneshot::channel::<MaintenanceJobOutcome>();
    // Deferred drains mutate subsystem state behind each subsystem's own lock.
    // Keeping every drain on MaintenanceCommit lets interactive reads and lazy
    // HeavyInit queries run while maintenance converges after the bind ack.
    let lane = Lane::MaintenanceCommit;
    let job: crate::executor::ExecutorJob = Box::new(move |ctx: &AppContext| {
        let outcome = match kind {
            MaintenanceDrainKind::Watcher => {
                let drained = runtime_drain::drain_watcher_events_bounded(
                    ctx,
                    runtime_drain::WATCHER_PATH_DRAIN_BATCH_CAP,
                );
                MaintenanceJobOutcome {
                    empty_bg_sessions: Vec::new(),
                    requeue_kind: drained.has_more.then_some(kind),
                }
            }
            MaintenanceDrainKind::Lsp => {
                let drained = runtime_drain::drain_lsp_events_bounded(
                    ctx,
                    runtime_drain::LSP_EVENT_DRAIN_BATCH_CAP,
                );
                MaintenanceJobOutcome {
                    empty_bg_sessions: Vec::new(),
                    requeue_kind: drained.has_more.then_some(kind),
                }
            }
            MaintenanceDrainKind::ConfigureTail => {
                runtime_drain::drain_deferred_configure_maintenance(ctx);
                runtime_drain::drain_configure_warning_events(ctx);
                MaintenanceJobOutcome::default()
            }
            MaintenanceDrainKind::CompletionDrains => {
                runtime_drain::drain_search_index_events(ctx);
                runtime_drain::drain_callgraph_store_events(ctx);
                runtime_drain::drain_semantic_index_events(ctx);
                runtime_drain::drain_semantic_refresh_events(ctx);
                runtime_drain::drain_inspect_events_for_generation(ctx, maintenance_generation);
                let empty_bg_sessions = bg_sessions_to_check
                    .into_iter()
                    .filter(|(session, _)| {
                        !ctx.bash_background()
                            .has_completions_for_session(Some(session.as_str()))
                    })
                    .collect();
                MaintenanceJobOutcome {
                    empty_bg_sessions,
                    requeue_kind: None,
                }
            }
        };
        let requeued = outcome.requeue_kind.is_some();
        let _ = outcome_tx.send(outcome);
        Response::success(
            response_id,
            json!({ "drained": true, "kind": kind.label(), "requeued": requeued }),
        )
    });
    let rx = match kind {
        MaintenanceDrainKind::Watcher => executor.submit_coalescable_maintenance_async(
            root_id,
            lane,
            request_id.clone(),
            crate::executor::MaintenanceCoalesceKey::WatcherDrain,
            job,
        ),
        MaintenanceDrainKind::Lsp => executor.submit_coalescable_maintenance_async(
            root_id,
            lane,
            request_id.clone(),
            crate::executor::MaintenanceCoalesceKey::LspDrain,
            job,
        ),
        MaintenanceDrainKind::ConfigureTail | MaintenanceDrainKind::CompletionDrains => {
            executor.submit_maintenance_async(root_id, lane, request_id.clone(), job)
        }
    };
    let completion_tx = completion_tx.clone();
    let completion_metrics = Arc::clone(metrics);
    tokio::spawn(async move {
        let _response_task = ResponseTaskGuard::new(&completion_metrics);
        let response = await_executor_response(rx, request_id).await;
        let outcome = outcome_rx.await.unwrap_or_default();
        let _ = send_counted_channel(
            &completion_tx,
            &completion_metrics.maintenance_queued,
            MaintenanceCompletion {
                root_id: completion_root_id,
                kind,
                response,
                empty_bg_sessions: outcome.empty_bg_sessions,
                requeue_kind: outcome.requeue_kind,
            },
        )
        .await;
    });
}

async fn await_executor_response(rx: oneshot::Receiver<Response>, request_id: String) -> Response {
    rx.await
        .unwrap_or_else(|_| Response::error(request_id, "internal_error", "executor dropped"))
}

async fn deliver_resolved_subc_response(
    tx: &WriterSender,
    mut resolved: ResolvedSubcResponse,
    routes: &HashMap<RouteChannel, RouteIdentity>,
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    executor: &Executor,
    active_tool_calls: &ActiveToolCalls,
    shutdown: &Arc<Notify>,
    metrics: &DispatchPathMetrics,
    tool_response_body_limit: usize,
) -> Result<(), SubcError> {
    let entry = &mut resolved.entry;
    finish_active_tool_call(active_tool_calls, entry.route, entry.corr);
    if let Some(meta) = live_roots.get_mut(&entry.root) {
        meta.note_activity();
    }

    let Some(identity) = routes.get(&entry.route) else {
        log::debug!(
            "subc attach: dropping deferred {} response {} for unbound route {}",
            entry.bare_name,
            entry.pending.request_id,
            entry.route
        );
        return Ok(());
    };
    let Some(ctx) = executor.actor_context(&entry.root) else {
        return Ok(());
    };
    entry.phase_trace.mark_execute_done();
    let finalizer = |response: &mut Response| {
        crate::response_finalize::finalize_response_with_bg_completions(
            response,
            &ctx,
            &entry.session_id,
            &entry.bare_name,
            entry.bind_trust.allows_bash_observation(),
        );
    };
    let result = finish_tool_call_response(
        &entry.bare_name,
        &entry.format_context,
        resolved.response,
        entry.surface_downgraded,
        Some(&finalizer),
        Some(&mut entry.phase_trace),
    );
    let fatal = response_is_fatal_panic(&result.response);
    let response_frame = build_tool_response_frame_with_limit(
        entry.ver,
        entry.route,
        entry.corr,
        entry.flags,
        &result,
        identity.trust,
        tool_response_body_limit,
    )?;
    let trace = ToolResponseWriteTrace::new(
        std::mem::replace(&mut entry.phase_trace, PhaseTrace::new(Instant::now())),
        entry.bare_name.clone(),
        identity.project_root.clone(),
        entry.session_id.clone(),
        entry.route.channel,
        entry.corr,
    );
    send_traced_tool_response_frame(tx, metrics, response_frame, trace).await?;
    if fatal {
        signal_fatal_teardown(
            tx,
            Some(entry.route),
            entry.ver,
            entry.corr,
            shutdown,
            metrics,
        )
        .await;
    }
    Ok(())
}

async fn signal_fatal_teardown(
    tx: &WriterSender,
    route: Option<RouteChannel>,
    ver: u8,
    corr: u64,
    shutdown: &Arc<Notify>,
    metrics: &DispatchPathMetrics,
) {
    if let Some(route) = route {
        if let Ok(frame) = build_goodbye_frame(ver, route.channel, route.epoch, corr) {
            if let Err(error) = send_frame(tx, metrics, frame).await {
                log::warn!(
                    "subc attach: failed to queue fatal route Goodbye for route {route}: {error}"
                );
            }
        }
    }
    if let Ok(frame) = build_goodbye_frame(ver, 0, 0, 0) {
        if let Err(error) = send_frame(tx, metrics, frame).await {
            log::warn!("subc attach: failed to queue fatal channel-0 Goodbye: {error}");
        }
    }
    shutdown.notify_one();
}
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RouteRequest {
    BgEvents(BgEventsRequest),
    ToolCall(ToolCallRequest),
}

#[derive(Debug, Deserialize)]
struct BgEventsRequest {
    op: BgEventsOp,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BgEventsOp {
    BgEvents,
}

#[derive(Debug, Deserialize)]
struct ToolCallRequest {
    name: String,
    #[serde(default)]
    arguments: Value,
    /// Host-computed registration fact; kept outside agent-controlled arguments.
    #[serde(default)]
    edit_slot_survives: Option<bool>,
    /// Server-owned preview control (B1c-0): the plugin's mutation flow is
    /// preview -> permission ask -> apply. Dropping this field made "preview"
    /// calls mutate disk before the permission prompt and the subsequent
    /// apply fail with not-found.
    #[serde(default)]
    preview: bool,
    /// Transport metadata generated by `SubcTransportPool`: the caller's
    /// remaining request budget in milliseconds. Trusted only as a time
    /// budget, never as scheduling authority; untrusted binds get the same
    /// cap while the server keeps owning lane and trust restrictions.
    #[serde(default)]
    deadline_ms_remaining: Option<u64>,
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::bash_background::BgTaskStatus;
    use crate::protocol::{
        BashCompletedFrame, BashLongRunningFrame, BashPatternMatchFrame, ConfigureWarningsFrame,
        ProgressFrame, StatusChangedFrame,
    };
    use serde_json::json;

    pub(super) fn test_root(name: &str) -> (tempfile::TempDir, ProjectRootId) {
        let dir = tempfile::Builder::new()
            .prefix(name)
            .tempdir()
            .expect("temp root");
        let root = ProjectRootId::from_path(dir.path()).expect("project root id");
        (dir, root)
    }

    pub(super) fn test_ctx() -> Arc<AppContext> {
        Arc::new(AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config::default(),
        ))
    }

    fn inspect_context(root: &Path) -> Arc<AppContext> {
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
        serde_json::from_value(json!({ "id": id, "command": "inspect" })).expect("inspect request")
    }

    fn submit_deferred_inspect_setup(
        executor: &Arc<Executor>,
        root: &ProjectRootId,
        ctx: &Arc<AppContext>,
        request_id: &str,
    ) -> (PendingResponse, JobCancellation) {
        let (pending_tx, pending_rx) = std::sync::mpsc::sync_channel(1);
        let request = inspect_request(request_id);
        let inspect_ctx = Arc::clone(ctx);
        let (_rx, cancellation) = executor.submit_cancellable_async(
            root.clone(),
            Lane::SerialLspStatus,
            request_id.to_string(),
            Box::new(move |_| {
                let DispatchOutcome::Deferred(pending) =
                    crate::commands::inspect::handle_inspect_deferred_with_restriction(
                        &request,
                        inspect_ctx,
                        true,
                    )
                else {
                    panic!("inspect setup must defer")
                };
                pending_tx.send(pending).expect("send pending inspect");
                Response::success("inspect-setup", json!({}))
            }),
        );
        let pending = pending_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("inspect setup leaves the executor lane");
        let deadline = Instant::now() + Duration::from_secs(1);
        while !executor.actor_is_idle(root) {
            assert!(
                Instant::now() < deadline,
                "inspect setup kept lane counters live"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        (pending, cancellation)
    }

    fn wait_for_inspect_terminal(pending: &mut PendingResponse, ctx: &AppContext) -> Response {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(response) = (pending.poll)(ctx) {
                return response;
            }
            assert!(Instant::now() < deadline, "inspect terminal timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn cold_navigation_context(root: &Path) -> (Arc<AppContext>, PathBuf) {
        let source_dir = root.join("src");
        std::fs::create_dir_all(&source_dir).expect("create source dir");
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"fixture\"\n")
            .expect("write Cargo manifest");
        let source = source_dir.join("main.rs");
        std::fs::write(&source, "fn main() {}\n").expect("write source");
        let binary = root.join("cold-navigation-server");
        std::fs::write(&binary, b"fixture").expect("write server placeholder");

        let ctx = inspect_context(root);
        ctx.lsp()
            .override_binary(crate::lsp::registry::ServerKind::Rust, binary);
        (ctx, source)
    }

    fn navigation_request(id: &str, source: &Path) -> RawRequest {
        serde_json::from_value(json!({
            "id": id,
            "command": "lsp_hover",
            "file": source.display().to_string(),
            "line": 1,
            "character": 1,
        }))
        .expect("navigation request")
    }

    fn submit_deferred_navigation_setup(
        executor: &Arc<Executor>,
        root: &ProjectRootId,
        ctx: &Arc<AppContext>,
        source: &Path,
        request_id: &str,
    ) -> (PendingResponse, JobCancellation) {
        let (pending_tx, pending_rx) = std::sync::mpsc::sync_channel(1);
        let request = navigation_request(request_id, source);
        let navigation_ctx = Arc::clone(ctx);
        let (_rx, cancellation) = executor.submit_cancellable_async(
            root.clone(),
            Lane::SerialLspStatus,
            request_id.to_string(),
            Box::new(move |_| {
                let DispatchOutcome::Deferred(pending) = crate::commands::lsp_navigation::
                    handle_lsp_navigation_deferred_with_restriction(
                        &request,
                        navigation_ctx,
                        true,
                    )
                else {
                    panic!("cold navigation setup must defer")
                };
                pending_tx.send(pending).expect("send pending navigation");
                Response::success("navigation-setup", json!({}))
            }),
        );
        let pending = pending_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("navigation setup leaves the executor lane");
        let deadline = Instant::now() + Duration::from_secs(1);
        while !executor.actor_is_idle(root) {
            assert!(
                Instant::now() < deadline,
                "navigation setup kept lane counters live"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        (pending, cancellation)
    }

    fn wait_for_navigation_terminal(pending: &mut PendingResponse, ctx: &AppContext) -> Response {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if let Some(response) = (pending.poll)(ctx) {
                return response;
            }
            assert!(Instant::now() < deadline, "navigation terminal timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn deferred_inspect_releases_lane_for_bind_and_mutation() {
        let _serial = crate::commands::inspect::deferred_inspect_test_lock();
        let executor = Arc::new(Executor::new());
        let (dir, root) = test_root("deferred-inspect-storm");
        std::fs::write(dir.path().join("README.md"), "# Fixture\n").expect("fixture");
        let ctx = inspect_context(dir.path());
        executor.register_actor(root.clone(), Arc::clone(&ctx));
        let (started_rx, release_tx) =
            crate::commands::inspect::install_deferred_inspect_stat_gate_for_test();
        let (mut pending, _cancellation) =
            submit_deferred_inspect_setup(&executor, &root, &ctx, "subc-inspect-storm");
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("deferred inspect body starts");

        for request_id in ["subc-bind-other-session", "subc-edit-other-session"] {
            let response = executor.submit(
                root.clone(),
                Lane::Mutating,
                request_id.to_string(),
                Box::new(move |_| Response::success(request_id, json!({ "admitted": true }))),
            );
            assert!(
                response
                    .recv_timeout(Duration::from_secs(1))
                    .expect("writer admits while inspect remains deferred")
                    .success
            );
        }
        assert_eq!(
            crate::commands::inspect::deferred_inspect_root_count_for_test(),
            1,
            "writer admissions must not finish the detached inspect"
        );

        release_tx.send(()).expect("release inspect body");
        let terminal = wait_for_inspect_terminal(&mut pending, &ctx);
        assert!(
            terminal.data.get("inspect_terminal").is_some(),
            "inspect must still produce its terminal: {:?}",
            terminal.data
        );
    }

    #[test]
    fn deferred_cold_navigation_releases_lsp_lane_for_reads_and_mutation() {
        let _serial = crate::commands::lsp_navigation::deferred_navigation_test_lock();
        let executor = Arc::new(Executor::new());
        let (dir, root) = test_root("deferred-cold-navigation");
        let (ctx, source) = cold_navigation_context(dir.path());
        executor.register_actor(root.clone(), Arc::clone(&ctx));
        let (started_rx, _release_tx) =
            crate::commands::lsp_navigation::install_deferred_navigation_gate_for_test();
        let (mut pending, cancellation) = submit_deferred_navigation_setup(
            &executor,
            &root,
            &ctx,
            &source,
            "subc-cold-navigation",
        );
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("detached navigation reaches its cold-start gate");

        for (lane, request_id) in [
            (Lane::PureRead, "subc-navigation-read"),
            (Lane::Mutating, "subc-navigation-write"),
        ] {
            let response = executor.submit(
                root.clone(),
                lane,
                request_id.to_string(),
                Box::new(move |_| Response::success(request_id, json!({ "admitted": true }))),
            );
            assert!(
                response
                    .recv_timeout(Duration::from_secs(1))
                    .expect("unrelated work admits while navigation remains deferred")
                    .success
            );
        }
        assert_eq!(
            crate::commands::lsp_navigation::deferred_navigation_worker_count_for_test(),
            1,
            "lane admissions must not finish the detached navigation"
        );

        cancellation.request_cancel();
        let terminal = wait_for_navigation_terminal(&mut pending, &ctx);
        assert!(!terminal.success);
        let deadline = Instant::now() + Duration::from_secs(1);
        while crate::commands::lsp_navigation::deferred_navigation_worker_count_for_test() != 0 {
            assert!(
                Instant::now() < deadline,
                "cancelled navigation worker did not settle"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(executor.actor_is_idle(&root));
    }

    #[test]
    fn cancelling_pending_navigation_removes_entry_without_reply() {
        let _serial = crate::commands::lsp_navigation::deferred_navigation_test_lock();
        let executor = Arc::new(Executor::new());
        let (dir, root) = test_root("cancelled-pending-navigation");
        let (ctx, source) = cold_navigation_context(dir.path());
        executor.register_actor(root.clone(), Arc::clone(&ctx));
        let (started_rx, _release_tx) =
            crate::commands::lsp_navigation::install_deferred_navigation_gate_for_test();
        let (pending, _cancellation) = submit_deferred_navigation_setup(
            &executor,
            &root,
            &ctx,
            &source,
            "subc-cancel-navigation",
        );
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("detached navigation reaches its cancellation gate");

        let route = RouteChannel {
            channel: 17,
            epoch: 1,
        };
        let mut registry = PendingSubcResponses::default();
        registry.register(PendingSubcResponse {
            route,
            corr: 71,
            flags: Flags::new(false, Priority::Passive, false),
            ver: PROTOCOL_VERSION,
            root: root.clone(),
            session_id: "navigation-cancel-session".to_string(),
            bare_name: "lsp_hover".to_string(),
            format_context: crate::subc_format::FormatContext::from_tool_call(
                "lsp_hover",
                &json!({}),
                dir.path(),
            ),
            bind_trust: BindTrust::FirstParty,
            pending,
            surface_downgraded: false,
            phase_trace: PhaseTrace::new(Instant::now()),
        });

        assert!(registry.cancel_request(route, 71));
        assert!(registry.is_empty());
        let deadline = Instant::now() + Duration::from_secs(1);
        while crate::commands::lsp_navigation::deferred_navigation_worker_count_for_test() != 0 {
            assert!(
                Instant::now() < deadline,
                "pending cancellation did not settle the detached worker"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            registry.poll_ready(executor.as_ref()).is_empty(),
            "a cancelled navigation must not leak a reply"
        );
    }

    #[test]
    fn same_root_deferred_inspects_are_single_flight() {
        let _serial = crate::commands::inspect::deferred_inspect_test_lock();
        let executor = Arc::new(Executor::new());
        let (dir, root) = test_root("single-flight-deferred-inspect");
        std::fs::write(dir.path().join("README.md"), "# Fixture\n").expect("fixture");
        let ctx = inspect_context(dir.path());
        executor.register_actor(root.clone(), Arc::clone(&ctx));
        let (started_rx, release_tx) =
            crate::commands::inspect::install_deferred_inspect_stat_gate_for_test();
        let (mut first, _first_cancellation) =
            submit_deferred_inspect_setup(&executor, &root, &ctx, "subc-inspect-first");
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("first inspect owns the root flight");
        let (mut second, second_cancellation) =
            submit_deferred_inspect_setup(&executor, &root, &ctx, "subc-inspect-second");

        assert_eq!(
            crate::commands::inspect::deferred_inspect_root_count_for_test(),
            1,
            "only one detached body may run for a root"
        );
        assert!((second.poll)(&ctx).is_none(), "second inspect must queue");
        second_cancellation.request_cancel();
        let second_terminal = wait_for_inspect_terminal(&mut second, &ctx);
        assert_eq!(second_terminal.data["inspect_terminal"], "interrupted");
        assert_eq!(
            crate::commands::inspect::deferred_inspect_root_count_for_test(),
            1,
            "cancelling the queued request must not release the active flight"
        );

        release_tx.send(()).expect("release first inspect");
        let first_terminal = wait_for_inspect_terminal(&mut first, &ctx);
        assert_eq!(first_terminal.data["inspect_terminal"], "fresh");
        assert_eq!(
            crate::commands::inspect::deferred_inspect_root_count_for_test(),
            0
        );
    }

    #[test]
    fn route_abandonment_cancels_detached_inspect_thread() {
        let _serial = crate::commands::inspect::deferred_inspect_test_lock();
        let executor = Arc::new(Executor::new());
        let (dir, root) = test_root("cancelled-deferred-inspect");
        std::fs::write(dir.path().join("README.md"), "# Fixture\n").expect("fixture");
        let ctx = inspect_context(dir.path());
        executor.register_actor(root.clone(), Arc::clone(&ctx));
        let active: ActiveToolCalls = Arc::new(StdMutex::new(HashMap::new()));
        let route = RouteChannel {
            channel: 7,
            epoch: 1,
        };
        let (started_rx, _release_tx) =
            crate::commands::inspect::install_deferred_inspect_body_gate_for_test();
        let (mut pending, cancellation) =
            submit_deferred_inspect_setup(&executor, &root, &ctx, "subc-inspect-abandoned");
        active.lock().expect("active tool call map").insert(
            (route, 41),
            ActiveToolCall {
                root_id: root.clone(),
                cancellation,
                detach_policy: RouteDetachPolicy::CancelOnDetach,
            },
        );
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("detached inspect reaches cancellation gate");
        assert!(ctx.request_force_restrict("subc-inspect-abandoned"));

        assert!(cancel_active_tool_call(
            &active,
            executor.as_ref(),
            route,
            41,
            "test route abandonment"
        ));
        let terminal = wait_for_inspect_terminal(&mut pending, &ctx);
        assert_eq!(terminal.data["inspect_terminal"], "interrupted");
        assert_eq!(
            crate::commands::inspect::deferred_inspect_root_count_for_test(),
            0
        );
        assert!(executor.actor_is_idle(&root));
        assert!(active.lock().expect("active tool call map").is_empty());
        let restriction_deadline = Instant::now() + Duration::from_secs(1);
        while ctx.request_force_restrict("subc-inspect-abandoned") {
            assert!(
                Instant::now() < restriction_deadline,
                "detached force-restrict guard leaked"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn true_abandonment_cancels_but_route_detach_retains_interactive_search() {
        // This scenario requires the replayable and terminal calls to run at the
        // same time. Pin two read slots instead of deriving the topology from
        // the host, where a small runner can queue the terminal call forever
        // behind the intentionally retained call.
        let executor = Arc::new(Executor::with_config(crate::executor::ExecutorConfig {
            pool_size: 3,
            read_cap: 2,
            actor_cap: 2,
            heavy_permits: 1,
            drr_quantum: 1,
            ..crate::executor::ExecutorConfig::default()
        }));
        let (_dir, root) = test_root("cancelled-interactive-search");
        executor.register_actor(root.clone(), test_ctx());
        let active: ActiveToolCalls = Arc::new(StdMutex::new(HashMap::new()));
        let route = RouteChannel {
            channel: 9,
            epoch: 1,
        };

        let disabled_iterations = Arc::new(AtomicUsize::new(0));
        let disabled_probe = Arc::clone(&disabled_iterations);
        let (disabled_started_tx, disabled_started_rx) = std::sync::mpsc::sync_channel(1);
        let (disabled_rx, disabled_cancellation) = executor.submit_cancellable_async(
            root.clone(),
            Lane::PureRead,
            "untracked-search".to_string(),
            Box::new(move |_| {
                disabled_started_tx
                    .send(())
                    .expect("signal untracked search");
                let deadline = Instant::now() + Duration::from_secs(5);
                while !crate::commands::semantic_search::search_cancellation_requested() {
                    if Instant::now() >= deadline {
                        return Response::error(
                            "untracked-search",
                            "test_timeout",
                            "untracked search did not receive cancellation",
                        );
                    }
                    disabled_probe.fetch_add(1, Ordering::Relaxed);
                    std::thread::yield_now();
                }
                Response::error(
                    "untracked-search",
                    "request_cancelled",
                    "cancelled at search checkpoint",
                )
            }),
        );
        disabled_started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("untracked search starts");
        assert_eq!(
            apply_route_work_disposition(
                &active,
                executor.as_ref(),
                route,
                RouteWorkDisposition::Abandon,
                "disabled cancellation wiring",
            ),
            0
        );
        let iterations_before = disabled_iterations.load(Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(10));
        assert!(
            disabled_iterations.load(Ordering::Relaxed) > iterations_before,
            "without route registration the search keeps computing"
        );
        disabled_cancellation.request_cancel();
        let disabled_response = disabled_rx
            .blocking_recv()
            .expect("untracked search settles after explicit cleanup");
        assert_eq!(disabled_response.data["code"], "request_cancelled");

        let tracked_iterations = Arc::new(AtomicUsize::new(0));
        let tracked_probe = Arc::clone(&tracked_iterations);
        let (tracked_started_tx, tracked_started_rx) = std::sync::mpsc::sync_channel(1);
        let tracked_rx = submit_active_tool_call(
            executor.as_ref(),
            &active,
            route,
            42,
            root.clone(),
            Lane::PureRead,
            "tracked-search".to_string(),
            RouteDetachPolicy::RetainForReplay,
            Box::new(move |_| {
                tracked_started_tx.send(()).expect("signal tracked search");
                let deadline = Instant::now() + Duration::from_secs(5);
                while !crate::commands::semantic_search::search_cancellation_requested() {
                    if Instant::now() >= deadline {
                        return Response::error(
                            "tracked-search",
                            "test_timeout",
                            "tracked search did not receive cancellation",
                        );
                    }
                    tracked_probe.fetch_add(1, Ordering::Relaxed);
                    std::thread::yield_now();
                }
                Response::error(
                    "tracked-search",
                    "request_cancelled",
                    "cancelled at search checkpoint",
                )
            }),
            None,
        );
        tracked_started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("tracked search starts");

        let (terminal_started_tx, terminal_started_rx) = std::sync::mpsc::sync_channel(1);
        let terminal_rx = submit_active_tool_call(
            executor.as_ref(),
            &active,
            route,
            43,
            root.clone(),
            Lane::PureRead,
            "teardown-terminal".to_string(),
            RouteDetachPolicy::CancelOnDetach,
            Box::new(move |_| {
                terminal_started_tx
                    .send(())
                    .expect("signal teardown-terminal call");
                let deadline = Instant::now() + Duration::from_secs(5);
                while !crate::executor::current_job_cancelled() {
                    if Instant::now() >= deadline {
                        return Response::error(
                            "teardown-terminal",
                            "test_timeout",
                            "terminal call did not receive cancellation",
                        );
                    }
                    std::thread::yield_now();
                }
                Response::error(
                    "teardown-terminal",
                    "request_cancelled",
                    "cancelled for terminal-emitting teardown",
                )
            }),
            None,
        );
        terminal_started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("teardown-terminal call starts");
        assert_eq!(
            apply_route_work_disposition(
                &active,
                executor.as_ref(),
                route,
                RouteWorkDisposition::RetainForReplay,
                "test route detach",
            ),
            1,
            "only the replayable search remains active after route detach"
        );
        let terminal_response = terminal_rx
            .blocking_recv()
            .expect("teardown-terminal call stops at cancellation checkpoint");
        assert_eq!(terminal_response.data["code"], "request_cancelled");
        let iterations_before_detach = tracked_iterations.load(Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(10));
        assert!(
            tracked_iterations.load(Ordering::Relaxed) > iterations_before_detach,
            "route detach must retain work whose response can replay after rebind"
        );
        assert_eq!(
            apply_route_work_disposition(
                &active,
                executor.as_ref(),
                route,
                RouteWorkDisposition::Abandon,
                "test session purge",
            ),
            1
        );
        let tracked_response = tracked_rx
            .blocking_recv()
            .expect("tracked search stops at cancellation checkpoint");
        assert_eq!(tracked_response.data["code"], "request_cancelled");
        assert!(active.lock().expect("active tool calls").is_empty());

        let deadline = Instant::now() + Duration::from_secs(1);
        while !executor.actor_is_idle(&root) {
            assert!(
                Instant::now() < deadline,
                "cancelled search must release the PureRead lane"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn shutdown_drain_emits_terminal_and_clears_pending_inspect() {
        let _serial = crate::commands::inspect::deferred_inspect_test_lock();
        let executor = Arc::new(Executor::new());
        let (dir, root) = test_root("shutdown-deferred-inspect");
        std::fs::write(dir.path().join("README.md"), "# Fixture\n").expect("fixture");
        let ctx = inspect_context(dir.path());
        executor.register_actor(root.clone(), Arc::clone(&ctx));
        let (started_rx, _release_tx) =
            crate::commands::inspect::install_deferred_inspect_body_gate_for_test();
        let (pending, cancellation) =
            submit_deferred_inspect_setup(&executor, &root, &ctx, "subc-inspect-shutdown");
        let route = RouteChannel {
            channel: 8,
            epoch: 1,
        };
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("detached inspect reaches shutdown gate");
        let mut registry = PendingSubcResponses::default();
        registry.register(PendingSubcResponse {
            route,
            corr: 42,
            flags: Flags::new(false, Priority::Passive, false),
            ver: PROTOCOL_VERSION,
            root: root.clone(),
            session_id: "shutdown-session".to_string(),
            bare_name: "inspect".to_string(),
            format_context: crate::subc_format::FormatContext::from_tool_call(
                "inspect",
                &json!({}),
                dir.path(),
            ),
            bind_trust: BindTrust::FirstParty,
            pending,
            surface_downgraded: false,
            phase_trace: PhaseTrace::new(Instant::now()),
        });
        let active: ActiveToolCalls = Arc::new(StdMutex::new(HashMap::from([(
            (route, 42),
            ActiveToolCall {
                root_id: root.clone(),
                cancellation,
                detach_policy: RouteDetachPolicy::CancelOnDetach,
            },
        )])));

        let resolved = registry.drain_on_shutdown(executor.as_ref());
        assert!(registry.is_empty());
        assert_eq!(resolved.len(), 1);
        assert_eq!(
            resolved[0].response.data["failure_reason"],
            "daemon_shutdown"
        );
        finish_active_tool_call(&active, route, 42);
        let deadline = Instant::now() + Duration::from_secs(1);
        while crate::commands::inspect::deferred_inspect_root_count_for_test() != 0 {
            assert!(Instant::now() < deadline, "shutdown cancellation was inert");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(active.lock().expect("active calls").is_empty());
        assert!(executor.actor_is_idle(&root));
    }

    pub(super) fn wait_for_watcher_count(ctx: &AppContext, expected: usize) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let observed = ctx.watcher_registry_count();
            if observed == expected {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "watcher count did not settle before deadline: expected={expected}, observed={observed}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Sweep until `root` is forgotten, mirroring how production reaps.
    ///
    /// `reap_idle_roots` probes actor idleness with a try-lock and retains the
    /// root when the scheduler holds that lock — correct behavior, since the
    /// real caller sweeps on a timer and simply catches the root next tick. A
    /// test that asserts a single sweep succeeds is therefore asserting it wins
    /// a lock race that nothing in production depends on: `register_actor` wakes
    /// the scheduler, which grabs the same lock, and on a loaded runner that
    /// window is wide enough to lose. Sweep to the outcome instead.
    pub(super) fn reap_until_forgotten(
        root: &ProjectRootId,
        live_roots: &mut HashMap<ProjectRootId, RootMeta>,
        pending_binds: &HashMap<RouteChannel, PendingBind>,
        root_channels: &HashMap<ProjectRootId, HashSet<RouteChannel>>,
        executor: &Arc<Executor>,
        metrics: &DispatchPathMetrics,
    ) -> IdleReapOutcome {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let outcome = reap_idle_roots(
                Instant::now(),
                live_roots,
                pending_binds,
                root_channels,
                executor,
                metrics,
            );
            if outcome.forgotten_deleted_roots.contains(root) {
                return outcome;
            }
            assert!(
                Instant::now() < deadline,
                "deleted root was never forgotten: {root:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub(super) fn wait_for_actor_root_count(app: &App, expected: usize) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let observed = app.actor_root_count();
            if observed == expected {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "actor root count did not settle before deadline: expected={expected}, observed={observed}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    pub(super) fn status_frame(seq: u64) -> PushFrame {
        status_frame_with_session(seq, None)
    }

    pub(super) fn status_frame_with_session(seq: u64, session_id: Option<&str>) -> PushFrame {
        PushFrame::StatusChanged(StatusChangedFrame {
            frame_type: "status_changed",
            session_id: session_id.map(str::to_string),
            snapshot: json!({ "seq": seq }),
        })
    }

    pub(super) fn completion_frame(task_id: &str) -> PushFrame {
        completion_frame_with_session(task_id, "session-1")
    }

    pub(super) fn completion_frame_with_session(task_id: &str, session_id: &str) -> PushFrame {
        PushFrame::BashCompleted(BashCompletedFrame {
            frame_type: "bash_completed",
            task_id: task_id.to_string(),
            session_id: session_id.to_string(),
            status: BgTaskStatus::Completed,
            exit_code: Some(0),
            command: format!("echo {task_id}"),
            output_preview: String::new(),
            output_truncated: false,
            original_tokens: None,
            compressed_tokens: None,
            tokens_skipped: false,
            status_reason: None,
        })
    }

    pub(super) fn long_running_frame(task_id: &str, elapsed_ms: u64) -> PushFrame {
        long_running_frame_with_session(task_id, "session-1", elapsed_ms)
    }

    pub(super) fn long_running_frame_with_session(
        task_id: &str,
        session_id: &str,
        elapsed_ms: u64,
    ) -> PushFrame {
        PushFrame::BashLongRunning(BashLongRunningFrame {
            frame_type: "bash_long_running",
            task_id: task_id.to_string(),
            session_id: session_id.to_string(),
            command: format!("sleep {elapsed_ms}"),
            elapsed_ms,
        })
    }

    pub(super) fn pattern_match_frame(session_id: &str) -> PushFrame {
        PushFrame::BashPatternMatch(BashPatternMatchFrame {
            frame_type: "bash_pattern_match",
            task_id: "task-pattern".to_string(),
            session_id: session_id.to_string(),
            watch_id: "watch-1".to_string(),
            match_text: "needle".to_string(),
            match_offset: 7,
            context: "haystack needle".to_string(),
            once: true,
            reason: "pattern_match",
        })
    }

    pub(super) fn configure_warnings_frame(session_id: Option<&str>) -> PushFrame {
        PushFrame::ConfigureWarnings(ConfigureWarningsFrame {
            frame_type: "configure_warnings",
            session_id: session_id.map(str::to_string),
            project_root: "/tmp/subc-test".to_string(),
            warnings: Vec::new(),
        })
    }

    pub(super) fn route_identity(root: &ProjectRootId, session_id: &str) -> RouteIdentity {
        route_identity_with_trust(root, session_id, BindTrust::FirstParty)
    }

    pub(super) fn route_identity_with_trust(
        root: &ProjectRootId,
        session_id: &str,
        trust: BindTrust,
    ) -> RouteIdentity {
        RouteIdentity(Arc::new(RouteIdentityData {
            root: root.clone(),
            project_root: root.as_path().to_path_buf(),
            harness: "opencode".to_string(),
            session: session_id.to_string(),
            trust,
            spawn_principal: AuthenticatedPrincipal::RouteBind {
                trust: trust.sandbox_trust(),
                route_channel: 0,
                route_epoch: 0,
                project_root: root.as_path().to_path_buf(),
                harness: "opencode".to_string(),
                session_id: session_id.to_string(),
                principal_id: Some(match trust {
                    BindTrust::FirstParty => "direct".to_string(),
                    BindTrust::Untrusted => "unverified".to_string(),
                }),
            },
            consumer_elicitation_capable: false,
        }))
    }

    pub(super) fn progress_frame(request_id: &str, kind: ProgressKind, chunk: &str) -> PushFrame {
        PushFrame::Progress(ProgressFrame::new(request_id, kind, chunk))
    }

    pub(super) fn status_seq(frame: &PushFrame) -> Option<u64> {
        match frame {
            PushFrame::StatusChanged(status) => status.snapshot.get("seq").and_then(|v| v.as_u64()),
            _ => None,
        }
    }

    pub(super) fn completion_task(frame: &PushFrame) -> Option<&str> {
        match frame {
            PushFrame::BashCompleted(completion) => Some(completion.task_id.as_str()),
            _ => None,
        }
    }

    pub(super) fn push_frame_task_id(frame: &Frame) -> Option<String> {
        let body: serde_json::Value = serde_json::from_slice(&frame.body).expect("push body");
        body.get("task_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{
        completion_frame, reap_until_forgotten, route_identity, test_ctx, test_root,
        wait_for_actor_root_count, wait_for_watcher_count,
    };
    use super::*;
    use crate::bash_background::BgTaskStatus;

    fn attach_error(kind: io::ErrorKind) -> SubcError {
        SubcError::Connect {
            endpoint: "127.0.0.1:1".to_string(),
            source: io::Error::new(kind, "constructed attach failure"),
        }
    }

    fn auth_io_error(kind: io::ErrorKind) -> SubcError {
        SubcError::Auth {
            endpoint: "127.0.0.1:1".to_string(),
            source: subc_transport::AuthError::Io {
                stage: subc_transport::AuthStage::ServerProof,
                source: io::Error::new(kind, "constructed auth failure"),
            },
        }
    }

    #[tokio::test]
    async fn reader_routes_control_frames_around_buffered_data_frames() {
        let (mut daemon, module) = tokio::io::duplex(16 * 1024);
        let (priority_tx, mut priority_rx) = mpsc::channel(4);
        let (data_tx, mut data_rx) = mpsc::channel(4);
        let reader = spawn_reader_task(module, priority_tx, data_tx);

        let data = Frame::build(
            FrameType::Request,
            control_flags(),
            7,
            1,
            1,
            br#"{}"#.to_vec(),
        )
        .unwrap();
        let ping = Frame::build(FrameType::Ping, control_flags(), 0, 0, 2, Vec::new()).unwrap();
        write_frame(&mut daemon, &data).await.unwrap();
        write_frame(&mut daemon, &ping).await.unwrap();

        tokio::time::sleep(Duration::from_millis(10)).await;
        let mut lane = PrioritizedFrameLane::default();
        let priority = tokio::time::timeout(
            Duration::from_secs(1),
            recv_prioritized_frame(&mut priority_rx, &mut data_rx, &mut lane),
        )
        .await
        .expect("priority frame timeout")
        .expect("priority ingress closed")
        .expect("priority ingress error");
        assert_eq!(priority.frame.header.ty, FrameType::Ping);

        let data = recv_prioritized_frame(&mut priority_rx, &mut data_rx, &mut lane)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(data.frame.header.ty, FrameType::Request);
        reader.abort();
    }

    #[tokio::test]
    async fn sustained_control_traffic_does_not_starve_data_frames() {
        let (control_tx, mut control_rx) = mpsc::channel(CONTROL_BURST_LIMIT + 2);
        let (data_tx, mut data_rx) = mpsc::channel(1);
        for seq in 0..=CONTROL_BURST_LIMIT {
            control_tx
                .send(Ok(DecodedFrame {
                    frame: Frame::build(
                        FrameType::Ping,
                        control_flags(),
                        0,
                        0,
                        seq as u64,
                        Vec::new(),
                    )
                    .unwrap(),
                    phase_trace: PhaseTrace::new(Instant::now()),
                }))
                .await
                .unwrap();
        }
        data_tx
            .send(Ok(DecodedFrame {
                frame: Frame::build(
                    FrameType::Request,
                    control_flags(),
                    1,
                    1,
                    99,
                    br#"{}"#.to_vec(),
                )
                .unwrap(),
                phase_trace: PhaseTrace::new(Instant::now()),
            }))
            .await
            .unwrap();

        let mut lane = PrioritizedFrameLane::default();
        for _ in 0..CONTROL_BURST_LIMIT {
            let frame = recv_prioritized_frame(&mut control_rx, &mut data_rx, &mut lane)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(frame.frame.header.ty, FrameType::Ping);
        }
        let frame = recv_prioritized_frame(&mut control_rx, &mut data_rx, &mut lane)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(frame.frame.header.ty, FrameType::Request);
    }

    #[tokio::test]
    async fn control_lane_remains_live_after_burst_when_data_is_idle() {
        let (control_tx, mut control_rx) = mpsc::channel(CONTROL_BURST_LIMIT + 1);
        let (_data_tx, mut data_rx) = mpsc::channel(1);
        for seq in 0..CONTROL_BURST_LIMIT {
            control_tx
                .send(Ok(DecodedFrame {
                    frame: Frame::build(
                        FrameType::Ping,
                        control_flags(),
                        0,
                        0,
                        seq as u64,
                        Vec::new(),
                    )
                    .unwrap(),
                    phase_trace: PhaseTrace::new(Instant::now()),
                }))
                .await
                .unwrap();
        }

        let mut lane = PrioritizedFrameLane::default();
        for _ in 0..CONTROL_BURST_LIMIT {
            let frame = recv_prioritized_frame(&mut control_rx, &mut data_rx, &mut lane)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(frame.frame.header.ty, FrameType::Ping);
        }

        control_tx
            .send(Ok(DecodedFrame {
                frame: Frame::build(FrameType::Ping, control_flags(), 0, 0, 99, Vec::new())
                    .unwrap(),
                phase_trace: PhaseTrace::new(Instant::now()),
            }))
            .await
            .unwrap();
        let frame = tokio::time::timeout(
            Duration::from_millis(100),
            recv_prioritized_frame(&mut control_rx, &mut data_rx, &mut lane),
        )
        .await
        .expect("control traffic must remain live while the data lane is idle")
        .unwrap()
        .unwrap();
        assert_eq!(frame.frame.header.ty, FrameType::Ping);
    }

    #[tokio::test]
    async fn closed_control_lane_drains_buffered_data_before_eof() {
        let (control_tx, mut control_rx) = mpsc::channel(1);
        let (data_tx, mut data_rx) = mpsc::channel(1);
        drop(control_tx);
        data_tx
            .send(Ok(DecodedFrame {
                frame: Frame::build(
                    FrameType::Request,
                    control_flags(),
                    1,
                    1,
                    1,
                    br#"{}"#.to_vec(),
                )
                .unwrap(),
                phase_trace: PhaseTrace::new(Instant::now()),
            }))
            .await
            .unwrap();
        drop(data_tx);

        let mut lane = PrioritizedFrameLane::default();
        let frame = recv_prioritized_frame(&mut control_rx, &mut data_rx, &mut lane)
            .await
            .expect("buffered data must drain")
            .unwrap();
        assert_eq!(frame.frame.header.ty, FrameType::Request);
        assert!(
            recv_prioritized_frame(&mut control_rx, &mut data_rx, &mut lane)
                .await
                .is_none(),
            "EOF only after both lanes drain"
        );
    }

    #[test]
    fn initial_attach_error_classifier_distinguishes_transient_and_permanent_failures() {
        let transient_errors = vec![
            attach_error(io::ErrorKind::ConnectionRefused),
            attach_error(io::ErrorKind::TimedOut),
            attach_error(io::ErrorKind::ConnectionReset),
            auth_io_error(io::ErrorKind::ConnectionAborted),
            auth_io_error(io::ErrorKind::BrokenPipe),
            SubcError::Auth {
                endpoint: "127.0.0.1:1".to_string(),
                source: subc_transport::AuthError::UnexpectedEof {
                    stage: subc_transport::AuthStage::ServerProof,
                    expected: 4,
                    actual: 0,
                },
            },
            SubcError::Auth {
                endpoint: "127.0.0.1:1".to_string(),
                source: subc_transport::AuthError::Timeout {
                    stage: subc_transport::AuthStage::ServerProof,
                    deadline: AUTH_DEADLINE,
                },
            },
        ];
        for error in &transient_errors {
            assert_eq!(
                classify_attach_error(error),
                AttachErrorClass::Transient,
                "expected transient: {error}"
            );
        }

        let permanent_errors = vec![
            attach_error(io::ErrorKind::PermissionDenied),
            auth_io_error(io::ErrorKind::InvalidData),
            SubcError::Auth {
                endpoint: "127.0.0.1:1".to_string(),
                source: subc_transport::AuthError::InvalidServerProof,
            },
            SubcError::Auth {
                endpoint: "127.0.0.1:1".to_string(),
                source: subc_transport::AuthError::DaemonIdMismatch,
            },
            SubcError::ConnectionFile {
                path: PathBuf::from("subc-connection.json"),
                source: subc_transport::ConnectionFileError::Invalid {
                    reason: "constructed invalid file".to_string(),
                },
            },
            SubcError::NoEndpoint {
                path: PathBuf::from("subc-connection.json"),
            },
            SubcError::InvalidEndpoint {
                path: PathBuf::from("subc-connection.json"),
                endpoint: "not-an-ip:1234".to_string(),
            },
        ];
        for error in &permanent_errors {
            assert_eq!(
                classify_attach_error(error),
                AttachErrorClass::Permanent,
                "expected permanent: {error}"
            );
        }
    }

    #[test]
    fn incompatible_wire_version_is_rejected_before_tcp_connect() {
        let conn_dir = tempfile::tempdir().expect("connection tempdir");
        let conn_path = conn_dir.path().join("subc-connection.json");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind listener");
        listener
            .set_nonblocking(true)
            .expect("set listener nonblocking");
        let port = listener.local_addr().expect("listener addr").port();
        connection_file::write_atomic(
            &conn_path,
            &connection_file::ConnectionInfo {
                schema: connection_file::SCHEMA_VERSION,
                wire_version: Some(PROTOCOL_VERSION.wrapping_add(1)),
                endpoints: vec![connection_file::Endpoint {
                    host: "127.0.0.1".to_string(),
                    port,
                }],
                key: vec![0x42; subc_transport::KEY_LEN],
                daemon_id: [0x24; subc_transport::DAEMON_ID_LEN],
                pid: std::process::id(),
                daemon_ver: "subc-test".to_string(),
            },
        )
        .expect("write connection file");

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let result = runtime.block_on(connect_and_authenticate_with_policy(
            &conn_path,
            AttachRetryPolicy {
                budget: Duration::from_secs(1),
                initial_backoff: Duration::from_millis(5),
                max_backoff: Duration::from_millis(10),
                jitter_percent: 0,
            },
            None,
        ));
        assert!(matches!(
            result,
            Err(SubcError::ConnectionFile {
                source: connection_file::ConnectionFileError::WireVersionMismatch { .. },
                ..
            })
        ));
        assert!(matches!(
            listener.accept(),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
        ));
    }

    #[test]
    fn initial_attach_unreachable_endpoint_retries_until_budget_then_fails_loud() {
        let conn_dir = tempfile::tempdir().expect("connection tempdir");
        let conn_path = conn_dir.path().join("subc-connection.json");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve port");
        let port = listener.local_addr().expect("reserved addr").port();
        drop(listener);
        connection_file::write_atomic(
            &conn_path,
            &connection_file::ConnectionInfo {
                schema: connection_file::SCHEMA_VERSION,
                wire_version: Some(PROTOCOL_VERSION),
                endpoints: vec![connection_file::Endpoint {
                    host: "127.0.0.1".to_string(),
                    port,
                }],
                key: vec![0x42; subc_transport::KEY_LEN],
                daemon_id: [0x24; subc_transport::DAEMON_ID_LEN],
                pid: std::process::id(),
                daemon_ver: "subc-test".to_string(),
            },
        )
        .expect("write connection file");

        let policy = AttachRetryPolicy {
            budget: Duration::from_millis(40),
            initial_backoff: Duration::from_millis(5),
            max_backoff: Duration::from_millis(10),
            jitter_percent: 0,
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let started_at = Instant::now();
        let result = runtime.block_on(connect_and_authenticate_with_policy(
            &conn_path, policy, None,
        ));
        let elapsed = started_at.elapsed();
        let error = match result {
            Ok(_) => panic!("unreachable endpoint unexpectedly attached"),
            Err(error) => error,
        };

        assert!(matches!(error, SubcError::Connect { .. }), "{error}");
        assert!(
            elapsed >= Duration::from_millis(35),
            "retry budget ended too early: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(1),
            "retry budget was not bounded: {elapsed:?}"
        );
    }

    fn due_maintenance_jobs_without_actor_context(
        live_roots: &mut HashMap<ProjectRootId, RootMeta>,
        budget: usize,
        pending_bind_roots: &HashSet<ProjectRootId>,
    ) -> (Vec<(ProjectRootId, MaintenanceDrainKind)>, bool) {
        due_maintenance_jobs(
            live_roots,
            None,
            &HashMap::new(),
            &HashSet::new(),
            budget,
            pending_bind_roots,
        )
    }

    fn actor_ctx_with_dirty_search_index(
        root: &Path,
        storage: &Path,
        file_name: &str,
        old_contents: &str,
        new_contents: &str,
    ) -> (Arc<AppContext>, PathBuf, PathBuf) {
        let file = root.join(file_name);
        std::fs::write(&file, old_contents).expect("write source");
        let canonical_root = std::fs::canonicalize(root).expect("canonical root");
        let ctx = Arc::new(AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            Config {
                project_root: Some(root.to_path_buf()),
                storage_dir: Some(storage.to_path_buf()),
                ..Config::default()
            },
        ));
        ctx.set_canonical_cache_root(canonical_root.clone());

        let cache_dir = crate::search_index::resolve_cache_dir(&canonical_root, Some(storage));
        let mut index = crate::search_index::SearchIndex::build(&canonical_root);
        let git_head = index.stored_git_head().map(str::to_owned);
        index.write_to_disk(&cache_dir, git_head.as_deref());

        std::fs::write(&file, new_contents).expect("edit source");
        index.update_file(&file);
        *ctx.search_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);
        (ctx, canonical_root, cache_dir)
    }

    #[test]
    fn graceful_shutdown_flushes_every_actor_search_index() {
        let storage = tempfile::tempdir().expect("storage tempdir");
        let (root1_dir, root1) = test_root("shutdown-flush-root-1");
        let (root2_dir, root2) = test_root("shutdown-flush-root-2");
        let (ctx1, canonical_root1, cache_dir1) = actor_ctx_with_dirty_search_index(
            root1_dir.path(),
            storage.path(),
            "alpha.txt",
            "old actor one token\n",
            "new actor one token\n",
        );
        let (ctx2, canonical_root2, cache_dir2) = actor_ctx_with_dirty_search_index(
            root2_dir.path(),
            storage.path(),
            "beta.txt",
            "old actor two token\n",
            "new actor two token\n",
        );

        let executor = Executor::new();
        assert!(executor.register_actor(root1.clone(), Arc::clone(&ctx1)));
        assert!(executor.register_actor(root2.clone(), Arc::clone(&ctx2)));

        flush_actor_indexes_on_graceful_shutdown(&executor.actor_contexts());

        let mut restored1 =
            crate::search_index::SearchIndex::read_from_disk(&cache_dir1, &canonical_root1)
                .expect("load flushed root one index");
        restored1.ready = true;
        assert_eq!(
            restored1
                .grep("new actor one token", true, &[], &[], &canonical_root1, 10)
                .matches
                .len(),
            1,
            "graceful subc shutdown should flush the first root's trigram delta"
        );

        let mut restored2 =
            crate::search_index::SearchIndex::read_from_disk(&cache_dir2, &canonical_root2)
                .expect("load flushed root two index");
        restored2.ready = true;
        assert_eq!(
            restored2
                .grep("new actor two token", true, &[], &[], &canonical_root2, 10)
                .matches
                .len(),
            1,
            "graceful subc shutdown should flush every registered root"
        );
    }

    #[test]
    fn idle_root_reaper_closes_artifacts_and_stops_watcher() {
        let _ = env_logger::builder().is_test(true).try_init();
        let (root_dir, root) = test_root("idle-root-reaper");
        let storage = tempfile::tempdir().expect("storage tempdir");
        std::fs::write(
            root_dir.path().join("main.rs"),
            "fn entry() { leaf(); }\nfn leaf() {}\n",
        )
        .expect("source file");
        let canonical_root = std::fs::canonicalize(root_dir.path()).expect("canonical root");
        let app = App::default_shared();
        let ctx = Arc::new(AppContext::from_app(
            Arc::clone(&app),
            Config {
                project_root: Some(canonical_root.clone()),
                storage_dir: Some(storage.path().to_path_buf()),
                callgraph_store: true,
                search_index: true,
                ..Config::default()
            },
        ));
        ctx.set_canonical_cache_root(canonical_root.clone());
        let project_key = crate::search_index::artifact_cache_key(&canonical_root);
        crate::root_cache::configure_artifact_access(&canonical_root, &project_key, false);
        assert!(ctx
            .ensure_callgraph_store()
            .expect("build callgraph store")
            .is_some());

        let cache_dir =
            crate::search_index::resolve_cache_dir(&canonical_root, Some(storage.path()));
        let mut index = crate::search_index::SearchIndex::build(&canonical_root);
        let git_head = index.stored_git_head().map(str::to_owned);
        index.write_to_disk(&cache_dir, git_head.as_deref());
        *ctx.search_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);
        // Seed a completed warm verification so the test can prove eviction
        // downgrades it to Strict rather than passing vacuously.
        let seeded_generation =
            crate::cache_freshness::artifact_generation(&cache_dir.join("cache.bin"))
                .expect("seeded artifact generation");
        crate::cache_freshness::record_verify_completed(
            &canonical_root,
            crate::cache_freshness::VerifyArtifact::Search,
            Some(seeded_generation),
        );
        assert!(
            matches!(
                crate::cache_freshness::warm_verify_plan(
                    canonical_root.as_path(),
                    crate::cache_freshness::VerifyArtifact::Search,
                    Some(seeded_generation),
                ),
                crate::cache_freshness::WarmVerifyPlan::Skip
            ),
            "memo must be warm before eviction for the downgrade assertion to bite"
        );

        let (dispatch_tx, dispatch_rx) = crate::watcher_filter::watcher_dispatch_channel();
        let _dispatch_tx = dispatch_tx;
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);
        let join = std::thread::spawn(move || {
            while !thread_shutdown.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
        });
        ctx.install_watcher_runtime(
            dispatch_rx,
            crate::watcher_filter::WatcherThreadHandle::new(shutdown, join),
        );
        wait_for_watcher_count(&ctx, 1);

        let executor = Arc::new(Executor::new());
        assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));
        ctx.mark_subc_unbound();
        let mut live_roots = HashMap::new();
        let mut meta = RootMeta::new(Instant::now());
        meta.last_touched = Instant::now() - IDLE_ROOT_TTL - Duration::from_secs(1);
        meta.unbound_quiesced = true;
        live_roots.insert(root.clone(), meta);

        let message = idle_root_eviction_message(&root, &ctx.memory_root_snapshot(), None);
        assert!(message.contains("evicted idle root"));
        assert!(message.contains("freed ~"));
        assert!(message.contains("semantic"));
        assert!(!message.contains("semantic not estimated retained"));
        assert!(message.contains("trigram"));
        assert!(message.contains("retained: bash"));
        assert!(message.contains("parser_pool"));

        assert_eq!(
            reap_idle_roots(
                Instant::now(),
                &mut live_roots,
                &HashMap::new(),
                &HashMap::new(),
                &executor,
                &DispatchPathMetrics::new(),
            )
            .evicted,
            1
        );
        assert!(ctx.search_index().read().unwrap().is_none());
        wait_for_watcher_count(&ctx, 0);
        // The watcher stopped with the eviction, so the idle interval is
        // unobserved: the pre-seeded warm-verify memo (Skip) must fall back to
        // strict content verification (stat-first would miss same-size,
        // preserved-mtime edits made while nobody was watching).
        assert!(
            matches!(
                crate::cache_freshness::warm_verify_plan(
                    canonical_root.as_path(),
                    crate::cache_freshness::VerifyArtifact::Search,
                    Some(seeded_generation),
                ),
                crate::cache_freshness::WarmVerifyPlan::Strict
            ),
            "idle eviction must force strict re-verification"
        );
        assert!(
            crate::search_index::SearchIndex::read_from_disk(&cache_dir, &canonical_root).is_some()
        );
        ctx.mark_subc_bound();
        assert!(ctx
            .ensure_callgraph_store()
            .expect("reopen callgraph store")
            .is_some());
        assert!(live_roots[&root].idle_artifacts_evicted);
    }

    #[test]
    fn idle_root_reaper_applies_ttl_to_unbound_roots() {
        let (_root_dir, root) = test_root("idle-root-ttl-gate");
        let ctx = test_ctx();
        let executor = Arc::new(Executor::new());
        assert!(executor.register_actor(root.clone(), ctx));
        let ctx = executor.actor_context(&root).expect("actor context");
        ctx.mark_subc_unbound();
        let now = Instant::now();
        let mut meta = RootMeta::new(now);
        meta.unbound_quiesced = true;
        let mut live_roots = HashMap::from([(root.clone(), meta)]);

        // A recently-unbound root stays warm: a transient unbind (host
        // restart) must not pay the strict-verify + forced-rebuild teardown
        // on the next maintenance sweep.
        assert_eq!(
            reap_idle_roots(
                now,
                &mut live_roots,
                &HashMap::new(),
                &HashMap::new(),
                &executor,
                &DispatchPathMetrics::new(),
            )
            .evicted,
            0
        );
        assert!(!live_roots[&root].idle_artifacts_evicted);

        // Past the TTL the same unbound root pays the full teardown. The
        // pending reconciliation paths retained across the transient-unbind
        // window would block eviction forever through
        // `artifact_eviction_blocked`; the reaper disposes them because the
        // strict gap invalidation subsumes their purpose.
        ctx.add_pending_search_index_paths([root.as_path().join("retained.rs")]);
        assert_eq!(
            reap_idle_roots(
                now + IDLE_ROOT_TTL,
                &mut live_roots,
                &HashMap::new(),
                &HashMap::new(),
                &executor,
                &DispatchPathMetrics::new(),
            )
            .evicted,
            1
        );
        assert!(live_roots[&root].idle_artifacts_evicted);
        assert!(
            ctx.take_pending_search_index_paths().is_empty(),
            "TTL eviction must dispose retained pending reconciliation paths"
        );
    }

    #[test]
    fn blocked_ttl_eviction_restores_taken_pending_reconciliation_state() {
        let (_root_dir, root) = test_root("ttl-eviction-blocked-restore");
        let ctx = test_ctx();
        let executor = Arc::new(Executor::new());
        assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));
        ctx.mark_subc_unbound();

        // Retained pending path from the transient-unbind window plus a
        // SECONDARY eviction blocker (a non-ready resident search index, the
        // dirty-index blocker in artifact_eviction_blocked). Disposal must be
        // transactional: the blocked eviction may be followed by a rebind, and
        // the path is the only repair record for its consumed watcher event.
        let pending = root.as_path().join("edited-while-unbound.rs");
        ctx.add_pending_search_index_paths([pending.clone()]);
        let dirty_source = root.as_path().join("dirty.rs");
        std::fs::write(&dirty_source, "fn dirty() {}\n").expect("dirty source");
        let mut dirty = crate::search_index::SearchIndex::new();
        dirty.ready = true;
        dirty.update_file(&dirty_source);
        *ctx.search_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(dirty);
        assert!(ctx.artifact_eviction_blocked());

        let mut live_roots = HashMap::new();
        let mut meta = RootMeta::new(Instant::now());
        meta.last_touched = Instant::now() - IDLE_ROOT_TTL - Duration::from_secs(1);
        meta.unbound_quiesced = true;
        live_roots.insert(root.clone(), meta);

        assert_eq!(
            reap_idle_roots(
                Instant::now(),
                &mut live_roots,
                &HashMap::new(),
                &HashMap::new(),
                &executor,
                &DispatchPathMetrics::new(),
            )
            .evicted,
            0,
            "the dirty index must still block this eviction"
        );
        assert_eq!(
            ctx.take_pending_search_index_paths(),
            vec![pending],
            "a blocked eviction must restore the taken pending paths"
        );
    }

    #[test]
    fn idle_reap_with_bound_route_keeps_watcher_running() {
        let (_root_dir, root) = test_root("bound-root-reap-gate");
        let ctx = test_ctx();
        let executor = Arc::new(Executor::new());
        assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));

        let (dispatch_tx, dispatch_rx) = crate::watcher_filter::watcher_dispatch_channel();
        let _dispatch_tx = dispatch_tx;
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);
        let join = std::thread::spawn(move || {
            while !thread_shutdown.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
        });
        ctx.install_watcher_runtime(
            dispatch_rx,
            crate::watcher_filter::WatcherThreadHandle::new(shutdown, join),
        );

        let mut meta = RootMeta::new(Instant::now());
        meta.last_touched = Instant::now() - IDLE_ROOT_TTL - Duration::from_secs(1);
        let mut live_roots = HashMap::from([(root.clone(), meta)]);
        let bound = HashMap::from([(root, HashSet::from([route_key(7, 1)]))]);
        assert_eq!(
            reap_idle_roots(
                Instant::now(),
                &mut live_roots,
                &HashMap::new(),
                &bound,
                &executor,
                &DispatchPathMetrics::new(),
            )
            .evicted,
            0
        );
        wait_for_watcher_count(&ctx, 1);
        ctx.stop_watcher_runtime_in_background();
        wait_for_watcher_count(&ctx, 0);
    }

    #[test]
    fn deleted_root_with_bound_route_is_reclaimed_after_confirmation_and_routes_are_purged() {
        let (root_dir, root) = test_root("deleted-bound-root-reap");
        let executor = Arc::new(Executor::new());
        assert!(executor.register_actor(root.clone(), test_ctx()));
        root_dir.close().expect("delete project root");

        let route = route_key(19, 3);
        let mut live_roots = HashMap::from([(root.clone(), RootMeta::new(Instant::now()))]);
        let cancel_signal = PersistentCancelSignal::new();
        let mut routes = HashMap::from([(route, route_identity(&root, "deleted-route"))]);
        let mut root_channels = HashMap::from([(root.clone(), HashSet::from([route]))]);
        let mut installed_route_epochs = HashMap::from([(route.channel, route.epoch)]);
        let mut route_bash_cancels = HashMap::from([(
            route,
            bash::RouteBashCancel {
                token: cancel_signal.clone(),
                active_waits: 0,
            },
        )]);
        let metrics = DispatchPathMetrics::new();

        let first = reap_idle_roots(
            Instant::now(),
            &mut live_roots,
            &HashMap::new(),
            &root_channels,
            &executor,
            &metrics,
        );
        assert!(first.forgotten_deleted_roots.is_empty());
        assert!(executor.actor_registered(&root));

        let mut forgotten = Vec::new();
        for _ in 0..100 {
            let outcome = reap_idle_roots(
                Instant::now(),
                &mut live_roots,
                &HashMap::new(),
                &root_channels,
                &executor,
                &metrics,
            );
            if !outcome.forgotten_deleted_roots.is_empty() {
                forgotten = outcome.forgotten_deleted_roots;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(forgotten, vec![root.clone()]);
        assert!(!executor.actor_registered(&root));

        let mut retry_buffer = HashMap::new();
        let mut reclaimed_routes = ReclaimedRoutes::default();
        let mut session_identity = HashMap::new();
        let mut push_buffer = HashMap::new();
        let mut bg_subs = HashMap::from([(
            route,
            BgSub {
                corr: 77,
                ver: PROTOCOL_VERSION,
                flags: control_flags(),
                root: root.clone(),
                session: "deleted-route".to_string(),
            },
        )]);
        let mut bg_sub_by_session = HashMap::from([(
            (root.clone(), "deleted-route".to_string()),
            HashSet::from([route]),
        )]);
        let mut bg_wake_pending = HashSet::from([route]);
        let mut bg_wake_epoch = HashMap::new();
        let mut pending_bash_asks = HashMap::new();
        let active_tool_calls: ActiveToolCalls = Arc::new(StdMutex::new(HashMap::new()));
        health::take_bg_observability_logs_for_test();
        purge_deleted_root_residents(
            &root,
            &mut routes,
            &mut root_channels,
            &mut installed_route_epochs,
            &mut route_bash_cancels,
            &active_tool_calls,
            executor.as_ref(),
            &mut retry_buffer,
            &mut reclaimed_routes,
            &mut session_identity,
            &mut push_buffer,
            &mut bg_subs,
            &mut bg_sub_by_session,
            &mut bg_wake_pending,
            &mut bg_wake_epoch,
            &mut pending_bash_asks,
            &metrics,
        );

        assert!(routes.is_empty());
        assert!(root_channels.is_empty());
        assert!(installed_route_epochs.is_empty());
        assert!(route_bash_cancels.is_empty());
        assert!(reclaimed_routes.contains(route));
        assert!(cancel_signal.is_cancelled());
        assert_eq!(
            health::take_bg_observability_logs_for_test(),
            vec![format!(
                "subc bg subscription: ended root={} session=deleted-route channel=19@3 cause=root-reclaim suppressed=0",
                root.as_path().display()
            )]
        );
    }

    /// The control for the deleted-root reclamation above: a root whose
    /// directory still EXISTS must stay retained while it holds a bound route,
    /// even with every other reap precondition satisfied. Without this, the
    /// suite cannot tell "reclaim roots that are provably gone" apart from
    /// "reap any root that looks idle" — the second would tear down live
    /// sessions, and both satisfy the deleted-root tests.
    #[test]
    fn live_root_with_bound_route_is_never_reclaimed() {
        let (_root_dir, root) = test_root("live-bound-root-retained");
        let ctx = test_ctx();
        ctx.mark_subc_unbound();
        let executor = Arc::new(Executor::new());
        assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));

        let route = RouteChannel {
            channel: 7,
            epoch: 1,
        };
        let mut meta = RootMeta::new(Instant::now() - IDLE_ROOT_TTL - Duration::from_secs(1));
        meta.unbound_quiesced = true;
        let mut live_roots = HashMap::from([(root.clone(), meta)]);
        let root_channels = HashMap::from([(root.clone(), HashSet::from([route]))]);

        // Sweep three times because reclamation requires the root path to be
        // observed missing on two CONSECUTIVE sweeps. A single sweep would pass
        // here even if reclamation were wrongly unconditional, since the first
        // absence never reclaims on its own.
        for _ in 0..3 {
            let outcome = reap_idle_roots(
                Instant::now(),
                &mut live_roots,
                &HashMap::new(),
                &root_channels,
                &executor,
                &DispatchPathMetrics::new(),
            );
            assert!(
                outcome.forgotten_deleted_roots.is_empty(),
                "a root whose directory exists must never be forgotten"
            );
        }

        assert!(live_roots.contains_key(&root), "live root must be retained");
        assert!(
            executor.actor_registered(&root),
            "live root's actor must survive"
        );
        assert!(
            root.as_path().exists(),
            "test vehicle must keep the directory alive; otherwise this control proves nothing"
        );
    }

    #[test]
    fn deleted_root_is_not_reclaimed_on_first_absence_observation() {
        let (root_dir, root) = test_root("deleted-root-first-observation");
        let ctx = test_ctx();
        ctx.mark_subc_unbound();
        let executor = Arc::new(Executor::new());
        assert!(executor.register_actor(root.clone(), ctx));
        root_dir.close().expect("delete project root");

        let mut meta = RootMeta::new(Instant::now());
        meta.unbound_quiesced = true;
        let mut live_roots = HashMap::from([(root.clone(), meta)]);
        let outcome = reap_idle_roots(
            Instant::now(),
            &mut live_roots,
            &HashMap::new(),
            &HashMap::new(),
            &executor,
            &DispatchPathMetrics::new(),
        );

        assert!(outcome.forgotten_deleted_roots.is_empty());
        assert!(live_roots.contains_key(&root));
        assert!(executor.actor_registered(&root));
    }

    fn spawn_background_for_root(
        ctx: &AppContext,
        root: &ProjectRootId,
        storage: &tempfile::TempDir,
        session_id: &str,
    ) -> (String, u32) {
        // Windows refuses to delete a directory that is a running process's
        // cwd (ERROR_SHARING_VIOLATION), so the task must not live inside the
        // project root these tests delete. The kill path matches on the task's
        // registered project_root, not its cwd, so pointing the cwd at task
        // storage keeps the association under test intact.
        let command = if cfg!(windows) {
            // timeout.exe requires a console; ping is the standard sleep shim.
            "ping -n 31 127.0.0.1 > nul"
        } else {
            "sleep 30"
        };
        let task_id = ctx
            .bash_background()
            .spawn(
                crate::sandbox_spawn::SpawnPlan::Unsandboxed,
                command,
                session_id.to_string(),
                storage.path().to_path_buf(),
                HashMap::new(),
                Some(Duration::from_secs(60)),
                storage.path().to_path_buf(),
                8,
                true,
                false,
                Some(root.as_path().to_path_buf()),
            )
            .expect("spawn background task");
        let snapshot = ctx
            .bash_background()
            .status(
                &task_id,
                session_id,
                Some(root.as_path()),
                Some(storage.path()),
                0,
            )
            .expect("background task status");
        (task_id, snapshot.child_pid.expect("background child pid"))
    }

    fn wait_for_background_exit(pid: u32) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while crate::bash_background::process::is_process_alive(pid) {
            assert!(
                Instant::now() < deadline,
                "background task process survived kill"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn deleted_root_reclaims_background_task_after_two_absence_sweeps() {
        let (root_dir, root) = test_root("deleted-root-background-task");
        let storage = tempfile::tempdir().expect("task storage");
        let ctx = test_ctx();
        let (task_id, pid) = spawn_background_for_root(&ctx, &root, &storage, "reclaim-session");
        assert!(crate::bash_background::process::is_process_alive(pid));

        let executor = Arc::new(Executor::new());
        assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));
        assert!(executor.actor_is_idle(&root));
        root_dir.close().expect("delete project root");
        let mut live_roots = HashMap::from([(root.clone(), RootMeta::new(Instant::now()))]);
        let pending_binds = HashMap::new();
        let root_channels = HashMap::new();
        let metrics = DispatchPathMetrics::new();

        let first = reap_idle_roots(
            Instant::now(),
            &mut live_roots,
            &pending_binds,
            &root_channels,
            &executor,
            &metrics,
        );
        assert!(first.forgotten_deleted_roots.is_empty());
        assert!(crate::bash_background::process::is_process_alive(pid));

        let outcome = reap_until_forgotten(
            &root,
            &mut live_roots,
            &pending_binds,
            &root_channels,
            &executor,
            &metrics,
        );
        assert_eq!(outcome.forgotten_deleted_roots, vec![root.clone()]);
        wait_for_background_exit(pid);

        let snapshot = ctx
            .bash_background()
            .status(
                &task_id,
                "reclaim-session",
                Some(root.as_path()),
                Some(storage.path()),
                0,
            )
            .expect("reclaimed task status");
        assert_eq!(snapshot.info.status, BgTaskStatus::Killed);
        assert_eq!(
            snapshot.info.status_reason.as_deref(),
            Some(crate::bash_background::registry::ROOT_RECLAIMED_REASON)
        );
        assert_eq!(
            serde_json::to_value(&snapshot).expect("serialize bash status")["status_reason"],
            crate::bash_background::registry::ROOT_RECLAIMED_REASON
        );
        let completion = ctx
            .bash_background()
            .drain_completions_for_session(Some("reclaim-session"))
            .pop()
            .expect("reclaimed task completion");
        assert_eq!(
            completion.status_reason.as_deref(),
            Some(crate::bash_background::registry::ROOT_RECLAIMED_REASON)
        );
    }

    #[test]
    fn existing_unbound_root_keeps_background_task_alive_across_sweeps() {
        let (root_dir, root) = test_root("existing-root-background-task");
        let storage = tempfile::tempdir().expect("task storage");
        let ctx = test_ctx();
        ctx.mark_subc_unbound();
        let (task_id, pid) = spawn_background_for_root(&ctx, &root, &storage, "existing-session");

        let executor = Arc::new(Executor::new());
        assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));
        let mut meta = RootMeta::new(
            Instant::now()
                .checked_sub(IDLE_ROOT_TTL + Duration::from_secs(1))
                .expect("old root timestamp"),
        );
        meta.unbound_quiesced = true;
        let mut live_roots = HashMap::from([(root.clone(), meta)]);
        let pending_binds = HashMap::new();
        let root_channels = HashMap::new();
        let metrics = DispatchPathMetrics::new();

        for _ in 0..8 {
            reap_idle_roots(
                Instant::now(),
                &mut live_roots,
                &pending_binds,
                &root_channels,
                &executor,
                &metrics,
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(root_dir.path().exists());
        assert!(crate::bash_background::process::is_process_alive(pid));
        let snapshot = ctx
            .bash_background()
            .status(
                &task_id,
                "existing-session",
                Some(root.as_path()),
                Some(storage.path()),
                0,
            )
            .expect("existing task status");
        assert_eq!(snapshot.info.status, BgTaskStatus::Running);
        let _ = ctx.bash_background().kill(&task_id, "existing-session");
        wait_for_background_exit(pid);
    }

    #[test]
    fn restored_root_between_absence_sweeps_keeps_background_task_alive() {
        let (root_dir, root) = test_root("restored-root-background-task");
        let storage = tempfile::tempdir().expect("task storage");
        let ctx = test_ctx();
        let (task_id, pid) = spawn_background_for_root(&ctx, &root, &storage, "restored-session");

        let executor = Arc::new(Executor::new());
        assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));
        root_dir.close().expect("delete project root");
        let mut live_roots = HashMap::from([(root.clone(), RootMeta::new(Instant::now()))]);
        let pending_binds = HashMap::new();
        let root_channels = HashMap::new();
        let metrics = DispatchPathMetrics::new();

        let first = reap_idle_roots(
            Instant::now(),
            &mut live_roots,
            &pending_binds,
            &root_channels,
            &executor,
            &metrics,
        );
        assert!(first.forgotten_deleted_roots.is_empty());
        std::fs::create_dir_all(root.as_path()).expect("restore project root");
        let second = reap_idle_roots(
            Instant::now(),
            &mut live_roots,
            &pending_binds,
            &root_channels,
            &executor,
            &metrics,
        );
        assert!(second.forgotten_deleted_roots.is_empty());
        assert!(crate::bash_background::process::is_process_alive(pid));
        let _ = ctx.bash_background().kill(&task_id, "restored-session");
        wait_for_background_exit(pid);
    }

    #[test]
    fn observing_root_again_resets_deleted_sweep_confirmation() {
        let (root_dir, root) = test_root("deleted-root-observation-reset");
        let ctx = test_ctx();
        ctx.mark_subc_unbound();
        let executor = Arc::new(Executor::new());
        assert!(executor.register_actor(root.clone(), ctx));
        root_dir.close().expect("delete project root");

        let mut meta = RootMeta::new(Instant::now());
        meta.unbound_quiesced = true;
        let mut live_roots = HashMap::from([(root.clone(), meta)]);
        let pending_binds = HashMap::new();
        let root_channels = HashMap::new();
        let metrics = DispatchPathMetrics::new();

        let first = reap_idle_roots(
            Instant::now(),
            &mut live_roots,
            &pending_binds,
            &root_channels,
            &executor,
            &metrics,
        );
        assert!(first.forgotten_deleted_roots.is_empty());

        std::fs::create_dir_all(root.as_path()).expect("restore project root");
        reap_idle_roots(
            Instant::now(),
            &mut live_roots,
            &pending_binds,
            &root_channels,
            &executor,
            &metrics,
        );
        std::fs::remove_dir_all(root.as_path()).expect("delete project root again");

        let after_reset = reap_idle_roots(
            Instant::now(),
            &mut live_roots,
            &pending_binds,
            &root_channels,
            &executor,
            &metrics,
        );
        assert!(after_reset.forgotten_deleted_roots.is_empty());
        assert!(live_roots.contains_key(&root));
        assert!(executor.actor_registered(&root));
    }

    #[test]
    fn deleted_idle_root_is_fully_forgotten_and_status_counts_drop() {
        let (root_dir, root) = test_root("deleted-root-reap");
        let app = App::default_shared();
        let ctx = Arc::new(AppContext::from_app(
            Arc::clone(&app),
            Config {
                project_root: Some(root.as_path().to_path_buf()),
                ..Config::default()
            },
        ));
        ctx.set_canonical_cache_root(root.as_path().to_path_buf());
        ctx.mark_subc_unbound();
        let executor = Arc::new(Executor::new());
        assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));
        assert_eq!(app.actor_root_count(), 1);
        drop(ctx);
        root_dir.close().expect("delete project root");

        let mut meta = RootMeta::new(Instant::now());
        meta.unbound_quiesced = true;
        let mut live_roots = HashMap::from([(root.clone(), meta)]);
        let outcome = reap_until_forgotten(
            &root,
            &mut live_roots,
            &HashMap::new(),
            &HashMap::new(),
            &executor,
            &DispatchPathMetrics::new(),
        );
        assert_eq!(outcome.forgotten_deleted_roots, vec![root.clone()]);
        assert!(!executor.actor_registered(&root));
        assert!(!live_roots.contains_key(&root));
        wait_for_actor_root_count(&app, 0);

        let status_ctx = AppContext::from_app(app, Config::default());
        let status = status_ctx.build_status_snapshot();
        assert_eq!(status["runtime"]["live_actor_roots"], 0);
        assert_eq!(status["runtime"]["open_routes"], 0);
    }

    #[test]
    fn deleted_root_reap_blocker_census_is_exposed_in_health_metrics() {
        let (root_dir, root) = test_root("deleted-root-reap-census");
        let executor = Arc::new(Executor::new());
        assert!(executor.register_actor(root.clone(), test_ctx()));
        root_dir.close().expect("delete project root");

        let mut live_roots = HashMap::from([(root, RootMeta::new(Instant::now()))]);
        let metrics = DispatchPathMetrics::new();
        let outcome = reap_idle_roots(
            Instant::now(),
            &mut live_roots,
            &HashMap::new(),
            &HashMap::new(),
            &executor,
            &metrics,
        );
        assert_eq!(outcome.evicted, 0);

        let app = crate::context::App::default_shared();
        let health_rollup_cache = HealthRollupCache::new();
        health_rollup_cache.refresh(&executor, &app);
        let report = build_health_report(
            &health_rollup_cache,
            &executor,
            &HashMap::new(),
            &metrics,
            &app,
        );
        let reap = report
            .metrics
            .as_ref()
            .and_then(|metrics| metrics.get("reap"))
            .expect("reap health metrics");
        assert_eq!(reap["deleted_retained"].as_u64(), Some(1));
        assert_eq!(reap["blockers"]["absence_unconfirmed"].as_u64(), Some(1));
        assert_eq!(reap["blockers"]["unbound_quiesced"].as_u64(), Some(0));
        assert_eq!(reap["blockers"]["actor_busy"].as_u64(), Some(0));
    }

    #[test]
    fn connection_exit_quiesces_queued_maintenance_and_deleted_root_is_purged() {
        let (root_dir, root) = test_root("connection-exit-deleted-root");
        let executor = Arc::new(Executor::new());
        assert!(executor.register_actor(root.clone(), test_ctx()));

        let route = route_key(11, 1);
        let mut meta = RootMeta::new(Instant::now());
        meta.maintenance_pending = true;
        meta.maintenance_queued_kinds
            .push_back(MaintenanceDrainKind::CompletionDrains);
        let mut live_roots = HashMap::from([(root.clone(), meta)]);
        let mut pending_binds = HashMap::new();
        let mut routes = HashMap::from([(route, route_identity(&root, "abandoned"))]);
        let mut root_channels = HashMap::from([(root.clone(), HashSet::from([route]))]);
        let mut installed_route_epochs = HashMap::from([(route.channel, route.epoch)]);
        let mut route_bash_cancels = HashMap::new();
        let active_tool_calls: ActiveToolCalls = Arc::new(StdMutex::new(HashMap::new()));

        quiesce_connection_roots(
            &mut live_roots,
            &mut pending_binds,
            &mut routes,
            &mut root_channels,
            &mut installed_route_epochs,
            &mut route_bash_cancels,
            &active_tool_calls,
            &executor,
        );
        assert!(live_roots[&root].unbound_quiesced);
        assert!(!live_roots[&root].maintenance_pending);
        assert!(live_roots[&root].maintenance_queued_kinds.is_empty());
        assert!(routes.is_empty());
        assert!(root_channels.is_empty());

        root_dir.close().expect("delete project root");
        let metrics = DispatchPathMetrics::new();
        let outcome = reap_until_forgotten(
            &root,
            &mut live_roots,
            &pending_binds,
            &root_channels,
            &executor,
            &metrics,
        );
        let mut session_identity = HashMap::new();
        let mut push_buffer = HashMap::new();
        let mut bg_subs = HashMap::new();
        let mut bg_sub_by_session = HashMap::new();
        let mut bg_wake_pending = HashSet::new();
        let mut bg_wake_epoch = HashMap::new();
        let mut pending_bash_asks = HashMap::new();
        let mut retry_buffer = HashMap::new();
        let mut reclaimed_routes = ReclaimedRoutes::default();
        for forgotten in &outcome.forgotten_deleted_roots {
            purge_deleted_root_residents(
                forgotten,
                &mut routes,
                &mut root_channels,
                &mut installed_route_epochs,
                &mut route_bash_cancels,
                &active_tool_calls,
                executor.as_ref(),
                &mut retry_buffer,
                &mut reclaimed_routes,
                &mut session_identity,
                &mut push_buffer,
                &mut bg_subs,
                &mut bg_sub_by_session,
                &mut bg_wake_pending,
                &mut bg_wake_epoch,
                &mut pending_bash_asks,
                &metrics,
            );
        }

        assert_eq!(outcome.forgotten_deleted_roots, vec![root.clone()]);
        assert!(!executor.actor_registered(&root));
        assert!(!live_roots.contains_key(&root));
    }

    #[test]
    fn unbound_root_quiesces_maintenance_without_removing_actor() {
        let (_root_dir, root) = test_root("unbound-root-quiesce");
        let ctx = test_ctx();
        let executor = Arc::new(Executor::new());
        assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));
        let mut meta = RootMeta::new(Instant::now());
        meta.maintenance_pending = true;
        meta.maintenance_jobs_in_flight = 1;
        meta.maintenance_queued_kinds
            .push_back(MaintenanceDrainKind::ConfigureTail);
        let mut live_roots = HashMap::from([(root.clone(), meta)]);
        // Warm state planted before the unbind: quiesce must keep it.
        *ctx.search_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(crate::search_index::SearchIndex::new());
        ctx.set_cache_writer_capabilities(true, true);
        let pending = root.as_path().join("pending.rs");
        ctx.add_pending_search_index_paths([pending.clone()]);
        // A warm verify memo must survive the transient unbind: the watcher
        // keeps running, so no unobserved window exists and the next warm
        // reload must not pay a strict full-corpus re-hash.
        let canonical_root = root.as_path().to_path_buf();
        let artifact = canonical_root.join("cache.bin");
        std::fs::write(&artifact, b"warm-artifact").expect("write artifact");
        let seeded_generation = crate::cache_freshness::artifact_generation(&artifact);
        crate::cache_freshness::record_verify_completed(
            &canonical_root,
            crate::cache_freshness::VerifyArtifact::Search,
            seeded_generation,
        );
        assert!(matches!(
            crate::cache_freshness::warm_verify_plan(
                &canonical_root,
                crate::cache_freshness::VerifyArtifact::Search,
                seeded_generation,
            ),
            crate::cache_freshness::WarmVerifyPlan::Skip
        ));
        // A live watcher runtime must survive quiesce (its events accumulate
        // for the rebind replay).
        let (dispatch_tx, dispatch_rx) = crate::watcher_filter::watcher_dispatch_channel();
        let _dispatch_tx = dispatch_tx;
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);
        let join = std::thread::spawn(move || {
            while !thread_shutdown.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
        });
        ctx.install_watcher_runtime(
            dispatch_rx,
            crate::watcher_filter::WatcherThreadHandle::new(shutdown, join),
        );
        assert!(ctx.watcher_runtime_active());

        quiesce_unbound_root(&root, &mut live_roots, &executor);
        let meta = &live_roots[&root];
        assert!(meta.unbound_quiesced);
        assert!(ctx.subc_unbound_quiesced());
        assert!(meta.maintenance_pending);
        assert!(meta.maintenance_queued_kinds.is_empty());
        assert!(executor.actor_registered(&root));
        // Transient unbind keeps the root warm: resident artifacts stay
        // resident, no forced callgraph rebuild is planted, and pending
        // reconciliation paths survive for the rebind replay.
        assert!(
            ctx.search_index()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some(),
            "quiesce must not evict resident artifacts"
        );
        assert_eq!(
            ctx.pending_callgraph_store_force_token(),
            None,
            "quiesce must not force a callgraph rebuild"
        );
        assert_eq!(
            ctx.take_pending_search_index_paths(),
            vec![pending],
            "quiesce must retain pending watcher-derived paths"
        );
        assert!(
            matches!(
                crate::cache_freshness::warm_verify_plan(
                    &canonical_root,
                    crate::cache_freshness::VerifyArtifact::Search,
                    seeded_generation,
                ),
                crate::cache_freshness::WarmVerifyPlan::Skip
            ),
            "quiesce must not invalidate the warm verify memo"
        );
        assert!(
            ctx.watcher_runtime_active(),
            "quiesce must not stop a running watcher"
        );
        ctx.stop_watcher_runtime();

        let meta = live_roots.get_mut(&root).expect("root metadata");
        note_maintenance_completion(
            meta,
            Some(MaintenanceDrainKind::ConfigureTail),
            false,
            meta.unbound_quiesced,
        );
        assert!(!meta.maintenance_pending);
        assert!(meta.maintenance_queued_kinds.is_empty());
    }

    #[test]
    fn same_root_higher_epoch_replacement_does_not_quiesce_between_generations() {
        let (_dir, root) = test_root("same-root-replacement");
        let route = route_key(7, 1);
        let installed_channels = HashMap::from([(root.clone(), HashSet::from([route]))]);
        let root_channels = HashMap::new();

        assert!(!route_removal_will_quiesce_root(
            &root,
            route,
            &installed_channels,
            false,
            Some(&root),
        ));
        assert!(route_removal_will_quiesce_root(
            &root,
            route,
            &installed_channels,
            false,
            None,
        ));
        assert!(!should_quiesce_removed_root(
            &root,
            &root_channels,
            false,
            Some(&root),
        ));
        assert!(should_quiesce_removed_root(
            &root,
            &root_channels,
            false,
            None,
        ));
        assert!(!should_quiesce_removed_root(
            &root,
            &root_channels,
            true,
            None,
        ));
    }

    #[test]
    fn root_quiesces_only_after_its_last_route_is_removed_and_reactivates_on_bind() {
        let (_root_dir, root) = test_root("unbound-root-route-count");
        let executor = Arc::new(Executor::new());
        assert!(executor.register_actor(root.clone(), test_ctx()));
        let mut live_roots = HashMap::from([(root.clone(), RootMeta::new(Instant::now()))]);
        let mut root_channels = HashMap::from([(
            root.clone(),
            HashSet::from([route_key(7, 1), route_key(8, 1)]),
        )]);

        remove_root_channel(&mut root_channels, &root, route_key(7, 1));
        if !root_channels.contains_key(&root) {
            quiesce_unbound_root(&root, &mut live_roots, &executor);
        }
        assert!(!live_roots[&root].unbound_quiesced);

        remove_root_channel(&mut root_channels, &root, route_key(8, 1));
        if !root_channels.contains_key(&root) {
            quiesce_unbound_root(&root, &mut live_roots, &executor);
        }
        assert!(live_roots[&root].unbound_quiesced);

        live_roots
            .get_mut(&root)
            .expect("root metadata")
            .note_activity();
        assert!(
            live_roots[&root].unbound_quiesced,
            "late asynchronous activity must not reactivate an unbound root"
        );

        live_roots
            .get_mut(&root)
            .expect("root metadata")
            .reactivate_bound();
        assert!(!live_roots[&root].unbound_quiesced);
    }

    #[test]
    fn allocator_pressure_relief_requires_every_root_to_be_idle() {
        let (_idle_dir, idle_root) = test_root("allocator-relief-idle");
        let (_active_dir, active_root) = test_root("allocator-relief-active");
        let now = Instant::now();
        let mut live_roots = HashMap::new();
        let mut idle = RootMeta::new(now);
        idle.last_touched = now - IDLE_ROOT_TTL - Duration::from_secs(1);
        live_roots.insert(idle_root, idle);
        assert!(process_has_been_idle(now, &live_roots));

        live_roots.insert(active_root.clone(), RootMeta::new(now));
        assert!(!process_has_been_idle(now, &live_roots));

        let active = live_roots
            .get_mut(&active_root)
            .expect("active root metadata");
        active.last_touched = now - IDLE_ROOT_TTL - Duration::from_secs(1);
        active.active_bash_waits = 1;
        assert!(!process_has_been_idle(now, &live_roots));
    }

    #[test]
    fn pressure_relief_log_reports_before_and_after_measurements() {
        let allocator = crate::memory::AllocatorMemorySnapshot {
            status: "measured",
            bytes_in_use: Some(8 * 1024 * 1024),
            size_allocated: Some(12 * 1024 * 1024),
            retained_slack_bytes: Some(4 * 1024 * 1024),
            not_estimated: None,
        };
        let relief = crate::memory::AllocatorPressureRelief {
            bytes_released: 3 * 1024 * 1024,
            rss_before_bytes: Some(20 * 1024 * 1024),
            rss_after_bytes: Some(17 * 1024 * 1024),
            allocator_before: allocator.clone(),
            allocator_after: crate::memory::AllocatorMemorySnapshot {
                size_allocated: Some(9 * 1024 * 1024),
                retained_slack_bytes: Some(1024 * 1024),
                ..allocator
            },
        };
        let message = pressure_relief_label(&relief);
        assert!(message.contains("RSS 20.0 MB -> 17.0 MB"));
        assert!(message.contains("allocated 12.0 MB -> 9.0 MB"));
        assert!(message.contains("slack 4.0 MB -> 1.0 MB"));
        assert!(message.contains("reported 3.0 MB released"));
    }

    #[test]
    fn due_maintenance_jobs_skip_poisoned_roots() {
        let (_healthy_dir, healthy_root) = test_root("maintenance-healthy");
        let (_poisoned_dir, poisoned_root) = test_root("maintenance-poisoned");
        let mut live_roots = HashMap::new();
        live_roots.insert(healthy_root.clone(), RootMeta::new(Instant::now()));
        let mut poisoned_meta = RootMeta::new(Instant::now());
        poisoned_meta.maintenance_poisoned = true;
        live_roots.insert(poisoned_root.clone(), poisoned_meta);

        let (due, deferred) = due_maintenance_jobs_without_actor_context(
            &mut live_roots,
            MAINTENANCE_SUBMIT_BUDGET,
            &HashSet::new(),
        );

        assert_eq!(due.len(), INITIAL_MAINTENANCE_JOB_COUNT);
        assert!(due.iter().all(|(root, _)| root == &healthy_root));
        assert!(!deferred);
        assert!(live_roots[&healthy_root].maintenance_pending);
        assert_eq!(
            live_roots[&healthy_root].maintenance_jobs_in_flight,
            INITIAL_MAINTENANCE_JOB_COUNT
        );
        assert!(!live_roots[&poisoned_root].maintenance_pending);
    }

    #[test]
    fn due_maintenance_jobs_do_not_restart_quiesced_root_work() {
        let (_dir, root) = test_root("maintenance-unbound");
        let mut meta = RootMeta::new(Instant::now());
        meta.unbound_quiesced = true;
        let mut live_roots = HashMap::from([(root.clone(), meta)]);

        let (due, deferred) = due_maintenance_jobs_without_actor_context(
            &mut live_roots,
            MAINTENANCE_SUBMIT_BUDGET,
            &HashSet::new(),
        );

        assert!(due.is_empty());
        assert!(!deferred);
        assert!(!live_roots[&root].maintenance_pending);
    }

    #[test]
    fn idle_bg_subscription_queues_no_jobs_until_a_wake_arrives() {
        let (_dir, root) = test_root("maintenance-idle-bg-subscription");
        let ctx = test_ctx();
        assert!(!ctx.completion_drains_have_work());

        let executor = Executor::new();
        assert!(executor.register_actor(root.clone(), ctx));
        let mut live_roots = HashMap::from([(root.clone(), RootMeta::new(Instant::now()))]);
        let session = "idle-session".to_string();
        let channel = route_key(17, 1);
        let metrics = DispatchPathMetrics::new();
        let bg_sub_by_session =
            HashMap::from([((root.clone(), session.clone()), HashSet::from([channel]))]);
        let mut bg_wake_pending = HashSet::new();

        let (idle_tick_jobs, deferred) = due_maintenance_jobs(
            &mut live_roots,
            Some(&executor),
            &bg_sub_by_session,
            &bg_wake_pending,
            MAINTENANCE_SUBMIT_BUDGET,
            &HashSet::new(),
        );
        assert!(idle_tick_jobs.is_empty());
        assert!(!deferred);
        assert!(!live_roots[&root].maintenance_pending);

        // A completion can arm its wake after the idle tick's probes. The wake
        // remains loop-owned state, so the following tick must observe it.
        let mut bg_wake_epoch = HashMap::new();
        push::arm_bg_wake(
            root.clone(),
            session,
            channel,
            &mut bg_wake_pending,
            &mut bg_wake_epoch,
            &metrics,
        );
        let (next_tick_jobs, deferred) = due_maintenance_jobs(
            &mut live_roots,
            Some(&executor),
            &bg_sub_by_session,
            &bg_wake_pending,
            MAINTENANCE_SUBMIT_BUDGET,
            &HashSet::new(),
        );
        assert_eq!(
            next_tick_jobs,
            vec![(root, MaintenanceDrainKind::CompletionDrains)]
        );
        assert!(!deferred);
    }

    async fn assert_slow_configure_tail_admission(
        config: crate::executor::ExecutorConfig,
        shape: &'static str,
    ) {
        let root_dir = tempfile::tempdir().unwrap();
        let root_path = std::fs::canonicalize(root_dir.path()).unwrap();
        let root = ProjectRootId::from_path(&root_path).unwrap();
        let ctx = test_ctx();
        ctx.mark_subc_bound();
        let storage_root = ctx.storage_dir();
        ctx.enqueue_configure_maintenance(crate::context::ConfigureMaintenanceJob {
            generation: ctx.configure_generation(),
            root_path: root_path.clone(),
            canonical_cache_root: root_path.clone(),
            harness: crate::harness::Harness::Opencode,
            storage_root: storage_root.clone(),
            harness_dir: storage_root.join("opencode"),
            session_id: "first-search-admission".to_string(),
            home_match: false,
            format_tool_cache_clear_needed: false,
            run_bash_replay: false,
            refresh_project_runtime: false,
            sync_bash_compress_flag: false,
            reset_filter_registry: false,
            clear_failed_spawns: false,
            warm_callgraph_store: false,
            supersede_search_artifact_persistence: false,
            supersede_callgraph_artifact_persistence: false,
            supersede_semantic_artifact_persistence: false,
            artifact_load_starts: Vec::new(),
        })
        .expect("queue configure maintenance");
        let (_gate, maintenance_reached, release_maintenance) =
            crate::commands::configure::gate_configure_deferred_maintenance_for_test(
                root_path.clone(),
            );

        let expected_pool_size = config.pool_size;
        let expected_actor_cap = config.actor_cap;
        let executor = Arc::new(Executor::with_config(config));
        assert_eq!(executor.pool_size(), expected_pool_size);
        assert_eq!(executor.actor_cap(), expected_actor_cap);
        assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));
        let metrics = Arc::new(DispatchPathMetrics::new());
        let (completion_tx, mut completion_rx) = mpsc::channel(2);
        submit_maintenance_job(
            &executor,
            root.clone(),
            MaintenanceDrainKind::ConfigureTail,
            Vec::new(),
            &completion_tx,
            &metrics,
        );
        maintenance_reached
            .recv_timeout(Duration::from_secs(2))
            .expect("configure tail reached gate");

        let admission_started = std::time::Instant::now();
        let (search_admitted_tx, search_admitted_rx) = crossbeam_channel::bounded(1);
        let search = executor.submit_async(
            root.clone(),
            Lane::HeavyInit,
            "first-search".to_string(),
            Box::new(move |_ctx| {
                search_admitted_tx
                    .send(())
                    .expect("signal search admission");
                Response::success("first-search", json!({}))
            }),
        );
        let (mutation_started_tx, mutation_started_rx) = crossbeam_channel::bounded(1);
        let mutation = executor.submit_async(
            root,
            Lane::Mutating,
            "queued-mutation".to_string(),
            Box::new(move |_ctx| {
                mutation_started_tx.send(()).expect("signal mutation start");
                Response::success("queued-mutation", json!({}))
            }),
        );

        // The decision under test is ORDERING, not latency: this recv happens
        // while the maintenance gate is still held, so a search that could only
        // admit after maintenance completes can never satisfy it. The budget is
        // a hang catch - a tight bound here just measures runner scheduling
        // (the census S-class shape) and flaked on loaded macOS CI at 100ms.
        let search_admission = search_admitted_rx.recv_timeout(Duration::from_secs(10));
        let admission_elapsed = admission_started.elapsed();
        let mutation_waited = mutation_started_rx.try_recv().is_err();
        release_maintenance
            .send(())
            .expect("release configure maintenance");
        search_admission
            .expect("first search must admit while configure maintenance remains gated");
        eprintln!(
            "first-search admission while configure maintenance is gated ({shape}): {}ms",
            admission_elapsed.as_millis()
        );
        assert!(
            mutation_waited,
            "mutating work must wait for configure maintenance to release its read epoch"
        );
        tokio::time::timeout(Duration::from_secs(5), search)
            .await
            .expect("first search completion timed out")
            .expect("first search completion channel closed");
        tokio::time::timeout(Duration::from_secs(5), mutation)
            .await
            .expect("mutation completion timed out")
            .expect("mutation completion channel closed");
        tokio::time::timeout(Duration::from_secs(5), completion_rx.recv())
            .await
            .expect("configure-tail completion timed out")
            .expect("configure-tail completion channel closed");
    }

    #[tokio::test]
    async fn slow_configure_tail_admits_first_search_but_not_mutating_work() {
        assert_slow_configure_tail_admission(
            crate::executor::ExecutorConfig {
                pool_size: 2,
                read_cap: 1,
                actor_cap: 1,
                heavy_permits: 1,
                drr_quantum: 1,
                ..crate::executor::ExecutorConfig::default()
            },
            "pool=2 actor_cap=1",
        )
        .await;
        assert_slow_configure_tail_admission(
            crate::executor::ExecutorConfig {
                pool_size: 4,
                read_cap: 3,
                actor_cap: 3,
                heavy_permits: 3,
                drr_quantum: 1,
                ..crate::executor::ExecutorConfig::default()
            },
            "pool=4 actor_cap=3",
        )
        .await;
    }

    #[tokio::test]
    async fn subc_configure_tail_precedes_completed_search_install() {
        let root_dir = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let root = ProjectRootId::from_path(root_dir.path()).unwrap();
        let (ctx, ignored_path) =
            runtime_drain::configure_search_order_context_for_test(root_dir.path(), storage.path());
        let ctx = Arc::new(ctx);
        assert!(!runtime_drain::watcher_path_is_ignored_by_current_matcher(
            &ctx,
            &ignored_path
        ));

        let executor = Arc::new(Executor::new());
        assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));
        let metrics = Arc::new(DispatchPathMetrics::new());
        let (completion_tx, mut completion_rx) = mpsc::channel(4);
        submit_maintenance_job(
            &executor,
            root.clone(),
            MaintenanceDrainKind::ConfigureTail,
            Vec::new(),
            &completion_tx,
            &metrics,
        );
        submit_maintenance_job(
            &executor,
            root,
            MaintenanceDrainKind::CompletionDrains,
            Vec::new(),
            &completion_tx,
            &metrics,
        );

        let first = tokio::time::timeout(Duration::from_secs(5), completion_rx.recv())
            .await
            .expect("configure-tail completion timed out")
            .expect("configure-tail completion channel closed");
        let second = tokio::time::timeout(Duration::from_secs(5), completion_rx.recv())
            .await
            .expect("completion-drains completion timed out")
            .expect("completion-drains completion channel closed");
        assert!(first.response.id.contains("configure-tail"));
        assert!(second.response.id.contains("completion-drains"));
        assert!(runtime_drain::watcher_path_is_ignored_by_current_matcher(
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
    fn post_bind_configure_and_completion_jobs_are_queued_in_order() {
        let (_dir, root) = test_root("maintenance-post-bind");
        let mut live_roots = HashMap::new();
        live_roots.insert(root.clone(), RootMeta::new(Instant::now()));

        queue_post_bind_configure_and_completion_maintenance(&root, &mut live_roots);
        queue_post_bind_configure_and_completion_maintenance(&root, &mut live_roots);

        let meta = live_roots.get(&root).expect("root metadata");
        assert!(meta.maintenance_pending);
        assert_eq!(meta.maintenance_jobs_in_flight, 0);
        assert_eq!(
            meta.maintenance_queued_kinds
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![
                MaintenanceDrainKind::ConfigureTail,
                MaintenanceDrainKind::CompletionDrains,
            ]
        );

        let (due, deferred) = due_maintenance_jobs_without_actor_context(
            &mut live_roots,
            MAINTENANCE_SUBMIT_BUDGET,
            &HashSet::new(),
        );

        assert_eq!(
            due,
            vec![
                (root.clone(), MaintenanceDrainKind::ConfigureTail),
                (root.clone(), MaintenanceDrainKind::CompletionDrains),
            ]
        );
        assert!(!deferred);
        assert_eq!(live_roots[&root].maintenance_jobs_in_flight, 2);
        assert!(live_roots[&root].maintenance_queued_kinds.is_empty());
    }

    #[test]
    fn due_maintenance_jobs_defers_unsubmitted_roots_without_marking_pending() {
        let mut live_roots = HashMap::new();
        let mut root_ids = Vec::new();
        let mut _dirs = Vec::new();
        for index in 0..4 {
            let (dir, root_id) = test_root(&format!("maintenance-budget-{index}"));
            live_roots.insert(root_id.clone(), RootMeta::new(Instant::now()));
            root_ids.push(root_id);
            _dirs.push(dir);
        }

        let small_budget = INITIAL_MAINTENANCE_JOB_COUNT + 1;
        let (first_due, first_deferred) = due_maintenance_jobs_without_actor_context(
            &mut live_roots,
            small_budget,
            &HashSet::new(),
        );

        assert_eq!(first_due.len(), small_budget);
        assert!(first_deferred);
        let first_due_set: HashSet<_> = first_due.into_iter().map(|(root, _)| root).collect();
        assert!(first_due_set
            .iter()
            .all(|root| live_roots[root].maintenance_pending));
        assert!(first_due_set
            .iter()
            .any(|root| !live_roots[root].maintenance_queued_kinds.is_empty()));

        let all_roots: HashSet<_> = root_ids.into_iter().collect();
        let deferred_roots: HashSet<_> = all_roots.difference(&first_due_set).cloned().collect();
        assert!(deferred_roots
            .iter()
            .all(|root| !live_roots[root].maintenance_pending));
    }

    #[test]
    fn due_maintenance_jobs_defers_pending_bind_roots() {
        let (_bind_dir, bind_root) = test_root("maintenance-pending-bind");
        let (_healthy_dir, healthy_root) = test_root("maintenance-no-bind");
        let mut live_roots = HashMap::new();
        live_roots.insert(bind_root.clone(), RootMeta::new(Instant::now()));
        live_roots.insert(healthy_root.clone(), RootMeta::new(Instant::now()));
        let pending_bind_roots = HashSet::from([bind_root.clone()]);

        let (due, deferred) = due_maintenance_jobs_without_actor_context(
            &mut live_roots,
            usize::MAX,
            &pending_bind_roots,
        );

        assert_eq!(due.len(), INITIAL_MAINTENANCE_JOB_COUNT);
        assert!(due.iter().all(|(root, _)| root == &healthy_root));
        assert!(!deferred);
        assert!(!live_roots[&bind_root].maintenance_pending);
        assert!(live_roots[&bind_root].maintenance_queued_kinds.is_empty());
    }

    #[test]
    fn maintenance_pending_survives_requeue_and_clears_after_final_batch() {
        let (_dir, root) = test_root("maintenance-requeue");
        let mut live_roots = HashMap::new();
        live_roots.insert(root.clone(), RootMeta::new(Instant::now()));
        let (due, deferred) = due_maintenance_jobs_without_actor_context(
            &mut live_roots,
            usize::MAX,
            &HashSet::new(),
        );
        assert_eq!(due.len(), INITIAL_MAINTENANCE_JOB_COUNT);
        assert!(due.iter().all(|(due_root, _)| due_root == &root));
        assert!(!deferred);

        let meta = live_roots.get_mut(&root).unwrap();
        note_maintenance_completion(meta, Some(MaintenanceDrainKind::Watcher), false, false);
        assert!(meta.maintenance_pending);
        assert_eq!(
            meta.maintenance_jobs_in_flight,
            INITIAL_MAINTENANCE_JOB_COUNT - 1
        );
        assert_eq!(meta.maintenance_queued_kinds.len(), 1);

        let (requeued, deferred) =
            due_maintenance_jobs_without_actor_context(&mut live_roots, 1, &HashSet::new());
        assert_eq!(
            requeued,
            vec![(root.clone(), MaintenanceDrainKind::Watcher)]
        );
        assert!(!deferred);
        let meta = live_roots.get_mut(&root).unwrap();
        assert_eq!(
            meta.maintenance_jobs_in_flight,
            INITIAL_MAINTENANCE_JOB_COUNT
        );
        assert!(meta.maintenance_queued_kinds.is_empty());

        for _ in 0..INITIAL_MAINTENANCE_JOB_COUNT {
            note_maintenance_completion(meta, None, false, false);
        }
        assert!(!meta.maintenance_pending);
        assert_eq!(meta.maintenance_jobs_in_flight, 0);
    }

    #[test]
    fn maintenance_requeue_drops_while_bind_is_pending() {
        let (_dir, root) = test_root("maintenance-bind-requeue");
        let mut live_roots = HashMap::new();
        live_roots.insert(root.clone(), RootMeta::new(Instant::now()));
        let (due, _) = due_maintenance_jobs_without_actor_context(
            &mut live_roots,
            usize::MAX,
            &HashSet::new(),
        );
        assert_eq!(due.len(), INITIAL_MAINTENANCE_JOB_COUNT);

        let meta = live_roots.get_mut(&root).unwrap();
        note_maintenance_completion(meta, Some(MaintenanceDrainKind::Watcher), false, true);

        assert_eq!(
            meta.maintenance_jobs_in_flight,
            INITIAL_MAINTENANCE_JOB_COUNT - 1
        );
        assert!(meta.maintenance_queued_kinds.is_empty());
        assert!(meta.maintenance_pending);
    }

    #[test]
    fn parked_lsp_completion_never_requiesces_or_cancels_a_pending_bind() {
        let mut meta = RootMeta::new(Instant::now());
        meta.unbound_quiesced = true;

        assert!(!should_requiesce_after_maintenance(
            &meta,
            MaintenanceDrainKind::Lsp,
            false,
        ));
        assert!(!should_requiesce_after_maintenance(
            &meta,
            MaintenanceDrainKind::ConfigureTail,
            true,
        ));
        assert!(should_requiesce_after_maintenance(
            &meta,
            MaintenanceDrainKind::ConfigureTail,
            false,
        ));
    }

    #[test]
    fn maintenance_pending_clears_and_poison_stops_requeue_after_fatal() {
        let (_dir, root) = test_root("maintenance-fatal");
        let mut live_roots = HashMap::new();
        live_roots.insert(root.clone(), RootMeta::new(Instant::now()));
        let (due, _) = due_maintenance_jobs_without_actor_context(
            &mut live_roots,
            usize::MAX,
            &HashSet::new(),
        );
        assert_eq!(due.len(), INITIAL_MAINTENANCE_JOB_COUNT);

        let meta = live_roots.get_mut(&root).unwrap();
        note_maintenance_completion(meta, Some(MaintenanceDrainKind::Watcher), true, false);
        assert!(meta.maintenance_poisoned);
        assert!(meta.maintenance_queued_kinds.is_empty());

        for _ in 1..INITIAL_MAINTENANCE_JOB_COUNT {
            note_maintenance_completion(meta, None, false, false);
        }
        assert!(!meta.maintenance_pending);
        assert_eq!(meta.maintenance_jobs_in_flight, 0);
    }

    #[test]
    fn trust_for_principal_matrix() {
        assert_eq!(
            trust_for_principal(&Some(Principal::Direct)),
            BindTrust::FirstParty
        );
        // Every first-party reserved id is asserted BY NAME, in one loop over
        // the full set, so two failure classes stay distinguishable: an empty
        // or broken allowlist reddens every name at once, while a dropped
        // single entry (the rename hazard) reddens exactly the missing name.
        // Both halves of each transitional rename pair stay listed until the
        // flip settles (see the allowlist comment).
        for module_id in [
            "llm-runner",
            "aft",
            "broca",
            "alfonso-core",
            "prefrontal",
            "prefrontal-core",
        ] {
            assert_eq!(
                trust_for_principal(&Some(Principal::Reserved {
                    module_id: module_id.to_string(),
                })),
                BindTrust::FirstParty,
                "reserved module id '{module_id}' must resolve to first-party trust"
            );
        }
        assert_eq!(
            trust_for_principal(&Some(Principal::Reserved {
                module_id: "subc-mcp".to_string(),
            })),
            BindTrust::Untrusted
        );
        assert_eq!(
            trust_for_principal(&Some(Principal::Reserved {
                module_id: "anything-unknown".to_string(),
            })),
            BindTrust::Untrusted
        );
        assert_eq!(
            trust_for_principal(&Some(Principal::Unverified)),
            BindTrust::Untrusted
        );
        assert_eq!(trust_for_principal(&None), BindTrust::Untrusted);
    }

    #[test]
    fn fed_harness_class_maps_to_untrusted_regardless_of_fingerprint_value() {
        let principal = Some(Principal::Direct);
        let fingerprint_a = "fed:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let fingerprint_b = "fed:0123456789abcdef111111111111111111111111111111111111111111111111";

        assert_eq!(
            trust_for_bind(fingerprint_a, &principal),
            BindTrust::Untrusted
        );
        assert_eq!(
            trust_for_bind(fingerprint_b, &principal),
            BindTrust::Untrusted
        );
    }

    /// The table above proves `trust_for_principal` maps correctly, and the
    /// test above proves the `fed:` harness override wins — but neither
    /// exercises the ordinary path, so an implementation that ignored the
    /// principal entirely for non-fed harnesses would satisfy both. Pin the
    /// delegation itself: on a normal harness the verdict must still come from
    /// the principal, in both directions.
    #[test]
    fn trust_for_bind_delegates_to_the_principal_on_ordinary_harnesses() {
        for harness in ["opencode", "pi", "runner", "mcp:claude"] {
            assert_eq!(
                trust_for_bind(harness, &Some(Principal::Direct)),
                BindTrust::FirstParty,
                "a direct principal must stay first-party on {harness}"
            );
            assert_eq!(
                trust_for_bind(harness, &Some(Principal::Unverified)),
                BindTrust::Untrusted,
                "an unverified principal must stay untrusted on {harness}"
            );
            assert_eq!(
                trust_for_bind(harness, &None),
                BindTrust::Untrusted,
                "an absent principal must fail closed on {harness}"
            );
            assert_eq!(
                trust_for_bind(
                    harness,
                    &Some(Principal::Reserved {
                        module_id: "subc-mcp".to_string(),
                    })
                ),
                BindTrust::Untrusted,
                "a non-allowlisted reserved module must stay untrusted on {harness}"
            );
        }
    }

    #[tokio::test]
    async fn persistent_cancel_resolves_when_fired_before_await() {
        // The lost-wakeup guard: cancel() fires exactly once via notify_waiters()
        // (no stored permit). A waiter that registers AFTER the cancel must still
        // observe it via the flag; a waiter racing the cancel must still be woken.
        let signal = PersistentCancelSignal::new();
        signal.cancel();
        // Fired before we ever call cancelled() — must return immediately, not park.
        tokio::time::timeout(Duration::from_secs(1), signal.cancelled())
            .await
            .expect("cancelled() must resolve when cancel fired beforehand");

        // A fresh signal cancelled concurrently with an in-flight cancelled().
        let racing = PersistentCancelSignal::new();
        let racing_for_task = racing.clone();
        let waiter = tokio::spawn(async move { racing_for_task.cancelled().await });
        racing.cancel();
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("cancelled() must resolve when cancel races the await")
            .expect("waiter task panicked");
    }

    #[test]
    fn ingress_epoch_validation_rejects_reclaimed_requests_and_drops_other_stale_epochs() {
        let installed = HashMap::from([(7, 9)]);
        let mut reclaimed = ReclaimedRoutes::default();
        reclaimed.insert(route_key(8, 1));
        for ty in [
            FrameType::Request,
            FrameType::Response,
            FrameType::Error,
            FrameType::Push,
            FrameType::Cancel,
            FrameType::Goodbye,
        ] {
            let body = if ty.is_pure_header() {
                Vec::new()
            } else {
                br#"{}"#.to_vec()
            };
            let stale = Frame::build(ty, control_flags(), 7, 8, 41, body).unwrap();
            assert!(
                !ingress_route_should_be_processed(&installed, &reclaimed, &stale),
                "{ty:?}"
            );
        }

        let reclaimed_request = Frame::build(
            FrameType::Request,
            control_flags(),
            8,
            1,
            42,
            br#"{}"#.to_vec(),
        )
        .unwrap();
        assert!(ingress_route_should_be_processed(
            &installed,
            &reclaimed,
            &reclaimed_request
        ));

        let never_installed = Frame::build(
            FrameType::Request,
            control_flags(),
            9,
            1,
            43,
            br#"{}"#.to_vec(),
        )
        .unwrap();
        assert!(!ingress_route_should_be_processed(
            &installed,
            &reclaimed,
            &never_installed
        ));

        let current = Frame::build(
            FrameType::Request,
            control_flags(),
            7,
            9,
            43,
            br#"{}"#.to_vec(),
        )
        .unwrap();
        let control = Frame::build(FrameType::Ping, control_flags(), 0, 0, 44, Vec::new()).unwrap();
        assert!(ingress_route_should_be_processed(
            &installed, &reclaimed, &current
        ));
        assert!(ingress_route_should_be_processed(
            &installed, &reclaimed, &control
        ));
        assert_eq!(installed, HashMap::from([(7, 9)]));
    }

    #[tokio::test]
    async fn route_bind_ack_precedes_route_egress_in_writer_queue() {
        let (_dir, root) = test_root("route-bind-b2-ordering");
        let route = route_key(7, 3);
        let identity = RouteIdentity(Arc::new(RouteIdentityData {
            root: root.clone(),
            project_root: root.as_path().to_path_buf(),
            harness: "opencode".to_string(),
            session: "b2-session".to_string(),
            trust: BindTrust::FirstParty,
            spawn_principal: AuthenticatedPrincipal::FirstParty,
            consumer_elicitation_capable: false,
        }));
        let replay_key = push::ReplayKey::from_identity(&identity);
        let completion = RouteBindCompletion {
            route,
            identity,
            bind_root_id: root.clone(),
            inserted_new_actor: false,
            configure_response: Response::success("subc-bind-7", json!({})),
            diagnostics_on_edit: false,
            ver: PROTOCOL_VERSION,
            corr: 91,
            flags: control_flags(),
        };
        let mut pending_binds = HashMap::from([(
            route,
            PendingBind {
                bind_root_id: root,
                inserted_new_actor: false,
                cancelled: false,
                configure_request_id: "subc-bind-7".to_string(),
                started_at: Instant::now(),
                warned_half_deadline: false,
                deadline_reported: false,
                corr: 91,
                ver: PROTOCOL_VERSION,
                flags: control_flags(),
                cancellation: crate::executor::JobCancellation::new(),
            },
        )]);
        let mut installed_route_epochs = HashMap::from([(route.channel, route.epoch)]);
        let mut push_buffer =
            HashMap::from([(replay_key, VecDeque::from([completion_frame("b2-replay")]))]);
        let (writer_tx, mut writer_rx) = mpsc::channel(8);
        let metrics = Arc::new(DispatchPathMetrics::new());
        let executor = Arc::new(Executor::new());
        let standing_actor =
            standing::StandingActor::new(App::default_shared(), Arc::clone(&executor));

        handle_route_bind_completion(
            &writer_tx,
            completion,
            &mut HashMap::new(),
            &mut HashMap::new(),
            &mut HashMap::new(),
            &mut push_buffer,
            &mut HashMap::new(),
            &mut pending_binds,
            &mut installed_route_epochs,
            &executor,
            &standing_actor,
            &Arc::new(Notify::new()),
            &metrics,
            None,
        )
        .await
        .unwrap();

        let ack = writer_rx.try_recv().expect("RouteBindAck");
        assert_eq!(ack.header.ty, FrameType::Response);
        assert_eq!((ack.header.channel, ack.header.epoch), (0, 0));
        let route_frame = writer_rx.try_recv().expect("post-ack route frame");
        assert_eq!(route_frame.header.ty, FrameType::Push);
        assert_eq!(
            (route_frame.header.channel, route_frame.header.epoch),
            (route.channel, route.epoch)
        );
    }
}
