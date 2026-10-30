use super::*;
use crate::subc::persistence::DatabaseWaits;
use crate::subc::rich_ui::{carrier, chunks};

const MANIFEST: &[u8] = include_bytes!("../../../../tests/fixtures/rich_ui/manifest.jcs.json");

#[tokio::test]
async fn session_read_targets_keep_frozen_gate_and_step_five_refusals() {
    let _manifest = chunks::set_test_manifest_for_test(Some(MANIFEST.to_vec()));
    let (_dir, root) = test_support::test_root("session-read-targets");
    let cases: Vec<Value> = serde_json::from_slice(include_bytes!(
        "../../../../tests/fixtures/rich_ui/target-arguments.json"
    ))
    .unwrap();
    for case in cases {
        let name = case["operation"].as_str().unwrap();
        let mut args = match name {
            "read" => json!({"path":"src/lib.rs"}),
            "outline" => json!({"target":"src/lib.rs"}),
            "zoom" => json!({"batch":[{"target":"src/lib.rs", "symbols":["one"]}]}),
            "search" | "grep" | "glob" => json!({"pattern":"one"}),
            "callgraph" => json!({"op":"callers", "path":"src/lib.rs", "symbol":"one"}),
            _ => unreachable!(),
        };
        match case["field"].as_str().unwrap() {
            "target[]" => args["target"] = json!(["src/lib.rs", case["form"]]),
            "batch[].target" => {
                args["batch"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"target":case["form"], "symbols":["one"]}));
            }
            field => args[field] = case["form"].clone(),
        }
        args["rich_ui_op"] = json!(format!("{name}@1"));
        let reply = exchange_with_actor(
            json!({"op":"tool.call", "name":name, "arguments":args}),
            occipital_identity(&root, RouteRole::ToolProviderV1),
            test_support::test_ctx(),
            &Arc::new(DispatchPathMetrics::new()),
            |_, _| panic!("remote and revision targets must not dispatch"),
        )
        .await;
        let body: Value = serde_json::from_slice(&reply.body).unwrap();
        if case["refused_by"] == "gate" {
            assert_eq!(reply.header.ty, FrameType::Error, "{case}: {body}");
            assert_eq!(body["code"], case["code"], "{case}: {body}");
        } else {
            assert_eq!(reply.header.ty, FrameType::Response, "{case}: {body}");
            assert_eq!(
                body,
                carrier::error_body(case["code"].as_str().unwrap()),
                "{case}"
            );
            carrier::validate_body(None, &body).unwrap();
        }
    }
}

#[tokio::test]
async fn operation_route_gate_and_admission_precedence() {
    let _manifest = chunks::set_test_manifest_for_test(Some(MANIFEST.to_vec()));
    let (_dir, root) = test_support::test_root("operation-route");
    for (name, args, code, gate) in [
        ("bash", json!({}), "carrier_read_only", true),
        (
            "bash",
            json!({"rich_ui_op":"bash_status@1"}),
            "carrier_read_only",
            true,
        ),
        (
            "edit",
            json!({"rich_ui_op":"garbled"}),
            "carrier_read_only",
            true,
        ),
        (
            "write",
            json!({"rich_ui_op":"garbled"}),
            "carrier_read_only",
            true,
        ),
        (
            "inspect",
            json!({"rich_ui_op":"garbled"}),
            "carrier_read_only",
            true,
        ),
        (
            "aft_outline",
            json!({"target":"https://example.com","rich_ui_op":"garbled"}),
            "carrier_read_only",
            true,
        ),
        (
            "read",
            json!({"path":"https://example.com","rich_ui_op":"garbled"}),
            "rich_ui_op_malformed",
            false,
        ),
        (
            "read",
            json!({"path":"x","rich_ui_op":"read@99","session":"bad"}),
            "operation_version_unsupported",
            false,
        ),
        (
            "grep",
            json!({"rich_ui_op":"read@1","session":"bad"}),
            "operation_target_mismatch",
            false,
        ),
        (
            "read",
            json!({"rich_ui_op":"read@1","session":"bad","path":"x".repeat(8193)}),
            "bound_field_supplied",
            false,
        ),
        (
            "read",
            json!({"rich_ui_op":"read@1","path":"x".repeat(8193)}),
            "args_invalid",
            false,
        ),
        (
            "read",
            json!({"rich_ui_op":"read@1","path":"x"}),
            "session_not_bound",
            true,
        ),
    ] {
        let reply = exchange_with_actor(
            json!({"op":"tool.call","name":name,"arguments":args}),
            occipital_identity(&root, RouteRole::ToolProviderV1),
            test_support::test_ctx(),
            &Arc::new(DispatchPathMetrics::new()),
            |_, _| panic!("no policy route exists"),
        )
        .await;
        let body: Value = serde_json::from_slice(&reply.body).unwrap();
        if gate {
            assert_eq!(reply.header.ty, FrameType::Error, "{name}: {body}");
            assert_eq!(body["code"], code, "{name}: {body}");
        } else {
            assert_eq!(reply.header.ty, FrameType::Response);
            assert_eq!(body, carrier::error_body(code), "{name}");
            carrier::validate_body(None, &body).unwrap();
        }
    }
}

#[tokio::test]
async fn operation_unpublished_carrier_status_and_read_fail_closed() {
    let _manifest = chunks::set_test_manifest_for_test(None);
    let (_dir, root) = test_support::test_root("operation-unpublished");
    for name in ["read", "bash_status"] {
        let reply = exchange_with_actor(
            json!({"name":name,"preset":"worker", "arguments":{"rich_ui_op":format!("{name}@1")}}),
            occipital_identity(&root, RouteRole::ToolProviderV1),
            test_support::test_ctx(),
            &Arc::new(DispatchPathMetrics::new()),
            |_, _| panic!("unpublished operations must not run"),
        )
        .await;
        let body: Value = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(body, carrier::error_body("operation_version_unsupported"));
    }
}

#[tokio::test]
async fn operation_non_carrier_keeps_shell_precedence_and_structured_content_omitted() {
    let (_dir, root) = test_support::test_root("operation-noncarrier");
    for (name, marker, expected) in [
        ("bash_status", true, Some("capability_not_admitted")),
        ("bash_status", false, Some("capability_not_admitted")),
        ("read", true, Some("rich_ui_op_not_allowed")),
        ("read", false, None),
    ] {
        let mut identity = occipital_identity(&root, RouteRole::ToolProviderV1);
        let data = Arc::get_mut(&mut identity.0).unwrap();
        data.scope = None;
        data.tool_result_body = true;
        if let AuthenticatedPrincipal::RouteBind { principal_id, .. } = &mut data.spawn_principal {
            *principal_id = Some("third-party:reader".into());
        }
        let mut args = if name == "read" {
            json!({"path":"x"})
        } else {
            json!({"taskId":"bash-0123456789abcdef"})
        };
        if marker {
            args["rich_ui_op"] = json!("read@1");
        }
        let reply = exchange_with_actor(
            json!({"name":name,"arguments":args}),
            identity,
            test_support::test_ctx(),
            &Arc::new(DispatchPathMetrics::new()),
            |request, _| Response::success(request.id, json!({"text":"ordinary"})),
        )
        .await;
        let body: Value = serde_json::from_slice(&reply.body).unwrap();
        if let Some(code) = expected {
            assert_eq!(body["code"], code);
        } else {
            assert_eq!(reply.header.ty, FrameType::Response);
            assert!(body.get("structuredContent").is_none());
        }
    }
}

fn decoded(corr: u64, key: &str) -> DecodedFrame {
    DecodedFrame {
        parsed_body: Default::default(),
        confinement: None,
        launch_restore: None,
        codemode_policies: None,
        frame: Frame::build(
            FrameType::Request,
            control_flags(),
            41,
            1,
            corr,
            serde_json::to_vec(&json!({"name":"bash_status", "call_key":key,
                "arguments":{"rich_ui_op":"bash_status@1","task":"viewed-call"}}))
            .unwrap(),
        )
        .unwrap(),
        phase_trace: PhaseTrace::new(Instant::now()),
    }
}

fn custodian_snapshot(conn: &rusqlite::Connection, owner: &str) -> (i64, i64, Vec<u8>) {
    let seq = conn
        .query_row(
            "SELECT last_seq FROM call_ledger_ack WHERE custodian=?1",
            [owner],
            |row| row.get(0),
        )
        .unwrap();
    let count = conn
        .query_row(
            "SELECT count(*) FROM call_ledger WHERE custodian=?1 AND state='Settled' AND acked=0",
            [owner],
            |row| row.get(0),
        )
        .unwrap();
    let feed = crate::db::call_ledger::late_results(
        conn,
        owner,
        "fixture-incarnation",
        None,
        None,
        crate::db::call_ledger::now_ms(),
        1_048_576,
    )
    .unwrap()
    .unwrap();
    (seq, count, feed)
}

#[test]
fn operation_ledger_namespace_preserves_unsigned_epoch() {
    let (_dir, root) = test_support::test_root("operation-namespace");
    for epoch in [i64::MAX as u64, i64::MAX as u64 + 1, u64::MAX] {
        let mut identity = occipital_identity(&root, RouteRole::ToolProviderV1);
        let scope = Arc::get_mut(&mut identity.0)
            .unwrap()
            .scope
            .as_mut()
            .unwrap();
        scope.owner = subc_protocol::Principal::Reserved {
            module_id: "prefrontal-core".into(),
        };
        scope.scope_ref = "session\"ref".into();
        scope.scope_epoch = epoch;
        assert_eq!(
            LedgerEdge::key(&identity, "frame-key").carrier,
            format!(
                "reserved:occipital#[\"reserved:prefrontal-core\",\"session\\\"ref\",\"{epoch}\"]"
            )
        );
    }
}

#[tokio::test]
async fn operation_structural_refusals_do_not_wait_for_database_or_confinement() {
    let _manifest = chunks::set_test_manifest_for_test(Some(MANIFEST.to_vec()));
    let (_dir, root) = test_support::test_root("operation-precheck");
    let ctx = test_support::test_ctx();
    ctx.mark_database_runtime_initializing_for_test();
    let executor = Executor::new();
    executor.register_actor(root.clone(), ctx);
    let routes = HashMap::from([(
        route_key(41, 1),
        occipital_identity(&root, RouteRole::ToolProviderV1),
    )]);
    let mut waits = DatabaseWaits::default();
    for explicit in [false, true] {
        let mut request = decoded(10, "frame-key");
        let mut body = json!({"name":"read","preset":"worker","arguments":{"rich_ui_op":"garbled","path":"x"}});
        if explicit {
            body["op"] = json!("tool.call");
        }
        request.frame = Frame::build(
            FrameType::Request,
            control_flags(),
            41,
            1,
            10,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap();
        let request = waits
            .restore_launch(request, &routes, &executor)
            .expect_err("carrier preset ignored");
        let request = waits
            .defer(request, &routes, &executor)
            .expect_err("operation refusal must not wait");
        assert!(waits.read_confinement(request, &routes, &executor).is_err());
        assert!(waits.is_empty());
    }
    executor.remove_actor(&root);
}

#[tokio::test]
async fn operation_ledger_isolates_custodian_sessions_and_replays_exact_body() {
    use crate::db::call_ledger::{self as ledger, Admission, Key, RecordedFrame, State};
    let _manifest = chunks::set_test_manifest_for_test(Some(MANIFEST.to_vec()));
    let (dir, root) = test_support::test_root("operation-ledger");
    let ctx = test_support::test_ctx();
    let db = Arc::new(StdMutex::new(
        crate::db::open(&dir.path().join("aft.db")).unwrap(),
    ));
    ctx.set_db(db.clone());
    let executor = Arc::new(Executor::new());
    executor.register_actor(root.clone(), ctx.clone());
    let mut identity = occipital_identity(&root, RouteRole::ToolProviderV1);
    Arc::get_mut(&mut identity.0).unwrap().tool_result_body = true;
    let scope = ledger_scope(identity.scope.as_ref().unwrap());
    let owner = scope.owner.clone();
    let agent = |key: &str| Key {
        carrier: owner.clone(),
        call_key: key.into(),
    };
    let agent_body = RecordedFrame {
        ty: FrameType::Response,
        body: br#"{"agent":"private"}"#.to_vec(),
    };
    {
        let conn = db.lock().unwrap();
        for key in ["settled-agent-key", "inflight-write-key"] {
            assert!(matches!(
                ledger::admit(
                    &conn,
                    &agent(key),
                    "agent-digest",
                    Some(&scope),
                    State::DispatchStarted
                )
                .unwrap(),
                Admission::New
            ));
        }
        ledger::settle(
            &conn,
            &agent("settled-agent-key"),
            &agent_body,
            "ok",
            None,
            ledger::now_ms(),
        )
        .unwrap();
    }
    let before = custodian_snapshot(&db.lock().unwrap(), &owner);
    let (edge, mut ready) = LedgerEdge::new();
    let (writer, mut replies) = mpsc::channel(10);
    let metrics = Arc::new(DispatchPathMetrics::new());
    let (proxy, proxy_task) = edge.proxy(writer.clone(), executor.clone(), metrics.clone());
    let routes = HashMap::from([(route_key(41, 1), identity.clone())]);
    let entry = crate::subc::rich_ui::operations::operations()
        .into_iter()
        .find(|entry| entry["id"] == "bash_status")
        .unwrap();
    let mut first_bytes = Vec::new();
    for (index, key) in ["settled-agent-key", "inflight-write-key"]
        .into_iter()
        .enumerate()
    {
        let request = decoded(7 + index as u64, key);
        assert!(edge
            .defer(request, &routes, &executor, &proxy, &metrics)
            .is_ok());
        let admitted = tokio::time::timeout(Duration::from_secs(5), ready.recv())
            .await
            .unwrap()
            .unwrap();
        let result = carrier::response_body(
            &entry,
            &Response::success(
                "forced-handler",
                json!({"status":"running","exit_code":null,"canary":"discard"}),
            ),
        );
        let outgoing =
            crate::subc::wire::build_carrier_response_frame(&admitted.frame, Some(&entry), result)
                .unwrap();
        proxy.send(outgoing).await.unwrap();
        let first = tokio::time::timeout(Duration::from_secs(5), replies.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&first.body).unwrap()["structuredContent"],
            json!({"running":true,"exitCode":null})
        );
        carrier::validate_body(
            Some(&entry),
            &serde_json::from_slice::<Value>(&first.body).unwrap(),
        )
        .unwrap();
        if index == 0 {
            first_bytes = first.body.clone();
        }
        let conn = db.lock().unwrap();
        let row = ledger::get(&conn, &LedgerEdge::key(&identity, key))
            .unwrap()
            .unwrap();
        assert!(row.scope.is_none(), "carrier rows have no custodian");
    }
    assert_eq!(custodian_snapshot(&db.lock().unwrap(), &owner), before);
    let retry = decoded(999, "settled-agent-key");
    assert!(edge
        .defer(retry, &routes, &executor, &proxy, &metrics)
        .is_ok());
    let mut replay = tokio::time::timeout(Duration::from_secs(5), replies.recv())
        .await
        .unwrap()
        .unwrap();
    replay.project_tool_result().unwrap();
    assert_eq!(replay.header.corr, 999);
    assert_eq!(replay.body, first_bytes);
    carrier::validate_body(
        Some(&entry),
        &serde_json::from_slice::<Value>(&replay.body).unwrap(),
    )
    .unwrap();

    let mut session2 = occipital_identity(&root, RouteRole::ToolProviderV1);
    Arc::get_mut(&mut session2.0)
        .unwrap()
        .scope
        .as_mut()
        .unwrap()
        .scope_ref
        .push_str("-session2");
    let routes2 = HashMap::from([(route_key(41, 1), session2.clone())]);
    assert_ne!(
        LedgerEdge::key(&identity, "settled-agent-key"),
        LedgerEdge::key(&session2, "settled-agent-key")
    );
    assert!(edge
        .defer(
            decoded(1000, "settled-agent-key"),
            &routes2,
            &executor,
            &proxy,
            &metrics
        )
        .is_ok());
    let fresh = tokio::time::timeout(Duration::from_secs(5), ready.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        fresh.frame.header.corr, 1000,
        "S2 receives a fresh evaluation, not S1 bytes"
    );
    let mut unscoped = occipital_identity(&root, RouteRole::ToolProviderV1);
    Arc::get_mut(&mut unscoped.0).unwrap().scope = None;
    assert_eq!(
        LedgerEdge::key(&unscoped, "settled-agent-key").carrier,
        "reserved:occipital"
    );
    let unscoped_routes = HashMap::from([(route_key(41, 1), unscoped.clone())]);
    assert!(edge
        .defer(
            decoded(1001, "settled-agent-key"),
            &unscoped_routes,
            &executor,
            &proxy,
            &metrics
        )
        .is_err());
    let reply = exchange_with_actor(
        json!({"name":"bash_status","call_key":"settled-agent-key","arguments":{}}),
        unscoped,
        ctx.clone(),
        &metrics,
        |_, _| panic!("unscoped carrier has no session"),
    )
    .await;
    assert_eq!(
        serde_json::from_slice::<Value>(&reply.body).unwrap()["code"],
        "session_not_bound"
    );
    {
        let conn = db.lock().unwrap();
        ledger::settle(
            &conn,
            &agent("inflight-write-key"),
            &agent_body,
            "ok",
            None,
            ledger::now_ms(),
        )
        .unwrap();
        let after = custodian_snapshot(&conn, &owner);
        assert_eq!(after.0, before.0 + 1);
        assert_eq!(after.1, before.1 + 1);
        let feed = String::from_utf8(after.2).unwrap();
        assert!(feed.contains("inflight-write-key"));
        assert!(feed.contains("private"));
    }
    drop(proxy);
    proxy_task.await.unwrap();
    executor.remove_actor(&root);
}
