//! The runner records each notice's send_id durably, so retries cannot deliver
//! the same notice twice. Runner sessions do not acknowledge AFT push frames.

mod bash_requests;
pub(super) use bash_requests::notice_send_id;
pub(crate) mod notice_templates;

use super::*;
use crate::bash_background::persistence::{RunnerCompletionDelivery, RunnerCompletionPhase};
use crate::bash_background::runner_notice_scheduler::{
    retry_delay, scheduler, Follow, FollowReceipt, Next, Scheduler, SCHEDULER,
};
#[cfg(test)]
use aft_engine::test_bridge::bash_remote::QUEUE_CAPACITY;
use cortexkit_role_llm_runner::{
    delivery_order::{render, template_digest, DeliveryOrder, Mode, TemplateUse},
    send::{states, SendReply, SendRequest},
    send_status::SendStatusRequest,
};
use subc_client_rs::{CallError, CallOptions, CloseRouteOptions, ConsumerOptions, SubcConsumer};
use subc_protocol::{
    scope::{ParentState, ScopeKind, ScopeSelector, ScopeStamp},
    BindIdentity, RouteTarget,
};

const MAX_ATTEMPTS: usize = 10;
const MAX_AGE_MS: u64 = 24 * 60 * 60 * 1000;
const MAX_GENERATIONS: u8 = 5;
const RUNNER_CARRIERS: &[&str] = &["reserved:thalamus-harness", "reserved:broca"];
// The send must be accepted before the result returns to ride that result,
// rather than the next. Two seconds caps how long a slow runner delays a tool call.
const CODE_SEARCH_NOTICE_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) fn known_runner(module_id: &str) -> bool {
    RUNNER_CARRIERS
        .iter()
        .any(|principal| principal.strip_prefix("reserved:") == Some(module_id))
}
static CONNECTION: OnceLock<std::sync::RwLock<PathBuf>> = OnceLock::new();
// These weak references are registered only for live runner routes. Closing a
// route removes its reference; disconnected sessions are not harness evidence.
static BOUND_ROUTES: OnceLock<
    std::sync::Mutex<HashMap<usize, std::sync::Weak<RouteIdentityData>>>,
> = OnceLock::new();
static EXPIRED: AtomicU64 = AtomicU64::new(0);
static IDLE_START_FALLBACK: AtomicU64 = AtomicU64::new(0);
static NOT_DELIVERED: AtomicU64 = AtomicU64::new(0);
static PENDING_UNRESOLVED: AtomicU64 = AtomicU64::new(0);
pub(super) fn health() -> (usize, u64, u64, u64, u64) {
    let waiting = SCHEDULER
        .get()
        .and_then(|result| result.as_ref().ok())
        .map(|scheduler| {
            scheduler
                .entries
                .lock()
                .unwrap()
                .values()
                .filter(|entry| entry.completion && entry.retrying && entry.work.is_some())
                .count()
        })
        .unwrap_or(0);
    (
        waiting,
        EXPIRED.load(Ordering::Relaxed),
        IDLE_START_FALLBACK.load(Ordering::Relaxed),
        NOT_DELIVERED.load(Ordering::Relaxed),
        PENDING_UNRESOLVED.load(Ordering::Relaxed),
    )
}

pub(super) fn install(path: PathBuf) {
    *CONNECTION.get_or_init(Default::default).write().unwrap() = path;
}

pub(super) fn is_runner(identity: &crate::bash_background::TaskIdentity) -> bool {
    identity.harness.parse::<crate::harness::Harness>() == Ok(crate::harness::Harness::Runner)
        || identity
            .principal
            .as_deref()
            .is_some_and(|principal| RUNNER_CARRIERS.contains(&principal))
        || retired(identity)
}

pub(super) fn retired(identity: &crate::bash_background::TaskIdentity) -> bool {
    identity.principal.as_deref() == Some("reserved:thalamus-pi")
}

pub(super) fn runner_route(route: &RouteIdentity) -> bool {
    is_runner(&route.task_identity())
}

pub(super) fn warning(
    route: &RouteIdentity,
    root: &ProjectRootId,
    warning: &crate::protocol::ConfigureWarningsFrame,
) {
    let owner = route.task_identity();
    let raw = match &route.spawn_principal {
        AuthenticatedPrincipal::RouteBind { harness, .. } => harness.clone(),
        _ => return,
    };
    let Some(address) = Address::new(&owner, raw, route.scope.as_ref()) else {
        return;
    };
    let Some(path) = CONNECTION.get().map(|path| path.read().unwrap().clone()) else {
        return;
    };
    for warning in &warning.warnings {
        let root_key = blake3::hash(root.as_path().to_string_lossy().as_bytes());
        let warning_key = blake3::hash(&serde_json::to_vec(warning).unwrap());
        let id = format!("aft:warning:{root_key}:{warning_key}");
        let request = request(
            id,
            super::bash_notice_text::warning(warning),
            false,
            address.worker,
        );
        let address = address.clone();
        let path = path.clone();
        let mut attempt = 0;
        let mut retry_logged = false;
        let Some(scheduler) = scheduler() else { return };
        if !scheduler.offer(
            request.send_id.clone(),
            false,
            Box::new(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return Next::Done;
                };
                let next = deliver(
                    &request.send_id,
                    attempt,
                    false,
                    runtime.block_on(send(&path, &address, &request, false)),
                    &mut retry_logged,
                );
                attempt += 1;
                next
            }),
            false,
        ) {
            log::warn!("runner warning deferred: notice queue full");
            return;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Address {
    module: String,
    identity: BindIdentity,
    scope: Option<ScopeSelector>,
    worker: bool,
}

/// A runner route remains a runner route even when its notice address is
/// unavailable. Such nudges are dropped, never appended to command output.
#[derive(Debug, Clone)]
pub struct CodeSearchNotice {
    address: Option<Address>,
    call_key: String,
    preset: String,
    remaining: Arc<std::sync::Mutex<Duration>>,
}

impl PartialEq for CodeSearchNotice {
    fn eq(&self, other: &Self) -> bool {
        self.address == other.address
            && self.call_key == other.call_key
            && self.preset == other.preset
    }
}
impl Eq for CodeSearchNotice {}

impl aft_base::pending_notices::NoticeSink for CodeSearchNotice {
    fn send(&self, notice: &aft_base::pending_notices::Notice) {
        CodeSearchNotice::send(self, notice);
    }
}

impl crate::subc_format::CodeSearchNotice for CodeSearchNotice {
    fn same_route(&self, other: &dyn crate::subc_format::CodeSearchNotice) -> bool {
        let other: &dyn std::any::Any = other;
        other.downcast_ref::<Self>() == Some(self)
    }

    fn log_code_search_ineligible(&self, reason: &'static str) {
        let (session, harness) = self.log_target();
        crate::slog_info!(
            "runner code-search nudge call_key={} session={session} harness={harness} not eligible reason={reason}",
            self.call_key
        );
    }
}

pub(super) fn code_search_notice(
    route: &RouteIdentity,
    call_key: &str,
) -> Option<CodeSearchNotice> {
    let route = route
        .run_owner
        .as_ref()
        .map_or(route, |owner| &owner.owner_route);
    if !runner_route(route) {
        return None;
    }
    let raw = match &route.spawn_principal {
        AuthenticatedPrincipal::RouteBind { harness, .. } => harness.clone(),
        _ => String::new(),
    };
    Some(CodeSearchNotice {
        address: Address::new(&route.task_identity(), raw, route.scope.as_ref()),
        call_key: call_key.to_owned(),
        preset: "head".into(),
        remaining: Arc::new(std::sync::Mutex::new(CODE_SEARCH_NOTICE_TIMEOUT)),
    })
}

/// Builds a nudge's ordered send. Callers send it only to a runner that
/// confirmed `delivery_order`; a plain send would start a turn of its own.
pub(super) fn nudge_request(
    call_key: &str,
    notice: &crate::subc::runner_notices::notice_templates::Notice,
    preset: &str,
    now_ms: u64,
    cleared: bool,
    fallback: bool,
) -> SendRequest {
    let kind = notice
        .template
        .as_ref()
        .map_or("aft.diagnostics", |(id, _)| *id);
    let send_id = format!(
        "aft:nudge:{kind}:{call_key}{}",
        if fallback { ":fallback" } else { "" }
    );
    let mut order = DeliveryOrder::new(vec![Mode::ToolResult]);
    order.dedupe_key = Some(format!("aft:nudge:{kind}"));
    order.expires_at_ms = Some(now_ms.saturating_add(60_000));
    let templates = crate::subc::runner_notices::notice_templates::templates(preset);
    let mut text = notice.text.clone();
    if let Some((id, slots)) = &notice.template {
        if let Some(template) = templates.iter().find(|template| template.id == *id) {
            let mut slots = slots.as_object().unwrap().clone();
            if *id == crate::subc::runner_notices::notice_templates::CODE_SEARCH
                && preset == "worker"
                && slots["redirect"]
                    .as_str()
                    .is_some_and(|s| s.contains("`aft_search`"))
            {
                slots["redirect"] =
                    crate::subc::runner_notices::notice_templates::code_search_slots(
                        Some("search"),
                        "bash",
                    )["redirect"]
                        .clone();
            }
            let use_ = TemplateUse::new(*id, template_digest(template), slots);
            if let Ok(rendered) = render(&templates, &use_, |_| None) {
                text = rendered;
                if !fallback {
                    order.modes.insert(
                        0,
                        if cleared {
                            Mode::TurnSystemCleared
                        } else {
                            Mode::TurnSystem
                        },
                    );
                    order.template = Some(use_);
                    return SendRequest::template(send_id, &order).with_notice(true);
                }
            }
        }
    }
    SendRequest::new(text, send_id)
        .with_notice(true)
        .with_delivery_order(&order)
}

impl CodeSearchNotice {
    pub(super) fn with_preset(mut self, role: super::tool_provider::CallerRole) -> Self {
        self.preset = match role {
            super::tool_provider::CallerRole::Head => "head",
            super::tool_provider::CallerRole::Worker => "worker",
            super::tool_provider::CallerRole::Reader => "reader",
        }
        .into();
        self
    }

    fn log_target(&self) -> (&str, &str) {
        self.address.as_ref().map_or(("unknown", "unknown"), |a| {
            (a.identity.session.as_str(), a.identity.harness.as_str())
        })
    }

    /// Send before returning the tool result. If the runner refuses the
    /// template (for example, the session plan omitted it), retry once as
    /// free text at tool-result authority, within the same deadline; never append.
    pub(crate) fn send(&self, notice: &crate::subc::runner_notices::notice_templates::Notice) {
        // Cloned render contexts belong to the same call. All its nudges,
        // capability probes and the one allowed fallback share this budget.
        let mut remaining = self.remaining.lock().unwrap();
        let budget = *remaining;
        let started = std::time::Instant::now();
        let is_code_search = notice
            .template
            .as_ref()
            .is_some_and(|(id, _)| *id == notice_templates::CODE_SEARCH);
        let mut attempt = NudgeAttempt::default();
        // Rendering can run on either an actor thread or a Tokio worker. Keep
        // the synchronous send's runtime off both callers' runtime stacks.
        let outcome = std::thread::scope(|scope| {
            scope
                .spawn(|| self.send_once(notice, budget, is_code_search.then_some(&mut attempt)))
                .join()
        })
        .unwrap_or_else(|_| retry(FailureClass::Runtime, "code-search notice sender panicked"));
        *remaining = remaining.saturating_sub(started.elapsed());
        if is_code_search {
            self.log_code_search_outcome(&attempt, &outcome);
            return;
        }
        match outcome {
            Outcome::Accepted | Outcome::Pending => {}
            Outcome::NotDelivered(reason) => log::info!(
                "runner code-search nudge call_key={} not_delivered reason={reason}; dropped",
                self.call_key
            ),
            Outcome::Final(failure) | Outcome::Retry(failure) => crate::slog_info!(
                "runner code-search nudge call_key={} dropped class={:?} code={} detail={}",
                self.call_key,
                failure.class,
                failure.code.as_deref().unwrap_or("none"),
                failure.detail
            ),
        }
    }

    fn log_code_search_outcome(&self, attempt: &NudgeAttempt, outcome: &Outcome) {
        let (session, harness) = self.log_target();
        let send_id = attempt.send_id.as_deref().unwrap_or("none");
        let probe = match attempt.ordered {
            Some(Some(true)) => "confirmed",
            Some(Some(false)) => "not advertised",
            Some(None) => "unavailable",
            None => "not completed",
        };
        let result = match outcome {
            Outcome::Pending => {
                "sent, ordered, awaiting delivery reason=eligible code search".into()
            }
            Outcome::Accepted => match attempt.ack {
                Some(SendAck::Delivered) => "delivered reason=runner confirmed delivery".into(),
                Some(SendAck::Finished) => {
                    "delivery not confirmed reason=runner finished without a receipt".into()
                }
                Some(SendAck::Superseded) => "dropped reason=runner superseded the notice".into(),
                Some(SendAck::Expired) => "dropped reason=notice expired".into(),
                Some(SendAck::TemplatePartGone) => "dropped reason=template part is gone".into(),
                None => "sent reason=runner accepted without a receipt".into(),
            },
            Outcome::NotDelivered(reason) => format!("not_delivered reason={reason}; dropped"),
            Outcome::Final(failure) | Outcome::Retry(failure) => {
                let code = failure.code.as_deref().unwrap_or("none");
                let class = failure.class;
                let detail = &failure.detail;
                if class == FailureClass::PeerRefusal {
                    format!("refused; dropped code={code} detail={detail}")
                } else {
                    format!("dropped class={class:?} code={code} detail={detail}")
                }
            }
        };
        crate::slog_info!(
            "runner code-search nudge call_key={} send_id={send_id} session={session} harness={harness} probe={probe}; {result}",
            self.call_key
        );
    }

    fn send_once(
        &self,
        notice: &crate::subc::runner_notices::notice_templates::Notice,
        remaining: Duration,
        mut attempt: Option<&mut NudgeAttempt>,
    ) -> Outcome {
        if remaining.is_zero() {
            return Outcome::Final(Failure::new(
                FailureClass::Timeout,
                "tool notice send budget exhausted",
            ));
        }
        (|| {
            let address = self
                .address
                .as_ref()
                .ok_or_else(|| retry(FailureClass::Address, "call has no runner notice address"))?;
            let path = CONNECTION
                .get()
                .map(|path| path.read().unwrap().clone())
                .ok_or_else(|| {
                    retry(
                        FailureClass::Address,
                        "runner notice connection unavailable",
                    )
                })?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| retry(FailureClass::Runtime, error))?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            Ok::<_, Outcome>(runtime.block_on(async {
                tokio::time::timeout(
                    remaining,
                    async {
                        let ordered = runner_delivery_order(&path, address).await;
                        if let Some(attempt) = attempt.as_deref_mut() {
                            attempt.ordered = Some(ordered);
                        }
                        let modern = ordered == Some(true);
                        if !modern {
                            // A send without a delivery order queues behind the
                            // running turn and then starts a turn of its own at
                            // idle. A nudge that costs the agent a whole turn
                            // defeats its purpose, so unless the runner confirms
                            // ordered delivery the nudge is dropped. That covers
                            // an older runner and a capability probe that failed.
                            return Outcome::Final(Failure::new(
                                FailureClass::NoOrderedDelivery,
                                "runner did not confirm delivery_order",
                            ));
                        }
                        let cleared = modern && runner_capability(&path, address, "delivery_order_modes:turn_system_cleared").await == Some(true);
                        let request = nudge_request(&self.call_key, notice, &self.preset, now, cleared, false);
                        let outcome = send_nudge(&path, address, &request, attempt.as_deref_mut()).await;
                        let refused = matches!(&outcome, Outcome::Final(failure) | Outcome::Retry(failure) if failure.code.as_deref() == Some("invalid_params") && failure.field.as_deref() == Some("template"));
                        if request.runner_params.contains_key("template") && refused {
                            send_nudge(&path, address, &nudge_request(&self.call_key, notice, &self.preset, now, false, true), attempt).await
                        } else { outcome }
                    },
                )
                .await
                .unwrap_or_else(|_| {
                    Outcome::Final(Failure::new(
                        FailureClass::Timeout,
                        "code-search notice send exceeded 2s total deadline",
                    ))
                })
            }))
        })()
        .unwrap_or_else(|outcome| outcome)
    }
}

#[derive(Default)]
struct NudgeAttempt {
    ordered: Option<Option<bool>>,
    send_id: Option<String>,
    ack: Option<SendAck>,
}

async fn send_nudge(
    path: &Path,
    address: &Address,
    request: &SendRequest,
    attempt: Option<&mut NudgeAttempt>,
) -> Outcome {
    let Some(attempt) = attempt else {
        return send(path, address, request, true).await;
    };
    attempt.send_id = Some(request.send_id.clone());
    match call(path, address, envelope(request)).await {
        Ok(bytes) => {
            let (outcome, ack) =
                decode_send_reply_with_ack(&bytes, tracks_delivery_receipt(request, true));
            attempt.ack = ack;
            outcome
        }
        Err(outcome) => outcome,
    }
}

fn concrete_route_harness(principal: Option<&str>, session: &str) -> Option<String> {
    let mut routes = BOUND_ROUTES.get_or_init(Default::default).lock().unwrap();
    routes.retain(|_, route| route.strong_count() > 0);
    let mut harness = None;
    for route in routes.values().filter_map(std::sync::Weak::upgrade) {
        let AuthenticatedPrincipal::RouteBind {
            principal_id,
            session_id,
            harness: raw,
            ..
        } = &route.spawn_principal
        else {
            continue;
        };
        if principal_id.as_deref() != principal || session_id != session || raw == "runner" {
            continue;
        }
        if harness.as_ref().is_some_and(|previous| previous != raw) {
            // Conflicting live bindings cannot safely identify a host harness.
            return None;
        }
        harness = Some(raw.clone());
    }
    harness
}

/// Resolve the concrete host used when binding back to a runner session.
/// The `reserved:broca` principal maps the legacy `runner` label to `broca`;
/// other principals need an unambiguous live binding before the host is known.
pub(super) fn concrete_runner_harness(
    principal: Option<&str>,
    session: &str,
    raw_harness: &str,
) -> Option<String> {
    if raw_harness != "runner" {
        Some(raw_harness.to_owned())
    } else if principal == Some("reserved:broca") {
        Some("broca".into())
    } else {
        concrete_route_harness(principal, session)
    }
}

pub(super) fn route_unbound(route: &RouteIdentity) {
    if let Some(routes) = BOUND_ROUTES.get() {
        routes
            .lock()
            .unwrap()
            .remove(&(Arc::as_ptr(&route.0) as usize));
    }
}

type NoticeScope = (ScopeSelector, ScopeKind);

fn route_notice_scope(stamp: &ScopeStamp) -> NoticeScope {
    (
        ScopeSelector::new(
            stamp.owner.clone(),
            stamp.scope_ref.clone(),
            Some(stamp.scope_epoch),
        ),
        stamp.kind,
    )
}

fn task_notice_scope(stamp: Option<&ScopeStamp>) -> Result<Option<NoticeScope>, ()> {
    let Some(stamp) = stamp else {
        return Ok(None);
    };
    if stamp.attributes.run_id.is_none() {
        return Ok(Some(route_notice_scope(stamp)));
    }

    // The stored stamp still governs the command's lifetime. Runner notices
    // use the parent session scope the daemon linked to the run instead.
    if !stamp.owner_authorized || stamp.parent_state != Some(ParentState::Linked) {
        return Err(());
    }
    let parent = stamp.parent.as_ref().ok_or(())?;
    if parent.scope_ref.is_empty()
        || (parent.owner == stamp.owner
            && parent.scope_ref == stamp.scope_ref
            && parent.scope_epoch == stamp.scope_epoch)
    {
        return Err(());
    }
    let kind = match super::codemode::run_kind(stamp).map_err(|_| ())?.0 {
        super::codemode::RunKind::Agent { .. } => ScopeKind::Head,
        super::codemode::RunKind::Worker { .. } => ScopeKind::Worker,
    };
    Ok(Some((
        ScopeSelector::new(
            parent.owner.clone(),
            parent.scope_ref.clone(),
            Some(parent.scope_epoch),
        ),
        kind,
    )))
}

fn warn_missing_run_parent(task_id: &str) {
    static LOGGED: OnceLock<std::sync::Mutex<HashSet<String>>> = OnceLock::new();
    if LOGGED
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .insert(task_id.to_owned())
    {
        log::warn!("runner notice task={task_id} has no verified run parent scope; not sent");
    }
}

impl Address {
    fn new(
        owner: &crate::bash_background::TaskIdentity,
        raw_harness: String,
        stamp: Option<&ScopeStamp>,
    ) -> Option<Self> {
        Self::with_scope(owner, raw_harness, stamp.map(route_notice_scope))
    }

    fn with_scope(
        owner: &crate::bash_background::TaskIdentity,
        raw_harness: String,
        scope: Option<NoticeScope>,
    ) -> Option<Self> {
        if retired(owner) {
            return None;
        }
        let module = owner
            .principal
            .as_deref()?
            .strip_prefix("reserved:")?
            .to_string();
        let raw_harness =
            concrete_runner_harness(owner.principal.as_deref(), &owner.session, &raw_harness)?;
        let (scope, kind) = scope.unzip();
        Some(Self {
            // Broca hosts heads as well as workers, and a head's completion
            // must be allowed to wake it (idle_start). On Broca only a head
            // scope qualifies; every other Broca route, including an unscoped
            // one with no stamp to ask, keeps the worker delivery.
            worker: match kind {
                Some(kind) if module == "broca" => kind != ScopeKind::Head,
                Some(kind) => kind == ScopeKind::Worker,
                None => module == "broca",
            },
            module,
            identity: BindIdentity::new(raw_harness, owner.session.clone()),
            scope,
        })
    }
}

fn request(id: String, text: String, completion: bool, worker: bool) -> SendRequest {
    let mut modes = vec![Mode::ToolResult, Mode::Steer];
    if completion && !worker {
        modes.push(Mode::IdleStart);
    }
    SendRequest::new(text, id)
        .with_notice(true)
        .with_delivery_order(&DeliveryOrder::new(modes))
}

fn task_request(
    request: SendRequest,
    task_id: &str,
    started_at: u64,
    finished_at: Option<u64>,
    reminder_elapsed_ms: Option<u64>,
    policy: crate::bash_background::persistence::RunnerNoticePolicy,
) -> SendRequest {
    if !policy.dedupe {
        return request;
    }
    let mut order = request.delivery_order().unwrap().unwrap();
    order.dedupe_key = Some(format!("aft:bash-task:{task_id}"));
    order.expires_at_ms = Some(match reminder_elapsed_ms {
        Some(elapsed) => started_at
            .saturating_add(elapsed)
            .saturating_add(policy.reminder_interval_ms),
        None => finished_at.unwrap_or(started_at).saturating_add(MAX_AGE_MS),
    });
    request.with_delivery_order(&order)
}

fn tracks_delivery_receipt(request: &SendRequest, delivery_order: bool) -> bool {
    // The base delivery modes predate receipt semantics. A compatibility fallback
    // removes both optional members and must keep the legacy acceptance behavior,
    // even though the frozen runner policy still declares delivery_order.
    delivery_order
        && request
            .delivery_order()
            .ok()
            .flatten()
            .is_some_and(|order| order.dedupe_key.is_some() || order.expires_at_ms.is_some())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RunnerIncarnation {
    daemon: String,
    generation: u64,
}

#[derive(Default)]
struct RunnerCapabilities {
    entries: HashMap<(PathBuf, String, String), (RunnerIncarnation, bool)>,
}

impl RunnerCapabilities {
    fn get(
        &self,
        path: &Path,
        module: &str,
        incarnation: &RunnerIncarnation,
        capability: &str,
    ) -> Option<bool> {
        self.entries
            .get(&(path.into(), module.into(), capability.into()))
            .filter(|(cached, _)| cached == incarnation)
            .map(|(_, supported)| *supported)
    }

    fn insert(
        &mut self,
        path: &Path,
        module: &str,
        incarnation: RunnerIncarnation,
        supported: bool,
        capability: &str,
    ) {
        self.entries.insert(
            (path.into(), module.into(), capability.into()),
            (incarnation, supported),
        );
    }
}

static RUNNER_CAPABILITIES: OnceLock<std::sync::Mutex<RunnerCapabilities>> = OnceLock::new();

type ProbeFailure = (&'static str, Option<String>);

async fn runner_incarnation(
    consumer: &SubcConsumer,
    module: &str,
) -> Result<RunnerIncarnation, ProbeFailure> {
    let snapshot = tokio::time::timeout(Duration::from_secs(5), consumer.spawn_snapshot())
        .await
        .map_err(|_| ("incarnation", None))?
        .map_err(|error| ("incarnation", error.code().map(str::to_owned)))?;
    let spawn = snapshot
        .live
        .iter()
        .find(|spawn| spawn.module_id == module)
        .ok_or(("incarnation", None))?;
    Ok(RunnerIncarnation {
        daemon: snapshot.cursor.daemon_incarnation,
        generation: spawn.spawn_generation,
    })
}

enum CapabilityConsumer {
    Runner(SubcConsumer),
    #[cfg(test)]
    Endpoint(test_endpoint::Probe),
}

impl CapabilityConsumer {
    async fn connect(path: &Path, _address: &Address) -> Result<Self, ProbeFailure> {
        #[cfg(test)]
        if let Some(probe) = test_endpoint::probe(_address) {
            return Ok(Self::Endpoint(probe));
        }
        super::registration::connect_consumer(
            "runner_notice_probe",
            path,
            ConsumerOptions::default(),
        )
        .await
        .map(Self::Runner)
        .map_err(|_| ("connect", None))
    }

    async fn incarnation(&self, module: &str) -> Result<RunnerIncarnation, ProbeFailure> {
        match self {
            Self::Runner(consumer) => runner_incarnation(consumer, module).await,
            #[cfg(test)]
            Self::Endpoint(probe) => Ok(probe.incarnation()),
        }
    }

    async fn describe(
        &self,
        address: &Address,
        before: &RunnerIncarnation,
    ) -> Result<Vec<u8>, ProbeFailure> {
        let body = br#"{"method":"role.describe","params":{}}"#.to_vec();
        let consumer = match self {
            Self::Runner(consumer) => consumer,
            #[cfg(test)]
            Self::Endpoint(probe) => return probe.describe(address, &body),
        };
        let route = open_notice_route(consumer, address)
            .await
            .map_err(|outcome| {
                let code = match outcome {
                    Outcome::Retry(failure) | Outcome::Final(failure) => failure.code,
                    _ => None,
                };
                ("route_open", code)
            })?;
        let result = async {
            // A route is tied to one runner process. Check both sides of the
            // probe so a restart cannot label an old reply as a new capability.
            if self.incarnation(&address.module).await? != *before {
                return Err(("runner_restarted", None));
            }
            consumer
                .request(
                    &route,
                    body,
                    CallOptions {
                        timeout: Duration::from_secs(5),
                        ..CallOptions::default()
                    },
                )
                .await
                .map_err(|error| ("describe", error.code().map(str::to_owned)))
        }
        .await;
        let _ = consumer
            .close_handle(&route, CloseRouteOptions::default())
            .await;
        result
    }

    async fn close(self) {
        match self {
            Self::Runner(consumer) => {
                let _ = consumer.close().await;
            }
            #[cfg(test)]
            Self::Endpoint(_) => {}
        }
    }
}

fn described_delivery_order(bytes: &[u8]) -> Option<bool> {
    described_capability(bytes, "delivery_order")
}

fn described_capability(bytes: &[u8], name: &str) -> Option<bool> {
    let description = cortexkit_role_llm_runner::wire::read_reply::<Value>(bytes).ok()?;
    if let Some(mode) = name.strip_prefix("delivery_order_modes:") {
        return Some(
            description
                .get("delivery_order_modes")
                .and_then(Value::as_array)
                .is_some_and(|modes| modes.iter().any(|item| item == mode)),
        );
    }
    let capabilities = description.get("capabilities")?.as_array()?;
    if !capabilities.iter().all(Value::is_string) {
        return None;
    }
    Some(capabilities.iter().any(|capability| capability == name))
}

async fn runner_delivery_order(path: &Path, address: &Address) -> Option<bool> {
    runner_capability(path, address, "delivery_order").await
}

fn capability_address(address: &Address) -> Address {
    // The SDK requires a session identity even on an unscoped route. This
    // AFT-owned session is only for runner-wide describes, never sends, so a
    // head session's plan cannot restrict the probe to its readers.
    Address {
        identity: BindIdentity::new(&address.identity.harness, "aft:runner-capability-probe"),
        scope: None,
        ..address.clone()
    }
}

async fn runner_capability(path: &Path, address: &Address, capability: &str) -> Option<bool> {
    #[cfg(test)]
    if let Some(supported) = test_endpoint::capability(address, capability) {
        return supported;
    }
    // Every failed step is logged by name: an unknown answer turns notices
    // off, so the log has to say why the runner could not be asked.
    let failed = |(step, code): ProbeFailure| {
        log::info!(
            "runner capability probe module={} capability={capability} failed at {step} code={}",
            address.module,
            code.as_deref().unwrap_or("none")
        );
        None
    };
    let consumer = match CapabilityConsumer::connect(path, address).await {
        Ok(consumer) => consumer,
        Err(error) => return failed(error),
    };
    let result = async {
        let before = consumer.incarnation(&address.module).await?;
        let cache = RUNNER_CAPABILITIES.get_or_init(Default::default);
        if let Some(supported) =
            cache
                .lock()
                .unwrap()
                .get(path, &address.module, &before, capability)
        {
            return Ok(supported);
        }
        let bytes = consumer
            .describe(&capability_address(address), &before)
            .await?;
        let supported = if capability == "delivery_order" {
            described_delivery_order(&bytes)
        } else {
            described_capability(&bytes, capability)
        }
        .ok_or(("decode", None))?;
        if consumer.incarnation(&address.module).await? != before {
            return Err(("runner_restarted", None));
        }
        cache
            .lock()
            .unwrap()
            .insert(path, &address.module, before, supported, capability);
        Ok(supported)
    }
    .await;
    consumer.close().await;
    match result {
        Ok(supported) => Some(supported),
        Err(error) => failed(error),
    }
}

fn envelope(request: &SendRequest) -> Vec<u8> {
    serde_json::to_vec(&json!({"method":"session.send", "params":request}))
        .expect("notice request is JSON")
}

fn target(address: &Address) -> RouteTarget {
    RouteTarget::ManagementSurface {
        module_id: address.module.clone(),
    }
}

fn role_versions() -> std::collections::BTreeMap<String, String> {
    std::collections::BTreeMap::from([(
        cortexkit_role_llm_runner::ROLE.into(),
        cortexkit_role_llm_runner::VERSION.into(),
    )])
}

#[cfg(test)]
pub(super) mod test_endpoint {
    use super::*;
    struct Endpoint {
        sender: std::sync::mpsc::Sender<Received>,
        stored: std::sync::Mutex<HashMap<String, Vec<u8>>>,
        delivery_order: std::sync::Mutex<Option<bool>>,
        cleared: AtomicBool,
        send_status: AtomicBool,
        replies: std::sync::Mutex<std::collections::VecDeque<Vec<u8>>>,
        states: std::sync::Mutex<HashMap<String, Vec<u8>>>,
        lose_response: AtomicBool,
        effects: Arc<AtomicU64>,
        retries: std::sync::Mutex<u64>,
        refusals: std::sync::Mutex<std::collections::VecDeque<Value>>,
        active: AtomicU64,
        max_active: AtomicU64,
        reply_delay_ms: AtomicU64,
        gate:
            std::sync::Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
        probe_incarnation: std::sync::Mutex<Option<RunnerIncarnation>>,
        probe_routes: AtomicU64,
    }
    fn endpoints() -> &'static std::sync::Mutex<HashMap<String, Arc<Endpoint>>> {
        static ENDPOINTS: OnceLock<std::sync::Mutex<HashMap<String, Arc<Endpoint>>>> =
            OnceLock::new();
        ENDPOINTS.get_or_init(Default::default)
    }
    pub(super) fn contains(id: &str) -> bool {
        endpoints().lock().unwrap().contains_key(id)
    }
    pub(in crate::subc) struct Received {
        pub harness: String,
        pub session: String,
        pub scope: Option<ScopeSelector>,
        pub request: Value,
        pub bytes: Vec<u8>,
    }
    pub(in crate::subc) struct Capture {
        key: String,
        pub receiver: std::sync::mpsc::Receiver<Received>,
        pub effects: Arc<AtomicU64>,
        endpoint: Arc<Endpoint>,
    }
    impl Capture {
        pub(super) fn probe_incarnation(&self, daemon: &str, generation: u64) {
            *self.endpoint.probe_incarnation.lock().unwrap() = Some(RunnerIncarnation {
                daemon: daemon.into(),
                generation,
            });
        }
        pub(super) fn probe_routes(&self) -> u64 {
            self.endpoint.probe_routes.load(Ordering::Relaxed)
        }
        pub(super) fn delivery_order(&self, supported: Option<bool>) {
            *self.endpoint.delivery_order.lock().unwrap() = supported;
        }
        pub(super) fn cleared(&self, supported: bool) {
            self.endpoint.cleared.store(supported, Ordering::Relaxed);
        }
        pub(super) fn retry(&self, count: u64) {
            *self.endpoint.retries.lock().unwrap() = count;
        }
        pub(super) fn send_status(&self, supported: bool) {
            self.endpoint
                .send_status
                .store(supported, Ordering::Relaxed);
        }
        pub(super) fn reply(&self, reply: SendReply) {
            self.endpoint
                .replies
                .lock()
                .unwrap()
                .push_back(cortexkit_role_llm_runner::wire::reply_body(&reply).unwrap());
        }
        pub(super) fn reply_fixture(&self, body: &[u8]) {
            let mut reply = b"{\"result\":".to_vec();
            reply.extend_from_slice(body);
            reply.push(b'}');
            self.endpoint.replies.lock().unwrap().push_back(reply);
        }
        pub(super) fn refuse(&self, code: &str, field: Option<&str>) {
            let mut error = json!({"code": code, "message": "notice refused"});
            if let Some(field) = field {
                error["detail"] = json!({"field": field});
            }
            self.endpoint
                .refusals
                .lock()
                .unwrap()
                .push_back(json!({"error": error}));
        }
        pub(super) fn block_next(
            &self,
        ) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
            let (entered, wait_entered) = std::sync::mpsc::channel();
            let (release, wait_release) = std::sync::mpsc::channel();
            *self.endpoint.gate.lock().unwrap() = Some((entered, wait_release));
            (wait_entered, release)
        }
        pub(super) fn max_active(&self) -> u64 {
            self.endpoint.max_active.load(Ordering::Relaxed)
        }
        pub(super) fn delay_reply(&self, delay: Duration) {
            self.endpoint
                .reply_delay_ms
                .store(delay.as_millis() as u64, Ordering::Relaxed);
        }
    }
    impl Drop for Capture {
        fn drop(&mut self) {
            endpoints().lock().unwrap().remove(&self.key);
        }
    }
    pub(in crate::subc) fn capture(task: &str, lose_response: bool) -> Capture {
        let key = format!("aft:bash:{task}");
        let (sender, receiver) = std::sync::mpsc::channel();
        let effects = Arc::new(AtomicU64::new(0));
        let endpoint = Arc::new(Endpoint {
            sender,
            stored: Default::default(),
            delivery_order: Default::default(),
            cleared: AtomicBool::new(false),
            send_status: AtomicBool::new(false),
            replies: Default::default(),
            states: Default::default(),
            lose_response: AtomicBool::new(lose_response),
            effects: effects.clone(),
            retries: Default::default(),
            refusals: Default::default(),
            active: AtomicU64::new(0),
            max_active: AtomicU64::new(0),
            reply_delay_ms: AtomicU64::new(0),
            gate: Default::default(),
            probe_incarnation: Default::default(),
            probe_routes: AtomicU64::new(0),
        });
        assert!(endpoints()
            .lock()
            .unwrap()
            .insert(key.clone(), endpoint.clone())
            .is_none());
        Capture {
            key,
            receiver,
            effects,
            endpoint,
        }
    }
    pub(super) fn capability(address: &Address, capability: &str) -> Option<Option<bool>> {
        let endpoint = endpoints()
            .lock()
            .unwrap()
            .get(&format!("aft:bash:{}", address.identity.session))
            .cloned()?;
        if endpoint.probe_incarnation.lock().unwrap().is_some() {
            return None;
        }
        let supported = if capability == "delivery_order_modes:turn_system_cleared" {
            Some(endpoint.cleared.load(Ordering::Relaxed))
        } else if capability == "send_status" {
            Some(endpoint.send_status.load(Ordering::Relaxed))
        } else {
            *endpoint.delivery_order.lock().unwrap()
        };
        Some(supported)
    }

    pub(super) struct Probe {
        endpoint: Arc<Endpoint>,
        readers_session: String,
    }
    pub(super) fn probe(address: &Address) -> Option<Probe> {
        let endpoint = endpoints()
            .lock()
            .unwrap()
            .get(&format!("aft:bash:{}", address.identity.session))
            .cloned()?;
        endpoint.probe_incarnation.lock().unwrap().as_ref()?;
        Some(Probe {
            endpoint,
            readers_session: address.identity.session.clone(),
        })
    }
    impl Probe {
        pub(super) fn incarnation(&self) -> RunnerIncarnation {
            self.endpoint
                .probe_incarnation
                .lock()
                .unwrap()
                .clone()
                .unwrap()
        }
        pub(super) fn describe(
            &self,
            address: &Address,
            bytes: &[u8],
        ) -> Result<Vec<u8>, ProbeFailure> {
            self.endpoint.probe_routes.fetch_add(1, Ordering::Relaxed);
            let request: Value = serde_json::from_slice(bytes).unwrap();
            assert_eq!(request, json!({"method":"role.describe", "params":{}}));
            let _ = self.endpoint.sender.send(Received {
                harness: address.identity.harness.clone(),
                session: address.identity.session.clone(),
                scope: address.scope.clone(),
                bytes: bytes.to_vec(),
                request,
            });
            if address.identity.session == self.readers_session {
                return Err(("describe", Some("read_access_refused".into())));
            }
            let Some(delivery_order) = *self.endpoint.delivery_order.lock().unwrap() else {
                return Err(("describe", Some("unavailable".into())));
            };
            let mut capabilities = Vec::new();
            if delivery_order {
                capabilities.push("delivery_order");
            }
            if self.endpoint.send_status.load(Ordering::Relaxed) {
                capabilities.push("send_status");
            }
            let modes = if self.endpoint.cleared.load(Ordering::Relaxed) {
                vec!["turn_system_cleared"]
            } else {
                Vec::new()
            };
            Ok(cortexkit_role_llm_runner::wire::reply_body(&json!({
                "capabilities": capabilities, "delivery_order_modes": modes,
            }))
            .unwrap())
        }
    }
    pub(super) async fn call(address: &Address, bytes: &[u8]) -> Option<Result<Vec<u8>, Outcome>> {
        let body: Value = serde_json::from_slice(bytes).unwrap();
        let key = body["params"]["send_id"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| format!("aft:bash:{}", address.identity.session));
        let endpoint = {
            let endpoints = endpoints().lock().unwrap();
            endpoints
                .get(&key)
                .or_else(|| endpoints.get(&format!("aft:bash:{}", address.identity.session)))
                .cloned()?
        };
        let active = endpoint.active.fetch_add(1, Ordering::Relaxed) + 1;
        endpoint.max_active.fetch_max(active, Ordering::Relaxed);
        struct Active<'a>(&'a AtomicU64);
        impl Drop for Active<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::Relaxed);
            }
        }
        let _active = Active(&endpoint.active);
        assert_eq!(body.as_object().unwrap().len(), 2);
        let _ = endpoint.sender.send(Received {
            harness: address.identity.harness.clone(),
            session: address.identity.session.clone(),
            scope: address.scope.clone(),
            request: body.clone(),
            bytes: bytes.to_vec(),
        });
        let delay_ms = endpoint.reply_delay_ms.load(Ordering::Relaxed);
        if delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
        if body["method"] != "session.send"
            && body["method"] != cortexkit_role_llm_runner::ops::SESSION_SEND_STATUS
        {
            assert_eq!(body["method"], "session.head");
            return Some(Err(refusal("invalid_params")));
        }
        let decoded = if body["method"] == cortexkit_role_llm_runner::ops::SESSION_SEND_STATUS {
            let request: SendStatusRequest =
                serde_json::from_value(body["params"].clone()).unwrap();
            assert_eq!(request.send_id, key);
            assert!(endpoint.send_status.load(Ordering::Relaxed));
            None
        } else {
            let decoded: SendRequest = serde_json::from_value(body["params"].clone()).unwrap();
            decoded.check_prompt().unwrap();
            decoded.delivery_order().unwrap();
            assert_eq!(body["params"]["notice"], true);
            Some(decoded)
        };
        let gate = endpoint.gate.lock().unwrap().take();
        if let Some((entered, release)) = gate {
            entered.send(()).unwrap();
            release.recv_timeout(Duration::from_secs(3)).unwrap();
        }
        if let Some(refusal) = endpoint.refusals.lock().unwrap().pop_front() {
            return Some(Ok(serde_json::to_vec(&refusal).unwrap()));
        }
        let should_retry = {
            let mut remaining = endpoint.retries.lock().unwrap();
            let retry = *remaining > 0;
            *remaining = remaining.saturating_sub(1);
            retry
        };
        if should_retry {
            return Some(Err(retry(
                FailureClass::TestEndpoint,
                "endpoint unavailable",
            )));
        }
        let mut stored = endpoint.stored.lock().unwrap();
        let new = !stored.contains_key(&key);
        if let Some(decoded) = decoded {
            let bytes = serde_json::to_vec(&decoded).unwrap();
            if let Some(first) = stored.get(&key) {
                if first != &bytes {
                    return Some(Err(refusal("send_id_reuse")));
                }
            } else {
                stored.insert(key.clone(), bytes);
            }
        } else if new {
            return Some(Err(refusal("unknown_send")));
        }
        let mut states = endpoint.states.lock().unwrap();
        let response = endpoint
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .or_else(|| states.get(&key).cloned())
            .unwrap_or_else(|| {
                cortexkit_role_llm_runner::wire::reply_body(
                    &SendReply::new("active")
                        .with_delivered(cortexkit_role_llm_runner::send::Delivered::new("step")),
                )
                .unwrap()
            });
        states.insert(key, response.clone());
        if new
            && !matches!(
                decode_send_reply(&response, false),
                Outcome::NotDelivered(_)
            )
        {
            endpoint.effects.fetch_add(1, Ordering::Relaxed);
        }
        if endpoint.lose_response.swap(false, Ordering::Relaxed) {
            return Some(Err(retry(FailureClass::TestEndpoint, "response lost")));
        }
        cortexkit_role_llm_runner::wire::read_reply::<SendReply>(&response).unwrap();
        Some(Ok(response))
    }
}

#[derive(Debug)]
enum Outcome {
    Accepted,
    Pending,
    NotDelivered(String),
    Final(Failure),
    Retry(Failure),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureClass {
    ConsumerIdentity,
    Connect,
    RouteOpen,
    Request,
    ReplyDecode,
    PeerRefusal,
    Timeout,
    Runtime,
    Address,
    Persistence,
    Expired,
    /// The runner did not confirm ordered delivery, so a notice that must ride
    /// a tool result is not sent at all.
    NoOrderedDelivery,
    #[cfg(test)]
    TestEndpoint,
}

#[derive(Debug)]
struct Failure {
    class: FailureClass,
    code: Option<String>,
    field: Option<String>,
    detail: String,
}

impl Failure {
    fn new(class: FailureClass, detail: impl ToString) -> Self {
        Self {
            class,
            code: None,
            field: None,
            detail: detail.to_string(),
        }
    }

    fn coded(class: FailureClass, code: &str, detail: impl ToString) -> Self {
        Self {
            class,
            code: Some(code.into()),
            field: None,
            detail: detail.to_string(),
        }
    }
}

fn retry(class: FailureClass, detail: impl ToString) -> Outcome {
    Outcome::Retry(Failure::new(class, detail))
}

fn refusal(code: &str) -> Outcome {
    // `session_held_elsewhere`: the session moved to another computer, whose
    // own AFT serves it now, so a resend from here can never land.
    // `unknown_session`: the session was deleted. A notice must never recreate
    // it, so a resend can't land either.
    match code {
        "scope_owner_mismatch"
        | "expired"
        | "session_released"
        | "unknown_session"
        | "session_unconverted"
        | "session_held_elsewhere"
        | "send_id_reuse"
        | "delivery_unsupported"
        | "invalid_params" => Outcome::Final(Failure::coded(FailureClass::PeerRefusal, code, code)),
        _ => Outcome::Retry(Failure::coded(FailureClass::PeerRefusal, code, code)),
    }
}

fn deliver(
    send_id: &str,
    attempt: usize,
    completion: bool,
    outcome: Outcome,
    retry_logged: &mut bool,
) -> Next {
    if completion && !*retry_logged {
        if let Outcome::Final(failure) | Outcome::Retry(failure) = &outcome {
            log::info!(
                "runner notice {send_id} first failure: class={:?} code={} detail={}",
                failure.class,
                failure.code.as_deref().unwrap_or("none"),
                failure.detail
            );
            *retry_logged = true;
        }
    }
    match outcome {
        Outcome::Accepted | Outcome::Pending => Next::Done,
        Outcome::NotDelivered(_) => Next::Done,
        Outcome::Final(failure) => {
            let code = failure.code.as_deref().unwrap_or("unknown");
            if completion && code == "expired" {
                EXPIRED.fetch_add(1, Ordering::Relaxed);
            }
            if code == "send_id_reuse" {
                log::error!("runner notice {send_id} refused: {code}");
            } else {
                log::warn!("runner notice {send_id} refused: {code}");
            }
            Next::Done
        }
        Outcome::Retry(_) => {
            if !completion && attempt.saturating_add(1) >= MAX_ATTEMPTS {
                log::warn!("runner notice {send_id} retired: retry budget exhausted");
                return Next::Done;
            }
            Next::Retry(retry_delay(attempt))
        }
    }
}

#[cfg(test)]
#[path = "runner_notices_tests.rs"]
mod tests;

#[cfg(feature = "test-runner-notice-fixtures")]
#[path = "runner_notice_recovery_fixture.rs"]
mod recovery_fixture;
#[cfg(feature = "test-runner-notice-fixtures")]
pub use recovery_fixture::RunnerNoticeRecoveryForTest;

async fn send(
    path: &Path,
    address: &Address,
    request: &SendRequest,
    delivery_order: bool,
) -> Outcome {
    let outcome = match call(path, address, envelope(request)).await {
        Ok(bytes) => decode_send_reply(&bytes, tracks_delivery_receipt(request, delivery_order)),
        Err(outcome) => outcome,
    };
    if matches!(outcome, Outcome::Accepted) {
        log::info!(
            "runner notice {} delivered to {} harness={} session={}",
            request.send_id,
            address.module,
            address.identity.harness,
            address.identity.session
        );
    }
    outcome
}

async fn check_send(
    path: &Path,
    address: &Address,
    request: &SendRequest,
    delivery_order: bool,
) -> Outcome {
    // The send_status group promises a read-only lookup. Older runners instead
    // answer an identical session.send from the durable idempotency record.
    let bytes = if runner_capability(path, address, "send_status").await == Some(true) {
        serde_json::to_vec(&json!({"method": cortexkit_role_llm_runner::ops::SESSION_SEND_STATUS, "params": SendStatusRequest::new(&request.send_id)})).unwrap()
    } else {
        envelope(request)
    };
    match call(path, address, bytes).await {
        Ok(bytes) => decode_send_reply(&bytes, tracks_delivery_receipt(request, delivery_order)),
        Err(outcome) => outcome,
    }
}

fn pending_delay(checks: u32) -> Duration {
    Duration::from_secs(match checks {
        0 => 5,
        1 => 30,
        2 => 120,
        _ => 600,
    })
}

type DeliveryUpdate = (SendRequest, RunnerCompletionDelivery);

fn flush_delivery_update(
    registry: &crate::bash_background::BgTaskRegistry,
    session: &str,
    task: &str,
    update: &mut Option<DeliveryUpdate>,
) -> std::io::Result<()> {
    if let Some((request, delivery)) = update {
        registry.persist_runner_completion_delivery(
            session,
            task,
            request.clone(),
            Some(delivery.clone()),
        )?;
    }
    *update = None;
    Ok(())
}

fn decode_send_reply(bytes: &[u8], receipt_semantics: bool) -> Outcome {
    decode_send_reply_with_ack(bytes, receipt_semantics).0
}

enum SendAck {
    Delivered,
    Finished,
    Superseded,
    Expired,
    TemplatePartGone,
}

fn decode_send_reply_with_ack(bytes: &[u8], receipt_semantics: bool) -> (Outcome, Option<SendAck>) {
    match cortexkit_role_llm_runner::wire::read_reply::<SendReply>(bytes) {
        Ok(reply) => {
            // llm-runner 0.8.53 CONTRACT §9: not_delivered is final for this
            // send_id; only a new send_id may try the same words again.
            if reply.state == states::NOT_DELIVERED {
                return (
                    Outcome::NotDelivered(reply.reason.unwrap_or_else(|| "unknown".into())),
                    None,
                );
            }
            // CONTRACT §9 and §9.7 list finished, superseded, expired
            // and template_part_gone as final. A run's active/paused state (or
            // a future state) does not prove ordered notice delivery: without a
            // receipt, both first and later replies need follow-up. Legacy sends
            // never get receipts, so only explicit pending keeps them followed.
            let ack = if reply.delivered.is_some() {
                Some(SendAck::Delivered)
            } else {
                match reply.state.as_str() {
                    states::FINISHED => Some(SendAck::Finished),
                    states::SUPERSEDED => Some(SendAck::Superseded),
                    states::EXPIRED => Some(SendAck::Expired),
                    states::TEMPLATE_PART_GONE => Some(SendAck::TemplatePartGone),
                    _ => None,
                }
            };
            if ack.is_none() && (receipt_semantics || reply.state == "pending") {
                (Outcome::Pending, None)
            } else {
                (Outcome::Accepted, ack)
            }
        }
        Err(error) => {
            if let Ok(body) = serde_json::from_slice::<Value>(bytes) {
                let refusal_body = body.get("error").unwrap_or(&body);
                if let Some(code) = refusal_body.get("code").and_then(Value::as_str) {
                    let mut outcome = refusal(code);
                    if let Outcome::Final(failure) | Outcome::Retry(failure) = &mut outcome {
                        failure.field = refusal_body
                            .get("detail")
                            .and_then(|detail| detail.get("field"))
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                        failure.detail = refusal_body.to_string();
                    }
                    return (outcome, None);
                }
            }
            (retry(FailureClass::ReplyDecode, error), None)
        }
    }
}

fn idle_start_fallback(request: &SendRequest, outcome: &Outcome) -> Option<SendRequest> {
    let Outcome::Final(failure) = outcome else {
        return None;
    };
    if failure.class != FailureClass::PeerRefusal
        || failure.code.as_deref() != Some("invalid_params")
        || failure.field.as_deref() != Some("delivery")
    {
        return None;
    }
    let mut order = request.delivery_order().ok()??;
    if !order.modes.contains(&Mode::IdleStart) {
        return None;
    }
    order.modes.retain(|mode| *mode != Mode::IdleStart);
    Some(request.clone().with_delivery_order(&order))
}

fn refused_notice_members(outcome: &Outcome) -> bool {
    matches!(outcome, Outcome::Final(failure)
        if failure.class == FailureClass::PeerRefusal
            && failure.code.as_deref() == Some("invalid_params")
            && matches!(failure.field.as_deref(), Some("dedupe_key" | "expires_at_ms")))
}

fn notice_members_fallback(request: &SendRequest, outcome: &Outcome) -> Option<SendRequest> {
    if !refused_notice_members(outcome) {
        return None;
    }
    let mut order = request.delivery_order().ok()??;
    if order.dedupe_key.is_none() && order.expires_at_ms.is_none() {
        return None;
    }
    order.dedupe_key = None;
    order.expires_at_ms = None;
    Some(request.clone().with_delivery_order(&order))
}

fn request_failure(error: CallError) -> Outcome {
    match error {
        CallError::Module(error) => {
            let mut outcome = refusal(&error.code);
            if let Outcome::Final(failure) | Outcome::Retry(failure) = &mut outcome {
                failure.field = error
                    .detail
                    .as_ref()
                    .and_then(|detail| detail.get("field"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                failure.detail = error.message;
            }
            outcome
        }
        error => retry(FailureClass::Request, error),
    }
}

#[cfg(feature = "test-runner-notice-fixtures")]
pub(super) async fn transport_for_test(path: &Path, session: &str, scope: ScopeSelector) -> Value {
    let address = Address::new(
        &crate::bash_background::TaskIdentity {
            principal: Some("reserved:broca".into()),
            harness: "runner".into(),
            session: session.into(),
        },
        "runner".into(),
        None,
    )
    .unwrap();
    let address = Address {
        scope: Some(scope),
        ..address
    };
    let request = request(
        format!("aft:bash:{session}"),
        "completion".into(),
        true,
        true,
    );
    let policy = crate::bash_background::persistence::RunnerNoticePolicy {
        dedupe: runner_delivery_order(path, &address).await == Some(true),
        reminder_interval_ms: 600_000,
    };
    let now = crate::bash_background::persistence::unix_millis();
    let request = task_request(request, session, now, Some(now), None, policy);
    let outcome = send(path, &address, &request, policy.dedupe).await;
    let mut result = match &outcome {
        Outcome::Accepted => json!({"outcome":"accepted"}),
        Outcome::Pending => json!({"outcome":"pending"}),
        Outcome::NotDelivered(reason) => json!({"outcome":"not_delivered","reason":reason}),
        Outcome::Final(failure) | Outcome::Retry(failure) => json!({
            "outcome": if matches!(outcome, Outcome::Final(_)) { "final" } else { "retry" },
            "class": format!("{:?}", failure.class), "code": failure.code, "detail": failure.detail,
        }),
    };
    if matches!(outcome, Outcome::Accepted) {
        let mut refusals = Vec::new();
        for (field, value) in [
            ("dedupe_key", json!(42)),
            ("dedupe_key", json!("")),
            ("expires_at_ms", json!(-1)),
            ("expires_at_ms", json!("tomorrow")),
            ("delivery", json!("steer")),
        ] {
            let mut body: Value = serde_json::from_slice(&envelope(&request)).unwrap();
            body["params"]["dedupe_key"] = json!("aft:bash-task:validation");
            body["params"][field] = value;
            let reply = call(path, &address, serde_json::to_vec(&body).unwrap()).await;
            let refusal = match reply {
                Ok(bytes) => serde_json::from_slice::<Value>(&bytes).unwrap(),
                Err(Outcome::Final(failure)) => {
                    json!({"error":{"code":failure.code,"detail":{"field":failure.field}}})
                }
                Err(_) => json!({"transport_error":true}),
            };
            refusals.push(refusal);
        }
        result["invalid_members"] = json!(refusals);
    }
    deliver(&request.send_id, 0, true, outcome, &mut false);
    result
}

fn route_failure(address: &Address, error: CallError) -> Outcome {
    let body = error.route_open_refusal().or_else(|| match &error {
        CallError::Module(body) => Some(body),
        _ => None,
    });
    if let Some(body) = body {
        let failure = Failure::coded(FailureClass::RouteOpen, &body.code, &error);
        if subc_protocol::error_codes::is_retryable_route_open(&body.code) {
            return Outcome::Retry(failure);
        }
        log_route_refusal(address, &failure);
        return Outcome::Final(failure);
    }
    retry(FailureClass::RouteOpen, error)
}

fn log_route_refusal(address: &Address, failure: &Failure) {
    static LOGGED: OnceLock<std::sync::Mutex<HashSet<(String, String)>>> = OnceLock::new();
    let key = (address.module.clone(), address.identity.session.clone());
    if LOGGED
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .insert(key)
    {
        log::error!("runner notice route refused: session={} module={} scope={} class={:?} code={} detail={}",
            address.identity.session, address.module,
            address.scope.as_ref().map(|scope| scope.scope_ref.as_str()).unwrap_or("unscoped"),
            failure.class, failure.code.as_deref().unwrap_or("none"), failure.detail);
    }
}

async fn call(path: &Path, address: &Address, body: Vec<u8>) -> Result<Vec<u8>, Outcome> {
    #[cfg(test)]
    if let Some(result) = test_endpoint::call(address, &body).await {
        return result;
    }
    if !super::registration::module_consumer_allowed() {
        return Err(retry(
            FailureClass::ConsumerIdentity,
            "AFT launch has no consumer identity",
        ));
    }
    let consumer = match super::registration::connect_consumer(
        "runner_notice_send",
        path,
        ConsumerOptions::default(),
    )
    .await
    {
        Ok(consumer) => consumer,
        Err(error) => return Err(retry(FailureClass::Connect, error)),
    };
    let route = open_notice_route(&consumer, address).await;
    let result = match route {
        Ok(route) => {
            let result = consumer
                .request(
                    &route,
                    body,
                    CallOptions {
                        timeout: Duration::from_secs(5),
                        ..CallOptions::default()
                    },
                )
                .await;
            let _ = consumer
                .close_handle(&route, CloseRouteOptions::default())
                .await;
            match result {
                Ok(bytes) => Ok(bytes),
                Err(error) => Err(request_failure(error)),
            }
        }
        Err(error) => Err(error),
    };
    let _ = consumer.close().await;
    result
}

async fn open_notice_route(
    consumer: &SubcConsumer,
    address: &Address,
) -> Result<subc_client_rs::RouteHandle, Outcome> {
    if !super::registration::module_consumer_allowed() {
        return Err(retry(
            FailureClass::ConsumerIdentity,
            "AFT launch has no consumer identity",
        ));
    }
    let target = target(address);
    let options = CallOptions {
        timeout: Duration::from_secs(5),
        route_retry_deadline: Duration::from_secs(5),
        role_versions: Some(role_versions()),
        ..CallOptions::default()
    };
    let route = match &address.scope {
        Some(scope) => {
            consumer
                .open_route_scoped(target, address.identity.clone(), scope.clone(), options)
                .await
        }
        None => {
            consumer
                .open_route(target, address.identity.clone(), options)
                .await
        }
    };
    route.map_err(|error| route_failure(address, error))
}

pub(super) fn enqueue(registry: crate::bash_background::BgTaskRegistry, frame: PushFrame) -> bool {
    enqueue_inner(registry, frame, false)
}

/// Offer only the undelivered completions explicitly recovered at startup;
/// their launching identity supplies authority even before a session rebinds.
pub(crate) fn offer_replayed(
    registry: crate::bash_background::BgTaskRegistry,
    replayed: &[(String, Vec<String>)],
) {
    for (session, tasks) in replayed {
        for task in tasks {
            let Some(metadata) = registry.notice_metadata(task) else {
                continue;
            };
            let Some(owner) = metadata.launching_identity.filter(is_runner) else {
                continue;
            };
            if owner.session != *session || metadata.completion_delivered {
                continue;
            }
            let _identity = crate::bash_background::task_identity_scope(Some(owner));
            if let Some(completion) = registry
                .pending_completions_for_session(session)
                .into_iter()
                .find(|completion| completion.task_id == *task)
            {
                enqueue(registry.clone(), completion.into_push_frame());
            }
        }
    }
}

pub(super) fn route_bound(registry: crate::bash_background::BgTaskRegistry, route: &RouteIdentity) {
    if !runner_route(route) || !route.trust.allows_bash_observation() {
        return;
    }
    if matches!(&route.spawn_principal, AuthenticatedPrincipal::RouteBind { harness, .. } if harness != "runner")
    {
        let mut routes = BOUND_ROUTES.get_or_init(Default::default).lock().unwrap();
        routes.retain(|_, route| route.strong_count() > 0);
        routes.insert(Arc::as_ptr(&route.0) as usize, Arc::downgrade(&route.0));
    }
    let owner = route.task_identity();
    let _identity = crate::bash_background::task_identity_scope(Some(owner.clone()));
    for completion in registry.pending_completions_for_session(&owner.session) {
        if completion.launching_identity.as_ref() == Some(&owner) {
            enqueue_inner(registry.clone(), completion.into_push_frame(), true);
        }
    }
}

fn enqueue_inner(
    registry: crate::bash_background::BgTaskRegistry,
    frame: PushFrame,
    expedite: bool,
) -> bool {
    let (_task_id, owner) = match &frame {
        PushFrame::BashCompleted(frame) => (&frame.task_id, &frame.launching_identity),
        PushFrame::BashLongRunning(frame) => (&frame.task_id, &frame.launching_identity),
        PushFrame::BashPatternMatch(frame) => (&frame.task_id, &frame.launching_identity),
        _ => return false,
    };
    if !owner.as_ref().is_some_and(is_runner) {
        return false;
    }
    let path = CONNECTION.get().map(|path| path.read().unwrap().clone());
    #[cfg(test)]
    let path = path
        .or_else(|| test_endpoint::contains(&format!("aft:bash:{_task_id}")).then(PathBuf::new));
    let Some(path) = path else {
        return false;
    };
    let Some(scheduler) = scheduler() else {
        return false;
    };
    offer(
        scheduler,
        registry,
        frame,
        path,
        expedite,
        crate::bash_background::persistence::unix_millis,
    )
}

fn offer(
    scheduler: &Scheduler,
    registry: crate::bash_background::BgTaskRegistry,
    frame: PushFrame,
    path: PathBuf,
    expedite: bool,
    now: impl Fn() -> u64 + Send + 'static,
) -> bool {
    let (task_id, owner) = match &frame {
        PushFrame::BashCompleted(frame) => (&frame.task_id, &frame.launching_identity),
        PushFrame::BashLongRunning(frame) => (&frame.task_id, &frame.launching_identity),
        PushFrame::BashPatternMatch(frame) => (&frame.task_id, &frame.launching_identity),
        _ => return false,
    };
    let Some(owner) = owner.clone().filter(is_runner) else {
        return false;
    };
    let task_id = task_id.clone();
    let (id, text, completion) = bash_requests::parts(&frame);
    let reminder_elapsed_ms = match &frame {
        PushFrame::BashLongRunning(frame) => Some(frame.elapsed_ms),
        _ => None,
    };
    let mut attempt = None;
    let mut retry_logged = false;
    let mut settled = false;
    let mut delivery_update: Option<DeliveryUpdate> = None;
    let follow = Arc::new(Follow::default());
    follow.attached.store(
        crate::late_result_bus::readers::sendstate_attached(),
        Ordering::Relaxed,
    );
    let work_follow = Arc::clone(&follow);
    let queued = scheduler.offer_follow(
        id.clone(),
        completion,
        Box::new(move || {
            let forced = work_follow.force.swap(false, Ordering::Relaxed);
            let receipt = FollowReceipt(std::mem::take(&mut *work_follow.receipts.lock().unwrap()));
            let _identity = crate::bash_background::task_identity_scope(Some(owner.clone()));
            // Keep a failed durable transition in memory: never send the old
            // generation again just because writing its replacement failed.
            if let Err(error) = flush_delivery_update(&registry, &owner.session, &task_id, &mut delivery_update) {
                log::warn!("runner notice task={task_id} could not persist delivery progress: {error}");
                return Next::Retry(retry_delay(0));
            }
            let Some(metadata) = registry.notice_metadata(&task_id) else {
                return Next::Done;
            };
            if reminder_elapsed_ms.is_some() && (metadata.is_terminal() || metadata.completion_delivered) {
                return Next::Done;
            }
            if completion && metadata.completion_delivered {
                receipt.applied();
                return Next::Done;
            }
            let timestamp = now();
            let age = timestamp.saturating_sub(metadata.finished_at.unwrap_or(metadata.started_at));
            let progress = metadata.runner_completion_delivery.clone();
            let generation = progress.as_ref().map_or(1, |progress| progress.generation);
            let checking = completion && matches!(progress.as_ref().map(|progress| &progress.phase), Some(RunnerCompletionPhase::Pending { .. }));
            *work_follow.send_id.lock().unwrap() = if checking { metadata.runner_completion_notice.as_ref().map(|request| request.send_id.clone()) } else { None };
            if completion {
                match progress.as_ref().map(|progress| &progress.phase) {
                    Some(RunnerCompletionPhase::Retired) => settled = true,
                    Some(RunnerCompletionPhase::Ready { due_ms } | RunnerCompletionPhase::Pending { due_ms, .. }) if !(forced && checking) && *due_ms > timestamp && age < MAX_AGE_MS => {
                        return Next::Retry(Duration::from_millis((*due_ms - timestamp).min(MAX_AGE_MS - age)));
                    }
                    _ => {}
                }
                if checking && age >= MAX_AGE_MS {
                    PENDING_UNRESOLVED.fetch_add(1, Ordering::Relaxed);
                    crate::slog_info!("runner notice task={task_id} session={} send still pending after 24 hours", owner.session);
                    delivery_update = Some((metadata.runner_completion_notice.clone().expect("pending send is frozen"), RunnerCompletionDelivery { generation, phase: RunnerCompletionPhase::Retired }));
                    if flush_delivery_update(&registry, &owner.session, &task_id, &mut delivery_update).is_err() {
                        return Next::Retry(retry_delay(0));
                    }
                    settled = true;
                }
            }
            let scope = task_notice_scope(metadata.scope.as_ref());
            if !settled && scope.is_err() {
                warn_missing_run_parent(&task_id);
                if !completion {
                    return Next::Done;
                }
                settled = true;
            }
            if settled {
                return if registry
                    .ack_completions_for_session(
                        Some(&owner.session),
                        std::slice::from_ref(&task_id),
                    )
                    .is_empty()
                {
                    Next::Retry(Duration::from_secs(60))
                } else {
                    Next::Done
                };
            }
            let attempt = attempt.get_or_insert(if completion {
                metadata.runner_completion_offers
            } else {
                0
            });
            let mut current_request = None;
            let outcome = if completion && age >= MAX_AGE_MS {
                Outcome::Final(Failure::coded(
                    FailureClass::Expired,
                    "expired",
                    "task finish age reached 24 hours",
                ))
            } else {
                let outcome = (|| {
                    let raw = metadata
                        .launching_raw_harness
                        .clone()
                        .unwrap_or_else(|| owner.harness.clone());
                    let Some(address) = Address::with_scope(&owner, raw, scope.expect("invalid scope was settled")) else {
                        return retry(FailureClass::Address, "task has no runner notice address");
                    };
                    let runtime = match tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        Ok(runtime) => runtime,
                        Err(error) => return retry(FailureClass::Runtime, error),
                    };
                    let mut request = request(id.clone(), text.clone(), completion, address.worker);
                    let mut delivery_order = false;
                    if completion || reminder_elapsed_ms.is_some() {
                        let policy = if let Some(policy) = metadata.runner_notice_policy.filter(|_| {
                            reminder_elapsed_ms <= metadata.runner_reminder_elapsed_ms
                        }) {
                            policy
                        } else {
                            let dedupe = metadata.runner_notice_policy.map(|policy| policy.dedupe)
                                .unwrap_or_else(|| runtime.block_on(runner_delivery_order(&path, &address)) == Some(true));
                            match registry.freeze_runner_notice_policy(&owner.session, &task_id, dedupe, reminder_elapsed_ms) {
                                Ok(policy) => policy,
                                Err(error) => return retry(FailureClass::Persistence, error),
                            }
                        };
                        delivery_order = policy.dedupe;
                        request = task_request(request, &task_id, metadata.started_at, metadata.finished_at, reminder_elapsed_ms, policy);
                    }
                    let request = if let Some(request) = metadata
                        .runner_completion_notice
                        .clone()
                        .filter(|_| completion)
                    {
                        request
                    } else if completion {
                        match registry.freeze_runner_completion_notice(
                            &owner.session,
                            &task_id,
                            request,
                        ) {
                            Ok(request) => request,
                            Err(error) => {
                                log::warn!(
                                    "runner notice task={task_id} could not freeze send: {error}"
                                );
                                return retry(FailureClass::Persistence, error);
                            }
                        }
                    } else {
                        request
                    };
                    current_request = Some(request.clone());
                    if checking {
                        return runtime.block_on(check_send(&path, &address, &request, delivery_order));
                    }
                    if completion {
                        let Some(offers) = registry.reserve_runner_completion_offer(&task_id)
                        else {
                            return retry(
                                FailureClass::Persistence,
                                "could not reserve completion offer",
                            );
                        };
                        if let Err(error) = registry.persist_runner_completion_offer(
                            &owner.session,
                            &task_id,
                            offers,
                        ) {
                            return retry(FailureClass::Persistence, error);
                        }
                    }
                    let send_request = |request: &SendRequest| {
                        let send = || runtime.block_on(async {
                            let stale_reminder = || reminder_elapsed_ms.is_some() && registry.notice_metadata(&task_id)
                                .is_none_or(|latest| latest.is_terminal() || latest.completion_delivered);
                            // Preparing the request and staging its display can both race with a kill.
                            if stale_reminder() {
                                return Outcome::Accepted;
                            }
                            if let Some(record) = super::rich_ui::notices::for_send(&registry, &frame, &metadata, &address.module, &address.identity.session, &request.send_id, timestamp) {
                                super::rich_ui::display::stage_notice(record).await;
                                if stale_reminder() {
                                    return Outcome::Accepted;
                                }
                            }
                            send(&path, &address, request, delivery_order).await
                        });
                        if completion {
                            registry.send_runner_completion(&task_id, send).unwrap_or(Outcome::Accepted)
                        } else {
                            send()
                        }
                    };
                    let mut outcome = send_request(&request);
                    let mut request = request;
                    if completion {
                        for _ in 0..2 {
                            let (fallback, reason) = if !address.worker {
                                (idle_start_fallback(&request, &outcome), "idle_start")
                            } else {
                                (None, "idle_start")
                            };
                            let (fallback, reason) = match fallback {
                                Some(request) => (Some(request), reason),
                                None => (notice_members_fallback(&request, &outcome), "dedupe_key/expires_at_ms"),
                            };
                            let Some(fallback) = fallback else { break };
                            // A refused send records no send_id on the runner. Persist
                            // the smaller request before sending it, including on replay.
                            if let Err(error) = registry.replace_runner_completion_notice(
                                &owner.session,
                                &task_id,
                                fallback.clone(),
                            ) {
                                return retry(FailureClass::Persistence, error);
                            }
                            if reason == "idle_start" {
                                IDLE_START_FALLBACK.fetch_add(1, Ordering::Relaxed);
                            }
                            crate::slog_info!(
                                "runner notice task={task_id} send_id={id} session={} retrying without {reason} after invalid_params refusal",
                                owner.session
                            );
                            request = fallback;
                            current_request = Some(request.clone());
                            outcome = send_request(&request);
                        }
                    } else if reminder_elapsed_ms.is_some() && refused_notice_members(&outcome) {
                        crate::slog_info!("runner reminder task={task_id} send_id={id} dropped after optional member refusal");
                    }
                    outcome
                })();
                outcome
            };
            let mut outcome = outcome;
            if let Outcome::NotDelivered(reason) = &outcome {
                NOT_DELIVERED.fetch_add(1, Ordering::Relaxed);
                if completion {
                    let mut request = current_request.expect("send reply has a frozen request");
                    // llm-runner 0.8.58 defines host_refused as permanent: resending cannot help.
                    let (next_generation, phase) = if reason == "host_refused" {
                        crate::slog_info!("runner notice task={task_id} session={} not_delivered reason={reason}; retired", owner.session);
                        (generation, RunnerCompletionPhase::Retired)
                    } else if generation < MAX_GENERATIONS {
                        let next_generation = generation + 1;
                        request.send_id = format!("{id}:{next_generation}");
                        crate::slog_info!("runner notice task={task_id} session={} not_delivered reason={reason}; retrying generation={next_generation}", owner.session);
                        (next_generation, RunnerCompletionPhase::Ready { due_ms: timestamp.saturating_add(retry_delay(*attempt).as_millis() as u64) })
                    } else {
                        crate::slog_error!("runner notice task={task_id} session={} not_delivered after {MAX_GENERATIONS} generations reason={reason}", owner.session);
                        (generation, RunnerCompletionPhase::Retired)
                    };
                    delivery_update = Some((request, RunnerCompletionDelivery { generation: next_generation, phase }));
                    if flush_delivery_update(&registry, &owner.session, &task_id, &mut delivery_update).is_err() {
                        return Next::Retry(retry_delay(0));
                    }
                    if next_generation != generation {
                        *work_follow.send_id.lock().unwrap() = None;
                        receipt.applied();
                        let delay = retry_delay(*attempt);
                        *attempt = attempt.saturating_add(1);
                        return Next::Retry(delay.min(Duration::from_millis(MAX_AGE_MS.saturating_sub(age))));
                    }
                    outcome = Outcome::Accepted;
                } else {
                    crate::slog_info!("runner reminder task={task_id} not_delivered reason={reason}; dropped");
                }
            } else if completion && (matches!(outcome, Outcome::Pending) || checking && matches!(outcome, Outcome::Retry(_) | Outcome::Final(_))) {
                let (checks, retrying) = match progress.as_ref().map(|progress| &progress.phase) {
                    Some(RunnerCompletionPhase::Pending { checks, retry, .. }) => (*checks, *retry),
                    _ => (0, false),
                };
                let failed = !matches!(outcome, Outcome::Pending);
                let (checks, retrying, delay) = if failed && !retrying {
                    (checks, true, retry_delay(0))
                } else {
                    let checks = if checking { checks.saturating_add(1) } else { checks };
                    (checks, false, if work_follow.attached.load(Ordering::Relaxed) { crate::late_result_bus::readers::BACKSTOP } else { pending_delay(checks) })
                };
                let delay = delay.min(Duration::from_millis(MAX_AGE_MS.saturating_sub(age)));
                *work_follow.send_id.lock().unwrap() = current_request.as_ref().or(metadata.runner_completion_notice.as_ref()).map(|request| request.send_id.clone());
                delivery_update = Some((current_request.or(metadata.runner_completion_notice).expect("pending send is frozen"), RunnerCompletionDelivery {
                    generation,
                    phase: RunnerCompletionPhase::Pending { checks, due_ms: timestamp.saturating_add(delay.as_millis() as u64), retry: retrying },
                }));
                if flush_delivery_update(&registry, &owner.session, &task_id, &mut delivery_update).is_err() {
                    return Next::Retry(retry_delay(0));
                }
                if !failed { receipt.applied(); }
                return Next::Retry(delay);
            }
            settled = completion && !matches!(outcome, Outcome::Retry(_));
            let next = deliver(&id, *attempt, completion, outcome, &mut retry_logged);
            *attempt = attempt.saturating_add(1);
            if settled {
                if registry
                    .ack_completions_for_session(
                        Some(&owner.session),
                        std::slice::from_ref(&task_id),
                    )
                    .is_empty()
                {
                    return Next::Retry(Duration::from_secs(60));
                }
            }
            match next {
                Next::Retry(delay) if completion => {
                    Next::Retry(delay.min(Duration::from_millis(MAX_AGE_MS.saturating_sub(age))))
                }
                next => { if settled { receipt.applied(); } next },
            }
        }),
        expedite,
        follow,
    );
    if !queued {
        log::warn!("runner notice deferred: notice queue full");
    }
    queued
}
