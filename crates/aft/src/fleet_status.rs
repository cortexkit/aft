//! Publisher for AFT's segment on the fleet status-holder plane.
//!
//! The holder owns retention and composed rendering. While its route is live,
//! AFT publishes its project-scoped segment. A live route only proves that a
//! publisher exists: AFT leaves the agent-facing bar to the holder's host plugin
//! only while the holder's publish acks show that something is actually reading
//! the project's scope (see [`FleetStatusClient::reader_present`]).

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::{json, Value};
use subc_client_rs::{
    CallOptions, CatalogList, CloseRouteOptions, ConnectionState, ConsumerOptions, ControlPush,
    PushEvent, RouteCloseReason, RouteHandle, SubcConsumer,
};
use subc_protocol::manifest::ProviderRole;
use subc_protocol::{BindIdentity, RouteTarget};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const STATUS_CADENCE: Duration = Duration::from_millis(2_500);
/// How recent the holder's last `status.line` read of a scope must be for AFT
/// to count it as a live reader. Prefrontal's status stamper polls every 3 s, so
/// a few of its cadences separate a live renderer from a stale or one-off
/// diagnostic read.
const READER_FRESH_WINDOW_MS: u64 = 10_000;
const DISCOVERY_INITIAL_BACKOFF: Duration = Duration::from_millis(250);
const DISCOVERY_MAX_BACKOFF: Duration = Duration::from_secs(5);
const STATUS_PUBLISH_TTL_MS: u64 = 7_500;
const STATUS_MODULE: &str = "aft";
const STATUS_HOLDER_MODULE: &str = "prefrontal-core";
const STATUS_LINE_OPERATION: &str = "status.line";

#[cfg(test)]
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct StatusLineSegment {
    pub(crate) module: String,
    pub(crate) scope: String,
    pub(crate) text: String,
}

#[cfg(test)]
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct StatusLineSnapshot {
    pub(crate) line: String,
    pub(crate) segments: Vec<StatusLineSegment>,
}

#[cfg(test)]
impl StatusLineSnapshot {
    fn parse(body: &[u8]) -> Option<Self> {
        serde_json::from_slice(body).ok()
    }

    fn has_foreign_segment(&self) -> bool {
        self.segments
            .iter()
            .any(|segment| segment.module != STATUS_MODULE && !segment.text.is_empty())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct StatusPublishAck {
    pub(crate) epoch: u64,
    pub(crate) accepted_revision: u64,
    /// Holder wall clock (ms since the Unix epoch) at the last `status.line`
    /// read that named the published scope. Null when nothing has read the
    /// scope in the holder process; absent from holders that predate the field.
    /// Both of those mean "no evidence of a reader".
    #[serde(default)]
    pub(crate) last_read_at_ms: Option<u64>,
}

impl StatusPublishAck {
    fn parse(body: &[u8]) -> Option<Self> {
        serde_json::from_slice(body).ok()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PublishFence {
    epoch: u64,
    accepted_revision: u64,
}

impl PublishFence {
    fn observe(&mut self, ack: StatusPublishAck) {
        if ack.epoch > self.epoch
            || (ack.epoch == self.epoch && ack.accepted_revision >= self.accepted_revision)
        {
            self.epoch = ack.epoch;
            self.accepted_revision = ack.accepted_revision;
        }
    }
}

#[derive(Default)]
struct ClientState {
    last_publish_at: HashMap<String, Instant>,
    publish_fence: PublishFence,
    /// Per scope: whether the holder's most recent publish ack reported a read
    /// of that scope within [`READER_FRESH_WINDOW_MS`] of the ack's arrival.
    reader_fresh_by_scope: HashMap<String, bool>,
}

impl ClientState {
    fn forget_route(&mut self) {
        self.last_publish_at.clear();
        self.reader_fresh_by_scope.clear();
    }
}

fn status_scope(project_root: &Path) -> String {
    format!("project:{}", project_root.to_string_lossy())
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
}

/// Whether an ack received at `ack_at_ms` (AFT's clock) proves a live reader.
/// A read stamped later than the ack (holder clock ahead of AFT's) counts as
/// age zero rather than as stale.
fn read_is_fresh(last_read_at_ms: Option<u64>, ack_at_ms: u64) -> bool {
    last_read_at_ms
        .is_some_and(|read_at| ack_at_ms.saturating_sub(read_at) <= READER_FRESH_WINDOW_MS)
}

struct FleetStatusInner {
    wire_tx: Option<mpsc::Sender<StatusWireRequest>>,
    route_live: AtomicBool,
    state: parking_lot::Mutex<ClientState>,
    next_revision: AtomicU64,
}

#[derive(Clone)]
pub(crate) struct FleetStatusClient {
    inner: Arc<FleetStatusInner>,
}

impl FleetStatusClient {
    #[cfg(test)]
    pub(crate) fn channel(capacity: usize) -> (Self, mpsc::Receiver<StatusWireRequest>) {
        Self::channel_with_liveness(capacity, true)
    }

    pub(crate) fn dial_channel(capacity: usize) -> (Self, mpsc::Receiver<StatusWireRequest>) {
        Self::channel_with_liveness(capacity, false)
    }

    fn channel_with_liveness(
        capacity: usize,
        route_live: bool,
    ) -> (Self, mpsc::Receiver<StatusWireRequest>) {
        let (wire_tx, wire_rx) = mpsc::channel(capacity);
        (
            Self {
                inner: Arc::new(FleetStatusInner {
                    wire_tx: Some(wire_tx),
                    route_live: AtomicBool::new(route_live),
                    state: parking_lot::Mutex::new(ClientState::default()),
                    next_revision: AtomicU64::new(1),
                }),
            },
            wire_rx,
        )
    }

    pub(crate) fn dormant() -> Self {
        Self {
            inner: Arc::new(FleetStatusInner {
                wire_tx: None,
                route_live: AtomicBool::new(false),
                state: parking_lot::Mutex::new(ClientState::default()),
                next_revision: AtomicU64::new(1),
            }),
        }
    }

    pub(crate) fn set_route_live(&self, route_live: bool) {
        self.inner.route_live.store(route_live, Ordering::Release);
        if !route_live {
            self.inner.state.lock().forget_route();
        }
    }

    /// Whether something is reading this project's fleet status line, so the
    /// holder's composed line (not AFT's own bar) is what the agent sees.
    ///
    /// Evidence is the holder's most recent publish ack for the scope: it must
    /// report a `status.line` read within [`READER_FRESH_WINDOW_MS`] of when AFT
    /// received that ack. Freshness is judged at ack arrival, not at render
    /// time, because acks only arrive after publishes and publishes only happen
    /// on tool results: after any pause of more than the window between two
    /// tool calls, a render-time age would call a steadily polling reader stale
    /// and show a duplicate bar. The ack for this call's own publish arrives
    /// after the decision, so a reader that stops is noticed one result late.
    pub(crate) fn reader_present(&self, project_root: &Path) -> bool {
        if !self.inner.route_live.load(Ordering::Acquire) {
            return false;
        }
        let scope = status_scope(project_root);
        self.inner
            .state
            .lock()
            .reader_fresh_by_scope
            .get(&scope)
            .copied()
            .unwrap_or(false)
    }

    /// Publish AFT's segment when the holder route is live. The return value
    /// reports route liveness only; it says nothing about whether anything
    /// reads the published scope (use [`Self::reader_present`] for that).
    pub(crate) fn publish(
        &self,
        project_root: &Path,
        harness: &str,
        session: &str,
        aft_text: &str,
    ) -> bool {
        let Some(wire_tx) = self.inner.wire_tx.as_ref() else {
            return false;
        };
        let route_live = self.inner.route_live.load(Ordering::Acquire);
        let scope = status_scope(project_root);
        let now = Instant::now();
        let revision = {
            let Some(mut state) = self.inner.state.try_lock() else {
                return true;
            };
            if state
                .last_publish_at
                .get(&scope)
                .is_some_and(|last| now.saturating_duration_since(*last) < STATUS_CADENCE)
            {
                return true;
            }
            state.last_publish_at.insert(scope.clone(), now);
            self.inner.next_revision.fetch_add(1, Ordering::Relaxed)
        };

        let request = StatusWireRequest::publish(
            Arc::downgrade(&self.inner),
            project_root,
            harness,
            session,
            &scope,
            aft_text,
            revision,
        );
        match wire_tx.try_send(request) {
            Ok(()) => route_live,
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.inner.state.lock().last_publish_at.remove(&scope);
                route_live
            }
            Err(mpsc::error::TrySendError::Closed(request)) => {
                request.complete_unavailable();
                false
            }
        }
    }

    #[cfg(test)]
    fn publish_fence(&self) -> PublishFence {
        self.inner.state.lock().publish_fence
    }
}

pub(crate) struct StatusWireRequest {
    body: Value,
    project_root: String,
    harness: String,
    session: String,
    client: Weak<FleetStatusInner>,
}

#[derive(Clone)]
struct FleetRouteIdentity {
    project_root: String,
    harness: String,
    session: String,
}

impl StatusWireRequest {
    fn publish(
        client: Weak<FleetStatusInner>,
        project_root: &Path,
        harness: &str,
        session: &str,
        scope: &str,
        text: &str,
        revision: u64,
    ) -> Self {
        Self {
            body: json!({
                "op": "status.publish",
                "module": STATUS_MODULE,
                "scope": scope,
                "text": text,
                "ttl_ms": STATUS_PUBLISH_TTL_MS,
                "revision": revision,
            }),
            project_root: project_root.to_string_lossy().into_owned(),
            harness: harness.to_owned(),
            session: session.to_owned(),
            client,
        }
    }

    pub(crate) fn project_root(&self) -> &str {
        &self.project_root
    }

    pub(crate) fn harness(&self) -> &str {
        &self.harness
    }

    pub(crate) fn session(&self) -> &str {
        &self.session
    }

    pub(crate) fn body(&self) -> &Value {
        &self.body
    }

    pub(crate) fn complete_response(self, body: &[u8]) -> bool {
        let Some(ack) = StatusPublishAck::parse(body) else {
            self.complete_unavailable();
            return false;
        };
        let Some(client) = self.client.upgrade() else {
            return true;
        };
        let reader_fresh = read_is_fresh(ack.last_read_at_ms, unix_now_ms());
        let mut state = client.state.lock();
        state.publish_fence.observe(ack);
        if let Some(scope) = self.body["scope"].as_str() {
            state
                .reader_fresh_by_scope
                .insert(scope.to_owned(), reader_fresh);
        }
        true
    }

    pub(crate) fn complete_unavailable(self) {
        if let Some(client) = self.client.upgrade() {
            client.route_live.store(false, Ordering::Release);
            client.state.lock().forget_route();
        }
    }
}

impl From<&StatusWireRequest> for FleetRouteIdentity {
    fn from(request: &StatusWireRequest) -> Self {
        Self {
            project_root: request.project_root().to_owned(),
            harness: request.harness().to_owned(),
            session: request.session().to_owned(),
        }
    }
}

/// Connect a management client using the daemon's authenticated subc dial path.
/// Profile and the status publisher share this helper so there is one client
/// implementation and one connection-file/authentication behavior.
pub async fn connect_subc_consumer(
    connection_file: &Path,
    options: ConsumerOptions,
) -> Result<SubcConsumer, subc_client_rs::ConsumerError> {
    SubcConsumer::connect(connection_file, options).await
}

pub(crate) fn spawn_fleet_status_dial(
    connection_file: &Path,
    capacity: usize,
) -> (FleetStatusClient, JoinHandle<()>) {
    if !consumer_identity_is_available() {
        return (FleetStatusClient::dormant(), tokio::spawn(async {}));
    }
    let (client, wire_rx) = FleetStatusClient::dial_channel(capacity);
    let task_client = client.clone();
    let connection_file = connection_file.to_path_buf();
    let task = tokio::spawn(async move {
        run_fleet_status_dial(connection_file, task_client, wire_rx).await;
    });
    (client, task)
}

async fn run_fleet_status_dial(
    connection_file: std::path::PathBuf,
    client: FleetStatusClient,
    mut wire_rx: mpsc::Receiver<StatusWireRequest>,
) {
    let Some(first_request) = wire_rx.recv().await else {
        return;
    };
    let route_identity = FleetRouteIdentity::from(&first_request);
    let mut pending_request = Some(first_request);
    let mut connect_backoff = DISCOVERY_INITIAL_BACKOFF;
    let consumer = loop {
        let options = ConsumerOptions {
            call_timeout: STATUS_CADENCE,
            ..ConsumerOptions::default()
        };
        match connect_subc_consumer(&connection_file, options).await {
            Ok(consumer) => break consumer,
            Err(error) => {
                client.set_route_live(false);
                if let Some(request) = pending_request.take() {
                    request.complete_unavailable();
                }
                log::debug!("fleet status dial: consumer connect unavailable: {error}");
                tokio::time::sleep(connect_backoff).await;
                connect_backoff = next_discovery_backoff(connect_backoff);
            }
        }
    };

    run_connected_status_dial(consumer, client, wire_rx, route_identity, pending_request).await;
}

// Keep the dial's discovery and publish loop identical for the real transport and
// the clock-driven test consumer; the latter can return actual bind errors.
trait StatusConsumer {
    type Route;

    fn on_connection_state(&self, cb: impl Fn(ConnectionState) + Send + 'static);
    /// The daemon's route-lifecycle notifications, sent on the connection's
    /// control channel (`route.closing`, `route.closed`, each with a reason).
    fn control_pushes(&self) -> mpsc::Receiver<ControlPush>;
    async fn catalog_list(&self) -> Result<CatalogList, String>;
    async fn open_route(
        &self,
        target: RouteTarget,
        identity: BindIdentity,
    ) -> Result<Self::Route, String>;
    fn push_events(&self, route: &Self::Route) -> Result<mpsc::Receiver<PushEvent>, String>;
    /// The channel this connection knows the route by, which the daemon's
    /// route-close pushes name.
    fn route_channel(&self, route: &Self::Route) -> u16;
    async fn close_handle(&self, route: &Self::Route) -> Result<(), String>;
    async fn request(&self, route: &Self::Route, body: Vec<u8>) -> Result<Vec<u8>, String>;
}

impl StatusConsumer for SubcConsumer {
    type Route = RouteHandle;

    fn on_connection_state(&self, cb: impl Fn(ConnectionState) + Send + 'static) {
        SubcConsumer::on_connection_state(self, cb);
    }

    fn control_pushes(&self) -> mpsc::Receiver<ControlPush> {
        SubcConsumer::control_pushes(self, CONTROL_PUSH_CAPACITY)
    }

    async fn catalog_list(&self) -> Result<CatalogList, String> {
        SubcConsumer::catalog_list(self)
            .await
            .map_err(|error| error.to_string())
    }

    async fn open_route(
        &self,
        target: RouteTarget,
        identity: BindIdentity,
    ) -> Result<Self::Route, String> {
        SubcConsumer::open_route(
            self,
            target,
            identity,
            CallOptions {
                consumer_identity: crate::launch_nonce::consumer_identity(),
                ..CallOptions::default()
            },
        )
        .await
        .map_err(|error| error.to_string())
    }

    fn push_events(&self, route: &Self::Route) -> Result<mpsc::Receiver<PushEvent>, String> {
        SubcConsumer::push_events(self, route).map_err(|error| error.to_string())
    }

    fn route_channel(&self, route: &Self::Route) -> u16 {
        route.channel
    }

    async fn close_handle(&self, route: &Self::Route) -> Result<(), String> {
        SubcConsumer::close_handle(self, route, CloseRouteOptions::default())
            .await
            .map_err(|error| error.to_string())
    }

    async fn request(&self, route: &Self::Route, body: Vec<u8>) -> Result<Vec<u8>, String> {
        SubcConsumer::request(self, route, body, CallOptions::default())
            .await
            .map_err(|error| error.to_string())
    }
}

async fn run_connected_status_dial<C: StatusConsumer>(
    consumer: C,
    client: FleetStatusClient,
    mut wire_rx: mpsc::Receiver<StatusWireRequest>,
    route_identity: FleetRouteIdentity,
    mut pending_request: Option<StatusWireRequest>,
) {
    let (connection_state_tx, mut connection_state_rx) = mpsc::unbounded_channel();
    let connection_state_client = client.clone();
    consumer.on_connection_state(move |state| {
        connection_state_client.set_route_live(false);
        let _ = connection_state_tx.send(state);
    });
    let mut control_pushes = Some(consumer.control_pushes());
    // Set when the daemon closed the holder's routes for a reason that forbids
    // reopening them (see `close_forbids_reopen`). Cleared when the connection
    // is restored, since a new daemon connection starts a new route history.
    let mut reopen_forbidden: Option<String> = None;
    // The channel of the route the dial opened last. It outlives the route
    // itself, because the daemon's close push can arrive after the dial has
    // already let the route go, and is replaced only by the next open.
    let mut last_route_channel: Option<u16> = None;

    let mut route: Option<C::Route> = None;
    let mut route_events = None;
    let mut next_discovery_at = tokio::time::Instant::now();
    let mut discovery_backoff = DISCOVERY_INITIAL_BACKOFF;
    loop {
        if tokio::time::Instant::now() >= next_discovery_at {
            match consumer.catalog_list().await {
                Ok(catalog) if catalog_advertises_status_line(&catalog.modules) => {
                    if route.is_none() && reopen_forbidden.is_none() {
                        // `BindIdentity::new` leaves the registered project id
                        // unset: the dial has no resolved id to send.
                        let identity = BindIdentity::new(
                            route_identity.project_root.clone(),
                            route_identity.harness.clone(),
                            route_identity.session.clone(),
                        );
                        match consumer
                            .open_route(
                                RouteTarget::ManagementSurface {
                                    module_id: STATUS_HOLDER_MODULE.to_string(),
                                },
                                identity,
                            )
                            .await
                        {
                            Ok(opened_route) => match consumer.push_events(&opened_route) {
                                Ok(events) => {
                                    route_events = Some(events);
                                    last_route_channel =
                                        Some(consumer.route_channel(&opened_route));
                                    route = Some(opened_route);
                                    discovery_backoff = DISCOVERY_INITIAL_BACKOFF;
                                }
                                Err(error) => {
                                    log::debug!(
                                        "fleet status dial: route event registration unavailable: {error}"
                                    );
                                    client.set_route_live(false);
                                }
                            },
                            Err(error) => {
                                log::debug!("fleet status dial: route unavailable: {error}");
                                client.set_route_live(false);
                            }
                        }
                    }
                    client.set_route_live(route.is_some());
                    if route.is_some() {
                        next_discovery_at = tokio::time::Instant::now() + STATUS_CADENCE;
                    } else {
                        next_discovery_at = tokio::time::Instant::now() + discovery_backoff;
                        discovery_backoff = next_discovery_backoff(discovery_backoff);
                    }
                }
                Ok(_) => {
                    if let Some(opened_route) = route.take() {
                        let _ = consumer.close_handle(&opened_route).await;
                    }
                    route_events = None;
                    client.set_route_live(false);
                    discovery_backoff = DISCOVERY_INITIAL_BACKOFF;
                    next_discovery_at = tokio::time::Instant::now() + STATUS_CADENCE;
                }
                Err(error) => {
                    route = None;
                    route_events = None;
                    client.set_route_live(false);
                    log::debug!("fleet status dial: catalog unavailable: {error}");
                    next_discovery_at = tokio::time::Instant::now() + discovery_backoff;
                    discovery_backoff = next_discovery_backoff(discovery_backoff);
                }
            }
        }

        if let Some(request) = pending_request.take() {
            let Some(opened_route) = route.as_ref() else {
                request.complete_unavailable();
                continue;
            };
            let body = match encode_status_publish_call(request.body()) {
                Some(body) => body,
                None => {
                    request.complete_unavailable();
                    continue;
                }
            };
            match consumer.request(opened_route, body).await {
                Ok(response) => {
                    if !complete_status_publish_call(request, &response) {
                        route = None;
                        route_events = None;
                        client.set_route_live(false);
                        next_discovery_at = tokio::time::Instant::now();
                    }
                }
                Err(error) => {
                    log::debug!("fleet status dial: publish unavailable: {error}");
                    request.complete_unavailable();
                    route = None;
                    route_events = None;
                    client.set_route_live(false);
                    next_discovery_at = tokio::time::Instant::now();
                }
            }
            continue;
        }

        tokio::select! {
            maybe_request = wire_rx.recv() => {
                let Some(request) = maybe_request else {
                    client.set_route_live(false);
                    return;
                };
                pending_request = Some(request);
            }
            maybe_state = connection_state_rx.recv() => {
                match maybe_state {
                    Some(state @ (ConnectionState::Dropped | ConnectionState::Restored { .. })) => {
                        if matches!(state, ConnectionState::Restored { .. }) {
                            reopen_forbidden = None;
                        }
                        // A new connection numbers its channels afresh.
                        last_route_channel = None;
                        route = None;
                        route_events = None;
                        client.set_route_live(false);
                        next_discovery_at = tokio::time::Instant::now();
                    }
                    None => {}
                }
            }
            maybe_push = async {
                control_pushes
                    .as_mut()
                    .expect("control push receiver guarded by select condition")
                    .recv()
                    .await
            }, if control_pushes.is_some() => {
                match maybe_push {
                    Some(push) => {
                        if record_forbidden_reopen(&push, last_route_channel, &mut reopen_forbidden) {
                            route = None;
                            route_events = None;
                            client.set_route_live(false);
                        }
                    }
                    None => control_pushes = None,
                }
            }
            maybe_event = async {
                route_events
                    .as_mut()
                    .expect("route event receiver guarded by select condition")
                    .recv()
                    .await
            }, if route_events.is_some() => {
                if maybe_event.is_none() {
                    // The daemon sends `route.closed` before it ends the route,
                    // so its reason is already queued: read it before deciding
                    // whether the next discovery may reopen.
                    if let Some(receiver) = control_pushes.as_mut() {
                        while let Ok(push) = receiver.try_recv() {
                            record_forbidden_reopen(&push, last_route_channel, &mut reopen_forbidden);
                        }
                    }
                    route = None;
                    route_events = None;
                    client.set_route_live(false);
                    next_discovery_at = tokio::time::Instant::now();
                }
            }
            _ = tokio::time::sleep_until(next_discovery_at) => {}
        }
    }
}

fn encode_status_publish_call(body: &Value) -> Option<Vec<u8>> {
    let mut params = body.as_object()?.clone();
    let method = params.remove("op")?.as_str()?.to_owned();
    if method != "status.publish" {
        return None;
    }
    serde_json::to_vec(&json!({
        "method": method,
        "params": params,
    }))
    .ok()
}

fn complete_status_publish_call(request: StatusWireRequest, body: &[u8]) -> bool {
    let result = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|response| response.get("result").cloned())
        .and_then(|result| serde_json::to_vec(&result).ok());
    let Some(result) = result else {
        request.complete_unavailable();
        return false;
    };
    request.complete_response(&result)
}

fn consumer_identity_is_available() -> bool {
    crate::launch_nonce::consumer_identity().is_some()
}

fn catalog_advertises_status_line(entries: &[subc_client_rs::CatalogEntry]) -> bool {
    entries.iter().any(|entry| {
        entry.module_id == STATUS_HOLDER_MODULE && entry.roles.iter().any(|role| {
            matches!(
                role,
                ProviderRole::ManagementSurface { operations, .. }
                    if operations.iter().any(|operation| operation.name == STATUS_LINE_OPERATION)
            )
        })
    })
}

/// Room for the daemon's control pushes between two turns of the dial loop.
/// They are rare (one per route close), and a full queue only drops advisory
/// pushes, so a small bound is enough.
const CONTROL_PUSH_CAPACITY: usize = 16;

/// Whether a route-close reason forbids reopening the route. A scope close
/// (`scope_ended`, `scope_carrier_removed`, `scope_delegation_changed`,
/// `scope_parent_ended`) means the work the route belonged to is over, and a
/// reason this build does not know may mean the same, so neither may bring the
/// route back. The reasons the dial already handled (reload, restart, crash,
/// disable, capability_denied) keep their behaviour: the next discovery reopens
/// when the holder is advertised again.
fn close_forbids_reopen(reason: &str) -> bool {
    reason.starts_with("scope_")
        || matches!(
            RouteCloseReason::from_wire(reason),
            RouteCloseReason::Unknown(_)
        )
}

/// Whether a route-close push is about the route the dial opened last.
///
/// A daemon on subc-control 0.27 or later names the client channels a close
/// covers, so a late push for a route the dial has since replaced cannot end
/// the new one. An older daemon names none; then the holder's module id alone
/// decides, as it always has.
fn push_names_dial_route(push: &ControlPush, last_route_channel: Option<u16>) -> bool {
    let channels: Vec<u64> = push
        .body
        .get("channels")
        .and_then(Value::as_array)
        .map(|channels| channels.iter().filter_map(Value::as_u64).collect())
        .unwrap_or_default();
    channels.is_empty()
        || last_route_channel.is_some_and(|channel| channels.contains(&u64::from(channel)))
}

/// Note a `route.closed` / `route.closing` push for the status holder's route
/// whose reason forbids reopening. Returns true when it did.
fn record_forbidden_reopen(
    push: &ControlPush,
    last_route_channel: Option<u16>,
    reopen_forbidden: &mut Option<String>,
) -> bool {
    if push.body.get("module_id").and_then(Value::as_str) != Some(STATUS_HOLDER_MODULE) {
        return false;
    }
    if !push_names_dial_route(push, last_route_channel) {
        return false;
    }
    let Some(reason) = push
        .route_close_reason()
        .and_then(|_| push.body.get("reason").and_then(Value::as_str))
    else {
        return false;
    };
    if !close_forbids_reopen(reason) {
        return false;
    }
    log::info!("fleet status dial: holder route closed ({reason}); not reopening it");
    *reopen_forbidden = Some(reason.to_owned());
    true
}

fn next_discovery_backoff(current: Duration) -> Duration {
    current
        .checked_mul(2)
        .unwrap_or(DISCOVERY_MAX_BACKOFF)
        .min(DISCOVERY_MAX_BACKOFF)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;
    use std::collections::HashSet;
    use subc_protocol::manifest::{ManagementOperation, ManagementOperationKind};

    const FIXTURES: &str = include_str!("../../../.cortexkit/status-line-fixtures-2026-08-14.json");
    const PUBLISH_ACK_FIXTURES: [&str; 9] = [
        "publish_aft",
        "supersede_r3",
        "supersede_r2_late",
        "ttl_publish",
        "quiet_publish_empty_text",
        "fat1",
        "fat2",
        "epoch_bump_republish_r1_new_conn",
        "publish_aft_project",
    ];
    const LINE_REPLY_FIXTURES: [&str; 9] = [
        "line_foreign_present",
        "supersede_line_after_r3",
        "supersede_line_after_late_r2",
        "ttl_line_before",
        "ttl_line_after_expiry",
        "quiet_line",
        "line_cap_overflow",
        "epoch_bump_line_after",
        "line_aft_solo_scope",
    ];

    fn fixtures() -> Map<String, Value> {
        serde_json::from_str::<Value>(FIXTURES)
            .expect("producer fixture JSON")
            .as_object()
            .expect("producer fixture object")
            .clone()
    }

    fn parse_line(value: &Value) -> StatusLineSnapshot {
        StatusLineSnapshot::parse(&serde_json::to_vec(value).expect("fixture bytes"))
            .expect("status.line fixture")
    }

    fn parse_ack(value: &Value) -> StatusPublishAck {
        StatusPublishAck::parse(&serde_json::to_vec(value).expect("fixture bytes"))
            .expect("status.publish fixture")
    }

    fn status_catalog_entry(module_id: &str, operation: &str) -> subc_client_rs::CatalogEntry {
        subc_client_rs::CatalogEntry {
            module_id: module_id.to_string(),
            ready: true,
            not_ready: None,
            module_version: None,
            roles: vec![ProviderRole::ManagementSurface {
                operations: vec![ManagementOperation {
                    name: operation.to_string(),
                    kind: ManagementOperationKind::Query,
                    description: None,
                }],
                config_schema: Value::Null,
                observability: Vec::new(),
                identity_scope: Vec::new(),
                concurrency: Default::default(),
            }],
            control_ops: Vec::new(),
            capabilities: None,
            self_signals: None,
        }
    }

    struct RejectingConsumer {
        attempts: Arc<parking_lot::Mutex<Vec<tokio::time::Instant>>>,
        advertises_status_line: bool,
        catalog_calls: Arc<AtomicU64>,
        publish_calls: Arc<AtomicU64>,
        succeeds_on: Option<usize>,
        connection_callback: Arc<parking_lot::Mutex<Option<Box<dyn Fn(ConnectionState) + Send>>>>,
        push_sender: Arc<parking_lot::Mutex<Option<mpsc::Sender<PushEvent>>>>,
        control_sender: Arc<parking_lot::Mutex<Option<mpsc::Sender<ControlPush>>>>,
    }

    impl StatusConsumer for RejectingConsumer {
        type Route = usize;

        fn on_connection_state(&self, cb: impl Fn(ConnectionState) + Send + 'static) {
            *self.connection_callback.lock() = Some(Box::new(cb));
        }

        fn control_pushes(&self) -> mpsc::Receiver<ControlPush> {
            let (tx, rx) = mpsc::channel(CONTROL_PUSH_CAPACITY);
            *self.control_sender.lock() = Some(tx);
            rx
        }

        async fn catalog_list(&self) -> Result<CatalogList, String> {
            self.catalog_calls.fetch_add(1, Ordering::Relaxed);
            Ok(CatalogList {
                generation: 1,
                modules: if self.advertises_status_line {
                    vec![status_catalog_entry(
                        STATUS_HOLDER_MODULE,
                        STATUS_LINE_OPERATION,
                    )]
                } else {
                    Vec::new()
                },
                subc_ops: Vec::new(),
            })
        }

        async fn open_route(
            &self,
            _target: RouteTarget,
            _identity: BindIdentity,
        ) -> Result<usize, String> {
            let mut attempts = self.attempts.lock();
            attempts.push(tokio::time::Instant::now());
            if self.succeeds_on == Some(attempts.len()) {
                Ok(attempts.len())
            } else {
                Err("module_rejected/classification_unavailable".to_owned())
            }
        }

        fn push_events(&self, _route: &usize) -> Result<mpsc::Receiver<PushEvent>, String> {
            let (tx, rx) = mpsc::channel(1);
            *self.push_sender.lock() = Some(tx);
            Ok(rx)
        }

        fn route_channel(&self, route: &usize) -> u16 {
            u16::try_from(*route).expect("test routes are small")
        }

        async fn close_handle(&self, _route: &usize) -> Result<(), String> {
            Ok(())
        }

        async fn request(&self, _route: &usize, _body: Vec<u8>) -> Result<Vec<u8>, String> {
            self.publish_calls.fetch_add(1, Ordering::Relaxed);
            Err("not used in discovery test".to_owned())
        }
    }

    async fn dial_with_rejections(
        succeeds_on: Option<usize>,
    ) -> (
        tokio::task::JoinHandle<()>,
        Arc<parking_lot::Mutex<Vec<tokio::time::Instant>>>,
        Arc<parking_lot::Mutex<Option<Box<dyn Fn(ConnectionState) + Send>>>>,
    ) {
        let (client, mut wire_rx) = FleetStatusClient::dial_channel(1);
        assert!(!client.publish(Path::new("/tmp/project"), "opencode", "session-1", "local"));
        let first_request = wire_rx.try_recv().expect("first discovery request");
        let identity = FleetRouteIdentity::from(&first_request);
        let attempts = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let callback = Arc::new(parking_lot::Mutex::new(None));
        let consumer = RejectingConsumer {
            attempts: attempts.clone(),
            advertises_status_line: true,
            catalog_calls: Arc::default(),
            publish_calls: Arc::default(),
            succeeds_on,
            connection_callback: callback.clone(),
            push_sender: Arc::default(),
            control_sender: Arc::default(),
        };
        let task = tokio::spawn(run_connected_status_dial(
            consumer,
            client,
            wire_rx,
            identity,
            Some(first_request),
        ));
        tokio::task::yield_now().await;
        (task, attempts, callback)
    }

    #[tokio::test]
    async fn subc_bridge_without_discovered_status_line_surface_emits_no_status_requests() {
        let (client, mut wire_rx) = FleetStatusClient::dial_channel(1);
        assert!(!client.publish(Path::new("/project"), "opencode", "session-1", "local"));
        let first_request = wire_rx.try_recv().expect("queued discovery publish");
        assert_eq!(first_request.body()["op"], "status.publish");
        assert_eq!(client.inner.state.lock().last_publish_at.len(), 1);
        let identity = FleetRouteIdentity::from(&first_request);
        let attempts = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let catalog_calls = Arc::new(AtomicU64::new(0));
        let publish_calls = Arc::new(AtomicU64::new(0));
        let consumer = RejectingConsumer {
            attempts: attempts.clone(),
            advertises_status_line: false,
            catalog_calls: catalog_calls.clone(),
            publish_calls: publish_calls.clone(),
            succeeds_on: Some(1),
            connection_callback: Arc::default(),
            push_sender: Arc::default(),
            control_sender: Arc::default(),
        };

        // Closing ingress lets the real dial finish after handling the queued
        // publish. Its return is a processing barrier, not a quiet-time guess.
        wire_rx.close();
        tokio::time::timeout(
            Duration::from_secs(5),
            run_connected_status_dial(
                consumer,
                client.clone(),
                wire_rx,
                identity,
                Some(first_request),
            ),
        )
        .await
        .expect("empty-catalog dial must finish processing the queued publish");

        assert!(
            attempts.lock().is_empty(),
            "empty catalog must not open a holder route"
        );
        assert_eq!(
            publish_calls.load(Ordering::Relaxed),
            0,
            "empty catalog must not send status requests"
        );
        assert_eq!(
            catalog_calls.load(Ordering::Relaxed),
            1,
            "catalog gate was exercised"
        );
        assert!(!client.inner.route_live.load(Ordering::Acquire));
        assert!(
            client.inner.state.lock().last_publish_at.is_empty(),
            "unavailable publish must release its cadence reservation"
        );
    }

    /// Opens the holder route, has the daemon close it for `reason` (the
    /// `route.closed` push, then the end of the route), lets several discovery
    /// cadences pass, and returns how many route opens the dial made.
    async fn route_opens_after_holder_close(reason: &str) -> usize {
        route_opens_after_holder_close_naming(reason, None).await
    }

    /// [`route_opens_after_holder_close`] with a push that names the closed
    /// client `channels`, as a subc-control 0.27 daemon sends it. The test
    /// consumer's first route is channel 1.
    async fn route_opens_after_holder_close_naming(
        reason: &str,
        channels: Option<&[u16]>,
    ) -> usize {
        let (client, mut wire_rx) = FleetStatusClient::dial_channel(1);
        assert!(!client.publish(Path::new("/tmp/project"), "opencode", "session-1", "local"));
        let first_request = wire_rx.try_recv().expect("first discovery request");
        let identity = FleetRouteIdentity::from(&first_request);
        // Answered here rather than handed to the dial: the test consumer
        // fails every publish, and a failed publish drops the route itself.
        first_request.complete_unavailable();
        let attempts = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let push_sender = Arc::new(parking_lot::Mutex::new(None));
        let control_sender = Arc::new(parking_lot::Mutex::new(None));
        let consumer = RejectingConsumer {
            attempts: attempts.clone(),
            succeeds_on: Some(1),
            advertises_status_line: true,
            catalog_calls: Arc::default(),
            publish_calls: Arc::default(),
            connection_callback: Arc::default(),
            push_sender: push_sender.clone(),
            control_sender: control_sender.clone(),
        };
        let task = tokio::spawn(run_connected_status_dial(
            consumer, client, wire_rx, identity, None,
        ));
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        assert_eq!(attempts.lock().len(), 1, "the holder route opened");

        let mut body = json!({
            "op": "route.closed",
            "module_id": STATUS_HOLDER_MODULE,
            "reason": reason,
        });
        if let Some(channels) = channels {
            body["channels"] = json!(channels);
        }
        let push = ControlPush {
            op: "route.closed".to_owned(),
            body,
        };
        control_sender
            .lock()
            .as_ref()
            .expect("dial registered for control pushes")
            .try_send(push)
            .expect("queue route.closed");
        push_sender.lock().take();
        for _ in 0..3 {
            tokio::time::advance(STATUS_CADENCE).await;
            for _ in 0..4 {
                tokio::task::yield_now().await;
            }
        }
        task.abort();
        let opens = attempts.lock().len();
        opens
    }

    #[tokio::test(start_paused = true)]
    async fn scope_and_unknown_route_closes_never_reopen_the_holder_route() {
        for reason in [
            "scope_ended",
            "scope_carrier_removed",
            "scope_delegation_changed",
            "scope_parent_ended",
            "a_reason_from_a_newer_daemon",
        ] {
            assert_eq!(
                route_opens_after_holder_close(reason).await,
                1,
                "a {reason} close must not reopen the route"
            );
        }
        assert!(
            route_opens_after_holder_close("reload").await > 1,
            "a reload close still reopens on the next discovery"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_scope_close_naming_channels_applies_only_to_the_dial_route() {
        assert_eq!(
            route_opens_after_holder_close_naming("scope_ended", Some(&[1])).await,
            1,
            "a scope close naming the dial's channel must not reopen the route"
        );
        assert!(
            route_opens_after_holder_close_naming("scope_ended", Some(&[7, 9])).await > 1,
            "a scope close naming only other channels is not about the dial's route"
        );
    }

    async fn advance_and_observe(
        attempts: &Arc<parking_lot::Mutex<Vec<tokio::time::Instant>>>,
        elapsed: Duration,
        expected: usize,
    ) {
        tokio::time::advance(elapsed).await;
        tokio::task::yield_now().await;
        assert_eq!(
            attempts.lock().len(),
            expected,
            "attempt count after {elapsed:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn rejected_binds_back_off_to_cap() {
        let (task, attempts, _) = dial_with_rejections(None).await;
        let start = attempts.lock()[0];
        for tick in 1..=71 {
            advance_and_observe(
                &attempts,
                DISCOVERY_INITIAL_BACKOFF,
                1 + [1, 3, 7, 15, 31, 51, 71]
                    .iter()
                    .filter(|&&attempt_tick| attempt_tick <= tick)
                    .count(),
            )
            .await;
        }
        let observed: Vec<_> = attempts.lock().iter().map(|at| *at - start).collect();
        assert_eq!(
            observed,
            vec![
                Duration::ZERO,
                Duration::from_millis(250),
                Duration::from_millis(750),
                Duration::from_millis(1750),
                Duration::from_millis(3750),
                Duration::from_millis(7750),
                Duration::from_millis(12750),
                Duration::from_millis(17750),
            ]
        );
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn successful_bind_resets_rejection_backoff() {
        let (task, attempts, callback) = dial_with_rejections(Some(4)).await;
        for (elapsed, expected) in [
            (Duration::from_millis(250), 2),
            (Duration::from_millis(500), 3),
            (Duration::from_secs(1), 4),
        ] {
            advance_and_observe(&attempts, elapsed, expected).await;
        }
        callback.lock().as_ref().expect("connection callback")(ConnectionState::Dropped);
        tokio::task::yield_now().await;
        assert_eq!(attempts.lock().len(), 5, "drop rediscovers immediately");
        let fifth = attempts.lock()[4];
        advance_and_observe(&attempts, Duration::from_millis(250), 6).await;
        assert_eq!(attempts.lock()[5] - fifth, DISCOVERY_INITIAL_BACKOFF);
        task.abort();
    }

    #[test]
    fn publish_ack_tolerates_fields_a_newer_holder_adds() {
        // The status holder is a separately released module, and its publish
        // ack is on every response's hot path. A newer holder adds fields (first
        // `last_read_at_ms`, so aft can tell whether anything reads a scope);
        // an ack decoder that refused unknown keys would turn that addition into
        // a failed publish on every call. Pin that unknown keys are ignored.
        let ack = StatusPublishAck::parse(
            br#"{"epoch":3,"accepted_revision":7,"last_read_at_ms":1790115735811,"future_field":{"nested":true}}"#,
        )
        .expect("an ack carrying unknown keys still parses");
        assert_eq!(
            ack,
            StatusPublishAck {
                epoch: 3,
                accepted_revision: 7,
                last_read_at_ms: Some(1790115735811),
            }
        );
        let null_read =
            StatusPublishAck::parse(br#"{"epoch":3,"accepted_revision":7,"last_read_at_ms":null}"#);
        assert_eq!(
            null_read
                .expect("a null last_read_at_ms still parses")
                .last_read_at_ms,
            None
        );
    }

    #[test]
    fn publish_ack_fixtures_drive_runtime_fencing() {
        let fixtures = fixtures();
        assert_eq!(fixtures.len(), 18, "fixture probe entry count changed");
        let mut fence = PublishFence::default();
        for name in PUBLISH_ACK_FIXTURES {
            fence.observe(parse_ack(&fixtures[name]));
        }
        assert_eq!(
            fence,
            PublishFence {
                epoch: 4,
                accepted_revision: 1
            }
        );
    }

    #[test]
    fn line_reply_fixtures_remain_wire_shape_documentation() {
        let fixtures = fixtures();
        let documented = PUBLISH_ACK_FIXTURES
            .into_iter()
            .chain(LINE_REPLY_FIXTURES)
            .collect::<HashSet<_>>();
        assert_eq!(documented.len(), fixtures.len());
        assert!(fixtures
            .keys()
            .all(|name| documented.contains(name.as_str())));

        let lines = LINE_REPLY_FIXTURES
            .into_iter()
            .map(|name| (name, parse_line(&fixtures[name])))
            .collect::<HashMap<_, _>>();
        assert_eq!(
            lines["line_foreign_present"].line,
            "aft E0 W0 idx fresh | pfc 1567 events (777 gaps)"
        );
        assert!(lines["line_foreign_present"].has_foreign_segment());
        assert_eq!(
            lines["supersede_line_after_r3"],
            lines["supersede_line_after_late_r2"]
        );
        assert!(lines["supersede_line_after_late_r2"]
            .line
            .contains("revision three"));
        assert_eq!(lines["ttl_line_before"].segments.len(), 4);
        assert_eq!(lines["ttl_line_after_expiry"].segments.len(), 3);
        assert_eq!(
            lines["quiet_line"]
                .segments
                .last()
                .map(|segment| segment.text.as_str()),
            Some("")
        );
        assert!(!lines["quiet_line"].line.contains("ttlfx"));

        let capped = &lines["line_cap_overflow"];
        assert!(capped.line.chars().count() <= 200);
        assert_eq!(
            capped.segments.len(),
            6,
            "the cap must not truncate segments[]"
        );
        assert!(
            !capped.line.contains("fat2"),
            "whole tail segments are dropped"
        );
        assert_eq!(
            lines["epoch_bump_line_after"].segments[3].text,
            "fresh epoch revision one"
        );
        assert!(!lines["line_aft_solo_scope"].has_foreign_segment());
    }

    #[test]
    fn publication_fence_drops_regressions_but_new_epoch_wins() {
        let fixtures = fixtures();
        let mut fence = PublishFence::default();
        fence.observe(parse_ack(&fixtures["supersede_r3"]));
        fence.observe(StatusPublishAck {
            epoch: 3,
            accepted_revision: 2,
            last_read_at_ms: None,
        });
        assert_eq!(
            fence,
            PublishFence {
                epoch: 3,
                accepted_revision: 3
            }
        );

        fence.observe(parse_ack(&fixtures["epoch_bump_republish_r1_new_conn"]));
        assert_eq!(
            fence,
            PublishFence {
                epoch: 4,
                accepted_revision: 1
            }
        );
    }

    #[test]
    fn catalog_gate_requires_matching_module_management_role_and_exact_operation() {
        assert!(catalog_advertises_status_line(&[status_catalog_entry(
            STATUS_HOLDER_MODULE,
            STATUS_LINE_OPERATION,
        )]));
        assert!(!catalog_advertises_status_line(&[status_catalog_entry(
            "other-module",
            STATUS_LINE_OPERATION,
        )]));
        assert!(!catalog_advertises_status_line(&[status_catalog_entry(
            STATUS_HOLDER_MODULE,
            "status.lines",
        )]));
    }

    #[test]
    fn dial_channel_queues_discovery_without_claiming_the_status_bar() {
        let (client, mut wire_rx) = FleetStatusClient::dial_channel(1);

        assert!(!client.publish(Path::new("/tmp/project"), "opencode", "session-1", "local"));
        let request = wire_rx.try_recv().expect("discovery request");
        assert_eq!(request.project_root(), "/tmp/project");
        assert_eq!(request.harness(), "opencode");
        assert_eq!(request.session(), "session-1");
        request.complete_unavailable();
    }

    #[test]
    fn closed_dial_channel_falls_back_to_the_solo_status_bar() {
        let (client, wire_rx) = FleetStatusClient::channel(1);
        drop(wire_rx);

        assert!(!client.publish(Path::new("/tmp/project"), "opencode", "session-1", "local"));
        assert!(!client.inner.route_live.load(Ordering::Acquire));
    }

    #[test]
    fn dormant_client_falls_back_without_publishing() {
        let client = FleetStatusClient::dormant();

        assert!(!client.publish(
            Path::new("/tmp/project"),
            "opencode",
            "session-1",
            "E0 W0 | D0 U0 C0 | T0"
        ));
        let state = client.inner.state.lock();
        assert!(state.last_publish_at.is_empty());
        assert_eq!(client.inner.next_revision.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn empty_local_status_publishes_alive_quiet() {
        let (client, mut wire_rx) = FleetStatusClient::channel(4);

        assert!(client.publish(Path::new("/tmp/project"), "opencode", "session-1", ""));
        let publish = wire_rx.try_recv().expect("publish request");
        assert_eq!(publish.body()["op"], "status.publish");
        assert_eq!(publish.body()["text"], "");
        assert_eq!(publish.body()["ttl_ms"], STATUS_PUBLISH_TTL_MS);
        assert!(
            wire_rx.try_recv().is_err(),
            "publisher emitted a pull request"
        );
    }

    #[test]
    fn cadence_suppresses_duplicate_publishes_and_ack_updates_fence() {
        let (client, mut wire_rx) = FleetStatusClient::channel(4);
        let fixtures = fixtures();
        let publish_fixture =
            serde_json::to_vec(&fixtures["publish_aft"]).expect("publish fixture bytes");

        assert!(client.publish(Path::new("/tmp/project"), "opencode", "session-1", "local"));
        let publish = wire_rx.try_recv().expect("first publish request");
        assert_eq!(publish.body()["revision"], 1);
        assert!(publish.complete_response(&publish_fixture));

        assert!(client.publish(Path::new("/tmp/project"), "opencode", "session-1", "local"));
        assert!(
            wire_rx.try_recv().is_err(),
            "publish cadence issued another request"
        );
        assert_eq!(
            client.publish_fence(),
            PublishFence {
                epoch: 3,
                accepted_revision: 1
            }
        );
    }

    fn ack_with_read(revision: u64, last_read_at_ms: Option<u64>) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "epoch": 1,
            "accepted_revision": revision,
            "last_read_at_ms": last_read_at_ms,
        }))
        .expect("ack bytes")
    }

    #[test]
    fn read_freshness_is_bounded_by_the_window_and_tolerates_a_holder_clock_ahead() {
        let ack_at = 1_000_000;
        assert!(read_is_fresh(Some(ack_at - READER_FRESH_WINDOW_MS), ack_at));
        assert!(!read_is_fresh(
            Some(ack_at - READER_FRESH_WINDOW_MS - 1),
            ack_at
        ));
        assert!(read_is_fresh(Some(ack_at + 5_000), ack_at));
        assert!(!read_is_fresh(None, ack_at));
    }

    #[test]
    fn reader_presence_is_per_scope_and_follows_the_latest_ack() {
        let (client, mut wire_rx) = FleetStatusClient::channel(4);
        let read = Path::new("/tmp/read");
        let unread = Path::new("/tmp/unread");
        assert!(!client.reader_present(read), "no ack yet is no evidence");

        assert!(client.publish(read, "opencode", "session", "text"));
        assert!(wire_rx
            .try_recv()
            .unwrap()
            .complete_response(&ack_with_read(1, Some(unix_now_ms() - 2_000))));
        assert!(client.publish(unread, "opencode", "session", "text"));
        assert!(wire_rx
            .try_recv()
            .unwrap()
            .complete_response(&ack_with_read(2, None)));
        assert!(client.reader_present(read));
        assert!(!client.reader_present(unread));

        // The reader stops: the next ack reports a read older than the window.
        client.inner.state.lock().last_publish_at.clear();
        assert!(client.publish(read, "opencode", "session", "text"));
        assert!(wire_rx
            .try_recv()
            .unwrap()
            .complete_response(&ack_with_read(
                3,
                Some(unix_now_ms() - READER_FRESH_WINDOW_MS - 5_000)
            )));
        assert!(!client.reader_present(read));
    }

    #[test]
    fn holder_without_read_field_or_dropped_route_is_no_reader() {
        let (client, mut wire_rx) = FleetStatusClient::channel(4);
        let root = Path::new("/tmp/project");
        assert!(client.publish(root, "opencode", "session", "text"));
        assert!(wire_rx
            .try_recv()
            .unwrap()
            .complete_response(br#"{"epoch":1,"accepted_revision":1}"#));
        assert!(
            !client.reader_present(root),
            "an older holder proves no reader"
        );

        client.inner.state.lock().last_publish_at.clear();
        assert!(client.publish(root, "opencode", "session", "text"));
        assert!(wire_rx
            .try_recv()
            .unwrap()
            .complete_response(&ack_with_read(2, Some(unix_now_ms()))));
        assert!(client.reader_present(root));
        client.set_route_live(false);
        assert!(
            !client.reader_present(root),
            "a dropped route has no reader"
        );
        client.set_route_live(true);
        assert!(
            !client.reader_present(root),
            "evidence from the dropped route does not survive a rebind"
        );
    }
}
