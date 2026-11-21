use super::*;

pub(super) async fn drive_management_surface_daemon(input: FakeDaemonInput) {
    let FakeDaemonSession {
        mut stream,
        root1,
        root2,
        state,
        app,
        executor,
        ..
    } = open_fake_daemon_session(input).await;

    // Seed one live actor so both the digest's live-root path and the census's
    // lifecycle join are exercised before opening the process-wide read plane.
    bind_route1(&mut stream, &root1).await;
    let actor_count = executor.actor_count();
    let configure_count = state.configure_count();
    let watcher_count = app.watcher_count();

    send_management_route_bind(
        &mut stream,
        2,
        20,
        &root2,
        Some(Principal::Reserved {
            module_id: "prefrontal".to_string(),
        }),
    )
    .await;
    expect_route_bind_ack(&mut stream, 20).await;
    assert_eq!(
        executor.actor_count(),
        actor_count,
        "management bind must not register an actor"
    );
    assert_eq!(
        state.configure_count(),
        configure_count,
        "management bind must not run configure"
    );
    assert_eq!(
        app.watcher_count(),
        watcher_count,
        "management bind must not start a watcher"
    );
    let root2_id = ProjectRootId::from_path(&root2).expect("root2 id");
    assert!(!executor.actor_registered(&root2_id));

    let fixture: Value =
        serde_json::from_str(include_str!("../../fixtures/bash-kill-scope/wire.json")).unwrap();
    send_management_request(&mut stream, 2, 320, fixture["not_owner"]["request"].clone()).await;
    let refused = read_management_response(&mut stream, 2, 320, "bash.kill_scope").await;
    assert_eq!(refused, fixture["not_owner"]["reply"]);
    send_management_route_bind(
        &mut stream,
        12,
        321,
        &root2,
        Some(Principal::Reserved {
            module_id: "prefrontal".into(),
        }),
    )
    .await;
    expect_route_bind_ack(&mut stream, 321).await;
    send_management_request(
        &mut stream,
        12,
        322,
        fixture["nothing_running"]["request"].clone(),
    )
    .await;
    let empty = read_management_response(&mut stream, 12, 322, "bash.kill_scope").await;
    assert_eq!(empty, fixture["nothing_running"]["reply"]);

    send_management_request(&mut stream, 2, 30, json!({ "op": "memory.census" })).await;
    let census = read_management_response(&mut stream, 2, 30, "memory.census").await;
    assert!(
        census.pointer("/data/roots").is_some(),
        "census must carry uncapped roots: {census:?}"
    );
    assert!(
        census.pointer("/data/process").is_some(),
        "census must carry process header: {census:?}"
    );
    let census_io = census
        .pointer("/data/process_io")
        .expect("census must carry process_io: {census:?}");
    assert!(census_io["available"].is_boolean());
    assert!(census_io["sampled_at_ms"].is_u64());

    let ownership = &census["data"]["route_ownership"];
    assert_eq!(ownership["summary"]["bound_routes"], 1);
    assert_eq!(
        ownership["summary"]["unscoped"],
        json!({"routes": 1, "roots": 1})
    );
    let route = &ownership["roots"][0]["routes"][0];
    assert_eq!(route["channel"], 1);
    assert_eq!(route["harness"], "opencode");
    assert_eq!(route["session_id"], "session-1");
    assert_eq!(route["principal"], "direct");
    assert_eq!(route["scope"], "none");
    assert!(route["last_tool_call_age_ms"].is_null());
    send_control_request(&mut stream, 330, ModuleControlRequest::HealthCheck {}).await;
    let health = expect_health_check_report(&mut stream, 330).await;
    assert_eq!(
        health.metrics.unwrap()["route_ownership_summary"]["bound_routes"],
        1
    );
    send_management_request(&mut stream, 2, 331, json!({"op":"memory.census"})).await;
    let polled = read_management_response(&mut stream, 2, 331, "memory.census").await;
    assert!(
        polled["data"]["route_ownership"]["roots"][0]["routes"][0]["last_tool_call_age_ms"]
            .is_null(),
        "health and census polling must not count as admitted tools"
    );
    expect_manifest_tool_success(&mut stream, 332, "glob", json!({"pattern":"*"})).await;
    send_management_request(&mut stream, 2, 333, json!({"op":"memory.census"})).await;
    let called = read_management_response(&mut stream, 2, 333, "memory.census").await;
    assert!(
        called["data"]["route_ownership"]["roots"][0]["routes"][0]["last_tool_call_age_ms"]
            .is_u64()
    );
    assert!(called["data"]["route_ownership"]["roots"][0]["last_tool_call_age_ms"].is_u64());

    send_management_request(
        &mut stream,
        2,
        31,
        json!({
            "op": "health.digest",
            "params": { "project_root": root1, "since": "cursor-1" }
        }),
    )
    .await;
    let digest = read_management_response(&mut stream, 2, 31, "health.digest").await;
    assert_eq!(digest.get("status").and_then(Value::as_str), Some("ok"));
    assert!(
        digest.get("data").is_some_and(Value::is_object),
        "digest must carry its structured response: {digest:?}"
    );

    send_management_request(
        &mut stream,
        2,
        34,
        json!({ "op": "health.digest", "params": { "root": root2 } }),
    )
    .await;
    let unbound = read_management_response(&mut stream, 2, 34, "health.digest").await;
    assert_eq!(unbound.get("status").and_then(Value::as_str), Some("error"));
    assert_eq!(
        unbound.pointer("/data/code").and_then(Value::as_str),
        Some("root_not_bound")
    );

    // The gh shim relay is served on the management route, and a ticket the
    // daemon never issued is refused there without any outbound call.
    send_management_request(
        &mut stream,
        2,
        35,
        json!({
            "op": "gh_shim.bot_request",
            "params": {
                "ticket": "0123456789abcdef0123456789abcdef",
                "request_nonce": "nonce-bridge",
                "session": "typed-head-session",
                "request": {"action": "issue comment", "metadata": {"agent_id": "a"}},
            }
        }),
    )
    .await;
    let relay = read_management_response(&mut stream, 2, 35, "gh_shim.bot_request").await;
    assert_eq!(relay.get("status").and_then(Value::as_str), Some("error"));
    assert_eq!(
        relay.pointer("/data/refusal_code").and_then(Value::as_str),
        Some("ticket_unknown")
    );

    // A federated front is untrusted even as a direct principal, so it can
    // never open the management route the relay is served on.
    send_management_route_bind_with_harness(
        &mut stream,
        4,
        41,
        &root2,
        "fed:facade",
        Some(Principal::Direct),
    )
    .await;
    expect_route_bind_error(&mut stream, 41, "route_refused").await;

    send_tool_call(&mut stream, 2, 32, "status", json!({})).await;
    expect_error_frame(&mut stream, 2, 32, "unknown_management_op").await;

    // A tool route does not serve management operations: the request is
    // refused with a protocol error naming the operation, never answered.
    send_management_request(&mut stream, 1, 33, json!({ "op": "memory.census" })).await;
    expect_error_frame(&mut stream, 1, 33, "unknown_op").await;

    send_management_route_bind(&mut stream, 3, 40, &root2, Some(Principal::Unverified)).await;
    expect_route_bind_error(&mut stream, 40, "route_refused").await;
    assert_eq!(executor.actor_count(), actor_count);
    assert_eq!(state.configure_count(), configure_count);
    assert_eq!(app.watcher_count(), watcher_count);

    let owner = Principal::Reserved {
        module_id: "prefrontal".into(),
    };
    let stamp = subc_protocol::scope::ScopeStamp::new(
        owner.clone(),
        "census-scope",
        7,
        subc_protocol::scope::ScopeKind::Head,
    );
    send_control_request(&mut stream, 340, {
        let mut bind = serde_json::to_value(ModuleControlRequest::RouteBind {
            route_channel: 6,
            epoch: 1,
            target: RouteTarget::ToolProvider {
                module_id: "aft".into(),
            },
            identity: BindIdentity::new("opencode", "census-scoped"),
            principal: Some(owner),
            scope: Some(stamp),
            consumer_capabilities: None,
            admission_facts: Default::default(),
            role_versions: None,
            call_mode: None,
        })
        .unwrap();
        bind["identity"]["project_root"] = serde_json::json!(root1.clone());

        bind
    })
    .await;
    expect_route_bind_ack(&mut stream, 340).await;
    send_management_request(&mut stream, 2, 341, json!({"op":"memory.census"})).await;
    let paired = read_management_response(&mut stream, 2, 341, "memory.census").await;
    let ownership = &paired["data"]["route_ownership"];
    assert_eq!(ownership["summary"]["bound_routes"], 2);
    assert_eq!(ownership["summary"]["bound_roots"], 1);
    assert_eq!(
        ownership["summary"]["scoped"],
        json!({"routes":1,"roots":1})
    );
    assert_eq!(
        ownership["summary"]["unscoped"],
        json!({"routes":1,"roots":1})
    );
    assert_eq!(
        ownership["roots"][0]["routes"][1]["scope"],
        json!({"owner":"reserved:prefrontal","ref":"census-scope","epoch":7})
    );
    assert_eq!(
        ownership["roots"][0]["routes"][1]["principal"],
        "reserved:prefrontal"
    );
    send_route_goodbye(&mut stream, 6, 342).await;
    send_management_request(&mut stream, 2, 343, json!({"op":"memory.census"})).await;
    let detached = read_management_response(&mut stream, 2, 343, "memory.census").await;
    assert_eq!(
        detached["data"]["route_ownership"]["summary"]["bound_routes"],
        1
    );
    assert_eq!(
        detached["data"]["route_ownership"]["summary"]["scoped"]["routes"],
        0
    );
    send_route_goodbye(&mut stream, 1, 344).await;
    send_management_request(&mut stream, 2, 345, json!({"op":"memory.census"})).await;
    let unbound = read_management_response(&mut stream, 2, 345, "memory.census").await;
    assert_eq!(
        unbound["data"]["route_ownership"]["summary"]["bound_roots"],
        0
    );
    assert_eq!(
        unbound["data"]["route_ownership"]["summary"]["bound_routes"],
        0
    );

    send_connection_goodbye(&mut stream).await;
}
