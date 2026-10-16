//! Cingulate consent client for stamped Alfonso workers.
//!
//! When the access policy says `ask` for a worker's in-process tool call, the
//! call is held while the question goes to Cingulate (`consent.request`,
//! `consent.await`, `consent.withdraw`). Cingulate routes a worker's ask to its
//! head and, if escalated, to the user in the Alfonso apps. AFT never shows a
//! host prompt for these calls and never falls back to one: if Cingulate
//! cannot be reached the call ends with `access_unavailable`.
//!
//! An ask never expires. The call is held for at most the route's reply budget
//! (never more than 100 s); an ask still open then ends the call with
//! `access_pending`, and the persisted ask (see [`crate::db::access_asks`]) lets
//! the retried call pick up the same Cingulate ask by its original call key.
//!
//! Wire contract (Cingulate's requester contract, `tool_permission` asks): a
//! management-surface route to the `cingulate` module, opened by AFT's own
//! principal (`reserved:aft`) under the asking route's scope stamp. Cingulate
//! takes the asker and its head only from that stamp. Each request is
//! `{"method", "params"}` and each reply `{"result"}`; refusals arrive as
//! module error frames carrying a code and message.
//! - `consent.request {kind, call_key, tool, operation, paths, reason,
//!   args_hash, pattern, head_policy?}` returns `{elicitation_id}`; repeating a
//!   call key returns the same id. `head_policy` is sent on worker-scope asks
//!   only, and unknown fields are refused.
//! - `consent.await {elicitation_id, timeout_ms}` returns the ask's record once
//!   it leaves its pending states or the timeout (at most 120 s) passes.
//! - `consent.withdraw {elicitation_id}` settles a pending ask as
//!   `access_withdrawn` and returns the record.
use super::*;
use crate::access_policy::{AccessGrant, AccessOperation, AskRoute, GrantKind, HeadPolicy};
use crate::db::access_asks::{self as asks, AskKey};
use std::future::Future;
use std::pin::Pin;
use subc_protocol::scope::ScopeSelector;

pub(super) const CINGULATE_MODULE_ID: &str = "cingulate";

/// Cingulate refuses an envelope larger than this many bytes.
const MAX_ENVELOPE_BYTES: usize = 1_310_720;
/// Cingulate holds one `consent.await` for at most this long.
const MAX_AWAIT: Duration = Duration::from_millis(120_000);

/// Clients give a call without a declared reply budget at least this long
/// (the shortest supported transport deadline).
const UNDECLARED_REPLY_BUDGET: Duration = Duration::from_secs(25);
/// Time kept back from the reply budget to run the approved call and deliver it.
const RERUN_MARGIN: Duration = Duration::from_secs(5);
/// No call is held longer than this, whatever its reply budget.
const MAX_HOLD: Duration = Duration::from_secs(100);
/// How often a held call checks for cancellation, route close and scope end.
const SIGNAL_POLL: Duration = Duration::from_millis(100);
/// Bound on one `consent.request` or `consent.withdraw` round trip.
const UNARY_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ConsentError {
    /// Cingulate or the daemon could not be reached.
    Unavailable(String),
    /// Cingulate answered with a named refusal.
    Refused { code: String, message: String },
}

pub(super) type ConsentFuture = Pin<Box<dyn Future<Output = Result<Value, ConsentError>> + Send>>;

/// One consent method sent under the asking route's scope.
#[derive(Clone, Debug)]
pub(super) struct ConsentCall {
    pub(super) scope: ScopeSelector,
    pub(super) project_root: PathBuf,
    pub(super) session: String,
    pub(super) method: &'static str,
    pub(super) params: Value,
    pub(super) timeout: Duration,
}

/// The outbound consent channel. Production talks to Cingulate over the
/// daemon; tests install a fake.
pub(super) trait ConsentTransport: Send + Sync {
    fn call(&self, call: ConsentCall) -> ConsentFuture;
}

static TRANSPORT: LazyLock<StdMutex<Option<Arc<dyn ConsentTransport>>>> =
    LazyLock::new(|| StdMutex::new(None));

#[cfg(test)]
static TEST_TRANSPORT: LazyLock<StdMutex<Option<Arc<dyn ConsentTransport>>>> =
    LazyLock::new(|| StdMutex::new(None));

/// Install the production transport for this process's daemon connection.
pub(super) fn install_subc_transport(connection_file: &Path) {
    *TRANSPORT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(SharedSubcConsent(
        Arc::new(SubcConsentTransport::new(connection_file.to_path_buf())),
    )));
}

/// Replace the transport for consent tests; the module loop's production
/// install never overrides it.
#[cfg(test)]
pub(super) fn install_test_transport(transport: Option<Arc<dyn ConsentTransport>>) {
    *TEST_TRANSPORT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = transport;
}

fn transport() -> Option<Arc<dyn ConsentTransport>> {
    #[cfg(test)]
    {
        let test = TEST_TRANSPORT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if test.is_some() {
            return test;
        }
    }
    TRANSPORT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

#[cfg(test)]
static HOLD_OVERRIDE_MS: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
pub(super) fn set_hold_budget_for_test(budget: Option<Duration>) {
    HOLD_OVERRIDE_MS.store(
        budget.map_or(0, |budget| budget.as_millis().max(1) as u64),
        Ordering::SeqCst,
    );
}

/// How long a call is held for an answer before it ends with `access_pending`.
/// A tool that declares a reply budget is held for that budget, otherwise for
/// the shortest client transport deadline, less a margin to run the approved
/// call and deliver it, and never longer than 100 s.
pub(super) fn hold_budget(declared_reply: Option<Duration>) -> Duration {
    #[cfg(test)]
    {
        let ms = HOLD_OVERRIDE_MS.load(Ordering::SeqCst);
        if ms > 0 {
            return Duration::from_millis(ms);
        }
    }
    declared_hold_budget(declared_reply)
}

fn declared_hold_budget(declared_reply: Option<Duration>) -> Duration {
    declared_reply
        .unwrap_or(UNDECLARED_REPLY_BUDGET)
        .saturating_sub(RERUN_MARGIN)
        .min(MAX_HOLD)
}

/// Production transport over the daemon's own authenticated subc connection,
/// as `reserved:aft`. The connection opens lazily and reopens after a failure.
struct SubcConsentTransport {
    connection_file: PathBuf,
    consumer: tokio::sync::Mutex<Option<Arc<subc_client_rs::SubcConsumer>>>,
}

impl SubcConsentTransport {
    fn new(connection_file: PathBuf) -> Self {
        Self {
            connection_file,
            consumer: tokio::sync::Mutex::new(None),
        }
    }

    async fn consumer(&self) -> Result<Arc<subc_client_rs::SubcConsumer>, ConsentError> {
        let mut slot = self.consumer.lock().await;
        if let Some(consumer) = slot.as_ref() {
            return Ok(Arc::clone(consumer));
        }
        if crate::launch_nonce::consumer_identity().is_none() {
            return Err(ConsentError::Unavailable(
                "the daemon has no subc module identity to ask with".into(),
            ));
        }
        let consumer = crate::fleet_status::connect_subc_consumer(
            &self.connection_file,
            subc_client_rs::ConsumerOptions::default(),
        )
        .await
        .map_err(|error| ConsentError::Unavailable(error.to_string()))?;
        let consumer = Arc::new(consumer);
        *slot = Some(Arc::clone(&consumer));
        Ok(consumer)
    }

    async fn forget(&self, failed: &Arc<subc_client_rs::SubcConsumer>) {
        let mut slot = self.consumer.lock().await;
        if slot
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, failed))
        {
            *slot = None;
        }
    }

    async fn send(self: Arc<Self>, call: ConsentCall) -> Result<Value, ConsentError> {
        use subc_client_rs::{CallError, CallOptions};
        let consumer = self.consumer().await?;
        let identity = subc_protocol::BindIdentity::new(
            call.project_root.clone(),
            "aft-consent",
            call.session.clone(),
        );
        let options = CallOptions {
            consumer_identity: crate::launch_nonce::consumer_identity(),
            timeout: call.timeout,
            route_retry_deadline: call.timeout,
            ..CallOptions::default()
        };
        // A refused scoped open is never retried unscoped: the daemon stamps
        // the asking route's scope onto Cingulate's route, and Cingulate reads
        // the asker and its head from that stamp.
        let route = match consumer
            .open_route_scoped(
                RouteTarget::ManagementSurface {
                    module_id: CINGULATE_MODULE_ID.into(),
                },
                identity,
                call.scope.clone(),
                options,
            )
            .await
        {
            Ok(route) => route,
            Err(CallError::Module(body)) => {
                return Err(ConsentError::Refused {
                    code: body.code,
                    message: body.message,
                })
            }
            Err(error) => {
                self.forget(&consumer).await;
                return Err(ConsentError::Unavailable(error.to_string()));
            }
        };
        let body = encode(call.method, &call.params)?;
        let options = CallOptions {
            timeout: call.timeout,
            ..CallOptions::default()
        };
        match consumer.request(&route, body, options).await {
            Ok(bytes) => decode_result(&bytes),
            Err(CallError::Module(body)) => Err(ConsentError::Refused {
                code: body.code,
                message: body.message,
            }),
            Err(error) => {
                self.forget(&consumer).await;
                Err(ConsentError::Unavailable(error.to_string()))
            }
        }
    }
}

/// Copy of `value` whose object keys are in sorted order at every level. The
/// crate builds JSON with insertion-ordered maps, and Cingulate's contract
/// vectors are written with sorted keys.
fn sorted_keys(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|left, right| left.0.cmp(right.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key.clone(), sorted_keys(value)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted_keys).collect()),
        other => other.clone(),
    }
}

/// The request envelope for one consent method: `{"method", "params"}`, compact,
/// with sorted keys. Cingulate refuses envelopes over its size cap, so an
/// oversized one is refused here by the same name instead of being sent.
pub(super) fn encode(method: &str, params: &Value) -> Result<Vec<u8>, ConsentError> {
    let body = serde_json::to_vec(&sorted_keys(&json!({"method": method, "params": params})))
        .map_err(|error| ConsentError::Unavailable(error.to_string()))?;
    if body.len() > MAX_ENVELOPE_BYTES {
        return Err(ConsentError::Refused {
            code: "elicitation_request_too_large".into(),
            message: format!("consent envelope exceeds {MAX_ENVELOPE_BYTES} bytes"),
        });
    }
    Ok(body)
}

/// The `result` of a reply envelope `{"result": ...}`. Refusals never arrive
/// here: Cingulate sends them as module error frames.
pub(super) fn decode_result(bytes: &[u8]) -> Result<Value, ConsentError> {
    let envelope: Value = serde_json::from_slice(bytes)
        .map_err(|_| ConsentError::Unavailable("cingulate's reply was not JSON".into()))?;
    match envelope {
        Value::Object(mut object) if object.len() == 1 && object.contains_key("result") => {
            Ok(object.remove("result").expect("result is present"))
        }
        _ => Err(ConsentError::Unavailable(
            "cingulate's reply is not a {result} envelope".into(),
        )),
    }
}

/// The installed handle: one shared connection state for every consent call.
struct SharedSubcConsent(Arc<SubcConsentTransport>);

impl ConsentTransport for SharedSubcConsent {
    fn call(&self, call: ConsentCall) -> ConsentFuture {
        Box::pin(Arc::clone(&self.0).send(call))
    }
}

/// The capability a tool response asks Cingulate for, when the in-process
/// access check refused with the internal ask code.
pub(super) fn requested_ask(response: &Response) -> Option<AccessGrant> {
    if response.success
        || response.data.get("code").and_then(Value::as_str)
            != Some(crate::access_policy::ACCESS_ASK_REQUIRED)
    {
        return None;
    }
    serde_json::from_value(response.data.get("ask")?.clone()).ok()
}

/// Calls currently held on each route, so a route close can withdraw their asks.
static HELD: LazyLock<StdMutex<HashMap<RouteChannel, Vec<Arc<AtomicBool>>>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

/// Marks one held call on its route for the length of the hold.
pub(super) struct RouteHold {
    route: RouteChannel,
    closed: Arc<AtomicBool>,
}

impl RouteHold {
    fn new(route: RouteChannel) -> Self {
        let closed = Arc::new(AtomicBool::new(false));
        HELD.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(route)
            .or_default()
            .push(Arc::clone(&closed));
        Self { route, closed }
    }

    fn closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}

impl Drop for RouteHold {
    fn drop(&mut self) {
        let mut held = HELD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(flags) = held.get_mut(&self.route) {
            flags.retain(|flag| !Arc::ptr_eq(flag, &self.closed));
            if flags.is_empty() {
                held.remove(&self.route);
            }
        }
    }
}

/// The consumer closed `route` (a Goodbye, not a transport drop or a
/// higher-epoch rebind): every call held on it withdraws its ask.
pub(super) fn note_route_closed(route: RouteChannel) {
    if let Some(flags) = HELD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&route)
    {
        for flag in flags {
            flag.store(true, Ordering::SeqCst);
        }
    }
}

/// The parts of a held call the consent flow reads.
pub(super) struct HeldCall {
    pub(super) reply_deadline: Instant,
    pub(super) identity: RouteIdentity,
    pub(super) tool: String,
    pub(super) arguments: Value,
    pub(super) request_id: String,
    pub(super) executor: Arc<Executor>,
    pub(super) route: RouteChannel,
    pub(super) corr: u64,
    pub(super) active: ActiveToolCalls,
    pub(super) connection_cancel: PersistentCancelSignal,
    pub(super) drain: drain::ModuleDrainWindow,
}

/// What a held call does after its ask was handled.
enum Hold {
    /// Run the call again with these capabilities approved once. `holder`
    /// owns them until the call finishes.
    Run {
        once: Vec<AccessGrant>,
        ask: AskKey,
        holder: String,
    },
    /// End the call with this response.
    Answer(Response),
    /// Nobody waits for this call any more (cancelled, route closed, transport
    /// gone); the caller sends nothing.
    Abandoned,
}

/// Run a stamped worker's call, holding it on Cingulate whenever the access
/// check asks. `first` is the call's first response; `rerun` runs it again
/// with approved capabilities installed. Returns the final response with its
/// formatted text when the run produced one, or `None` when nobody waits.
pub(super) async fn run_with_consent<F, Fut>(
    call: HeldCall,
    first: (Response, String),
    mut rerun: F,
) -> Option<(Response, Option<String>)>
where
    F: FnMut(AskRoute) -> Fut,
    Fut: Future<Output = (Response, String)>,
{
    let Some(mut grant) = requested_ask(&first.0) else {
        return Some((first.0, Some(first.1)));
    };
    let route_hold = RouteHold::new(call.route);
    let deadline = (Instant::now() + hold_budget(None)).min(
        call.reply_deadline
            .checked_sub(RERUN_MARGIN)
            .unwrap_or_else(Instant::now),
    );
    let mut holder: Option<(AskKey, String)> = None;
    loop {
        let own = holder.as_ref().map(|(_, holder)| holder.clone());
        match hold(&call, grant, deadline, &route_hold, own).await {
            Hold::Run {
                once,
                ask,
                holder: claimed,
            } => {
                holder = Some((ask, claimed));
                let running = rerun(AskRoute::Consent {
                    once,
                    worker: access_grants::alfonso_worker(&call.identity),
                });
                tokio::pin!(running);
                let (response, text) = loop {
                    tokio::select! {
                        done = &mut running => break done,
                        _ = tokio::time::sleep(SIGNAL_POLL) => {
                            // The consumer closed the route while the approved
                            // run was in flight: stop the run (it is the call's
                            // active job) and drop what was approved for it.
                            if route_hold.closed() {
                                cancel_active_tool_call(
                                    &call.active,
                                    &call.executor,
                                    call.route,
                                    call.corr,
                                    "route closed during an approved rerun",
                                );
                                if let Some((ask, _)) = holder.take() {
                                    let _ = with_db(&call, move |conn, _| {
                                        asks::discard(conn, &ask).map(|_| ())
                                    })
                                    .await;
                                }
                                return None;
                            }
                        }
                    }
                };
                match requested_ask(&response) {
                    Some(next) => grant = next,
                    None => {
                        if let Some((ask, claimed)) = holder.take() {
                            // The approvals are used up by this run, whatever it returned.
                            let _ = with_db(&call, move |conn, _| {
                                asks::consume(conn, &ask, &claimed).map(|_| ())
                            })
                            .await;
                        }
                        return Some((response, Some(text)));
                    }
                }
            }
            Hold::Answer(response) => return Some((response, None)),
            Hold::Abandoned => return None,
        }
    }
}

fn consent_path_representable(path: &Path) -> bool {
    // The consent service's pinned decoder (Cingulate 6bb5f4d) requires paths
    // to start with '/'. Drive and UNC paths have no agreed representation:
    // do not send them and do not invent a conversion.
    path.to_str()
        .is_some_and(|path| path.starts_with('/') && !path.contains('\0'))
}

async fn with_db<T: Send + 'static>(
    call: &HeldCall,
    work: impl FnOnce(&rusqlite::Connection, &AppContext) -> rusqlite::Result<T> + Send + 'static,
) -> Result<T, String> {
    let Some(ctx) = call.executor.actor_context(&call.identity.root) else {
        return Err("executor actor is not registered".into());
    };
    tokio::task::spawn_blocking(move || {
        let db = ctx
            .db()
            .ok_or_else(|| "aft.db is unavailable".to_string())?;
        let conn = db
            .lock()
            .map_err(|_| "aft.db mutex is poisoned".to_string())?;
        work(&conn, &ctx).map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

/// One ask ready to send.
#[derive(Clone)]
struct Prepared {
    key: AskKey,
    call_key: String,
    request: Value,
    selector: ScopeSelector,
    grant: AccessGrant,
}

enum Prep {
    Ask(Prepared),
    /// Another dispatch of the same call is running with this call's approvals.
    Busy,
}

fn selector(stamp: &subc_protocol::scope::ScopeStamp) -> ScopeSelector {
    ScopeSelector {
        owner: stamp.owner.clone(),
        scope_ref: stamp.scope_ref.clone(),
        scope_epoch: Some(stamp.scope_epoch),
    }
}

fn operation_name(operation: AccessOperation) -> &'static str {
    match operation {
        AccessOperation::Read => "read",
        AccessOperation::Write => "write",
    }
}

/// The ask's identity: tool, canonical arguments, resolved cwd, and the asking
/// scope at its epoch. A changed cwd or epoch is a different ask.
fn args_hash(identity: &RouteIdentity, tool: &str, arguments: &Value) -> String {
    let stamp = identity.scope.as_ref();
    cortexkit_role_tool_provider::catalog::composition_digest(&json!({
        "tool": tool,
        "arguments": arguments,
        "cwd": identity.project_root,
        "scope_ref": stamp.map(|stamp| stamp.scope_ref.clone()),
        "scope_epoch": stamp.map(|stamp| stamp.scope_epoch),
    }))
    .expect("ask identity is JSON")
}

/// Whether the worker's head holds `grant` itself, by its own policy or a
/// standing grant. A worker with no head never has an `allow` head policy.
fn head_policy(
    conn: &rusqlite::Connection,
    ctx: &AppContext,
    identity: &RouteIdentity,
    grant: &AccessGrant,
) -> rusqlite::Result<HeadPolicy> {
    let Some(head) = access_grants::consent_head(conn, identity)? else {
        return Ok(HeadPolicy::NotHeld);
    };
    Ok(access_grants::policy_for_scope(conn, &head, ctx)?
        .map_or(HeadPolicy::NotHeld, |policy| policy.head_policy(grant)))
}

fn new_call_key() -> String {
    let mut id = [0u8; 16];
    getrandom::fill(&mut id).expect("operating system random source for consent call keys");
    let hex: String = id.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("aft-ask-{hex}")
}

fn prepare(
    conn: &rusqlite::Connection,
    ctx: &AppContext,
    identity: &RouteIdentity,
    tool: &str,
    arguments: &Value,
    grant: &AccessGrant,
    own_holder: Option<&str>,
) -> rusqlite::Result<Prep> {
    let stamp = identity
        .scope
        .as_ref()
        .expect("only stamped Alfonso agents ask Cingulate");
    let key = AskKey {
        scope: access_grants::scope_key(identity),
        args_hash: args_hash(identity, tool, arguments),
    };
    let selector = selector(stamp);
    if let Some(record) = asks::get(conn, &key)? {
        if let (Some(call_key), Some(request), Some(pending)) = (
            record.call_key.clone(),
            record.request.clone(),
            record.pending.clone(),
        ) {
            // Re-send the ask under the call key it was first sent with, so
            // Cingulate returns the same ask rather than a second one.
            return Ok(Prep::Ask(Prepared {
                key,
                call_key,
                request,
                selector,
                grant: pending,
            }));
        }
        if record.holder_is_live() && record.holder.as_deref() != own_holder {
            return Ok(Prep::Busy);
        }
        if !record.holder_is_live() {
            // Approvals claimed by a process that has since ended are not
            // reused: the earlier call may or may not have run.
            asks::discard(conn, &key)?;
        }
    }
    let call_key = new_call_key();
    // `head_policy` says whether the worker's head itself holds the requested
    // access, which decides whether the head may allow it. Cingulate requires
    // it on asks from a `worker` scope and refuses it on every other kind.
    let head = match identity.scope.as_ref().map(|stamp| stamp.kind) {
        Some(subc_protocol::scope::ScopeKind::Worker) => {
            Some(head_policy(conn, ctx, identity, grant)?)
        }
        _ => None,
    };
    let path = grant.path.to_string_lossy();
    let request = request_params(
        &call_key,
        tool,
        operation_name(grant.operation),
        &path,
        &format!(
            "{tool} needs to {} a path outside this route's roots",
            operation_name(grant.operation)
        ),
        &key.args_hash,
        &pattern(grant),
        head.map(head_policy_name),
    );
    asks::open(
        conn,
        &key,
        &call_key,
        &serde_json::to_value(&selector).expect("scope selector is JSON"),
        &request,
        grant,
        crate::db::call_ledger::now_ms(),
    )?;
    Ok(Prep::Ask(Prepared {
        key,
        call_key,
        request,
        selector,
        grant: grant.clone(),
    }))
}

/// The display pattern Cingulate shows for the capability an answer grants:
/// the path itself, or `<path>/**` for a whole directory tree.
fn pattern(grant: &AccessGrant) -> String {
    let path = grant.path.to_string_lossy();
    match grant.kind {
        GrantKind::File => path.into_owned(),
        GrantKind::Subtree => format!("{}/**", path.trim_end_matches('/')),
    }
}

fn head_policy_name(policy: HeadPolicy) -> &'static str {
    match policy {
        HeadPolicy::Allow => "allow",
        HeadPolicy::NotHeld => "not_held",
    }
}

/// `consent.request` parameters asking permission for one in-process read or
/// write of `path`: exactly the fields Cingulate accepts, since it refuses
/// unknown ones. Cingulate's `command` field is only for asks to run a shell
/// command, which AFT does not send (shell file access is never asked).
#[allow(clippy::too_many_arguments)]
pub(super) fn request_params(
    call_key: &str,
    tool: &str,
    operation: &str,
    path: &str,
    reason: &str,
    args_hash: &str,
    pattern: &str,
    head_policy: Option<&str>,
) -> Value {
    let mut params = json!({
        "args_hash": args_hash,
        "call_key": call_key,
        "kind": "tool_permission",
        "operation": operation,
        "paths": [path],
        "pattern": pattern,
        "reason": reason,
        "tool": tool,
    });
    if let Some(policy) = head_policy {
        params["head_policy"] = json!(policy);
    }
    sorted_keys(&params)
}

/// Cingulate's answer as AFT applies it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Settlement {
    Pending,
    Allow { scope: bool, by_user: bool },
    Deny { note: Option<String> },
    Refused(String),
}

fn malformed(what: &str) -> ConsentError {
    ConsentError::Unavailable(format!("cingulate's reply is not understood: {what}"))
}

/// The id in a `consent.request` result `{"elicitation_id": "acc_..."}`.
pub(super) fn decode_elicitation_id(result: &Value) -> Result<String, ConsentError> {
    result
        .as_object()
        .filter(|object| object.len() == 1)
        .and_then(|object| object.get("elicitation_id"))
        .and_then(Value::as_str)
        .filter(|id| id.starts_with("acc_"))
        .map(str::to_string)
        .ok_or_else(|| malformed("consent.request did not return an acc_ elicitation_id"))
}

/// The state of the ask record `consent.await` and `consent.withdraw` return.
/// `pending:head` and `pending:user` are still open. `settled` carries the
/// answer and who gave it; an allow without `answered_by` counts as the
/// head's, so `finish` checks again that the head itself still holds the
/// access. `refused:<code>` (withdrawn, head gone) grants nothing, and the held
/// call ends with that `<code>` as its error code.
pub(super) fn decode_record(record: &Value) -> Result<Settlement, ConsentError> {
    if record.get("kind").and_then(Value::as_str) == Some("macos_permission") {
        return Err(ConsentError::Refused {
            code: "access_informational".into(),
            message: "macOS permission cards are informational and cannot be awaited".into(),
        });
    }
    let field = |name: &str| record.get(name).and_then(Value::as_str);
    let state = field("state").ok_or_else(|| malformed("the record has no state"))?;
    match state {
        "pending:head" | "pending:user" => Ok(Settlement::Pending),
        "settled" => {
            let by_user = match record.get("answered_by") {
                None | Some(Value::Null) => false,
                Some(Value::String(by)) if by == "head" => false,
                Some(Value::String(by)) if by == "user" => true,
                Some(_) => return Err(malformed("answered_by is neither head nor user")),
            };
            match field("outcome") {
                Some("allow_once") => Ok(Settlement::Allow {
                    scope: false,
                    by_user,
                }),
                Some("allow_scope") => Ok(Settlement::Allow {
                    scope: true,
                    by_user,
                }),
                Some("deny") => Ok(Settlement::Deny {
                    note: field("note").map(str::to_string),
                }),
                _ => Err(malformed("a settled record has no known outcome")),
            }
        }
        refused if refused.starts_with("refused:") => {
            let code = field("outcome")
                .filter(|outcome| outcome.starts_with("access_"))
                .unwrap_or(&refused["refused:".len()..]);
            Ok(Settlement::Refused(code.to_string()))
        }
        _ => Err(malformed("the record's state is unknown")),
    }
}

fn answer(call: &HeldCall, code: &str, message: String, prep_grant: &AccessGrant) -> Response {
    Response::error_with_data(&call.request_id, code, message, json!({"ask": prep_grant}))
}

fn pending_answer(call: &HeldCall, grant: &AccessGrant) -> Response {
    let recipient = if access_grants::alfonso_worker(&call.identity) {
        "your head"
    } else {
        "the user"
    };
    answer(
        call,
        "access_pending",
        format!(
            "Waiting for permission to {} {}. The request has gone to {}. End your turn now; you will be resumed with the answer, and then you should run this same call again.",
            operation_name(grant.operation),
            grant.path.display(),
            recipient
        ),
        grant,
    )
}

fn unavailable_answer(call: &HeldCall, grant: &AccessGrant, detail: &str) -> Response {
    answer(
        call,
        "access_unavailable",
        format!(
            "Outside access denied: the consent service is unavailable, so permission to {} {} could not be requested ({detail}).",
            operation_name(grant.operation),
            grant.path.display()
        ),
        grant,
    )
}

fn refused_answer(
    call: &HeldCall,
    grant: &AccessGrant,
    code: &str,
    note: Option<&str>,
) -> Response {
    let target = format!(
        "{} {}",
        operation_name(grant.operation),
        grant.path.display()
    );
    let message = match code {
        "access_declined" => match note {
            Some(note) => format!("Permission to {target} was declined: {note}"),
            None => format!("Permission to {target} was declined."),
        },
        "access_no_head" | "access_head_gone" => {
            format!("Outside access denied ({code}): there is no head to ask for permission to {target}.")
        }
        "access_not_placed" => {
            format!("Outside access denied: the request for permission to {target} could not be placed.")
        }
        "access_withdrawn" => format!("The request for permission to {target} was withdrawn."),
        _ => match note {
            Some(detail) => format!("Outside access denied ({code}) for {target}: {detail}"),
            None => format!("Outside access denied ({code}) for {target}."),
        },
    };
    answer(call, code, message, grant)
}

enum Stop {
    /// Nobody waits for the call any more and the ask goes with it.
    Withdraw,
    /// The transport dropped or the module is restarting: keep the ask so the
    /// retried call picks it up again.
    Detach,
    /// The codemode run's scope ended.
    RunEnded,
}

fn stop_signal(call: &HeldCall, route_hold: &RouteHold) -> Option<Stop> {
    let detached = || call.connection_cancel.is_cancelled() || call.drain.is_active();
    if detached() {
        return Some(Stop::Detach);
    }
    if route_hold.closed() {
        return Some(Stop::Withdraw);
    }
    if call
        .identity
        .run_scope
        .get()
        .is_some_and(|scope| scope.revoked())
    {
        return Some(Stop::RunEnded);
    }
    if !active_tool_call_is_registered(&call.active, call.route, call.corr) {
        // Connection teardown cancels the connection before it drops calls,
        // so a call gone with the connection still up was cancelled or its
        // asker's route was reclaimed.
        return Some(if detached() {
            Stop::Detach
        } else {
            Stop::Withdraw
        });
    }
    None
}

fn consent_call(
    call: &HeldCall,
    prep: &Prepared,
    method: &'static str,
    params: Value,
    timeout: Duration,
) -> ConsentCall {
    ConsentCall {
        scope: prep.selector.clone(),
        project_root: call.identity.project_root.clone(),
        session: call.identity.session.clone(),
        method,
        params,
        timeout,
    }
}

async fn withdraw(call: &HeldCall, prep: &Prepared, elicitation: &str) {
    if let Some(transport) = transport() {
        if let Err(error) = transport
            .call(consent_call(
                call,
                prep,
                "consent.withdraw",
                json!({"elicitation_id": elicitation}),
                UNARY_TIMEOUT,
            ))
            .await
        {
            log::warn!("consent.withdraw failed for {elicitation}: {error:?}");
        }
    }
    let key = prep.key.clone();
    let _ = with_db(call, move |conn, _| asks::discard(conn, &key).map(|_| ())).await;
}

async fn discard(call: &HeldCall, prep: &Prepared) {
    let key = prep.key.clone();
    let _ = with_db(call, move |conn, _| asks::discard(conn, &key).map(|_| ())).await;
}

async fn hold(
    call: &HeldCall,
    grant: AccessGrant,
    deadline: Instant,
    route_hold: &RouteHold,
    own_holder: Option<String>,
) -> Hold {
    if !consent_path_representable(&grant.path) {
        let mut response = refused_answer(call, &grant, "access_not_placed", None);
        response.data["reason"] = json!("consent_path_unrepresentable");
        return Hold::Answer(response);
    }
    let identity = call.identity.clone();
    let tool = call.tool.clone();
    let arguments = call.arguments.clone();
    let asked = grant.clone();
    let prepared = with_db(call, move |conn, ctx| {
        prepare(
            conn,
            ctx,
            &identity,
            &tool,
            &arguments,
            &asked,
            own_holder.as_deref(),
        )
    })
    .await;
    let prep = match prepared {
        Ok(Prep::Ask(prep)) => prep,
        Ok(Prep::Busy) => return Hold::Answer(pending_answer(call, &grant)),
        Err(error) => return Hold::Answer(unavailable_answer(call, &grant, &error)),
    };
    let Some(transport) = transport() else {
        return Hold::Answer(unavailable_answer(
            call,
            &prep.grant,
            "no consent transport is connected",
        ));
    };
    let reply = transport
        .call(consent_call(
            call,
            &prep,
            "consent.request",
            prep.request.clone(),
            UNARY_TIMEOUT,
        ))
        .await;
    let elicitation = match reply.and_then(|reply| decode_elicitation_id(&reply)) {
        Ok(elicitation) => elicitation,
        Err(error) => return failed(call, &prep, error).await,
    };
    {
        let key = prep.key.clone();
        let call_key = prep.call_key.clone();
        let id = elicitation.clone();
        let _ = with_db(call, move |conn, _| {
            asks::set_elicitation(conn, &key, &call_key, &id)
        })
        .await;
    }
    // Await at least once, even with no budget left, so an ask that already
    // settled (a retry after the answer) is applied rather than parked.
    let mut first = true;
    loop {
        let now = Instant::now();
        if now >= deadline && !first {
            park(call, &prep, &elicitation);
            return Hold::Answer(pending_answer(call, &prep.grant));
        }
        first = false;
        let wait = deadline.saturating_duration_since(now).min(MAX_AWAIT);
        let awaited = transport.call(consent_call(
            call,
            &prep,
            "consent.await",
            json!({"elicitation_id": elicitation, "timeout_ms": wait.as_millis() as u64}),
            wait + UNARY_TIMEOUT,
        ));
        tokio::pin!(awaited);
        let reply = loop {
            tokio::select! {
                reply = &mut awaited => break Some(reply),
                _ = tokio::time::sleep(SIGNAL_POLL) => {
                    match stop_signal(call, route_hold) {
                        Some(Stop::Detach) => return Hold::Abandoned,
                        Some(Stop::Withdraw) => {
                            withdraw(call, &prep, &elicitation).await;
                            return Hold::Abandoned;
                        }
                        Some(Stop::RunEnded) => {
                            withdraw(call, &prep, &elicitation).await;
                            return Hold::Answer(refused_answer(call, &prep.grant, "access_withdrawn", None));
                        }
                        None if Instant::now() >= deadline => break None,
                        None => {}
                    }
                }
            }
        };
        match reply.map(|reply| reply.and_then(|record| decode_record(&record))) {
            None | Some(Ok(Settlement::Pending)) => {}
            Some(Ok(settled)) => return finish(call, &prep, settled).await,
            Some(Err(error)) => return failed(call, &prep, error).await,
        }
    }
}

/// A transport failure, and Cingulate's own `access_unavailable`, keep the
/// persisted ask so the retried call re-sends its call key. Any other refusal
/// code Cingulate sends (for example `access_no_head`, `access_not_alfonso`,
/// `elicitation_not_found`, `elicitation_invalid_request`) discards the ask, and
/// the held call ends with that same code.
async fn failed(call: &HeldCall, prep: &Prepared, error: ConsentError) -> Hold {
    match error {
        ConsentError::Unavailable(detail) => {
            Hold::Answer(unavailable_answer(call, &prep.grant, &detail))
        }
        ConsentError::Refused { code, message } if code == "access_unavailable" => {
            Hold::Answer(unavailable_answer(call, &prep.grant, &message))
        }
        ConsentError::Refused { code, message } => {
            discard(call, prep).await;
            Hold::Answer(refused_answer(call, &prep.grant, &code, Some(&message)))
        }
    }
}

enum Applied {
    Run(Vec<AccessGrant>),
    /// The head no longer holds what it allowed.
    HeadNotHeld,
    /// Another dispatch claimed the answer first, or the ask is gone.
    NotClaimed,
}

async fn finish(call: &HeldCall, prep: &Prepared, settled: Settlement) -> Hold {
    match settled {
        Settlement::Pending => unreachable!("finish runs on settled answers"),
        Settlement::Deny { note } => {
            discard(call, prep).await;
            Hold::Answer(refused_answer(
                call,
                &prep.grant,
                "access_declined",
                note.as_deref(),
            ))
        }
        Settlement::Refused(code) => {
            discard(call, prep).await;
            Hold::Answer(refused_answer(call, &prep.grant, &code, None))
        }
        Settlement::Allow { scope, by_user } => {
            let holder = asks::new_holder();
            let identity = call.identity.clone();
            let key = prep.key.clone();
            let call_key = prep.call_key.clone();
            let grant = prep.grant.clone();
            let claimant = holder.clone();
            let applied = with_db(call, move |conn, ctx| {
                // A head may pass on only what it holds now: its authority is
                // checked again against the head's current policy and grants.
                if !by_user && head_policy(conn, ctx, &identity, &grant)? != HeadPolicy::Allow {
                    asks::discard(conn, &key)?;
                    return Ok(Applied::HeadNotHeld);
                }
                let Some(approved) = asks::claim(conn, &key, &call_key, &claimant)? else {
                    return Ok(Applied::NotClaimed);
                };
                if scope {
                    crate::db::access_grants::insert(
                        conn,
                        &access_grants::policy_scope_key(&identity),
                        &grant,
                    )?;
                }
                Ok(Applied::Run(approved))
            })
            .await;
            match applied {
                Ok(Applied::Run(once)) => Hold::Run {
                    once,
                    ask: prep.key.clone(),
                    holder,
                },
                Ok(Applied::HeadNotHeld) => Hold::Answer(refused_answer(
                    call,
                    &prep.grant,
                    "access_declined",
                    Some("the head no longer holds this permission itself"),
                )),
                Ok(Applied::NotClaimed) => Hold::Answer(pending_answer(call, &prep.grant)),
                Err(error) => Hold::Answer(unavailable_answer(call, &prep.grant, &error)),
            }
        }
    }
}

/// A run-scoped call that ends with `access_pending` leaves its ask with the
/// run, so the ask is withdrawn when the run's scope ends.
fn park(call: &HeldCall, prep: &Prepared, elicitation: &str) {
    let Some(scope) = call.identity.run_scope.get() else {
        return;
    };
    let Some(db) = call
        .executor
        .actor_context(&call.identity.root)
        .and_then(|ctx| ctx.db())
    else {
        return;
    };
    scope.park_ask(ParkedAsk {
        call: consent_call(
            call,
            prep,
            "consent.withdraw",
            json!({"elicitation_id": elicitation}),
            UNARY_TIMEOUT,
        ),
        key: prep.key.clone(),
        db,
    });
}

/// An ask left open by a run-scoped call.
pub(super) struct ParkedAsk {
    call: ConsentCall,
    key: AskKey,
    db: Arc<std::sync::Mutex<crate::db::TrackedConnection>>,
}

/// Withdraw the asks a codemode run left open, once its scope has ended.
pub(super) async fn withdraw_parked(parked: Vec<ParkedAsk>) {
    for ask in parked {
        if let Some(transport) = transport() {
            if let Err(error) = transport.call(ask.call.clone()).await {
                log::warn!("consent.withdraw for an ended run failed: {error:?}");
            }
        }
        let db = Arc::clone(&ask.db);
        let key = ask.key.clone();
        let _ = tokio::task::spawn_blocking(move || {
            if let Ok(conn) = db.lock() {
                let _ = asks::discard(&conn, &key);
            }
        })
        .await;
    }
}

static REATTACHED: LazyLock<StdMutex<HashSet<(String, String, u64)>>> =
    LazyLock::new(|| StdMutex::new(HashSet::new()));

/// The first time this process binds a route for a stamped Alfonso worker
/// scope (see `access_grants::alfonso_worker`), re-send every ask that scope
/// left open under its persisted call key, so Cingulate keeps the same ask;
/// nothing is withdrawn. Approvals claimed by an earlier process are
/// discarded, never reused.
pub(super) fn reattach(
    ctx: &AppContext,
    identity: &RouteIdentity,
    runtime: &tokio::runtime::Handle,
) {
    if !access_grants::alfonso_worker(identity) {
        return;
    }
    let scope = access_grants::scope_key(identity);
    if !REATTACHED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert((scope.owner.clone(), scope.scope_ref.clone(), scope.epoch))
    {
        return;
    }
    let Some(db) = ctx.db() else {
        return;
    };
    let records = {
        let Ok(conn) = db.lock() else {
            return;
        };
        let Ok(records) = asks::for_scope(&conn, &scope) else {
            return;
        };
        for record in records
            .iter()
            .filter(|r| r.call_key.is_none() && !r.holder_is_live())
        {
            let _ = asks::discard(&conn, &record.key);
        }
        records
    };
    let Some(transport) = transport() else {
        return;
    };
    for record in records {
        let (Some(call_key), Some(request)) = (record.call_key.clone(), record.request.clone())
        else {
            continue;
        };
        let Ok(scope) = serde_json::from_value::<ScopeSelector>(record.selector.clone()) else {
            continue;
        };
        let call = ConsentCall {
            scope,
            project_root: identity.project_root.clone(),
            session: identity.session.clone(),
            method: "consent.request",
            params: request,
            timeout: UNARY_TIMEOUT,
        };
        let transport = Arc::clone(&transport);
        let db = Arc::clone(&db);
        runtime.spawn(async move {
            match transport.call(call).await {
                Ok(reply) => {
                    if let Ok(id) = decode_elicitation_id(&reply) {
                        let key = record.key.clone();
                        let _ = tokio::task::spawn_blocking(move || {
                            if let Ok(conn) = db.lock() {
                                let _ = asks::set_elicitation(&conn, &key, &call_key, &id);
                            }
                        })
                        .await;
                    }
                }
                Err(error) => log::warn!("consent reattach failed: {error:?}"),
            }
        });
    }
}

#[cfg(test)]
#[path = "consent_tests.rs"]
mod tests;

// Informational cards never hold a tool call. Serializing their updates keeps
// concurrent probes from publishing two cards or withdrawing an unrecorded id.
static MACOS_CARDS_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn macos_request_params(folder: &str, program: &str, steps: &str) -> Value {
    json!({"kind":"macos_permission","folder":folder,"program":program,"steps":steps})
}

pub(super) async fn macos_permissions(
    db: Arc<StdMutex<crate::db::TrackedConnection>>,
    identity: RouteIdentity,
    observations: Vec<crate::bounded_io::PermissionObservation>,
) -> Result<(), ConsentError> {
    use rusqlite::{params, OptionalExtension};
    if !matches!(
        access_grants::ask_route(&identity),
        AskRoute::Consent { .. }
    ) {
        return Ok(());
    }
    let Some(transport) = transport() else {
        return Err(ConsentError::Unavailable("Cingulate is unavailable".into()));
    };
    let _serial = MACOS_CARDS_LOCK.lock().await;
    let stamp = identity.scope.as_ref().expect("consent route is stamped");
    let program = "ck-aft";
    for observation in observations {
        let folder = observation.folder.to_string_lossy().into_owned();
        let row: Option<(String, String, String, Option<String>)> = {
            let conn = db
                .lock()
                .map_err(|e| ConsentError::Unavailable(e.to_string()))?;
            if !observation.readable {
                // The existing database column is not a wire idempotency key:
                // informational cards use only the folder/program pair.
                conn.execute("INSERT OR IGNORE INTO macos_permission_cards(folder,program,call_key,selector,project_root,session) VALUES (?1,?2,'',?3,?4,?5)", params![folder,program,serde_json::to_string(&selector(stamp)).expect("selector serializes"),identity.project_root.to_string_lossy(),identity.session])
                    .map_err(|e| ConsentError::Unavailable(e.to_string()))?;
            }
            conn.query_row("SELECT selector,project_root,session,elicitation_id FROM macos_permission_cards WHERE folder=?1 AND program=?2", params![folder,program], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()
                .map_err(|e| ConsentError::Unavailable(e.to_string()))?
        };
        let Some((scope, root, session, id)) = row else {
            continue;
        };
        let scope: ScopeSelector =
            serde_json::from_str(&scope).map_err(|e| ConsentError::Unavailable(e.to_string()))?;
        // Cingulate deduplicates informational cards by (folder, program), not
        // call_key. If consent.request lost its response, resending that pair
        // recovers the open elicitation id. A folder that can now be read uses
        // that id to withdraw its existing card.
        let id = match id {
            Some(id) => id,
            None => {
                let params = macos_request_params(&folder, program, "System Settings → Privacy & Security → Files and Folders: allow ck-aft access to this folder, then retry.");
                let result = tokio::time::timeout(
                    UNARY_TIMEOUT,
                    transport.call(ConsentCall {
                        scope: scope.clone(),
                        project_root: PathBuf::from(&root),
                        session: session.clone(),
                        method: "consent.request",
                        params,
                        timeout: UNARY_TIMEOUT,
                    }),
                )
                .await
                .map_err(|_| {
                    ConsentError::Unavailable("macOS permission card request timed out".into())
                })?;
                let result = match result {
                    Err(error @ ConsentError::Refused { .. }) if matches!(&error, ConsentError::Refused { code, .. } if code == "access_macos_only" || code == "access_informational") =>
                    {
                        // access_macos_only and access_informational cannot
                        // produce a permission answer. Remove the unpublished
                        // local row and preserve the named refusal for logging,
                        // without changing the tool's macos_permission_denied result.
                        db.lock().map_err(|e| ConsentError::Unavailable(e.to_string()))?.execute("DELETE FROM macos_permission_cards WHERE folder=?1 AND program=?2 AND elicitation_id IS NULL", params![folder,program])
                            .map_err(|e| ConsentError::Unavailable(e.to_string()))?;
                        return Err(error);
                    }
                    result => result?,
                };
                let id = decode_elicitation_id(&result)?;
                db.lock().map_err(|e| ConsentError::Unavailable(e.to_string()))?.execute("UPDATE macos_permission_cards SET elicitation_id=?3 WHERE folder=?1 AND program=?2", params![folder,program,id])
                    .map_err(|e| ConsentError::Unavailable(e.to_string()))?;
                id
            }
        };
        if observation.readable {
            tokio::time::timeout(
                UNARY_TIMEOUT,
                transport.call(ConsentCall {
                    scope,
                    project_root: root.into(),
                    session,
                    method: "consent.withdraw",
                    params: json!({"elicitation_id":id}),
                    timeout: UNARY_TIMEOUT,
                }),
            )
            .await
            .map_err(|_| {
                ConsentError::Unavailable("macOS permission card withdrawal timed out".into())
            })??;
            db.lock()
                .map_err(|e| ConsentError::Unavailable(e.to_string()))?
                .execute(
                    "DELETE FROM macos_permission_cards WHERE folder=?1 AND program=?2",
                    params![folder, program],
                )
                .map_err(|e| ConsentError::Unavailable(e.to_string()))?;
        }
    }
    Ok(())
}
