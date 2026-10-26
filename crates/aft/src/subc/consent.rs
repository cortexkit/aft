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
//! Once an answer arrives, local application gets a separate bounded wait;
//! exhausting that wait asks for an immediate retry, not another notification
//! from Cingulate.
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
use crate::access_policy::{
    AccessGrant, AccessOperation, AccessPolicy, AskRoute, GrantKind, HeadPolicy,
};
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
pub(super) const RERUN_MARGIN: Duration = Duration::from_secs(5);
/// No call is held longer than this, whatever its reply budget.
const MAX_HOLD: Duration = Duration::from_secs(100);
/// How often a held call checks for cancellation, route close and scope end.
const SIGNAL_POLL: Duration = Duration::from_millis(100);
/// Bound on one `consent.request` or `consent.withdraw` round trip.
const UNARY_TIMEOUT: Duration = Duration::from_secs(10);
/// A received answer must not depend on a second notification from Cingulate.
const SETTLED_WAIT: Duration = Duration::from_secs(5);
const ACCESS_RETRY_MESSAGE: &str = "The permission answer has been received but is still being applied. Retry this same call now; do not wait for another resume.";

fn settled_wait_budget() -> Duration {
    #[cfg(test)]
    {
        let ms = tests::SETTLED_WAIT_OVERRIDE_MS.load(Ordering::SeqCst);
        if ms > 0 {
            return Duration::from_millis(ms);
        }
    }
    SETTLED_WAIT
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ConsentError {
    /// Cingulate or the daemon could not be reached.
    Unavailable(String),
    /// Cingulate answered with a named refusal.
    Refused { code: String, message: String },
}

pub(super) type ConsentFuture = Pin<Box<dyn Future<Output = Result<Value, ConsentError>> + Send>>;
pub(super) type ConsentReceipt = Box<dyn FnOnce(Value) -> ConsentFuture + Send>;

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

    /// Apply a reply before asynchronous transport cleanup can release or
    /// delay the held call. Simple transports have no separate cleanup phase.
    fn call_with_receipt(&self, call: ConsentCall, receipt: ConsentReceipt) -> ConsentFuture {
        let reply = self.call(call);
        Box::pin(async move { receipt(reply.await?).await })
    }
}

static TRANSPORT: LazyLock<StdMutex<Option<Arc<dyn ConsentTransport>>>> =
    LazyLock::new(|| StdMutex::new(None));

#[cfg(test)]
static TEST_TRANSPORT: LazyLock<StdMutex<Option<Arc<dyn ConsentTransport>>>> =
    LazyLock::new(|| StdMutex::new(None));

/// Install the production transport for this process's daemon connection.
pub(super) fn install_subc_transport(connection_file: &Path) {
    let transport: Arc<dyn ConsentTransport> = Arc::new(SharedSubcConsent(Arc::new(
        SubcConsentTransport::new(connection_file.to_path_buf()),
    )));
    *TRANSPORT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::clone(&transport));
    super::secret_consent::install_subc_transport(transport);
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
    routes: StdMutex<HashMap<ConsentRouteKey, ConsentRouteEntry>>,
}

// subc-client-rs 0.28.0 consumer.rs: RouteKey and ScopeKey include the bind
// identity and every selector field. Target, consumer identity, capabilities
// and role versions are fixed for this transport's consent opens.
#[derive(Clone, Hash, PartialEq, Eq)]
struct ConsentRouteKey {
    project_root: PathBuf,
    harness: String,
    session: String,
    scope_owner: String,
    scope_ref: String,
    scope_epoch: Option<u64>,
}

impl ConsentRouteKey {
    fn new(identity: &subc_protocol::BindIdentity, scope: &ScopeSelector) -> Self {
        Self {
            project_root: identity.project_root.clone(),
            harness: identity.harness.clone(),
            session: identity.session.clone(),
            scope_owner: serde_json::to_string(&scope.owner)
                .unwrap_or_else(|_| format!("{:?}", scope.owner)),
            scope_ref: scope.scope_ref.clone(),
            scope_epoch: scope.scope_epoch,
        }
    }
}

#[derive(Default)]
struct ConsentRouteState {
    gate: tokio::sync::Mutex<()>,
    leases: StdMutex<HashMap<subc_client_rs::RouteHandle, usize>>,
}

#[derive(Default)]
struct ConsentRouteEntry {
    state: Arc<ConsentRouteState>,
    users: usize,
}

struct ConsentRouteIdentity {
    transport: Arc<SubcConsentTransport>,
    key: ConsentRouteKey,
    state: Arc<ConsentRouteState>,
}

impl Drop for ConsentRouteIdentity {
    fn drop(&mut self) {
        let mut routes = self
            .transport
            .routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Queued opens retain the state too: removing it while a caller waits
        // would let a new caller use a different lock for the same cached handle.
        let entry = routes.get_mut(&self.key).expect("registered consent route");
        entry.users -= 1;
        if entry.users == 0 {
            routes.remove(&self.key);
        }
    }
}

struct ConsentRoute {
    consumer: Arc<subc_client_rs::SubcConsumer>,
    route: Option<(subc_client_rs::RouteHandle, ConsentRouteIdentity)>,
    runtime: tokio::runtime::Handle,
}

impl ConsentRoute {
    async fn close(&mut self) {
        if let Some((route, identity)) = self.route.take() {
            let consumer = Arc::clone(&self.consumer);
            // Finish decrement and close even if the consent call is cancelled.
            let _ = self
                .runtime
                .spawn(async move { identity.release_route(&consumer, route).await })
                .await;
        }
    }
}

impl Drop for ConsentRoute {
    fn drop(&mut self) {
        if let Some((route, identity)) = self.route.take() {
            let consumer = Arc::clone(&self.consumer);
            // A held consent.await can be cancelled when its asking route closes.
            self.runtime
                .spawn(async move { identity.release_route(&consumer, route).await });
        }
    }
}

impl SubcConsentTransport {
    fn new(connection_file: PathBuf) -> Self {
        Self {
            connection_file,
            consumer: tokio::sync::Mutex::new(None),
            routes: StdMutex::new(HashMap::new()),
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
}

impl ConsentRouteIdentity {
    async fn release_route(
        &self,
        consumer: &subc_client_rs::SubcConsumer,
        route: subc_client_rs::RouteHandle,
    ) {
        let _gate = self.state.gate.lock().await;
        let last = {
            let mut leases = self
                .state
                .leases
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(users) = leases.get_mut(&route) {
                *users -= 1;
                if *users == 0 {
                    leases.remove(&route);
                    true
                } else {
                    false
                }
            } else {
                false
            }
        };
        if last {
            let _ = consumer
                .close_handle(&route, subc_client_rs::CloseRouteOptions::default())
                .await;
        }
    }
}

impl SubcConsentTransport {
    fn route_identity(self: &Arc<Self>, key: ConsentRouteKey) -> ConsentRouteIdentity {
        let mut routes = self
            .routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = routes.entry(key.clone()).or_default();
        entry.users += 1;
        ConsentRouteIdentity {
            transport: Arc::clone(self),
            key,
            state: Arc::clone(&entry.state),
        }
    }

    async fn send(self: Arc<Self>, call: ConsentCall) -> Result<Value, ConsentError> {
        self.send_with_receipt(call, None).await
    }

    async fn send_with_receipt(
        self: Arc<Self>,
        call: ConsentCall,
        receipt: Option<ConsentReceipt>,
    ) -> Result<Value, ConsentError> {
        use subc_client_rs::{CallError, CallOptions};
        let body = encode(call.method, &call.params)?;
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
        // The SDK may return the same cached handle for overlapping calls with
        // the same scope stamp. Lease it until the last call settles, then close
        // immediately rather than leaving an idle consent route in the daemon.
        let route_identity = self.route_identity(ConsentRouteKey::new(&identity, &call.scope));
        let gate = route_identity.state.gate.lock().await;
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
                drop(gate);
                self.forget(&consumer).await;
                return Err(ConsentError::Unavailable(error.to_string()));
            }
        };
        *route_identity
            .state
            .leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(route)
            .or_default() += 1;
        drop(gate);
        let mut lease = ConsentRoute {
            consumer: Arc::clone(&consumer),
            route: Some((route, route_identity)),
            runtime: tokio::runtime::Handle::current(),
        };
        let options = CallOptions {
            timeout: call.timeout,
            ..CallOptions::default()
        };
        let reply = consumer.request(&route, body, options).await;
        let result = match reply {
            Ok(bytes) => decode_result(&bytes),
            Err(CallError::Module(body)) => Err(ConsentError::Refused {
                code: body.code,
                message: body.message,
            }),
            Err(error) => {
                self.forget(&consumer).await;
                Err(ConsentError::Unavailable(error.to_string()))
            }
        };
        // A settled scope answer must be durable before closing the route:
        // close_handle can wait while another tool is already checking access.
        let result = match (result, receipt) {
            (Ok(record), Some(receipt)) => receipt(record).await,
            (result, _) => result,
        };
        lease.close().await;
        result
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

    fn call_with_receipt(&self, call: ConsentCall, receipt: ConsentReceipt) -> ConsentFuture {
        Box::pin(Arc::clone(&self.0).send_with_receipt(call, Some(receipt)))
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
    pub(super) fn new(route: RouteChannel) -> Self {
        let closed = Arc::new(AtomicBool::new(false));
        HELD.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(route)
            .or_default()
            .push(Arc::clone(&closed));
        Self { route, closed }
    }

    pub(super) fn closed(&self) -> bool {
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
#[derive(Clone)]
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
    /// Run the call again with these capabilities approved once. `claim`
    /// owns them until the call finishes.
    Run {
        once: Vec<AccessGrant>,
        claim: Arc<ClaimedAsk>,
    },
    /// End the call with this response.
    Answer(Response),
    /// An overlapping call owns the one-time approval; reuse its result only.
    Shared((Response, Option<String>)),
    /// Nobody waits for this call any more (cancelled, route closed, transport
    /// gone); the caller sends nothing.
    Abandoned,
}

#[derive(Clone)]
struct SharedReply {
    success: bool,
    data: Value,
    text: Option<String>,
}

/// Tracks only overlapping dispatches, not a cache of reusable approvals.
struct SettlementFlight {
    root: PathBuf,
    session: String,
    binding: Value,
    key: AskKey,
    member: Arc<()>,
    group: Arc<SettlementGroup>,
}

/// Members retain the result while they overlap, even after its publisher exits.
/// The owner token identifies the only publisher without retaining its flight.
struct SettlementGroup {
    owner: StdMutex<Option<Arc<()>>>,
    settled: AtomicBool,
    done: tokio::sync::watch::Sender<Option<Option<SharedReply>>>,
}

static SETTLEMENT_FLIGHTS: LazyLock<StdMutex<Vec<std::sync::Weak<SettlementFlight>>>> =
    LazyLock::new(|| StdMutex::new(Vec::new()));

struct SettlementOwner(Arc<SettlementFlight>);

impl SettlementOwner {
    fn new(call: &HeldCall) -> Self {
        let mut flight = SettlementFlight {
            root: call.identity.root.clone().into(),
            session: call.identity.session.clone(),
            binding: serde_json::to_value(&call.identity.scope).expect("scope stamp is JSON"),
            key: AskKey {
                scope: access_grants::scope_key(&call.identity),
                args_hash: args_hash(&call.identity, &call.tool, &call.arguments),
            },
            member: Arc::new(()),
            group: Arc::new(SettlementGroup {
                owner: StdMutex::new(None),
                settled: AtomicBool::new(false),
                done: tokio::sync::watch::channel(None).0,
            }),
        };
        let mut flights = SETTLEMENT_FLIGHTS.lock().unwrap();
        let retain_overlap = true;
        #[cfg(test)]
        let retain_overlap =
            retain_overlap && tests::SETTLED_GROUP_RETENTION_ENABLED.load(Ordering::SeqCst);
        if retain_overlap {
            if let Some(other) = flights
                .iter()
                .filter_map(std::sync::Weak::upgrade)
                .find(|other| {
                    other.root == flight.root
                        && other.session == flight.session
                        && other.binding == flight.binding
                        && other.key == flight.key
                    // A published group belongs to calls that overlapped it; a call that
                    // arrives after publication needs its own approval.
                    && other.group.done.borrow().is_none()
                })
            {
                // Join before either receipt arrives. Looking up only settled
                // flights can miss a publisher that already returned and dropped.
                flight.group = Arc::clone(&other.group);
            }
        }
        let flight = Arc::new(flight);
        flights.push(Arc::downgrade(&flight));
        Self(flight)
    }

    fn complete(&self, result: &Option<(Response, Option<String>)>) {
        if !self.0.owns_group() {
            return;
        }
        self.0
            .group
            .done
            .send_replace(Some(result.as_ref().map(|(response, text)| SharedReply {
                success: response.success,
                data: response.data.clone(),
                text: text.clone(),
            })));
    }
}

impl Drop for SettlementOwner {
    fn drop(&mut self) {
        if self.0.owns_group() && self.0.group.done.borrow().is_none() {
            self.0.group.done.send_replace(Some(None));
        }
        SETTLEMENT_FLIGHTS.lock().unwrap().retain(|flight| {
            flight
                .upgrade()
                .is_some_and(|flight| !Arc::ptr_eq(&flight, &self.0))
        });
    }
}

impl SettlementFlight {
    fn owns_group(&self) -> bool {
        self.group
            .owner
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|owner| Arc::ptr_eq(owner, &self.member))
    }

    /// Elect one application owner even if two open awaits receive the answer.
    fn other_settled(self: &Arc<Self>, receiving: bool) -> Option<Arc<SettlementGroup>> {
        #[cfg(test)]
        if !tests::SETTLED_JOIN_ENABLED.load(Ordering::SeqCst) {
            return None;
        }
        let mut owner = self.group.owner.lock().unwrap();
        if let Some(publisher) = owner.as_ref() {
            if !Arc::ptr_eq(publisher, &self.member) {
                return (receiving
                    || self.group.settled.load(Ordering::SeqCst)
                    || self.group.done.borrow().is_some())
                .then(|| Arc::clone(&self.group));
            }
        } else if receiving {
            *owner = Some(Arc::clone(&self.member));
        }
        if receiving {
            self.group.settled.store(true, Ordering::SeqCst);
        }
        None
    }
}

impl SettlementGroup {
    async fn result(
        &self,
        call: &HeldCall,
        grant: &AccessGrant,
        route_hold: &RouteHold,
    ) -> Option<(Response, Option<String>)> {
        let mut done = self.done.subscribe();
        #[cfg(test)]
        tests::settled_waiter_joined();
        let wait = settled_wait_budget().min(
            call.reply_deadline
                .saturating_duration_since(Instant::now()),
        );
        let waiting = tokio::time::timeout(wait, async {
            loop {
                if let Some(result) = done.borrow_and_update().clone() {
                    break result;
                }
                if done.changed().await.is_err() {
                    break None;
                }
            }
        });
        tokio::pin!(waiting);
        let result = loop {
            tokio::select! {
                result = &mut waiting => break result,
                _ = tokio::time::sleep(SIGNAL_POLL) => match stop_signal(call, route_hold) {
                    Some(Stop::Detach | Stop::Withdraw) => return None,
                    Some(Stop::RunEnded) => return Some((refused_answer(call, grant, "access_withdrawn", None), None)),
                    None => {}
                }
            }
        };
        match result {
            Ok(Some(reply)) => Some((
                Response {
                    id: call.request_id.clone(),
                    success: reply.success,
                    data: reply.data,
                },
                reply.text,
            )),
            _ => {
                crate::log_ctx::with_session(Some(call.identity.session.clone()), || {
                    crate::slog_warn!("consent settled result wait ended without completion");
                });
                Some((retry_answer(call, grant), None))
            }
        }
    }
}

/// Keep ownership of an approval from the blocking claim through its rerun.
/// Cancellation consumes an unused claim; hold expiry can return a one-time
/// answer to its original ask so an identical retry does not need approval again.
struct ClaimedAsk {
    db: Arc<StdMutex<crate::db::TrackedConnection>>,
    key: AskKey,
    holder: String,
    armed: AtomicBool,
    retry_after_deadline: Arc<AtomicBool>,
    original: Option<Prepared>,
    elicitation: String,
}

impl ClaimedAsk {
    fn cleanup(&self) -> impl FnOnce() + Send + 'static {
        let db = Arc::clone(&self.db);
        let key = self.key.clone();
        let holder = self.holder.clone();
        let original = self.original.clone();
        let elicitation = self.elicitation.clone();
        let retry = self.retry_after_deadline.load(Ordering::SeqCst);
        move || match db.lock() {
            Ok(conn) => {
                let cleaned = match original.filter(|_| retry) {
                    Some(original) => asks::release_claim(
                        &conn,
                        &key,
                        &holder,
                        &original.call_key,
                        Some(&elicitation),
                        &original.request,
                        &original.grant,
                    ),
                    None => asks::consume(&conn, &key, &holder),
                };
                if let Err(error) = cleaned {
                    crate::slog_warn!("consent approval cleanup failed: {error}");
                }
            }
            Err(_) => crate::slog_warn!("consent approval cleanup failed: database mutex poisoned"),
        }
    }

    async fn clear(self: Arc<Self>) {
        if self.armed.swap(false, Ordering::SeqCst) {
            self.retry_after_deadline.store(false, Ordering::SeqCst);
            // The cleanup task outlives this await if its caller is cancelled.
            if let Err(error) = tokio::task::spawn_blocking(self.cleanup()).await {
                crate::slog_warn!("consent approval cleanup task failed: {error}");
            }
        }
    }
}

impl Drop for ClaimedAsk {
    fn drop(&mut self) {
        if self.armed.load(Ordering::SeqCst) {
            let cleanup = self.cleanup();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                let _ = runtime.spawn_blocking(cleanup);
            } else {
                cleanup();
            }
        }
    }
}

/// Run a stamped worker's call, holding it on Cingulate whenever the access
/// check asks. `first` is the call's first response; `rerun` runs it again
/// with approved capabilities installed. Returns the final response with its
/// formatted text when the run produced one, or `None` when nobody waits.
pub(super) async fn run_with_consent<F, Fut>(
    call: HeldCall,
    first: (Response, String),
    rerun: F,
) -> Option<(Response, Option<String>)>
where
    F: FnMut(AskRoute) -> Fut,
    Fut: Future<Output = (Response, String)>,
{
    if !matches!(
        access_grants::ask_route(&call.identity),
        AskRoute::Consent { .. }
    ) {
        return Some((
            Response::error(
                &call.request_id,
                "path_outside_root",
                "outside-path access needs a permission-capable route",
            ),
            None,
        ));
    }
    let Some(grant) = requested_ask(&first.0) else {
        return Some((first.0, Some(first.1)));
    };
    let owner = SettlementOwner::new(&call);
    let result = run_held(&call, grant, rerun, &owner.0).await;
    owner.complete(&result);
    result
}

async fn run_held<F, Fut>(
    call: &HeldCall,
    mut grant: AccessGrant,
    mut rerun: F,
    flight: &Arc<SettlementFlight>,
) -> Option<(Response, Option<String>)>
where
    F: FnMut(AskRoute) -> Fut,
    Fut: Future<Output = (Response, String)>,
{
    let route_hold = RouteHold::new(call.route);
    if let Some(other) = flight.other_settled(false) {
        return other.result(call, &grant, &route_hold).await;
    }
    let deadline = (Instant::now() + hold_budget(None)).min(
        call.reply_deadline
            .checked_sub(RERUN_MARGIN)
            .unwrap_or_else(Instant::now),
    );
    let mut holder: Option<Arc<ClaimedAsk>> = None;
    loop {
        let own = holder.as_ref().map(|claim| claim.holder.clone());
        match hold(call, grant, deadline, &route_hold, own, flight).await {
            Hold::Run { once, claim } => {
                claim.retry_after_deadline.store(false, Ordering::SeqCst);
                holder = Some(claim);
                let running = rerun(AskRoute::Consent {
                    once,
                    launch: super::access_grants::launch_key(&call.identity, "worker"),
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
                                if let Some(claim) = holder.take() {
                                    claim.clear().await;
                                }
                                return None;
                            }
                        }
                    }
                };
                match requested_ask(&response) {
                    Some(next) => {
                        // A new outside path has its own unanswered question.
                        flight.group.settled.store(false, Ordering::SeqCst);
                        grant = next;
                    }
                    None => {
                        if let Some(claim) = holder.take() {
                            // The approvals are used up by this run, whatever it returned.
                            claim.clear().await;
                        }
                        return Some((response, Some(text)));
                    }
                }
            }
            Hold::Answer(response) => return Some((response, None)),
            Hold::Shared(result) => return Some(result),
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
    with_db_handle(call, move |db, ctx| {
        let conn = db.lock().map_err(|_| POISONED.to_string())?;
        work(&conn, ctx).map_err(|error| error.to_string())
    })
    .await
}

const POISONED: &str = "aft.db mutex is poisoned";

/// Run `work` on a blocking thread with the aft.db handle unlocked, for work
/// that takes the lock itself, possibly more than once.
async fn with_db_handle<T: Send + 'static>(
    call: &HeldCall,
    work: impl FnOnce(&std::sync::Mutex<crate::db::TrackedConnection>, &AppContext) -> Result<T, String>
        + Send
        + 'static,
) -> Result<T, String> {
    let Some(ctx) = call.executor.actor_context(&call.identity.root) else {
        return Err("executor actor is not registered".into());
    };
    tokio::task::spawn_blocking(move || {
        let db = ctx
            .db()
            .ok_or_else(|| "aft.db is unavailable".to_string())?;
        work(&db, &ctx)
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
    ScopeSelector::new(
        stamp.owner.clone(),
        stamp.scope_ref.clone(),
        Some(stamp.scope_epoch),
    )
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
fn head_policy(head: Option<&AccessPolicy>, grant: &AccessGrant) -> HeadPolicy {
    head.map_or(HeadPolicy::NotHeld, |policy| policy.head_policy(grant))
}

/// Run `commit` under the aft.db lock with the worker's head policy, when
/// `needed`, resolved on disk while the lock is released.
fn with_head_policy<T>(
    db: &std::sync::Mutex<crate::db::TrackedConnection>,
    ctx: &AppContext,
    identity: &RouteIdentity,
    needed: bool,
    commit: impl FnOnce(&rusqlite::Connection, Option<AccessPolicy>) -> rusqlite::Result<T>,
) -> Result<T, String> {
    let storage = ctx.storage_dir();
    access_grants::resolve_unlocked(
        db,
        |conn| {
            if needed {
                access_grants::consent_head_snapshot(conn, identity)
                    .map_err(|error| error.to_string())
            } else {
                Ok(access_grants::HeadSnapshot::default())
            }
        },
        |head| head.resolve(&storage),
        |conn, head| commit(conn, head).map_err(|error| error.to_string()),
        || POISONED.to_string(),
    )
}

fn new_call_key() -> String {
    let mut id = [0u8; 16];
    getrandom::fill(&mut id).expect("operating system random source for consent call keys");
    let hex: String = id.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("aft-ask-{hex}")
}

/// Prepare an ask. A worker's ask carries its head's policy, which is resolved
/// on disk with the aft.db lock released; the ask's records are read and
/// written under the lock.
#[allow(clippy::too_many_arguments)]
fn prepare(
    db: &std::sync::Mutex<crate::db::TrackedConnection>,
    ctx: &AppContext,
    identity: &RouteIdentity,
    tool: &str,
    arguments: &Value,
    grant: &AccessGrant,
    directory_scope: &AccessGrant,
    own_holder: Option<&str>,
) -> Result<Prep, String> {
    let worker = identity
        .scope
        .as_ref()
        .is_some_and(|stamp| stamp.kind == subc_protocol::scope::ScopeKind::Worker);
    with_head_policy(db, ctx, identity, worker, |conn, head| {
        prepare_locked(
            conn,
            head.as_ref(),
            identity,
            tool,
            arguments,
            grant,
            directory_scope,
            own_holder,
        )
    })
}

/// `head_access` is the worker's resolved head policy; it is only read for
/// asks from a `worker` scope.
#[allow(clippy::too_many_arguments)]
fn prepare_locked(
    conn: &rusqlite::Connection,
    head_access: Option<&AccessPolicy>,
    identity: &RouteIdentity,
    tool: &str,
    arguments: &Value,
    grant: &AccessGrant,
    directory_scope: &AccessGrant,
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
    let mut scope_grant = directory_scope.clone();
    // `head_policy` says whether the worker's head itself holds the requested
    // access, which decides whether the head may allow it. Cingulate requires
    // it on asks from a `worker` scope and refuses it on every other kind.
    let head = match identity.scope.as_ref().map(|stamp| stamp.kind) {
        Some(subc_protocol::scope::ScopeKind::Worker) => {
            let file_head = head_policy(head_access, grant);
            // A head can approve a file without holding its containing tree.
            // Advertise that tree only when the head can delegate both choices.
            if file_head != HeadPolicy::Allow
                || (scope_grant != *grant
                    && head_policy(head_access, &scope_grant) != HeadPolicy::Allow)
            {
                scope_grant = grant.clone();
            }
            Some(file_head)
        }
        _ => None,
    };
    let path = grant.path.to_string_lossy();
    let request = request_params(
        &call_key,
        tool,
        operation_name(grant.operation),
        &path,
        scope_reason(&scope_grant),
        &key.args_hash,
        &pattern(&scope_grant),
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

const READ_TREE_SCOPE_REASON: &str = "Allow scope: read-only access for all AFT read tools within the displayed directory tree. Protected paths remain denied.";
const READ_FILE_SCOPE_REASON: &str = "Allow scope: read-only access for all AFT read tools to the displayed file only. Protected paths remain denied.";
const WRITE_TREE_SCOPE_REASON: &str = "Allow scope: read/write access for all AFT file tools within the displayed directory tree. Protected paths remain denied.";
const WRITE_FILE_SCOPE_REASON: &str = "Allow scope: read/write access for all AFT file tools to the displayed file only. Protected paths remain denied.";

fn scope_reason(grant: &AccessGrant) -> &'static str {
    match (grant.operation, grant.kind) {
        (AccessOperation::Read, GrantKind::Subtree) => READ_TREE_SCOPE_REASON,
        (AccessOperation::Read, GrantKind::File) => READ_FILE_SCOPE_REASON,
        (AccessOperation::Write, GrantKind::Subtree) => WRITE_TREE_SCOPE_REASON,
        (AccessOperation::Write, GrantKind::File) => WRITE_FILE_SCOPE_REASON,
    }
}

/// Offer a file ask's immediate directory, for reads and writes alike, only
/// when that directory passes `probe_directory_scope_with`: it is not the home
/// folder, a home ancestor, a root-level or volume-level folder, or a protected
/// system tree (including nested system folders). macOS per-user temporary
/// trees remain eligible. Unknown home/volume identity stays file-only.
pub(crate) fn directory_scope(grant: &AccessGrant) -> AccessGrant {
    directory_scope_with(grant, directory_is_root_alias)
}

const DIRECTORY_SCOPE_PROBE_BUDGET: Duration = Duration::from_millis(250);
#[cfg(test)]
static DIRECTORY_SCOPE_PROBE_OVERRIDE_MS: AtomicU64 = AtomicU64::new(0);

fn directory_scope_probe_budget() -> Duration {
    #[cfg(test)]
    {
        let ms = DIRECTORY_SCOPE_PROBE_OVERRIDE_MS.load(Ordering::SeqCst);
        if ms > 0 {
            return Duration::from_millis(ms);
        }
    }
    DIRECTORY_SCOPE_PROBE_BUDGET
}

const DIRECTORY_SCOPE_MEMO_TTL: Duration = Duration::from_secs(60);
type DirectoryScopeAnswer = Result<bool, &'static str>;

#[derive(Default)]
struct DirectoryScopeProbe {
    answer: std::sync::Mutex<Option<(Instant, DirectoryScopeAnswer)>>,
    ready: std::sync::Condvar,
}

impl DirectoryScopeProbe {
    fn reusable(&self) -> bool {
        self.answer
            .lock()
            .unwrap()
            .as_ref()
            .is_none_or(|(at, answer)| answer.is_ok() && at.elapsed() < DIRECTORY_SCOPE_MEMO_TTL)
    }

    fn wait(&self, deadline: Instant) -> DirectoryScopeAnswer {
        let answer = self.answer.lock().unwrap();
        let (answer, _) = self
            .ready
            .wait_timeout_while(
                answer,
                deadline.saturating_duration_since(Instant::now()),
                |answer| answer.is_none(),
            )
            .unwrap();
        answer
            .filter(|(at, _)| *at <= deadline)
            .map(|(_, answer)| answer)
            .unwrap_or(Err("shared directory probe timed out"))
    }

    fn finish(&self, result: DirectoryScopeAnswer) {
        *self.answer.lock().unwrap() = Some((Instant::now(), result));
        self.ready.notify_all();
    }
}

#[derive(Default)]
struct DirectoryScopeProbes {
    probes: HashMap<PathBuf, Arc<DirectoryScopeProbe>>,
    logged: HashMap<(PathBuf, &'static str), Instant>,
    last_pruned: Option<Instant>,
}

impl DirectoryScopeProbes {
    fn get(&mut self, directory: &Path) -> (Arc<DirectoryScopeProbe>, bool) {
        if self
            .last_pruned
            .is_none_or(|at| at.elapsed() >= DIRECTORY_SCOPE_MEMO_TTL)
        {
            self.probes.retain(|_, probe| probe.reusable());
            self.logged
                .retain(|_, at| at.elapsed() < DIRECTORY_SCOPE_MEMO_TTL);
            self.last_pruned = Some(Instant::now());
        }
        // Ask grants already contain canonical policy identities. Do not resolve
        // the candidate again outside the bounded filesystem helper.
        if let Some(probe) = self.probes.get(directory).filter(|probe| probe.reusable()) {
            return (Arc::clone(probe), false);
        }
        let probe = Arc::new(DirectoryScopeProbe::default());
        self.probes
            .insert(directory.to_path_buf(), Arc::clone(&probe));
        (probe, true)
    }

    fn note_fallback(&mut self, directory: &Path, reason: &'static str) -> bool {
        match self.logged.entry((directory.to_path_buf(), reason)) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                if entry.get().elapsed() < DIRECTORY_SCOPE_MEMO_TTL {
                    return false;
                }
                entry.insert(Instant::now());
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(Instant::now());
            }
        }
        true
    }
}

static DIRECTORY_SCOPE_PROBES: LazyLock<std::sync::Mutex<DirectoryScopeProbes>> =
    LazyLock::new(|| std::sync::Mutex::new(DirectoryScopeProbes::default()));

fn directory_scope_with<F>(grant: &AccessGrant, root_alias_check: F) -> AccessGrant
where
    F: Fn(&Path, &Path) -> Option<bool> + Send + Sync + 'static,
{
    let deadline = Instant::now() + directory_scope_probe_budget();
    if grant.kind != GrantKind::File {
        return grant.clone();
    }
    let Some(parent) = grant.path.parent() else {
        return grant.clone();
    };
    // One eligibility answer per directory serves read and write asks alike:
    // the directory either may be offered as a whole or it may not.
    let (probe, leader) = DIRECTORY_SCOPE_PROBES.lock().unwrap().get(parent);
    let answer = if leader {
        let result = if let Some(permit) = DIRECTORY_SCOPE_PROBE_LIMITER.acquire_until(deadline) {
            // Keep the slot with filesystem work even after its caller times
            // out, so stalled probes cannot exhaust the shared helper pool.
            crate::bounded_io::run(parent, Some(deadline), move |directory| {
                let _permit = permit;
                Ok(probe_directory_scope_with(&directory, root_alias_check))
            })
            .unwrap_or(Err(
                "directory probe timed out or filesystem helper unavailable",
            ))
        } else {
            Err("directory probe permit timed out")
        };
        // Only an answer delivered within this request's budget is memoized;
        // a helper completing after timeout cannot populate the memo later.
        let result = if Instant::now() <= deadline {
            result
        } else {
            Err("directory probe timed out")
        };
        probe.finish(result);
        result
    } else {
        probe.wait(deadline)
    };
    match answer {
        Ok(true) => AccessGrant {
            path: parent.to_path_buf(),
            kind: GrantKind::Subtree,
            operation: grant.operation,
        },
        Ok(false) => grant.clone(),
        Err(reason) => {
            let should_log = DIRECTORY_SCOPE_PROBES
                .lock()
                .unwrap()
                .note_fallback(parent, reason);
            if should_log {
                log::warn!(
                    "consent directory scope fell back to file-only for {}: {reason}",
                    parent.display()
                );
            }
            grant.clone()
        }
    }
}

async fn scope_before_db(grant: &AccessGrant) -> AccessGrant {
    let probing = grant.clone();
    // A bounded helper still waits for its deadline. Keep that wait off the
    // async executor and outside the database mutex used for head-policy checks.
    tokio::task::spawn_blocking(move || directory_scope(&probing))
        .await
        .unwrap_or_else(|_| grant.clone())
}

fn probe_directory_scope_with(
    parent: &Path,
    root_alias_check: impl Fn(&Path, &Path) -> Option<bool>,
) -> DirectoryScopeAnswer {
    let home = crate::environment::non_empty_os_var("HOME")
        .or_else(|| crate::environment::non_empty_os_var("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .map(|path| {
            #[cfg(all(test, unix))]
            crate::bounded_io::stall_helper_if_requested(&path);
            crate::access_policy::policy_path(&path)
        })
        .ok_or("home identity unknown")?;
    // statfs and stat fail on a directory that is gone, such as task scratch
    // that cleanup removed after its command exited. Name that cause instead
    // of reporting it as an unknown volume.
    if matches!(std::fs::symlink_metadata(parent), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
    {
        return Err("directory does not exist");
    }
    let (volume, _) =
        crate::storage_retention::volume_anchor(parent).ok_or("volume identity unknown")?;
    let volume = crate::access_policy::policy_path(&volume);
    let root = parent.ancestors().last().unwrap_or(parent);
    if !directory_scope_is_narrow(parent, &home, &volume) {
        return Ok(false);
    }
    if directory_is_protected_alias(parent, &home, &volume)
        .ok_or("protected alias identity unknown")?
    {
        return Ok(false);
    }
    if root_alias_check(parent, root).ok_or("root alias identity unknown or scan unavailable")? {
        return Ok(false);
    }
    if root != volume
        && root_alias_check(parent, &volume)
            .ok_or("volume alias identity unknown or scan unavailable")?
    {
        return Ok(false);
    }
    Ok(true)
}

use crate::path_identity::{directory_is_protected_alias, directory_scope_is_narrow};

/// A canonical directory can still be a root-level alias, such as /etc pointing
/// to /private/etc. Only resolvable aliases have a target identity to compare.
/// Reverse aliases cannot be recovered from the candidate's canonical path.
/// Give their enumeration a separate helper and one process-wide slot: unknown
/// or busy scans must never widen a file grant.
const ROOT_ALIAS_SCAN_BUDGET: Duration = Duration::from_millis(250);

struct ScopeProbeLimiter {
    in_flight: Arc<std::sync::atomic::AtomicBool>,
    available: Arc<(std::sync::Mutex<()>, std::sync::Condvar)>,
}

impl ScopeProbeLimiter {
    fn new() -> Self {
        Self {
            in_flight: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            available: Arc::default(),
        }
    }

    fn try_acquire(&self) -> Option<ScopeProbePermit> {
        self.in_flight
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .ok()
            .map(|_| ScopeProbePermit(Arc::clone(&self.in_flight), Arc::clone(&self.available)))
    }

    fn acquire_until(&self, deadline: Instant) -> Option<ScopeProbePermit> {
        let (mutex, available) = &*self.available;
        let mut guard = mutex.lock().unwrap();
        loop {
            let remaining = deadline.checked_duration_since(Instant::now())?;
            if let Some(permit) = self.try_acquire() {
                return Some(permit);
            }
            (guard, _) = available.wait_timeout(guard, remaining).unwrap();
        }
    }

    fn scan<F>(&self, scan: F) -> Option<bool>
    where
        F: FnOnce() -> Option<bool> + Send + 'static,
    {
        let permit = self.try_acquire()?;

        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let spawned = std::thread::Builder::new()
            .name("aft-root-alias-scan".into())
            .spawn(move || {
                let result = scan();
                drop(permit);
                let _ = sender.send(result);
            });
        // A failed spawn drops the closure and its permit, which releases the
        // slot; clearing it here too could release a newer caller's slot.
        if spawned.is_err() {
            return None;
        }

        receiver.recv_timeout(ROOT_ALIAS_SCAN_BUDGET).ok().flatten()
    }
}

struct ScopeProbePermit(
    Arc<std::sync::atomic::AtomicBool>,
    Arc<(std::sync::Mutex<()>, std::sync::Condvar)>,
);

impl Drop for ScopeProbePermit {
    fn drop(&mut self) {
        let (mutex, available) = &*self.1;
        let _guard = mutex.lock().unwrap();
        self.0.store(false, std::sync::atomic::Ordering::Release);
        available.notify_all();
    }
}

static DIRECTORY_SCOPE_PROBE_LIMITER: LazyLock<ScopeProbeLimiter> =
    LazyLock::new(ScopeProbeLimiter::new);
static ROOT_ALIAS_SCAN_LIMITER: LazyLock<ScopeProbeLimiter> = LazyLock::new(ScopeProbeLimiter::new);

fn directory_is_root_alias(directory: &Path, root: &Path) -> Option<bool> {
    let directory = directory.to_path_buf();
    let root = root.to_path_buf();
    ROOT_ALIAS_SCAN_LIMITER.scan(move || scan_root_alias(&directory, &root))
}

fn scan_root_alias(directory: &Path, root: &Path) -> Option<bool> {
    for entry in std::fs::read_dir(root).ok()? {
        let entry = entry.ok()?;
        if entry.file_type().ok()?.is_symlink() {
            let Ok(target) = entry.path().canonicalize() else {
                continue;
            };
            if crate::access_policy::policy_path(&target) == directory {
                return Some(true);
            }
        }
    }
    Some(false)
}

/// Settle the persisted card, not a fresh probe that can time out minutes after
/// the directory was offered. Older file-only cards must stay file-only.
fn advertised_scope(prep: &Prepared) -> Option<AccessGrant> {
    if prep.request.get("kind").and_then(Value::as_str) != Some("tool_permission")
        || prep.request.get("operation").and_then(Value::as_str)
            != Some(operation_name(prep.grant.operation))
    {
        return None;
    }
    let advertised = prep.request.get("pattern").and_then(Value::as_str)?;
    if advertised == pattern(&prep.grant) {
        return Some(prep.grant.clone());
    }
    if prep.grant.kind != GrantKind::File {
        return None;
    }
    let parent = prep.grant.path.parent()?;
    // The filesystem root is never offered as a file's directory scope.
    parent.parent()?;
    let directory = AccessGrant {
        path: parent.to_path_buf(),
        kind: GrantKind::Subtree,
        operation: prep.grant.operation,
    };
    (advertised == pattern(&directory)).then_some(directory)
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

enum ConsentEvent<'a> {
    Filed { tool: &'a str, epoch: u64 },
    Settled(&'a Settlement),
    Granted { epoch: u64 },
}

fn log_consent(session: &str, event: ConsentEvent<'_>, ask: &str, grant: &AccessGrant) {
    if !crate::logging::slog_info_enabled(module_path!()) {
        return;
    }
    let path_hash = cortexkit_role_tool_provider::catalog::composition_digest(&json!(grant.path))
        .expect("canonical consent path is JSON");
    let kind = match grant.kind {
        GrantKind::File => "file",
        GrantKind::Subtree => "subtree",
    };
    // Attribute only this emission; consent work may run on async or blocking
    // workers whose thread-local logging context belongs to an earlier call.
    crate::log_ctx::with_session(Some(session.to_owned()), || match event {
        ConsentEvent::Filed { tool, epoch } => {
            crate::slog_info!("consent ask filed ask={ask} tool={tool} kind={kind} scope_epoch={epoch} path_hash={path_hash:.12}");
        }
        ConsentEvent::Settled(settled) => {
            let outcome = match settled {
                Settlement::Allow { scope: true, .. } => "allow_scope",
                Settlement::Allow { scope: false, .. } => "allow_once",
                Settlement::Deny { .. } => "deny",
                Settlement::Refused(_) => "refused",
                Settlement::Pending => "pending",
            };
            crate::slog_info!(
                "consent answer settled ask={ask} outcome={outcome} path_hash={path_hash:.12}"
            );
        }
        ConsentEvent::Granted { epoch } => {
            crate::slog_info!("consent grant inserted ask={ask} kind={kind} scope_epoch={epoch} path_hash={path_hash:.12}");
        }
    });
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

fn retry_answer(call: &HeldCall, grant: &AccessGrant) -> Response {
    answer(call, "access_retry", ACCESS_RETRY_MESSAGE.to_owned(), grant)
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
    flight: &Arc<SettlementFlight>,
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
    let directory_scope = scope_before_db(&grant).await;
    let prepared = with_db_handle(call, move |db, ctx| {
        prepare(
            db,
            ctx,
            &identity,
            &tool,
            &arguments,
            &asked,
            &directory_scope,
            own_holder.as_deref(),
        )
    })
    .await;
    let prep = match prepared {
        Ok(Prep::Ask(prep)) => prep,
        Ok(Prep::Busy) => {
            return match flight.other_settled(false) {
                Some(other) => match other.result(call, &grant, route_hold).await {
                    Some(result) => Hold::Shared(result),
                    None => Hold::Abandoned,
                },
                // A claimed row has already received an answer. Its owner may
                // have been cancelled while asynchronous cleanup is still running.
                None => Hold::Answer(retry_answer(call, &grant)),
            };
        }
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
    log_consent(
        &call.identity.session,
        ConsentEvent::Filed {
            tool: &call.tool,
            epoch: prep.key.scope.epoch,
        },
        &elicitation,
        &prep.grant,
    );
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
        let applied = Arc::new(StdMutex::new(None));
        let settled_received = Arc::new(AtomicBool::new(false));
        let receipt_settled = Arc::clone(&settled_received);
        let receipt_flight = Arc::clone(flight);
        let retry_after_deadline = Arc::new(AtomicBool::new(false));
        let receipt_retry = Arc::clone(&retry_after_deadline);
        let received = Arc::clone(&applied);
        let receipt_call = call.clone();
        let receipt_prep = prep.clone();
        let ask_id = elicitation.clone();
        let awaited = transport.call_with_receipt(
            consent_call(
                call,
                &prep,
                "consent.await",
                json!({"elicitation_id": elicitation, "timeout_ms": wait.as_millis() as u64}),
                wait + UNARY_TIMEOUT,
            ),
            Box::new(move |record| {
                Box::pin(async move {
                    if let Ok(settled) = decode_record(&record) {
                        if settled != Settlement::Pending {
                            receipt_settled.store(true, Ordering::SeqCst);
                            if let Some(other) = receipt_flight.other_settled(true) {
                                let route_hold = RouteHold::new(receipt_call.route);
                                let result = other
                                    .result(&receipt_call, &receipt_prep.grant, &route_hold)
                                    .await;
                                *received.lock().unwrap() = Some(match result {
                                    Some(result) => Hold::Shared(result),
                                    None => Hold::Abandoned,
                                });
                                return Ok(record);
                            }
                            log_consent(
                                &receipt_call.identity.session,
                                ConsentEvent::Settled(&settled),
                                &ask_id,
                                &receipt_prep.grant,
                            );
                            let result = finish(
                                &receipt_call,
                                &receipt_prep,
                                settled,
                                receipt_retry,
                                &ask_id,
                            )
                            .await;
                            *received
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(result);
                        }
                    }
                    Ok(record)
                })
            }),
        );
        tokio::pin!(awaited);
        let mut local_deadline = None;
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
                        None if Instant::now() >= deadline => {
                            let received_settlement = settled_received.load(Ordering::SeqCst);
                            #[cfg(test)]
                            let received_settlement = received_settlement && tests::SETTLED_DEADLINE_ENABLED.load(Ordering::SeqCst);
                            if received_settlement {
                                if let Some(result) = applied.lock().unwrap().take() {
                                    return result;
                                }
                                let bound = local_deadline.get_or_insert_with(|| (Instant::now() + settled_wait_budget()).min(call.reply_deadline));
                                #[cfg(test)]
                                tests::settled_deadline_waiting();
                                if Instant::now() < *bound {
                                    continue;
                                }
                                retry_after_deadline.store(true, Ordering::SeqCst);
                                crate::log_ctx::with_session(Some(call.identity.session.clone()), || {
                                    crate::slog_warn!("consent settled application exceeded local wait bound ask={elicitation}");
                                });
                                return Hold::Answer(retry_answer(call, &prep.grant));
                            }
                            // Mark expiry before dropping the receipt: its blocking
                            // claim can still complete after we promise a retry.
                            retry_after_deadline.store(true, Ordering::SeqCst);
                            break None;
                        }
                        None => {}
                    }
                }
            }
        };
        if let Some(result) = applied
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            return result;
        }
        match reply.map(|reply| reply.and_then(|record| decode_record(&record))) {
            None | Some(Ok(Settlement::Pending)) => {}
            Some(Ok(settled)) => {
                return finish(call, &prep, settled, retry_after_deadline, &elicitation).await
            }
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

async fn finish(
    call: &HeldCall,
    prep: &Prepared,
    settled: Settlement,
    retry_after_deadline: Arc<AtomicBool>,
    elicitation: &str,
) -> Hold {
    #[cfg(test)]
    if matches!(settled, Settlement::Deny { .. } | Settlement::Refused(_)) {
        let request = call.request_id.clone();
        tokio::task::spawn_blocking(move || tests::wait_at_finish(&request, false))
            .await
            .expect("refusal application gate completes");
    }
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
            let grant = if scope {
                let Some(grant) = advertised_scope(prep) else {
                    discard(call, prep).await;
                    return Hold::Answer(refused_answer(
                        call,
                        &prep.grant,
                        "access_declined",
                        Some("the advertised scope does not match the requested permission"),
                    ));
                };
                grant
            } else {
                prep.grant.clone()
            };
            let Some(ctx) = call.executor.actor_context(&call.identity.root) else {
                return Hold::Answer(unavailable_answer(
                    call,
                    &prep.grant,
                    "executor actor is not registered",
                ));
            };
            let Some(db) = ctx.db() else {
                return Hold::Answer(unavailable_answer(
                    call,
                    &prep.grant,
                    "aft.db is unavailable",
                ));
            };
            // Application and cleanup must use the same pinned database even
            // if the actor's context or database slot is replaced meanwhile.
            let claim = Arc::new(ClaimedAsk {
                db: Arc::clone(&db),
                key: key.clone(),
                holder: holder.clone(),
                armed: AtomicBool::new(true),
                retry_after_deadline,
                // Claim clears the persisted request fields. Keep the original
                // card rather than rereading a row another dispatch may own.
                original: (!scope).then(|| prep.clone()),
                elicitation: elicitation.to_owned(),
            });
            let ownership = Arc::clone(&claim);
            let claimant = holder.clone();
            #[cfg(test)]
            let request_id = call.request_id.clone();
            let applied = tokio::task::spawn_blocking(move || {
                // Blocking work survives cancellation of the waiting future.
                // Retain the owner until after the claim has finished committing.
                let _ownership = ownership;
                #[cfg(test)]
                tests::wait_at_finish(&request_id, false);
                let applied = apply_allow(
                    &db, &ctx, &identity, &key, &call_key, &claimant, &grant, scope, by_user,
                );
                #[cfg(test)]
                tests::wait_at_finish(&request_id, true);
                applied
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|applied| applied);
            match applied {
                Ok(Applied::Run(once)) => Hold::Run { once, claim },
                Ok(Applied::HeadNotHeld) => Hold::Answer(refused_answer(
                    call,
                    &prep.grant,
                    "access_declined",
                    Some("the head no longer holds this permission itself"),
                )),
                Ok(Applied::NotClaimed) => Hold::Answer(retry_answer(call, &prep.grant)),
                Err(error) => Hold::Answer(unavailable_answer(call, &prep.grant, &error)),
            }
        }
    }
}

/// Record an allow answer: claim the ask's approvals and, for a scope answer,
/// store the grant. A head's answer is first checked against the head's
/// current policy, resolved on disk with the aft.db lock released.
#[allow(clippy::too_many_arguments)]
fn apply_allow(
    db: &std::sync::Mutex<crate::db::TrackedConnection>,
    ctx: &AppContext,
    identity: &RouteIdentity,
    key: &AskKey,
    call_key: &str,
    claimant: &str,
    grant: &AccessGrant,
    scope: bool,
    by_user: bool,
) -> Result<Applied, String> {
    with_head_policy(db, ctx, identity, !by_user, |conn, head| {
        // A head may pass on only what it holds now: its authority is
        // checked again against the head's current policy and grants.
        if !by_user && head_policy(head.as_ref(), grant) != HeadPolicy::Allow {
            asks::discard(conn, key)?;
            return Ok(Applied::HeadNotHeld);
        }
        let audit = if scope { asks::get(conn, key)? } else { None };
        let Some(approved) = asks::claim(conn, key, call_key, claimant)? else {
            return Ok(Applied::NotClaimed);
        };
        if scope {
            crate::db::access_grants::insert(
                conn,
                &access_grants::policy_scope_key(identity),
                grant,
            )?;
            if let Some(record) = audit {
                log_consent(
                    &identity.session,
                    ConsentEvent::Granted {
                        epoch: key.scope.epoch,
                    },
                    record.elicitation_id.as_deref().unwrap_or(call_key),
                    grant,
                );
            }
        }
        Ok(Applied::Run(approved))
    })
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
