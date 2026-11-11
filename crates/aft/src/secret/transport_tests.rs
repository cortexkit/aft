use super::fake::{golden, refusal, FakeClaustrum, NONCE};
use super::*;
use crate::secret::{parse_answer, SecretUseAnswer, SecretUseRefusal, SecretUseReply};

fn principal(module_id: &str) -> ConsumerIdentity {
    ConsumerIdentity {
        module_id: module_id.into(),
        launch_nonce: NONCE.into(),
    }
}

fn transport(daemon: &FakeClaustrum) -> SubcSecretUseTransport {
    SubcSecretUseTransport {
        connection_file: daemon.path.clone(),
        identity: BindIdentity::new("aft-secret-use", "secret-call"),
        timeout: Duration::from_secs(2),
        worker: Arc::default(),
    }
}

fn call(transport: &SubcSecretUseTransport, params: Value) -> Value {
    transport.dispatch(params, principal("aft")).unwrap()
}

#[test]
fn secret_use_route_preserves_the_stamp_and_releases_only_after_phone_approval() {
    let daemon = FakeClaustrum::start();
    let transport = transport(&daemon);
    let pending = call(&transport, golden(0));
    assert_eq!(pending, json!({"result":golden(5)}));
    assert_eq!(
        call(&transport, golden(1)),
        pending,
        "defaults recover the same statement bytes"
    );
    daemon
        .state
        .lock()
        .unwrap()
        .approve(pending["result"]["statement_b64url"].as_str().unwrap());
    let released = call(&transport, golden(0));
    assert_eq!(released, json!({"result":golden(3)}));
    let SecretUseAnswer::Reply(SecretUseReply::ReleasedApiKey(key)) =
        parse_answer(&released).unwrap()
    else {
        panic!("the approved API key must be released");
    };
    assert_eq!(key.material(), b"sk-example");
    assert!(!format!("{key:?}").contains("sk-example"));
    assert!(!format!("{key:?}").contains("c2stZXhhbXBsZQ"));
    let state = daemon.state.lock().unwrap();
    assert_eq!(state.calls, 3);
    assert_eq!(state.connections, 1);
    assert_eq!(state.opens.len(), 1);
    for open in &state.opens {
        assert_eq!(
            open["consumer_identity"],
            json!({"module_id":"aft","launch_nonce":NONCE})
        );
        let stamp: ScopeStamp = serde_json::from_value(golden(0)["stamp"].clone()).unwrap();
        assert_eq!(
            open["scope"],
            json!(ScopeSelector::new(
                stamp.owner,
                stamp.scope_ref,
                Some(stamp.scope_epoch)
            ))
        );
    }
}

#[test]
fn secret_use_route_keeps_module_errors_distinct_from_uniform_not_found() {
    let daemon = FakeClaustrum::start();
    let transport = transport(&daemon);
    let mut extra_name = golden(0);
    extra_name["display_name"] = json!("not trusted");
    let rejected = call(&transport, extra_name);
    assert_eq!(
        parse_answer(&rejected).unwrap().refusal(),
        Some(SecretUseRefusal::InvalidParams)
    );

    let mut unknown = golden(0);
    unknown["name"] = json!("unknown");
    let not_found = call(&transport, unknown);
    assert_eq!(not_found, refusal("not_found", "permanent"));
    let mut mismatch = golden(0);
    mismatch["target"]["host"] = json!("other.test");
    assert_eq!(call(&transport, mismatch), not_found);
    let mut mismatch = golden(0);
    mismatch["target"]["path"] = json!("/repos/another");
    assert_eq!(call(&transport, mismatch), not_found);
    assert!(daemon.state.lock().unwrap().pending.is_empty());
}

#[test]
fn secret_use_route_maps_all_sender_refusals_without_releasing_material() {
    use SecretUseRefusal::*;
    let daemon = FakeClaustrum::start();
    let transport = transport(&daemon);
    let mut oversized = golden(0);
    oversized["name"] = json!("a".repeat(65));
    let limit = call(&transport, oversized);
    assert_eq!(limit, refusal("limit_exceeded", "context_overflow"));
    assert_eq!(parse_answer(&limit).unwrap().refusal(), Some(LimitExceeded));
    for expected in [SetupIncomplete, PendingLimit, StoreError] {
        {
            let mut state = daemon.state.lock().unwrap();
            state.setup_incomplete = expected == SetupIncomplete;
            state.other_pending_in_scope = if expected == PendingLimit { 8 } else { 0 };
            state.store_error = expected == StoreError;
        }
        let answer = parse_answer(&call(&transport, golden(0))).unwrap();
        assert_eq!(answer.refusal(), Some(expected));
    }
}

#[test]
fn secret_use_fake_grant_lookup_is_independent_of_requested_coverage_and_covers_linked_workers() {
    let daemon = FakeClaustrum::start();
    let transport = transport(&daemon);
    let mut head = golden(0);
    head["coverage"] = json!("agent_and_workers");
    let pending = call(&transport, head);
    daemon
        .state
        .lock()
        .unwrap()
        .approve(pending["result"]["statement_b64url"].as_str().unwrap());
    assert_eq!(call(&transport, golden(1)), json!({"result":golden(3)}));
    let mut worker = golden(0);
    worker["stamp"] = golden(2)["stamp"].clone();
    assert_eq!(
        call(&transport, worker.clone()),
        json!({"result":golden(3)})
    );
    worker["stamp"]["parent_state"] = json!("ended");
    assert!(matches!(
        parse_answer(&call(&transport, worker)).unwrap(),
        SecretUseAnswer::Reply(SecretUseReply::ApprovalRequired(_))
    ));
}

#[test]
fn secret_use_deadline_covers_scoped_open_reply_and_disconnect_without_secret_errors() {
    for phase in ["open", "reply", "disconnect", "no-daemon"] {
        let daemon = FakeClaustrum::start();
        let mut transport = transport(&daemon);
        transport.timeout = Duration::from_millis(150);
        {
            let mut state = daemon.state.lock().unwrap();
            state.hold_open = phase == "open";
            state.hold_reply = phase == "reply";
            state.disconnect = phase == "disconnect";
        }
        if phase == "no-daemon" {
            transport.connection_file = daemon.path.with_extension("absent");
        }
        let started = std::time::Instant::now();
        let error = transport.dispatch(golden(0), principal("aft")).unwrap_err();
        assert_eq!(error, UNAVAILABLE, "{phase}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{phase} exceeded the call deadline"
        );
        assert!(!error.contains("sk-example"));
        assert!(!error.contains("c2stZXhhbXBsZQ"));
    }
}

#[test]
fn secret_use_sender_never_sends_a_non_https_or_non_443_target() {
    let daemon = FakeClaustrum::start();
    let transport = transport(&daemon);
    for (scheme, port) in [("http", 443), ("https", 8443)] {
        let mut params = golden(0);
        params["target"]["scheme"] = json!(scheme);
        params["target"]["port"] = json!(port);
        assert_eq!(
            transport.dispatch(params, principal("aft")).unwrap_err(),
            UNAVAILABLE
        );
    }
    assert_eq!(daemon.state.lock().unwrap().opens.len(), 0);
}

#[test]
fn secret_use_standalone_cannot_construct_a_module_transport_and_keeps_unavailable_vault() {
    let _nonce = crate::launch_nonce::install_for_tests(None);
    assert!(SubcSecretUseTransport::new(
        PathBuf::from("absent-connection.json"),
        BindIdentity::new("standalone", "standalone"),
        Duration::from_millis(150),
    )
    .is_none());
    let stamp: ScopeStamp = serde_json::from_value(golden(0)["stamp"].clone()).unwrap();
    let vault = crate::bash_background::secrets::vault();
    assert!(matches!(
        vault.resolve(&crate::secret::SecretScope::from_stamp(&stamp), "key"),
        crate::secret::Resolution::Unavailable
    ));
}

// Negative contract inputs deliberately bypass the sender's typed construction.
// They still travel through the SDK's authenticated connection, scoped open, and
// request path, rather than calling the fake endpoint's handler directly.
fn raw_call(daemon: &FakeClaustrum, identity: Option<ConsumerIdentity>, params: Value) -> Value {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let consumer = crate::fleet_status::connect_subc_consumer(
                &daemon.path,
                ConsumerOptions::default(),
            )
            .await
            .unwrap();
            let stamp: ScopeStamp = serde_json::from_value(golden(0)["stamp"].clone()).unwrap();
            let route = consumer
                .open_route_scoped(
                    RouteTarget::ManagementSurface {
                        module_id: "claustrum".into(),
                    },
                    BindIdentity::new("secret-contract", "negative"),
                    ScopeSelector::new(stamp.owner, stamp.scope_ref, Some(stamp.scope_epoch)),
                    CallOptions {
                        consumer_identity: identity,
                        timeout: Duration::from_secs(1),
                        ..CallOptions::default()
                    },
                )
                .await
                .unwrap();
            let reply = consumer
                .request(
                    &route,
                    serde_json::to_vec(&json!({"method":"secret.use","params":params})).unwrap(),
                    CallOptions {
                        timeout: Duration::from_secs(1),
                        ..CallOptions::default()
                    },
                )
                .await;
            let reply = match reply {
                Ok(bytes) => serde_json::from_slice(&bytes).unwrap(),
                Err(CallError::Module(error)) => json!({"code":error.code,"message":error.message}),
                Err(error) => panic!("fake route failed: {error}"),
            };
            let _ = consumer.close().await;
            reply
        })
}

#[test]
fn fake_claustrum_orders_caller_grammar_scope_and_target_checks() {
    let daemon = FakeClaustrum::start();
    let missing = refusal("not_found", "permanent");
    assert_eq!(
        raw_call(&daemon, Some(principal("other")), json!(null)),
        missing
    );
    assert_eq!(
        raw_call(&daemon, None, golden(0)),
        missing,
        "direct callers are not AFT"
    );
    let mut unstamped = golden(0);
    unstamped["stamp"]
        .as_object_mut()
        .unwrap()
        .remove("owner_authorized");
    assert_eq!(
        raw_call(&daemon, Some(principal("aft")), unstamped),
        missing
    );
    for (field, value) in [("scheme", json!("http")), ("port", json!(8443))] {
        let mut mismatched = golden(0);
        mismatched["target"][field] = value;
        assert_eq!(
            raw_call(&daemon, Some(principal("aft")), mismatched),
            missing
        );
    }
    for target in [
        json!({"scheme":"HTTP","host":"x.test","path":"/repos/a"}),
        json!({"scheme":"https","host":"x.test","path":"/repos/a/%2e"}),
        json!({"scheme":"https","host":"x.test","path":"/repos/a/..;x"}),
        json!({"scheme":"https","host":"x.test","path":"/repos/a?secret"}),
    ] {
        let mut bad = golden(0);
        bad["target"] = target;
        let answer = raw_call(&daemon, Some(principal("aft")), bad);
        assert_eq!(
            parse_answer(&answer).unwrap().refusal(),
            Some(SecretUseRefusal::InvalidParams)
        );
    }
}
