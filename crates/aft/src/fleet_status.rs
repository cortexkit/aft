//! Publisher for AFT's segment on the fleet status-holder plane.
//!
//! The holder owns retention and composed rendering. While its route is live,
//! AFT publishes its project-scoped segment and leaves status-bar attachment to
//! the holder's host plugin.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};
use subc_client_rs::{
    CallOptions, CloseRouteOptions, ConnectionState, ConsumerOptions, RouteHandle, SubcConsumer,
};
use subc_protocol::manifest::ProviderRole;
use subc_protocol::{BindIdentity, RouteTarget};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const STATUS_CADENCE: Duration = Duration::from_millis(2_500);
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
            self.inner.state.lock().last_publish_at.clear();
        }
    }

    /// Publish AFT's segment when the holder route is live. The return value is
    /// ownership, not delivery: `true` means the holder's host plugin owns the
    /// response bar even when cadence, contention, or backpressure skips a send.
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
        let scope = format!("project:{}", project_root.to_string_lossy());
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
        client.state.lock().publish_fence.observe(ack);
        true
    }

    pub(crate) fn complete_unavailable(self) {
        if let Some(client) = self.client.upgrade() {
            client.route_live.store(false, Ordering::Release);
            client.state.lock().last_publish_at.clear();
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

    let (connection_state_tx, mut connection_state_rx) = mpsc::unbounded_channel();
    let connection_state_client = client.clone();
    consumer.on_connection_state(move |state| {
        connection_state_client.set_route_live(false);
        let _ = connection_state_tx.send(state);
    });

    let mut route: Option<RouteHandle> = None;
    let mut route_events = None;
    let mut next_discovery_at = tokio::time::Instant::now();
    let mut discovery_backoff = DISCOVERY_INITIAL_BACKOFF;
    loop {
        if tokio::time::Instant::now() >= next_discovery_at {
            match consumer.catalog_list().await {
                Ok(catalog) if catalog_advertises_status_line(&catalog.modules) => {
                    discovery_backoff = DISCOVERY_INITIAL_BACKOFF;
                    if route.is_none() {
                        let identity = BindIdentity {
                            project_root: route_identity.project_root.clone().into(),
                            harness: route_identity.harness.clone(),
                            session: route_identity.session.clone(),
                        };
                        match consumer
                            .open_route(
                                RouteTarget::ManagementSurface {
                                    module_id: STATUS_HOLDER_MODULE.to_string(),
                                },
                                identity,
                                CallOptions::default(),
                            )
                            .await
                        {
                            Ok(opened_route) => match consumer.push_events(&opened_route) {
                                Ok(events) => {
                                    route_events = Some(events);
                                    route = Some(opened_route);
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
                    next_discovery_at = tokio::time::Instant::now() + STATUS_CADENCE;
                }
                Ok(_) => {
                    if let Some(opened_route) = route.take() {
                        let _ = consumer
                            .close_handle(&opened_route, CloseRouteOptions::default())
                            .await;
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
            match consumer
                .request(opened_route, body, CallOptions::default())
                .await
            {
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
                    Some(ConnectionState::Dropped | ConnectionState::Restored { .. }) => {
                        route = None;
                        route_events = None;
                        client.set_route_live(false);
                        next_discovery_at = tokio::time::Instant::now();
                    }
                    None => {}
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
    ["SUBC_MODULE_ID", "SUBC_LAUNCH_NONCE"].iter().all(|key| {
        std::env::var(key)
            .ok()
            .is_some_and(|value| !value.trim().is_empty())
    })
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
            module_version: None,
            roles: vec![ProviderRole::ManagementSurface {
                operations: vec![ManagementOperation {
                    name: operation.to_string(),
                    kind: ManagementOperationKind::Query,
                }],
                config_schema: Value::Null,
                observability: Vec::new(),
                identity_scope: Vec::new(),
            }],
            control_ops: Vec::new(),
        }
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
}
