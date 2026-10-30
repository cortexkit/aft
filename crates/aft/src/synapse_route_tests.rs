use super::*;
use std::sync::{Arc, Condvar};
use subc_protocol::{Flags, Frame, FrameType, Priority};
use subc_transport::connection_file::{self, ConnectionInfo, Endpoint, SCHEMA_VERSION};

#[derive(Default)]
pub(crate) struct Counts {
    pub opens: usize,
    pub closes: usize,
    pub active: usize,
    pub queries: usize,
    pub connections: usize,
}

pub(crate) struct Daemon {
    _directory: tempfile::TempDir,
    pub path: PathBuf,
    pub counts: Arc<(Mutex<Counts>, Condvar)>,
    stop: tokio::sync::oneshot::Sender<()>,
    server: std::thread::JoinHandle<()>,
    release: tokio::sync::watch::Sender<bool>,
    release_open: tokio::sync::watch::Sender<bool>,
    disconnect: tokio::sync::watch::Sender<bool>,
}

impl Daemon {
    pub fn start(query_group: usize, disconnect_first_group: bool) -> Self {
        Self::start_inner(query_group, disconnect_first_group, None)
    }

    pub fn with_held_consent_open(scope: Value) -> Self {
        Self::start_inner(1, false, Some(scope))
    }

    fn start_inner(
        query_group: usize,
        disconnect_first_group: bool,
        held_open_scope: Option<Value>,
    ) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("connection.json");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let key = vec![0x42; subc_transport::KEY_LEN];
        let daemon_id = [0x24; subc_transport::DAEMON_ID_LEN];
        connection_file::write_atomic(
            &path,
            &ConnectionInfo {
                schema: SCHEMA_VERSION,
                wire_version: Some(subc_protocol::PROTOCOL_VERSION),
                endpoints: vec![Endpoint {
                    host: "127.0.0.1".into(),
                    port: listener.local_addr().unwrap().port(),
                }],
                key: key.clone(),
                daemon_id,
                pid: std::process::id(),
                daemon_ver: "route-test".into(),
            },
        )
        .unwrap();
        let counts = Arc::new((Mutex::new(Counts::default()), Condvar::new()));
        let recorded = counts.clone();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::watch::channel(false);
        let (release_open, open_released) = tokio::sync::watch::channel(false);
        let (disconnect, disconnected) = tokio::sync::watch::channel(false);
        let server = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                let mut tasks = tokio::task::JoinSet::new();
                tokio::pin!(stopped);
                loop {
                    tokio::select! {
                        _ = &mut stopped => break,
                        accepted = listener.accept() => {
                            let (mut stream, _) = accepted.unwrap();
                            let key = key.clone();
                            let counts = recorded.clone();
                            let mut released = released.clone();
                            let mut open_released = open_released.clone();
                            let mut disconnected = disconnected.clone();
                            let held_open_scope = held_open_scope.clone();
                            tasks.spawn(async move {
                                subc_transport::authenticate_server(&mut stream, &key, &daemon_id, "route-test", Duration::from_secs(2)).await.unwrap();
                                let connection = {
                                    let mut count = counts.0.lock().unwrap();
                                    count.connections += 1;
                                    count.connections
                                };
                                let mut active = 0;
                                let mut pending = Vec::new();
                                let mut held = Vec::new();
                                let mut held_opens = Vec::new();
                                let mut generation = 0;
                                loop {
                                    let request = tokio::select! {
                                        Ok(()) = disconnected.changed(), if connection == 1 => break,
                                        request = subc_transport::read_frame(&mut stream) => match request {
                                            Ok(Some(request)) => request,
                                            _ => break,
                                        },
                                        Ok(()) = released.changed() => {
                                            for (request, value) in held.drain(..) {
                                                respond(&mut stream, &request, value, false).await;
                                            }
                                            continue;
                                        },
                                        Ok(()) = open_released.changed() => {
                                            for (request, value) in held_opens.drain(..) {
                                                respond(&mut stream, &request, value, false).await;
                                            }
                                            continue;
                                        }
                                    };
                                    // Slow-query tests withhold application replies, not transport
                                    // liveness. Ignoring probes makes the SDK retire a healthy route.
                                    if request.header.ty == FrameType::Ping {
                                        let pong = Frame::build(FrameType::Pong, Flags::new(false, Priority::Interactive, false), 0, 0, request.header.corr, Vec::new()).unwrap();
                                        subc_transport::write_frame(&mut stream, &pong).await.unwrap();
                                        continue;
                                    }
                                    if request.header.ty == FrameType::Goodbye {
                                        active -= 1;
                                        let mut count = counts.0.lock().unwrap();
                                        count.active -= 1;
                                        count.closes += 1;
                                        counts.1.notify_all();
                                        continue;
                                    }
                                    if request.header.ty != FrameType::Request { continue; }
                                    let value: Value = serde_json::from_slice(&request.body).unwrap();
                                    let reply = match value["op"].as_str() {
                                        Some("catalog.list") => json!({"op":"catalog.list","generation":1,"modules":[{
                                            "module_id":"synapse","module_version":"1","roles":[{"role":"management_surface",
                                            "operations":[{"name":"models.list","kind":"query"},{"name":"embed.query","kind":"query"},{"name":"embed.batch","kind":"query"},{"name":"rerank.score","kind":"query"}],
                                            "config_schema":{"type":"object"},"observability":[],"identity_scope":[]}],"control_ops":[]}],"subc_ops":["catalog.list","route.open"]}),
                                        Some("route.open") => {
                                            {
                                                let mut count = counts.0.lock().unwrap();
                                                count.opens += 1;
                                                generation = count.opens;
                                                count.active += 1;
                                            }
                                            active += 1;
                                            counts.1.notify_all();
                                            let reply = json!({"op":"route.open","route_channel":6 + generation,"route_epoch":1});
                                            if held_open_scope.as_ref() == Some(&value["scope"]) && !*open_released.borrow() {
                                                held_opens.push((request, reply));
                                                continue;
                                            }
                                            reply
                                        },
                                        _ if value["method"] == MODELS_LIST_OPERATION => json!({"models":[{"model":pause_test_support::MODEL,"fingerprint":"pinned","table_epoch":1,"dims":2,"recommended_batch":{"rows":100,"token_budget":8192}}]}),
                                        _ if value["method"] == QUERY_OPERATION || value["method"] == "rerank.score" => {
                                            counts.0.lock().unwrap().queries += 1;
                                            counts.1.notify_all();
                                            match value["params"]["text"].as_str() {
                                                Some("timeout") => continue,
                                                Some("held") if !*released.borrow() => {
                                                    held.push((request, query_reply(&value)));
                                                    continue;
                                                },
                                                Some("module-error") => {
                                                    respond(&mut stream, &request, json!({"code":"test_refused","message":"request rejected"}), true).await;
                                                    continue;
                                                },
                                                Some("malformed") => {
                                                    let frame = Frame::build(FrameType::Response, Flags::new(false, Priority::Interactive, false), request.header.channel, request.header.epoch, request.header.corr, b"{".to_vec()).unwrap();
                                                    subc_transport::write_frame(&mut stream, &frame).await.unwrap();
                                                    continue;
                                                },
                                                Some("application-error") => {
                                                    respond(&mut stream, &request, json!({"error":{"code":"test_rejected","message":"application rejected"}}), false).await;
                                                    continue;
                                                },
                                                _ => {}
                                            }
                                            pending.push((request, value));
                                            if pending.len() < query_group { continue; }
                                            // The real consumer enforces its 1024-request route window.
                                            // Holding these replies proves requests overlap on the wire.
                                            if disconnect_first_group && generation == 1 {
                                                // Keep every caller pending until the test drops the socket.
                                                continue;
                                            }
                                            for (request, value) in pending.drain(..) {
                                                respond(&mut stream, &request, query_reply(&value), false).await;
                                            }
                                            continue;
                                        },
                                        _ if value["method"] == "consent.test_timeout" => {
                                            counts.0.lock().unwrap().queries += 1;
                                            counts.1.notify_all();
                                            continue;
                                        },
                                        _ if value["method"] == "consent.test_held" => {
                                            counts.0.lock().unwrap().queries += 1;
                                            counts.1.notify_all();
                                            if !*released.borrow() {
                                                held.push((request, json!({"result":{"ok":true}})));
                                                continue;
                                            }
                                            json!({"result":{"ok":true}})
                                        },
                                        _ if value["method"] == "consent.test_error" => {
                                            respond(&mut stream, &request, json!({"code":"test_refused","message":"refused"}), true).await;
                                            continue;
                                        },
                                        _ => json!({"result":{"ok":true}}),
                                    };
                                    respond(&mut stream, &request, reply, false).await;
                                }
                                counts.0.lock().unwrap().active -= active;
                                counts.1.notify_all();
                            });
                        }
                    }
                }
                tasks.abort_all();
            });
        });
        Self {
            _directory: directory,
            path,
            counts,
            stop,
            server,
            release,
            release_open,
            disconnect,
        }
    }

    pub fn release_consent_open(&self) {
        self.release_open.send(true).unwrap();
    }

    pub fn release_replies(&self) {
        self.release.send(true).unwrap();
    }

    fn disconnect_first_connection(&self) {
        self.disconnect.send(true).unwrap();
    }

    pub fn config(&self, root: usize) -> SemanticBackendConfig {
        SemanticBackendConfig {
            backend: crate::config::SemanticBackend::Synapse,
            model: pause_test_support::MODEL.into(),
            subc_connection_file: Some(self.path.clone()),
            route_project_root: Some(self._directory.path().join(format!("root-{root}"))),
            timeout_ms: 5_000,
            query_instruction: "off".into(),
            ..Default::default()
        }
    }

    pub fn wait_closed(&self) {
        let (count, _) = self
            .counts
            .1
            .wait_timeout_while(
                self.counts.0.lock().unwrap(),
                Duration::from_secs(2),
                |count| count.active != 0,
            )
            .unwrap();
        assert_eq!(count.active, 0, "completed clients left routes open");
    }

    fn wait_queries(&self, queries: usize) {
        self.wait_queries_with_timeout(queries, Duration::from_secs(2));
    }

    fn wait_queries_with_timeout(&self, queries: usize, timeout: Duration) {
        let (counts, _) = self
            .counts
            .1
            .wait_timeout_while(self.counts.0.lock().unwrap(), timeout, |counts| {
                counts.queries < queries
            })
            .unwrap();
        assert!(
            counts.queries >= queries,
            "expected {queries} requests on the wire, received {}",
            counts.queries
        );
    }

    pub fn finish(self) {
        let _ = self.stop.send(());
        self.server.join().unwrap();
    }
}

fn query_reply(value: &Value) -> Value {
    json!({"fingerprint":"pinned","table_epoch":1,"vectors":[{"id":value["params"]["id"],"content_sha256":value["params"]["content_sha256"],"vector":[1.0,0.0]}]})
}

async fn respond(stream: &mut tokio::net::TcpStream, request: &Frame, value: Value, error: bool) {
    let frame = Frame::build_with_version(
        request.header.ver,
        if error {
            FrameType::Error
        } else {
            FrameType::Response
        },
        Flags::new(false, Priority::Interactive, false),
        request.header.channel,
        request.header.epoch,
        request.header.corr,
        serde_json::to_vec(&value).unwrap(),
    )
    .unwrap();
    let _ = subc_transport::write_frame(stream, &frame).await;
}

#[test]
fn synapse_semantic_models_share_one_route_across_roots() {
    let daemon = Daemon::start(1, false);
    let models = (0..12)
        .map(|n| {
            crate::semantic_index::SemanticEmbeddingModel::from_config(&daemon.config(n % 3))
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        daemon.counts.0.lock().unwrap().opens,
        1,
        "semantic models must share their management route"
    );
    drop(models);
    daemon.wait_closed();
    daemon.finish();
}

#[test]
fn synapse_shared_route_keeps_lane_pauses_on_the_requesting_root() {
    use pause_test_support::*;
    crate::semantic_index::with_lane_pause_clock(START, |clock| {
        with_fake_synapse(clock, None, |first_config, _| {
            let mut second_config = first_config.clone();
            let first_root = first_config.route_project_root.as_ref().unwrap();
            let second_root = first_root.join("second-root");
            second_config.route_project_root = Some(second_root.clone());
            let mut first = SynapseEmbeddingClient::from_config(&first_config).unwrap();
            let mut second = SynapseEmbeddingClient::from_config(&second_config).unwrap();
            assert!(Arc::ptr_eq(
                &first.state.connection,
                &second.state.connection
            ));
            assert!(matches!(
                first.embed_query("first", Duration::from_secs(1)),
                Err(SynapseEmbeddingError::LanePaused(_))
            ));
            assert!(lane_pause_for_root(first_root).is_some());
            assert!(lane_pause_for_root(&second_root).is_none());
            assert!(matches!(
                second.embed_query("second", Duration::from_secs(1)),
                Err(SynapseEmbeddingError::LanePaused(_))
            ));
            assert!(lane_pause_for_root(&second_root).is_some());
        });
    });
}

#[test]
fn transient_retry_time_extends_only_the_pause_it_waits_on() {
    use pause_test_support::*;
    crate::semantic_index::with_lane_pause_clock(START, |clock| {
        // No hint: both roots record identical pauses that end at once.
        let reply = serde_json::json!({"result": {"error": {
            "class": "transient", "code": "model_loading", "message": "loading",
        }}});
        with_fake_synapse_refusing(
            clock,
            START + 1,
            move |_| reply.clone(),
            |first_config, _| {
                let mut second_config = first_config.clone();
                let first_root = first_config.route_project_root.clone().unwrap();
                let second_root = first_root.join("second-root");
                second_config.route_project_root = Some(second_root.clone());
                let mut first = SynapseEmbeddingClient::from_config(&first_config).unwrap();
                let mut second = SynapseEmbeddingClient::from_config(&second_config).unwrap();
                let Err(SynapseEmbeddingError::LanePaused(first_pause)) =
                    first.embed_query("first", Duration::from_secs(1))
                else {
                    panic!("the first root was not paused");
                };
                assert!(matches!(
                    second.embed_query("second", Duration::from_secs(1)),
                    Err(SynapseEmbeddingError::LanePaused(_))
                ));
                let until = |root: &Path| lane_pause_for_root(root).map(|p| p.paused_until_ms);

                // The first root's worker will retry after a 15 s backoff.
                note_pause_retry_at(&first_pause, START + 15_000);
                assert_eq!(until(&first_root), Some(START + 15_000));
                assert_eq!(
                    until(&second_root),
                    None,
                    "another root's identical pause took this root's retry time"
                );

                // A newer refusal replaces the first root's pause; a late note
                // for the older pause must not extend its replacement.
                assert!(matches!(
                    first.embed_query("again", Duration::from_secs(1)),
                    Err(SynapseEmbeddingError::LanePaused(_))
                ));
                note_pause_retry_at(&first_pause, START + 30_000);
                assert_eq!(
                    until(&first_root),
                    None,
                    "an older pause's retry time extended its replacement"
                );
            },
        );
    });
}

#[test]
fn synapse_shared_reset_reconnects_once_without_serializing_callers() {
    const CALLERS: usize = 8;
    let daemon = Daemon::start(CALLERS, true);
    let clients = (0..CALLERS)
        .map(|n| {
            let mut config = daemon.config(n);
            config.timeout_ms = 2_000;
            SynapseEmbeddingClient::from_config(&config).unwrap()
        })
        .collect::<Vec<_>>();
    let shared = Arc::clone(&clients[0].state.connection);
    let old = shared
        .runtime
        .block_on(clients[0].state.ensure_route())
        .unwrap();
    let (dropped, observed_drop) = std::sync::mpsc::channel();
    old.consumer.on_connection_state(move |state| {
        if matches!(state, subc_client_rs::ConnectionState::Dropped) {
            let _ = dropped.send(());
        }
    });
    let barrier = Arc::new(std::sync::Barrier::new(CALLERS));
    std::thread::scope(|scope| {
        let threads = clients
            .into_iter()
            .map(|mut client| {
                let barrier = Arc::clone(&barrier);
                scope.spawn(move || {
                    barrier.wait();
                    assert_eq!(
                        client
                            .embed_query("concurrent", Duration::from_secs(5))
                            .unwrap(),
                        vec![1.0, 0.0]
                    );
                    client
                })
            })
            .collect::<Vec<_>>();
        daemon.wait_queries(CALLERS);
        // Hold AFT's reset behind the slot lock while the SDK observes the loss.
        // A control call drives the old consumer's reconnect path to completion:
        // it must fail, not authenticate a spare socket before AFT replaces it.
        let resetting = shared.runtime.block_on(shared.active.lock());
        daemon.disconnect_first_connection();
        observed_drop.recv_timeout(Duration::from_secs(5)).unwrap();
        let retired_catalog = shared.runtime.block_on(old.consumer.catalog_list());
        drop(resetting);
        assert!(
            retired_catalog.is_err(),
            "the retired SDK consumer reconnected independently of AFT"
        );
        let clients = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            daemon.counts.0.lock().unwrap().opens,
            2,
            "one failed generation must cause only one reconnect"
        );
        assert_eq!(daemon.counts.0.lock().unwrap().queries, CALLERS * 2);
        assert_eq!(daemon.counts.0.lock().unwrap().connections, 2);
        let replacement = shared
            .runtime
            .block_on(clients[0].state.ensure_route())
            .unwrap();
        assert!(!Arc::ptr_eq(&old, &replacement));
        shared.runtime.block_on(shared.reset(&old));
        for client in &clients {
            let current = shared
                .runtime
                .block_on(client.state.ensure_route())
                .unwrap();
            assert!(
                Arc::ptr_eq(&replacement, &current),
                "late reset replaced a healthy connection"
            );
        }
    });
    drop(old);
    drop(shared);
    daemon.wait_closed();
    daemon.finish();
}

#[test]
fn synapse_idle_disconnect_reconnects_once_on_the_next_query() {
    let daemon = Daemon::start(1, false);
    let mut client = SynapseEmbeddingClient::from_config(&daemon.config(0)).unwrap();
    let shared = Arc::clone(&client.state.connection);
    let old = shared
        .runtime
        .block_on(client.state.ensure_route())
        .unwrap();
    let (dropped, observed_drop) = std::sync::mpsc::channel();
    old.consumer.on_connection_state(move |state| {
        if matches!(state, subc_client_rs::ConnectionState::Dropped) {
            let _ = dropped.send(());
        }
    });
    daemon.disconnect_first_connection();
    observed_drop.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(daemon.counts.0.lock().unwrap().queries, 0);
    assert_eq!(
        client
            .embed_query("after-idle-disconnect", Duration::from_secs(5))
            .unwrap(),
        vec![1.0, 0.0]
    );
    let replacement = shared
        .runtime
        .block_on(client.state.ensure_route())
        .unwrap();
    assert!(!Arc::ptr_eq(&old, &replacement));
    {
        let counts = daemon.counts.0.lock().unwrap();
        assert_eq!(counts.opens, 2);
        assert_eq!(counts.connections, 2);
        assert_eq!(counts.queries, 1);
    }
    drop(replacement);
    drop(old);
    drop(client);
    drop(shared);
    daemon.wait_closed();
    daemon.finish();
}

#[test]
fn synapse_request_timeout_does_not_reset_another_roots_inflight_request() {
    let daemon = Daemon::start(1, false);
    let mut impatient = SynapseEmbeddingClient::from_config(&daemon.config(0)).unwrap();
    let mut patient = SynapseEmbeddingClient::from_config(&daemon.config(1)).unwrap();
    let shared = Arc::clone(&impatient.state.connection);
    let original = shared
        .runtime
        .block_on(impatient.state.ensure_route())
        .unwrap();
    std::thread::scope(|scope| {
        let answer = scope.spawn(move || {
            let result = patient.embed_query("held", Duration::from_secs(10));
            (patient, result)
        });
        daemon.wait_queries(1);
        let result = impatient.embed_query("timeout", Duration::from_millis(100));
        daemon.release.send(true).unwrap();
        let (patient, answer) = answer.join().unwrap();
        // One logical call shares one deadline, so it counts one timeout.
        assert!(matches!(result, Err(SynapseEmbeddingError::Timeout(_))));
        assert_eq!(impatient.state.consecutive_timeouts, 1);
        assert_eq!(patient.state.consecutive_timeouts, 0);
        assert!(patient.state.circuit_open_until.is_none());
        assert_eq!(answer.unwrap(), vec![1.0, 0.0]);
        let current = shared
            .runtime
            .block_on(patient.state.ensure_route())
            .unwrap();
        assert!(
            Arc::ptr_eq(&original, &current),
            "a request timeout reset a healthy shared connection"
        );
        assert_eq!(daemon.counts.0.lock().unwrap().opens, 1);
        assert_eq!(daemon.counts.0.lock().unwrap().connections, 1);
        // The impatient call's single attempt holds its whole deadline, so
        // it sends one query instead of one per retry.
        assert_eq!(daemon.counts.0.lock().unwrap().queries, 2);
    });
    drop(original);
    drop(impatient);
    drop(shared);
    daemon.wait_closed();
    daemon.finish();
}

/// Sets the per-attempt transport deadline for this thread until dropped.
struct AttemptTimeout;

impl AttemptTimeout {
    fn set(timeout: Duration) -> Self {
        ATTEMPT_TIMEOUT_FOR_TEST.with(|slot| slot.set(Some(timeout)));
        Self
    }
}

impl Drop for AttemptTimeout {
    fn drop(&mut self) {
        ATTEMPT_TIMEOUT_FOR_TEST.with(|slot| slot.set(None));
    }
}

#[test]
fn synapse_presend_deadlines_are_timeouts_and_count_once_per_call() {
    let daemon = Daemon::start(1, false);
    let mut client = SynapseEmbeddingClient::from_config(&daemon.config(0)).unwrap();
    let shared = Arc::clone(&client.state.connection);
    let original = shared
        .runtime
        .block_on(client.state.ensure_route())
        .unwrap();
    // SDK 0.31 has a fixed 1024-request route window. Unanswered requests
    // retain all its permits, so subsequent attempts cannot send a body.
    const ROUTE_WINDOW: usize = 1024;
    let held = shared.runtime.block_on(async {
        let mut held = Vec::new();
        for _ in 0..ROUTE_WINDOW {
            let connection = Arc::clone(&original);
            held.push(tokio::spawn(async move {
                connection
                    .consumer
                    .request(
                        &connection.route,
                        serde_json::to_vec(&json!({
                            "method": QUERY_OPERATION, "params": {"text": "timeout"}
                        }))
                        .unwrap(),
                        CallOptions {
                            timeout: Duration::from_secs(3600),
                            ..CallOptions::default()
                        },
                    )
                    .await
            }));
        }
        held
    });
    daemon.wait_queries_with_timeout(ROUTE_WINDOW, Duration::from_secs(30));
    let _attempts = AttemptTimeout::set(Duration::from_secs(60));
    let clock = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap();
    for call in 0..CIRCUIT_TIMEOUT_THRESHOLD {
        let result = clock.block_on(client.state.call(
            QUERY_OPERATION,
            json!({"text": "timeout"}),
            Duration::from_secs(3600),
        ));
        if call + 1 < CIRCUIT_TIMEOUT_THRESHOLD {
            assert!(
                matches!(result, Err(SynapseEmbeddingError::Timeout(_))),
                "call {call}: {result:?}"
            );
            assert!(client.state.circuit_open_until.is_none());
        } else {
            assert!(matches!(result, Err(SynapseEmbeddingError::CircuitOpen)));
        }
        assert_eq!(client.state.consecutive_timeouts, call + 1);
        assert_eq!(daemon.counts.0.lock().unwrap().queries, ROUTE_WINDOW);
    }
    let current = shared
        .runtime
        .block_on(client.state.ensure_route())
        .unwrap();
    assert!(Arc::ptr_eq(&original, &current));
    shared.runtime.block_on(async {
        for request in held {
            request.abort();
            assert!(request.await.unwrap_err().is_cancelled());
        }
    });
    drop(current);
    drop(original);
    drop(client);
    drop(shared);
    daemon.wait_closed();
    daemon.finish();
}

#[test]
fn synapse_stale_handle_before_send_retries_fresh_route_without_timeout() {
    let daemon = Daemon::start(1, false);
    let mut client = SynapseEmbeddingClient::from_config(&daemon.config(0)).unwrap();
    let shared = Arc::clone(&client.state.connection);
    let original = shared
        .runtime
        .block_on(client.state.ensure_route())
        .unwrap();
    // Capture a handle, then replace its transport before sending. The handle's
    // connection token is now stale even though the channel and epoch match.
    let consumer = shared
        .runtime
        .block_on(SubcConsumer::connect(
            &daemon.path,
            ConsumerOptions {
                reconnect_backoff: subc_client_rs::RetryBackoff {
                    max_attempts: 0,
                    ..Default::default()
                },
                ..ConsumerOptions::default()
            },
        ))
        .unwrap();
    let stale = Arc::new(ActiveConnection {
        consumer,
        route: original.route.clone(),
    });
    let error = shared
        .runtime
        .block_on(stale.consumer.request(
            &stale.route,
            br#"{"method":"embed.query","params":{"text":"not-sent"}}"#.to_vec(),
            CallOptions::default(),
        ))
        .unwrap_err();
    assert!(matches!(error, CallError::StaleRouteHandle(_)), "{error:?}");
    assert_eq!(daemon.counts.0.lock().unwrap().queries, 0);
    *shared.runtime.block_on(shared.active.lock()) = Some(Arc::clone(&stale));
    // One extra timeout would open the circuit instead of allowing recovery.
    client.state.consecutive_timeouts = CIRCUIT_TIMEOUT_THRESHOLD - 1;
    assert_eq!(
        client
            .embed_query("fresh-route", Duration::from_secs(10))
            .unwrap(),
        vec![1.0, 0.0]
    );
    assert_eq!(client.state.consecutive_timeouts, 0);
    assert!(client.state.circuit_open_until.is_none());
    let current = shared
        .runtime
        .block_on(client.state.ensure_route())
        .unwrap();
    assert!(!Arc::ptr_eq(&stale, &current));
    {
        let counts = daemon.counts.0.lock().unwrap();
        assert_eq!(counts.connections, 3);
        assert_eq!(counts.opens, 2);
        assert_eq!(counts.queries, 1);
    }
    drop(current);
    drop(stale);
    drop(original);
    drop(client);
    drop(shared);
    daemon.wait_closed();
    daemon.finish();
}

#[test]
fn synapse_attempt_deadlines_inside_one_call_count_one_timeout() {
    let daemon = Daemon::start(1, false);
    let mut client = SynapseEmbeddingClient::from_config(&daemon.config(0)).unwrap();
    // The daemon receives but never answers. Allow ample time for sending
    // instead of racing a loaded runner's writer with a 50ms deadline.
    let attempts = AttemptTimeout::set(Duration::from_secs(2));
    let result = client.embed_query("timeout", Duration::from_secs(30));
    drop(attempts);
    assert!(
        matches!(result, Err(SynapseEmbeddingError::Timeout(_))),
        "{result:?}"
    );
    assert_eq!(
        daemon.counts.0.lock().unwrap().queries,
        MAX_CALL_ATTEMPTS,
        "every attempt of the call must have timed out on the wire"
    );
    assert_eq!(client.state.consecutive_timeouts, 1);
    assert!(client.state.circuit_open_until.is_none());
    assert_eq!(
        client
            .embed_query("healthy", Duration::from_secs(5))
            .unwrap(),
        vec![1.0, 0.0],
        "one slow call must not refuse the next one"
    );
    drop(client);
    daemon.wait_closed();
    daemon.finish();
}

#[test]
fn synapse_threshold_many_slow_calls_still_open_the_circuit() {
    let daemon = Daemon::start(1, false);
    let mut client = SynapseEmbeddingClient::from_config(&daemon.config(0)).unwrap();
    // Keep the send budget generous; only the missing reply should expire it.
    let attempts = AttemptTimeout::set(Duration::from_secs(2));
    for call in 0..CIRCUIT_TIMEOUT_THRESHOLD {
        let queries = daemon.counts.0.lock().unwrap().queries;
        let result = client.embed_query("timeout", Duration::from_secs(30));
        if call + 1 < CIRCUIT_TIMEOUT_THRESHOLD {
            assert!(
                matches!(result, Err(SynapseEmbeddingError::Timeout(_))),
                "call {call}: {result:?}"
            );
            assert!(client.state.circuit_open_until.is_none());
        } else {
            assert!(
                matches!(result, Err(SynapseEmbeddingError::CircuitOpen)),
                "call {call}: {result:?}"
            );
        }
        assert_eq!(client.state.consecutive_timeouts, call + 1);
        assert_eq!(
            daemon.counts.0.lock().unwrap().queries - queries,
            if call + 1 < CIRCUIT_TIMEOUT_THRESHOLD {
                MAX_CALL_ATTEMPTS
            } else {
                1
            },
            "each attempt must reach the daemon before its deadline"
        );
    }
    drop(attempts);
    assert!(client.state.circuit_open_until.is_some());
    let queries = daemon.counts.0.lock().unwrap().queries;
    assert!(matches!(
        client.embed_query("healthy", Duration::from_secs(5)),
        Err(SynapseEmbeddingError::CircuitOpen)
    ));
    assert_eq!(
        daemon.counts.0.lock().unwrap().queries,
        queries,
        "an open circuit refuses the call before it reaches the wire"
    );
    drop(client);
    daemon.wait_closed();
    daemon.finish();
}

#[test]
fn synapse_module_and_application_errors_leave_shared_connection_up() {
    let daemon = Daemon::start(1, false);
    let mut first = SynapseEmbeddingClient::from_config(&daemon.config(0)).unwrap();
    let mut second = SynapseEmbeddingClient::from_config(&daemon.config(1)).unwrap();
    let shared = Arc::clone(&first.state.connection);
    let original = shared.runtime.block_on(first.state.ensure_route()).unwrap();
    for (text, message) in [
        ("module-error", "test_refused: request rejected"),
        ("application-error", "test_rejected: application rejected"),
    ] {
        let error = first.embed_query(text, Duration::from_secs(1)).unwrap_err();
        assert!(error.to_string().contains(message), "{error}");
        assert_eq!(
            second
                .embed_query("healthy", Duration::from_secs(1))
                .unwrap(),
            vec![1.0, 0.0]
        );
        let current = shared
            .runtime
            .block_on(second.state.ensure_route())
            .unwrap();
        assert!(
            Arc::ptr_eq(&original, &current),
            "a module reply reset a healthy shared connection"
        );
    }
    assert_eq!(daemon.counts.0.lock().unwrap().opens, 1);
    assert_eq!(
        daemon.counts.0.lock().unwrap().queries,
        4,
        "module replies must not be retried"
    );
    drop(original);
    drop(first);
    drop(second);
    drop(shared);
    daemon.wait_closed();
    daemon.finish();
}

#[test]
fn synapse_rerank_request_errors_and_timeouts_keep_the_route() {
    let daemon = Daemon::start(1, false);
    let mut transport = SynapseRerankTransport::new(&daemon.config(0)).unwrap();
    let original = transport
        .runtime
        .block_on(transport.state.ensure_route())
        .unwrap();
    assert!(matches!(
        transport.call(
            "rerank.score",
            json!({"text":"timeout"}),
            Instant::now() + Duration::from_millis(100)
        ),
        Err(SynapseEmbeddingError::Timeout(_))
    ));
    let error = transport
        .call(
            "rerank.score",
            json!({"text":"module-error"}),
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap_err();
    assert!(error.to_string().contains("test_refused: request rejected"));
    let error = transport
        .call(
            "rerank.score",
            json!({"text":"malformed"}),
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap_err();
    assert!(matches!(error, SynapseEmbeddingError::InvalidEnvelope(_)));
    let reply = transport
        .call(
            "rerank.score",
            json!({"text":"application-error"}),
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap();
    assert_eq!(reply["error"]["code"], "test_rejected");
    transport
        .call(
            "rerank.score",
            json!({"text":"healthy"}),
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap();
    let current = transport
        .runtime
        .block_on(transport.state.ensure_route())
        .unwrap();
    assert!(Arc::ptr_eq(&original, &current));
    assert_eq!(daemon.counts.0.lock().unwrap().opens, 1);
    assert_eq!(daemon.counts.0.lock().unwrap().queries, 5);
    drop(original);
    drop(current);
    drop(transport);
    daemon.wait_closed();
    daemon.finish();
}

#[test]
fn synapse_closed_consumer_replaces_the_stale_route() {
    let daemon = Daemon::start(1, false);
    let mut client = SynapseEmbeddingClient::from_config(&daemon.config(0)).unwrap();
    let original = client
        .runtime
        .block_on(client.state.ensure_route())
        .unwrap();
    client.runtime.block_on(original.consumer.close());
    assert_eq!(
        client
            .embed_query("healthy", Duration::from_secs(1))
            .unwrap(),
        vec![1.0, 0.0]
    );
    let current = client
        .runtime
        .block_on(client.state.ensure_route())
        .unwrap();
    assert!(!Arc::ptr_eq(&original, &current));
    assert_eq!(daemon.counts.0.lock().unwrap().opens, 2);
    drop(original);
    drop(current);
    drop(client);
    daemon.wait_closed();
    daemon.finish();
}

#[test]
fn synapse_untyped_not_sent_and_unknown_outcomes_do_not_imply_a_dead_route() {
    let not_sent = CallError::NotSent(
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "writer closed: request timed out",
        )
        .into(),
    );
    let unknown = CallError::OutcomeUnknown(
        std::io::Error::new(std::io::ErrorKind::Other, "connection failed").into(),
    );
    assert!(!connection_is_gone(&not_sent));
    assert!(!connection_is_gone(&unknown));
    assert!(connection_is_gone(&CallError::StaleRouteHandle(
        RouteHandle::detached(7, 1)
    )));
}
