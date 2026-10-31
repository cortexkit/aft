#[tokio::test]
async fn audit_existing_direct_worker_contract_allows_prompt_and_worker_grant() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    let control = Controlled::install(f.fake.clone());
    let target = f.dir.path().join("elsewhere.txt");
    let mut replies = f.send_read(&f.worker, &target, 9400).await;
    let reply = tokio::select! {
        request = control.request(0) => {
            control.answer(&request, "allow_scope");
            tokio::time::timeout(Duration::from_secs(10), replies.recv()).await.unwrap().unwrap()
        }
        reply = replies.recv() => reply.unwrap(),
    };
    let body: Value = serde_json::from_slice(&reply.body).unwrap();
    let grants = f.worker_grants();
    eprintln!(
        "AUDIT direct-worker: questions={} grants={} result={body}",
        f.fake.calls("consent.request").len(),
        grants.len()
    );
    assert_eq!(f.fake.calls("consent.request").len(), 1);
    assert!(!grants.is_empty());
    assert!(text(&body).contains("unheld contents"), "{body}");
}

#[tokio::test]
async fn audit_scope_end_ledger_failure_must_still_prevent_late_approval() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    let control = Controlled::install(f.fake.clone());
    let run = program(&f, false, "run:audit-revoke");
    let gate = FinishGateOverride::install("subc-41-9401", false);
    let target = f.dir.path().join("elsewhere.txt");
    let mut replies = f.send_read(&run, &target, 9401).await;
    held(&mut replies).await;
    let request = control.request(0).await;
    control.answer(&request, "allow_scope");
    wait_for(|| gate.0.entered.load(Ordering::SeqCst)).await;
    f.ctx
        .db()
        .unwrap()
        .lock()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER audit_fail_end BEFORE UPDATE OF state ON call_ledger
         WHEN NEW.outcome='not_started' BEGIN SELECT RAISE(ABORT, 'audit end failure'); END;",
        )
        .unwrap();
    audit_confirm_scope_end(&f, &run).await;
    {
        let db = f.ctx.db().unwrap();
        let conn = db.lock().unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT ended FROM access_scopes WHERE scope_ref=?1",
                [&run.scope.as_ref().unwrap().scope_ref],
                |r| r.get::<_, u64>(0)
            )
            .unwrap(),
            1
        );
        assert_eq!(
            ledger::get(&conn, &tool_provider::LedgerEdge::key(&run, "program-9401"))
                .unwrap()
                .unwrap()
                .state,
            ledger::State::Prepared
        );
    }
    run.run_scope.get().unwrap().end_for_test();
    f.ctx
        .db()
        .unwrap()
        .lock()
        .unwrap()
        .execute_batch("DROP TRIGGER audit_fail_end")
        .unwrap();
    gate.release();
    let result = late(&f, &run, 9401).await;
    let grants = crate::db::access_grants::list(
        &f.ctx.db().unwrap().lock().unwrap(),
        &access_grants::policy_scope_key(&run),
    )
    .unwrap();
    eprintln!(
        "AUDIT failed durable revoke: result={result} grants={}",
        grants.len()
    );
    assert!(
        result["isError"] == true && grants.is_empty(),
        "revoked RunScope was applied after its entry check"
    );
}

async fn audit_confirm_scope_end(f: &Fixture, run: &RouteIdentity) {
    use subc_protocol::session::ModuleControlResponseToModule;
    let (writer, mut frames) = mpsc::channel(8);
    let pending = readiness::PendingControlReplies::default();
    let control = Arc::new(readiness::DaemonControl::new(
        writer,
        Arc::new(DispatchPathMetrics::new()),
        PROTOCOL_VERSION,
        vec![subc_protocol::scope::SCOPE_DESCRIBE_OP.into()],
        vec![],
        pending.clone(),
        HELLO_CORR + 1,
    ));
    let scopes = Arc::new(run_scope::RunScopes::new(control.clone()));
    // The fixture's active RunScope is not registered in this worker. A parked
    // copy of its ask makes production discovery track the run's owner/ref/epoch.
    let mut watched = identity(&run.project_root, run.scope.clone());
    Arc::get_mut(&mut watched.0).unwrap().call_mode = subc_protocol::call_mode::CallMode::Codemode;
    let watched_scope = scopes.bind(&watched).unwrap();
    let db = f.ctx.db().unwrap();
    let ask = {
        let conn = db.lock().unwrap();
        let mut rows = asks::for_scope(&conn, &access_grants::scope_key(run)).unwrap();
        assert_eq!(rows.len(), 1);
        rows.pop().unwrap()
    };
    watched_scope.park_ask(ParkedAsk {
        call: ConsentCall {
            scope: serde_json::from_value(ask.selector).unwrap(),
            project_root: run.project_root.clone(),
            session: run.session.clone(),
            method: "consent.withdraw",
            params: json!({"elicitation_id":ask.elicitation_id.unwrap()}),
            timeout: UNARY_TIMEOUT,
        },
        key: ask.key,
        db,
    });
    let (describes, mut ended, worker) = scope_describe::start(control, f.executor.clone(), scopes);
    describes.goodbye(run.scope.clone().unwrap());
    let request = tokio::time::timeout(Duration::from_secs(10), frames.recv())
        .await
        .unwrap()
        .unwrap();
    let answer = ModuleControlResponseToModule::ScopeDescribe {
        status: subc_protocol::scope::ScopeStatus::Ended,
        scope_epoch: Some(7),
        daemon_incarnation: "audit-daemon".into(),
        owner_synced: true,
        owner_configured: true,
        scope: None,
    };
    assert!(pending.resolve(
        Frame::build(
            FrameType::Response,
            control_flags(),
            0,
            0,
            request.header.corr,
            serde_json::to_vec(&answer).unwrap()
        )
        .unwrap()
    ));
    let notice = tokio::time::timeout(Duration::from_secs(10), ended.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        notice.stamp.scope_ref,
        run.scope.as_ref().unwrap().scope_ref
    );
    worker.abort();
}

#[tokio::test]
async fn audit_durable_scope_end_before_application_prevents_execution_and_grant() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    let control = Controlled::install(f.fake.clone());
    let run = program(&f, false, "run:audit-durable-end");
    let gate = FinishGateOverride::install("subc-41-9402", false);
    let target = f.dir.path().join("elsewhere.txt");
    let mut replies = f.send_read(&run, &target, 9402).await;
    held(&mut replies).await;
    let request = control.request(0).await;
    control.answer(&request, "allow_scope");
    wait_for(|| gate.0.entered.load(Ordering::SeqCst)).await;
    let key = tool_provider::LedgerEdge::key(&run, "program-9402");
    {
        let db = f.ctx.db().unwrap();
        let conn = db.lock().unwrap();
        let row = ledger::get(&conn, &key).unwrap().unwrap();
        assert_eq!(
            ledger::end_scope(&conn, row.scope.as_ref().unwrap(), ledger::now_ms()).unwrap(),
            vec![key.clone()]
        );
    }
    run.run_scope.get().unwrap().end_for_test();
    gate.release();
    wait_for(|| gate.0.completed.load(Ordering::SeqCst)).await;
    let db = f.ctx.db().unwrap();
    let conn = db.lock().unwrap();
    let row = ledger::get(&conn, &key).unwrap().unwrap();
    assert_eq!(row.outcome.as_deref(), Some("not_started"));
    assert!(
        crate::db::access_grants::list(&conn, &access_grants::policy_scope_key(&run))
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn audit_parallel_wait_budget_expires_all_calls_without_execution() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    let control = Controlled::install(f.fake.clone());
    let run = program(&f, false, "run:audit-parallel-budget");
    let a = f.dir.path().join("elsewhere.txt");
    let b = f.dir.path().join("second.txt");
    std::fs::write(&b, "second private contents").unwrap();
    let mut replies_a = f.send_read(&run, &a, 9403).await;
    held(&mut replies_a).await;
    control.request(0).await;
    let mut replies_b = f.send_read(&run, &b, 9404).await;
    held(&mut replies_b).await;
    control.request(1).await;
    run.run_scope
        .get()
        .unwrap()
        .permission_budget
        .advance(Duration::from_secs(301));
    control.changed.notify_waiters();
    assert_eq!(late(&f, &run, 9403).await["errorCode"], "access_timeout");
    assert_eq!(late(&f, &run, 9404).await["errorCode"], "access_timeout");
    assert!(f.fake.calls("consent.withdraw").len() >= 2);
}

#[tokio::test]
async fn audit_shell_always_does_not_approve_the_next_command() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    f.ctx
        .update_config(|c| c.sandbox.profile.head = crate::config::SandboxMode::Off);
    let control = Controlled::install(f.fake.clone());
    let run = program(&f, false, "run:audit-shell-always");
    let marker = run.project_root.join("executions");
    let command = format!(
        "printf x >> '{}'; printf audit-shell-output",
        marker.display()
    );
    let args = json!({"command":command,"sandbox":"host"});
    let mut replies = f
        .send_tool(&run, "bash", args.clone(), 9405, escalation_dispatch)
        .await;
    held(&mut replies).await;
    let request = control.request(0).await;
    assert_eq!(request["command"], command);
    control.answer(&request, "allow_scope");
    let result = late(&f, &run, 9405).await;
    assert!(result["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("audit-shell-output"));
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "x");
    let mut replies = f
        .send_tool(&run, "bash", args, 9406, escalation_dispatch)
        .await;
    held(&mut replies).await;
    let second = control.request(1).await;
    assert_ne!(second["call_key"], request["call_key"]);
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "x");
    control.answer(&second, "deny");
    assert_eq!(late(&f, &run, 9406).await["errorCode"], "access_declined");
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "x");
}

#[tokio::test]
async fn audit_owner_feed_contains_tagged_result_and_excludes_other_principals() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    let control = Controlled::install(f.fake.clone());
    let run = program(&f, false, "run:audit-feed");
    let mut replies = f
        .send_read(&run, &f.dir.path().join("elsewhere.txt"), 9407)
        .await;
    held(&mut replies).await;
    let request = control.request(0).await;
    control.answer(&request, "allow_once");
    let result = late(&f, &run, 9407).await;
    assert!(result["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("unheld contents"));
    let feed = f
        .checkout_tool(&run, "late_results", json!({}), 9408, read_dispatch)
        .await;
    assert!(
        feed["entries"].as_array().unwrap().is_empty(),
        "the carrier is not the custodian: {feed}"
    );
    let mut owner = identity(&run.project_root, f.head.scope.clone());
    if let AuthenticatedPrincipal::RouteBind { principal_id, .. } =
        &mut Arc::get_mut(&mut owner.0).unwrap().spawn_principal
    {
        *principal_id = Some("reserved:prefrontal-core".into());
    }
    let feed =
        audit_keyed_reply(&f, &owner, "late_results", json!({}), "unused-provider-key").await;
    let entries = feed["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "{feed}");
    assert_eq!(entries[0]["call_key"], "program-9407");
    assert_eq!(entries[0]["owner"], "reserved:prefrontal-core");
    assert_eq!(entries[0]["custodian"], "reserved:prefrontal-core");
    assert_eq!(entries[0]["ref"], "run:audit-feed");
    assert_eq!(entries[0]["scope_epoch"], 7);
    let wrong_ack = audit_keyed_reply(
        &f,
        &run,
        "late_results.ack",
        json!({"through":feed["cursor"]}),
        "unused-provider-key",
    )
    .await;
    assert!(
        wrong_ack["code"].is_string(),
        "an unrelated custodian must not acknowledge this cursor: {wrong_ack}"
    );
    let again =
        audit_keyed_reply(&f, &owner, "late_results", json!({}), "unused-provider-key").await;
    assert_eq!(again["entries"], feed["entries"]);
}

#[test]
fn audit_restart_wait_is_not_started_and_changed_scope_or_digest_conflicts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.db");
    let db = crate::db::open(&path).unwrap();
    let key = ledger::Key {
        carrier: "reserved:basal".into(),
        call_key: "audit-restart".into(),
    };
    let scope = ledger::ScopeIdentity {
        owner: "reserved:prefrontal-core".into(),
        scope_ref: "run:audit-restart".into(),
        scope_epoch: 7,
    };
    assert!(matches!(
        ledger::admit(
            &db,
            &key,
            "original-command",
            Some(&scope),
            ledger::State::DispatchStarted
        )
        .unwrap(),
        ledger::Admission::New
    ));
    let hold = serde_json::to_vec(
        &json!({"content":[{"type":"text","text":""}],"held":{"reason":"consent"}}),
    )
    .unwrap();
    assert!(ledger::hold_permission(&db, &key, &hold, true).unwrap());
    drop(db);
    let db = crate::db::open(&path).unwrap();
    ledger::recover(&db).unwrap();
    let row = ledger::get(&db, &key).unwrap().unwrap();
    assert_eq!(row.state, ledger::State::Settled);
    assert_eq!(row.outcome.as_deref(), Some("not_started"));
    assert!(!ledger::begin_approved(&db, &key).unwrap());
    assert!(matches!(
        ledger::admit(
            &db,
            &key,
            "original-command",
            Some(&scope),
            ledger::State::DispatchStarted
        )
        .unwrap(),
        ledger::Admission::Repeat(_)
    ));
    assert!(matches!(
        ledger::admit(
            &db,
            &key,
            "different-command",
            Some(&scope),
            ledger::State::DispatchStarted
        )
        .unwrap(),
        ledger::Admission::Conflict
    ));
    let other = ledger::ScopeIdentity {
        scope_ref: "run:other".into(),
        ..scope
    };
    assert!(matches!(
        ledger::admit(
            &db,
            &key,
            "original-command",
            Some(&other),
            ledger::State::DispatchStarted
        )
        .unwrap(),
        ledger::Admission::Conflict
    ));
}

async fn audit_keyed_reply(
    f: &Fixture,
    run: &RouteIdentity,
    name: &str,
    arguments: Value,
    call_key: &str,
) -> Value {
    let (edge, mut ready) = tool_provider::LedgerEdge::new();
    let (writer, mut replies) = mpsc::channel(8);
    let routes = HashMap::from([(route_key(41, 1), run.clone())]);
    let frame = Frame::build(
        FrameType::Request,
        control_flags(),
        41,
        1,
        9499,
        serde_json::to_vec(
            &json!({"name":name,"preset":"head","arguments":arguments,"call_key":call_key}),
        )
        .unwrap(),
    )
    .unwrap();
    let decoded = DecodedFrame {
        parsed_body: Default::default(),
        confinement: None,
        launch_restore: None,
        codemode_policies: None,
        frame,
        phase_trace: PhaseTrace::new(Instant::now()),
    };
    let metrics = Arc::new(DispatchPathMetrics::new());
    assert!(edge
        .defer(decoded, &routes, &f.executor, &writer, &metrics)
        .is_ok());
    let reply = tokio::time::timeout(Duration::from_secs(10), replies.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        ready.try_recv().is_err(),
        "replay must not admit another execution"
    );
    serde_json::from_slice(&reply.body).unwrap()
}

#[tokio::test]
async fn audit_shell_same_key_replay_is_once_and_changed_command_conflicts() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    f.ctx
        .update_config(|c| c.sandbox.profile.head = crate::config::SandboxMode::Off);
    let control = Controlled::install(f.fake.clone());
    let run = program(&f, false, "run:audit-replay");
    let marker = run.project_root.join("executions");
    let command = format!(
        "printf x >> '{}'; printf audit-replay-output",
        marker.display()
    );
    let args = json!({"command":command,"sandbox":"host"});
    let mut replies = f
        .send_tool(&run, "bash", args.clone(), 9409, escalation_dispatch)
        .await;
    held(&mut replies).await;
    let request = control.request(0).await;
    control.answer(&request, "allow_once");
    let result = late(&f, &run, 9409).await;
    assert!(result["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("audit-replay-output"));
    let reopened = crate::db::open(&f.dir.path().join("store/aft.db")).unwrap();
    ledger::recover(&reopened).unwrap();
    control.answer(&request, "allow_once");
    let replay = audit_keyed_reply(&f, &run, "bash", args, "program-9409").await;
    assert!(
        replay.to_string().contains("audit-replay-output"),
        "{replay}"
    );
    let conflict = audit_keyed_reply(
        &f,
        &run,
        "bash",
        json!({"command":"printf malicious","sandbox":"host"}),
        "program-9409",
    )
    .await;
    assert!(
        conflict.to_string().contains("call_key_conflict"),
        "{conflict}"
    );
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "x");
    assert_eq!(f.fake.calls("consent.request").len(), 1);
}

#[tokio::test]
async fn audit_pending_call_binds_key_operation_path_cwd_and_scope() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    let control = Controlled::install(f.fake.clone());
    let run = program(&f, false, "run:audit-binding");
    let target = f.dir.path().join("elsewhere.txt");
    let mut replies = f
        .send_tool(&run, "read", json!({"path":target}), 9410, read_dispatch)
        .await;
    held(&mut replies).await;
    let request = control.request(0).await;
    assert_eq!(request["operation"], "read");
    assert_eq!(request["paths"], json!([target]));
    assert_eq!(request["tool"], "read");
    assert_eq!(
        control.scopes.lock().unwrap()[0].scope_ref,
        "run:audit-binding"
    );
    let args = json!({"path":target});
    // An in-flight repeat attaches until the original terminal answer; it does
    // not receive another initial held reply or dispatch a second execution.
    let replay = audit_keyed_reply(&f, &run, "read", args.clone(), "program-9410");
    let finish_original = async {
        for (name, changed) in [
            ("read", json!({"path":f.head_root.join("shared.txt")})),
            ("write", json!({"path":target,"content":"not approved"})),
            (
                "bash",
                json!({"command":"printf not-approved","workdir":f.head_root}),
            ),
        ] {
            let reply = audit_keyed_reply(&f, &run, name, changed, "program-9410").await;
            assert!(reply.to_string().contains("call_key_conflict"), "{reply}");
        }
        let other = program(&f, false, "run:audit-other-binding");
        let reply = audit_keyed_reply(&f, &other, "read", args, "program-9410").await;
        assert!(reply.to_string().contains("call_key_conflict"), "{reply}");
        assert_eq!(f.fake.calls("consent.request").len(), 1);
        control.answer(&request, "allow_once");
        assert!(late(&f, &run, 9410)
            .await
            .to_string()
            .contains("unheld contents"));
    };
    let (replay, ()) = tokio::join!(replay, finish_original);
    assert!(replay.to_string().contains("unheld contents"), "{replay}");
    assert_eq!(
        std::fs::read_to_string(target).unwrap(),
        "unheld contents\n"
    );
}

#[tokio::test]
async fn audit_shell_approval_cannot_release_a_different_waiting_command() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    f.ctx
        .update_config(|c| c.sandbox.profile.head = crate::config::SandboxMode::Off);
    let control = Controlled::install(f.fake.clone());
    let run = program(&f, false, "run:audit-shell-binding");
    let approved = run.project_root.join("approved");
    let unapproved = run.project_root.join("unapproved");
    let command_a = format!("printf x >> '{}'; pwd", approved.display());
    let command_b = format!("printf x >> '{}'", unapproved.display());
    let mut a = f
        .send_tool(
            &run,
            "bash",
            json!({"command":command_a,"sandbox":"host"}),
            9411,
            escalation_dispatch,
        )
        .await;
    held(&mut a).await;
    let request_a = control.request(0).await;
    let mut b = f
        .send_tool(
            &run,
            "bash",
            json!({"command":command_b,"sandbox":"host"}),
            9412,
            escalation_dispatch,
        )
        .await;
    held(&mut b).await;
    let request_b = control.request(1).await;
    assert_eq!(request_a["operation"], "exec");
    assert_eq!(request_a["command"], command_a);
    assert_eq!(request_a["pattern"], command_a);
    assert_eq!(request_a["paths"], json!([run.project_root]));
    assert_eq!(request_b["command"], command_b);
    assert_ne!(request_a["call_key"], request_b["call_key"]);
    assert_ne!(request_a["args_hash"], request_b["args_hash"]);
    assert!(!approved.exists() && !unapproved.exists());
    let changed_cwd = audit_keyed_reply(
        &f,
        &run,
        "bash",
        json!({"command":command_a,"sandbox":"host","workdir":f.dir.path()}),
        "program-9411",
    )
    .await;
    assert!(
        changed_cwd.to_string().contains("call_key_conflict"),
        "{changed_cwd}"
    );
    control.answer(&request_a, "allow_once");
    assert!(late(&f, &run, 9411)
        .await
        .to_string()
        .contains(run.project_root.to_str().unwrap()));
    assert_eq!(std::fs::read_to_string(approved).unwrap(), "x");
    assert!(
        !unapproved.exists(),
        "approval of a different call ran this command"
    );
    control.answer(&request_b, "deny");
    assert_eq!(late(&f, &run, 9412).await["errorCode"], "access_declined");
    assert!(!unapproved.exists());
}

#[tokio::test]
async fn audit_shell_transport_errors_have_no_side_effects_or_grants() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    for case in ["unreachable", "malformed", "refused"] {
        let f = fixture(FakeCingulate::up());
        f.ctx
            .update_config(|c| c.sandbox.profile.head = crate::config::SandboxMode::Off);
        let control = Controlled::install(f.fake.clone());
        let run = program(&f, false, &format!("run:audit-shell-{case}"));
        let marker = run.project_root.join("must-not-run");
        if case == "unreachable" {
            f.fake.state.lock().unwrap().up = false;
        }
        let mut replies = f
            .send_tool(
                &run,
                "bash",
                json!({"command":format!("printf x >> '{}'", marker.display()),"sandbox":"host"}),
                9413,
                escalation_dispatch,
            )
            .await;
        held(&mut replies).await;
        if case != "unreachable" {
            let request = control.request(0).await;
            f.fake.answer(
                request["call_key"].as_str().unwrap(),
                if case == "malformed" {
                    json!({"state":"settled","outcome":"allow_scope","answered_by":false})
                } else {
                    json!({"state":"refused:access_not_placed","outcome":"access_not_placed"})
                },
            );
            control.changed.notify_waiters();
        }
        let result = late(&f, &run, 9413).await;
        assert_eq!(result["isError"], true, "{case}: {result}");
        assert!(
            matches!(
                result["errorCode"].as_str(),
                Some("access_unavailable" | "access_not_placed")
            ),
            "{case}: {result}"
        );
        assert!(!marker.exists(), "{case} dispatched a command");
        assert!(crate::db::access_grants::list(
            &f.ctx.db().unwrap().lock().unwrap(),
            &access_grants::policy_scope_key(&run)
        )
        .unwrap()
        .is_empty());
    }
}

#[tokio::test]
async fn audit_recovery_of_waiting_shell_rejects_late_allow() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    f.ctx
        .update_config(|c| c.sandbox.profile.head = crate::config::SandboxMode::Off);
    let control = Controlled::install(f.fake.clone());
    let run = program(&f, false, "run:audit-restart-shell");
    let marker = run.project_root.join("must-not-run");
    let mut replies = f
        .send_tool(
            &run,
            "bash",
            json!({"command":format!("printf x >> '{}'", marker.display()),"sandbox":"host"}),
            9414,
            escalation_dispatch,
        )
        .await;
    held(&mut replies).await;
    let request = control.request(0).await;
    let reopened = crate::db::open(&f.dir.path().join("store/aft.db")).unwrap();
    ledger::recover(&reopened).unwrap();
    let key = tool_provider::LedgerEdge::key(&run, "program-9414");
    let before = ledger::get(&reopened, &key).unwrap().unwrap();
    let entry: Value = serde_json::from_str(before.late_entry.as_deref().unwrap()).unwrap();
    assert_eq!(entry["kind"], "not_started");
    assert_eq!(entry["reason"], "restart_before_dispatch");
    assert!(!ledger::begin_approved(&reopened, &key).unwrap());
    let gate = FinishGateOverride::install("subc-41-9414", false);
    control.answer(&request, "allow_scope");
    wait_for(|| gate.0.entered.load(Ordering::SeqCst)).await;
    gate.release();
    wait_for(|| gate.0.completed.load(Ordering::SeqCst)).await;
    assert!(!marker.exists());
    assert_eq!(
        ledger::get(&reopened, &key).unwrap().unwrap().late_entry,
        before.late_entry
    );
}

#[tokio::test]
async fn audit_existing_acknowledged_key_reuse_is_a_new_call() {
    static READS: AtomicU64 = AtomicU64::new(0);
    fn counted_read(request: RawRequest, ctx: &AppContext) -> Response {
        let result = read_dispatch(request, ctx);
        if result.success {
            READS.fetch_add(1, Ordering::SeqCst);
        }
        result
    }
    let _lock = CONSENT_TEST_LOCK.lock().await;
    READS.store(0, Ordering::SeqCst);
    let f = fixture(FakeCingulate::up());
    let control = Controlled::install(f.fake.clone());
    let run = program(&f, false, "run:audit-lost-ack");
    let path = f.dir.path().join("elsewhere.txt");
    let mut replies = f
        .send_tool(&run, "read", json!({"path":path}), 9415, counted_read)
        .await;
    held(&mut replies).await;
    let request = control.request(0).await;
    control.answer(&request, "allow_scope");
    assert!(late(&f, &run, 9415)
        .await
        .to_string()
        .contains("unheld contents"));
    assert_eq!(READS.load(Ordering::SeqCst), 1);
    let key = tool_provider::LedgerEdge::key(&run, "program-9415");
    let reopened = crate::db::open(&f.dir.path().join("store/aft.db")).unwrap();
    ledger::recover(&reopened).unwrap();
    let row = ledger::get(&reopened, &key).unwrap().unwrap();
    let cursor = cortexkit_role_tool_provider::late_results::Cursor {
        provider_incarnation: ledger::provider_incarnation().into(),
        seq: row.seq.unwrap(),
    };
    // Acknowledgement ends the old call's lifetime; reusing its key names a new call.
    ledger::ack_through(
        &reopened,
        &row.scope.unwrap().owner,
        ledger::provider_incarnation(),
        &cursor,
    )
    .unwrap()
    .unwrap();
    let mut resend = f
        .send_tool(&run, "read", json!({"path":path}), 9415, counted_read)
        .await;
    let reply = tokio::time::timeout(Duration::from_secs(10), resend.recv())
        .await
        .unwrap()
        .unwrap();
    let body: Value = serde_json::from_slice(&reply.body).unwrap();
    assert!(body.to_string().contains("unheld contents"), "{body}");
    assert_eq!(
        READS.load(Ordering::SeqCst),
        2,
        "acknowledgement ends the old call's deduplication lifetime"
    );
}

#[tokio::test]
async fn audit_folder_grant_does_not_authorize_another_head_session() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let mut f = fixture(FakeCingulate::up());
    let control = Controlled::install(f.fake.clone());
    let original_head = f.head.clone();
    let run = program(&f, false, "run:audit-head-a");
    let path = f.dir.path().join("elsewhere.txt");
    let mut replies = f.send_read(&run, &path, 9416).await;
    held(&mut replies).await;
    let request = control.request(0).await;
    control.answer(&request, "allow_scope");
    assert_ne!(late(&f, &run, 9416).await["isError"], true);
    f.head = identity(&f.head_root, Some(stamp("other-head", "head", None)));
    access_grants::register(&f.head, &f.ctx).unwrap();
    remote_policy::catalog(
        json!({"preset":"head","params":{"mode":"ask"}}),
        &f.head,
        &f.ctx,
    )
    .unwrap();
    let other = program(&f, false, "run:audit-head-b");
    assert_ne!(other.session, run.session);
    let mut replies = f.send_read(&other, &path, 9417).await;
    held(&mut replies).await;
    let request = control.request(1).await;
    control.answer(&request, "deny");
    assert_eq!(late(&f, &other, 9417).await["errorCode"], "access_declined");
    let db = f.ctx.db().unwrap();
    let conn = db.lock().unwrap();
    assert!(
        !crate::db::access_grants::list(&conn, &access_grants::scope_key(&original_head))
            .unwrap()
            .is_empty()
    );
    for scope in [&f.head, &f.worker, &run, &other] {
        assert!(
            crate::db::access_grants::list(&conn, &access_grants::scope_key(scope))
                .unwrap()
                .is_empty()
        );
    }
}

#[tokio::test]
async fn audit_worker_program_shell_and_file_never_ask_or_save_grants() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    let run = program(&f, true, "run:audit-worker-no-approval");
    let marker = run.project_root.join("must-not-run");
    for (corr, name, args, dispatch) in [
        (
            9418,
            "read",
            json!({"path":f.dir.path().join("elsewhere.txt")}),
            read_dispatch as DispatchFn,
        ),
        (
            9419,
            "bash",
            json!({"command":format!("printf x >> '{}'", marker.display()),"sandbox":"host"}),
            escalation_dispatch as DispatchFn,
        ),
    ] {
        let result = f.checkout_tool(&run, name, args, corr, dispatch).await;
        assert!(
            result.to_string().contains("permission_ask_unavailable"),
            "{result}"
        );
        assert!(
            result.get("held").is_none(),
            "worker programs must not be parked for consent"
        );
    }
    assert!(f.fake.calls("consent.request").is_empty());
    assert!(!marker.exists());
    let db = f.ctx.db().unwrap();
    let conn = db.lock().unwrap();
    for scope in [&f.head, &f.worker, &run] {
        assert!(
            crate::db::access_grants::list(&conn, &access_grants::scope_key(scope))
                .unwrap()
                .is_empty()
        );
    }
}

#[tokio::test]
async fn audit_concurrent_runs_cannot_cross_apply_answers_or_results() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    let control = Controlled::install(f.fake.clone());
    let a = program(&f, false, "run:audit-routing-a");
    let b = program(&f, false, "run:audit-routing-b");
    let target_a = f.dir.path().join("elsewhere.txt");
    let target_b = f.dir.path().join("private-b.txt");
    std::fs::write(&target_b, "only run b may receive this").unwrap();
    let mut replies_a = f.send_read(&a, &target_a, 9420).await;
    held(&mut replies_a).await;
    let request_a = control.request(0).await;
    let mut replies_b = f.send_read(&b, &target_b, 9421).await;
    held(&mut replies_b).await;
    let request_b = control.request(1).await;
    assert_ne!(request_a["call_key"], request_b["call_key"]);
    assert_eq!(
        control.scopes.lock().unwrap()[0].scope_ref,
        "run:audit-routing-a"
    );
    assert_eq!(
        control.scopes.lock().unwrap()[1].scope_ref,
        "run:audit-routing-b"
    );
    control.answer(&request_b, "allow_once");
    let result_b = late(&f, &b, 9421).await;
    assert!(result_b.to_string().contains("only run b may receive this"));
    let key_a = tool_provider::LedgerEdge::key(&a, "program-9420");
    let row_a = ledger::get(&f.ctx.db().unwrap().lock().unwrap(), &key_a)
        .unwrap()
        .unwrap();
    assert_eq!(row_a.state, ledger::State::Prepared);
    assert!(row_a.late_entry.is_none());
    control.answer(&request_a, "deny");
    assert_eq!(late(&f, &a, 9420).await["errorCode"], "access_declined");
    let db = f.ctx.db().unwrap();
    let conn = db.lock().unwrap();
    for (run, key, text) in [
        (&a, "program-9420", "access_declined"),
        (&b, "program-9421", "only run b may receive this"),
    ] {
        let row = ledger::get(&conn, &tool_provider::LedgerEdge::key(run, key))
            .unwrap()
            .unwrap();
        let entry: Value = serde_json::from_str(row.late_entry.as_deref().unwrap()).unwrap();
        assert_eq!(entry["ref"], run.scope.as_ref().unwrap().scope_ref);
        assert_eq!(entry["call_key"], key);
        assert!(entry["result"].to_string().contains(text));
    }
}

#[test]
fn audit_schema25_additive_migration_preserves_all_ledger_columns_and_refuses_future_schema() {
    fn snapshot(db: &rusqlite::Connection) -> Vec<Vec<rusqlite::types::Value>> {
        let mut stmt = db
            .prepare("SELECT * FROM call_ledger ORDER BY carrier,call_key")
            .unwrap();
        let columns = stmt.column_count();
        stmt.query_map([], |row| {
            (0..columns).map(|column| row.get(column)).collect()
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aft.db");
    let db = crate::db::open(&path).unwrap();
    let key = ledger::Key {
        carrier: "audit".into(),
        call_key: "migration".into(),
    };
    let scope = ledger::ScopeIdentity {
        owner: "reserved:prefrontal-core".into(),
        scope_ref: "run:audit".into(),
        scope_epoch: 7,
    };
    ledger::admit_with_tool_result_source(
        &db,
        &key,
        "kept",
        Some(&scope),
        ledger::State::DispatchStarted,
        true,
    )
    .unwrap();
    let frame = ledger::RecordedFrame {
        ty: FrameType::Response,
        body: serde_json::to_vec(
            &json!({"content":[{"type":"text","text":"retained sensitive output"}]}),
        )
        .unwrap(),
    };
    ledger::settle_with_tool_result_source(
        &db,
        &key,
        &frame,
        "ok",
        None,
        ledger::now_ms(),
        Some(b"source"),
    )
    .unwrap();
    for (name, state) in [
        ("prepared", ledger::State::Prepared),
        ("authorized", ledger::State::Authorized),
        ("started", ledger::State::DispatchStarted),
    ] {
        let key = ledger::Key {
            carrier: "other-carrier".into(),
            call_key: name.into(),
        };
        ledger::admit(
            &db,
            &key,
            name,
            if name == "authorized" {
                None
            } else {
                Some(&scope)
            },
            state,
        )
        .unwrap();
    }
    db.execute(
        "UPDATE call_ledger SET withdraw_answer=?1,recovered=1 WHERE call_key='migration'",
        [b"retained-withdrawal".as_slice()],
    )
    .unwrap();
    let before = snapshot(&db);
    assert_eq!(before.len(), 4);
    db.execute_batch("DROP TABLE call_ledger_updates; UPDATE schema_version SET version=24;")
        .unwrap();
    drop(db);
    let db = crate::db::open(&path).unwrap();
    let after = snapshot(&db);
    assert_eq!(before, after);
    assert_eq!(
        db.query_row("SELECT version FROM schema_version", [], |r| r
            .get::<_, u32>(0))
            .unwrap(),
        25
    );
    db.execute_batch("UPDATE schema_version SET version=26;")
        .unwrap();
    drop(db);
    assert!(
        crate::db::open(&path).is_err(),
        "a future schema must not be downgraded"
    );
    let raw = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        raw.query_row("SELECT version FROM schema_version", [], |r| r
            .get::<_, u32>(0))
            .unwrap(),
        26
    );
    assert_eq!(
        raw.query_row(
            "SELECT count(*) FROM call_ledger WHERE call_key='migration'",
            [],
            |r| r.get::<_, u32>(0)
        )
        .unwrap(),
        1
    );
}
