use super::route_fixture::*;
use super::*;
use std::sync::Arc;

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

/// Each project context builds its own reranker and keeps it while the context
/// lives. They must all share one daemon connection and one route, or AFT's
/// connections grow with every root it serves.
#[test]
fn synapse_rerankers_across_roots_share_one_connection_and_route() {
    const ROOTS: usize = 6;
    let daemon = Daemon::start(1, false);
    let mut transports = (0..ROOTS)
        .map(|root| SynapseRerankTransport::new(&daemon.config(root)).unwrap())
        .collect::<Vec<_>>();
    for transport in &mut transports {
        transport
            .call(
                "rerank.score",
                json!({"text":"healthy"}),
                Instant::now() + Duration::from_secs(5),
            )
            .unwrap();
    }
    {
        let counts = daemon.counts.0.lock().unwrap();
        assert_eq!(counts.queries, ROOTS);
        assert_eq!(counts.connections, 1, "rerankers opened a connection each");
        assert_eq!(counts.live, 1);
        assert_eq!(counts.opens, 1, "rerankers opened a route each");
        assert_eq!(counts.active, 1);
    }
    drop(transports);
    daemon.wait_closed();
    daemon.wait_live(0);
    daemon.finish();
}

/// Lane pauses and a lost connection must leave exactly one connection and
/// one route: a pause keeps the route, and a reconnect replaces the old
/// connection instead of adding to it.
#[test]
fn synapse_embed_calls_with_lane_pauses_and_a_lost_connection_keep_one_connection() {
    const ROOTS: usize = 4;
    let daemon = Daemon::start(1, false);
    let mut clients = (0..ROOTS)
        .map(|root| SynapseEmbeddingClient::from_config(&daemon.config(root)).unwrap())
        .collect::<Vec<_>>();
    for client in &mut clients {
        assert!(matches!(
            client.embed_query("lane-paused", Duration::from_secs(5)),
            Err(SynapseEmbeddingError::LanePaused(_))
        ));
        assert_eq!(
            client
                .embed_query("healthy", Duration::from_secs(5))
                .unwrap(),
            vec![1.0, 0.0]
        );
    }
    {
        let counts = daemon.counts.0.lock().unwrap();
        assert_eq!((counts.connections, counts.live), (1, 1));
        assert_eq!((counts.opens, counts.active), (1, 1));
    }
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
    drop(old);
    daemon.disconnect_first_connection();
    observed_drop.recv_timeout(Duration::from_secs(5)).unwrap();
    daemon.wait_live(0);
    for client in &mut clients {
        assert!(matches!(
            client.embed_query("lane-paused", Duration::from_secs(5)),
            Err(SynapseEmbeddingError::LanePaused(_))
        ));
        assert_eq!(
            client
                .embed_query("after-disconnect", Duration::from_secs(5))
                .unwrap(),
            vec![1.0, 0.0]
        );
    }
    {
        let counts = daemon.counts.0.lock().unwrap();
        assert_eq!(
            counts.connections, 2,
            "only the lost connection is replaced"
        );
        assert_eq!((counts.live, counts.active), (1, 1));
        assert_eq!(counts.opens, 2);
    }
    drop(clients);
    drop(shared);
    daemon.wait_closed();
    daemon.wait_live(0);
    daemon.finish();
}

/// The per-owner gauge counts a tracked consumer while it is open and drops
/// the count once the consumer is released.
#[test]
fn consumer_gauge_returns_to_zero_after_the_consumer_is_dropped() {
    const OWNER: &str = "gauge_test";
    let daemon = Daemon::start(1, false);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let consumer = runtime
        .block_on(aft_base::registration::connect_consumer(
            OWNER,
            &daemon.path,
            subc_client_rs::ConsumerOptions::default(),
        ))
        .unwrap();
    assert_eq!(
        aft_base::registration::live_consumers().get(OWNER),
        Some(&1)
    );
    drop(consumer);
    // The SDK releases the connection's shared state once its aborted
    // transport tasks have been dropped, which needs the runtime to run.
    runtime.block_on(async {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while aft_base::registration::live_consumers().get(OWNER) != Some(&0) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the gauge still counts a dropped consumer"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    daemon.wait_live(0);
    drop(runtime);
    daemon.finish();
}
