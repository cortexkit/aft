#[tokio::test]
async fn audit_direct_worker_must_not_ask_or_save_a_standing_grant() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    let control = Controlled::install(f.fake.clone());
    let target = f.dir.path().join("elsewhere.txt");
    let mut replies = f.send_read(&f.worker, &target, 9400).await;
    let request = control.request(0).await;
    control.answer(&request, "allow_scope");
    let reply = tokio::time::timeout(Duration::from_secs(10), replies.recv()).await.unwrap().unwrap();
    let body: Value = serde_json::from_slice(&reply.body).unwrap();
    let grants = f.worker_grants();
    eprintln!("AUDIT direct-worker: questions={} grants={} result={body}", f.fake.calls("consent.request").len(), grants.len());
    assert!(f.fake.calls("consent.request").is_empty() && grants.is_empty(), "direct worker obtained a question and a standing grant");
}

#[tokio::test]
async fn audit_revoke_after_settlement_before_application_must_not_run() {
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
    run.run_scope.get().unwrap().end_for_test();
    gate.release();
    let result = late(&f, &run, 9401).await;
    let grants = crate::db::access_grants::list(&f.ctx.db().unwrap().lock().unwrap(), &access_grants::policy_scope_key(&run)).unwrap();
    eprintln!("AUDIT atomic-only revoke: result={result} grants={}", grants.len());
    assert!(result["isError"] == true && grants.is_empty(), "revoked RunScope was applied after its entry check");
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
        assert_eq!(ledger::end_scope(&conn, row.scope.as_ref().unwrap(), ledger::now_ms()).unwrap(), vec![key.clone()]);
    }
    run.run_scope.get().unwrap().end_for_test();
    gate.release();
    wait_for(|| gate.0.completed.load(Ordering::SeqCst)).await;
    let db = f.ctx.db().unwrap();
    let conn = db.lock().unwrap();
    let row = ledger::get(&conn, &key).unwrap().unwrap();
    assert_eq!(row.outcome.as_deref(), Some("not_started"));
    assert!(crate::db::access_grants::list(&conn, &access_grants::policy_scope_key(&run)).unwrap().is_empty());
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
    run.run_scope.get().unwrap().permission_budget.advance(Duration::from_secs(301));
    control.changed.notify_waiters();
    assert_eq!(late(&f, &run, 9403).await["errorCode"], "access_timeout");
    assert_eq!(late(&f, &run, 9404).await["errorCode"], "access_timeout");
    assert!(f.fake.calls("consent.withdraw").len() >= 2);
}

#[tokio::test]
async fn audit_shell_always_does_not_approve_the_next_command() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    f.ctx.update_config(|c| c.sandbox.profile.head = crate::config::SandboxMode::Off);
    let control = Controlled::install(f.fake.clone());
    let run = program(&f, false, "run:audit-shell-always");
    let marker = run.project_root.join("executions");
    let command = format!("printf x >> '{}'; printf audit-shell-output", marker.display());
    let args = json!({"command":command,"sandbox":"host"});
    let mut replies = f.send_tool(&run, "bash", args.clone(), 9405, escalation_dispatch).await;
    held(&mut replies).await;
    let request = control.request(0).await;
    assert_eq!(request["command"], command);
    control.answer(&request, "allow_scope");
    let result = late(&f, &run, 9405).await;
    assert!(result["content"][0]["text"].as_str().unwrap().contains("audit-shell-output"));
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "x");
    let mut replies = f.send_tool(&run, "bash", args, 9406, escalation_dispatch).await;
    held(&mut replies).await;
    let second = control.request(1).await;
    assert_ne!(second["call_key"], request["call_key"]);
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "x");
    control.answer(&second, "deny");
    assert_eq!(late(&f, &run, 9406).await["errorCode"], "access_declined");
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "x");
}

#[tokio::test]
async fn audit_public_run_feed_contains_its_approved_result() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    let control = Controlled::install(f.fake.clone());
    let run = program(&f, false, "run:audit-feed");
    let mut replies = f.send_read(&run, &f.dir.path().join("elsewhere.txt"), 9407).await;
    held(&mut replies).await;
    let request = control.request(0).await;
    control.answer(&request, "allow_once");
    let result = late(&f, &run, 9407).await;
    assert!(result["content"][0]["text"].as_str().unwrap().contains("unheld contents"));
    let feed = f.checkout_tool(&run, "late_results", json!({}), 9408, read_dispatch).await;
    eprintln!("AUDIT public run feed: {feed}");
    assert!(feed["entries"].as_array().unwrap().iter().any(|e| e["call_key"] == "program-9407"), "the run route's feed did not deliver its result");
}

#[test]
fn audit_restart_wait_is_not_started_and_changed_scope_or_digest_conflicts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.db");
    let db = crate::db::open(&path).unwrap();
    let key = ledger::Key { carrier:"reserved:basal".into(), call_key:"audit-restart".into() };
    let scope = ledger::ScopeIdentity { owner:"reserved:prefrontal-core".into(), scope_ref:"run:audit-restart".into(), scope_epoch:7 };
    assert!(matches!(ledger::admit(&db, &key, "original-command", Some(&scope), ledger::State::DispatchStarted).unwrap(), ledger::Admission::New));
    let hold = serde_json::to_vec(&json!({"content":[{"type":"text","text":""}],"held":{"reason":"consent"}})).unwrap();
    assert!(ledger::hold_permission(&db, &key, &hold, true).unwrap());
    drop(db);
    let db = crate::db::open(&path).unwrap();
    ledger::recover(&db).unwrap();
    let row = ledger::get(&db, &key).unwrap().unwrap();
    assert_eq!(row.state, ledger::State::Settled);
    assert_eq!(row.outcome.as_deref(), Some("not_started"));
    assert!(!ledger::begin_approved(&db, &key).unwrap());
    assert!(matches!(ledger::admit(&db, &key, "original-command", Some(&scope), ledger::State::DispatchStarted).unwrap(), ledger::Admission::Repeat(_)));
    assert!(matches!(ledger::admit(&db, &key, "different-command", Some(&scope), ledger::State::DispatchStarted).unwrap(), ledger::Admission::Conflict));
    let other = ledger::ScopeIdentity { scope_ref:"run:other".into(), ..scope };
    assert!(matches!(ledger::admit(&db, &key, "original-command", Some(&other), ledger::State::DispatchStarted).unwrap(), ledger::Admission::Conflict));
}

async fn audit_replay(f: &Fixture, run: &RouteIdentity, arguments: Value) -> Value {
    let (edge, mut ready) = tool_provider::LedgerEdge::new();
    let (writer, mut replies) = mpsc::channel(8);
    let routes = HashMap::from([(route_key(41, 1), run.clone())]);
    let frame = Frame::build(FrameType::Request, control_flags(), 41, 1, 9499,
        serde_json::to_vec(&json!({"name":"bash","preset":"head","arguments":arguments,"call_key":"program-9409"})).unwrap()).unwrap();
    let decoded = DecodedFrame { parsed_body:Default::default(), confinement:None, launch_restore:None, codemode_policies:None, frame, phase_trace:PhaseTrace::new(Instant::now()) };
    let metrics = Arc::new(DispatchPathMetrics::new());
    assert!(edge.defer(decoded, &routes, &f.executor, &writer, &metrics).is_ok());
    let reply = tokio::time::timeout(Duration::from_secs(10), replies.recv()).await.unwrap().unwrap();
    assert!(ready.try_recv().is_err(), "replay must not admit another execution");
    serde_json::from_slice(&reply.body).unwrap()
}

#[tokio::test]
async fn audit_shell_same_key_replay_is_once_and_changed_command_conflicts() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    f.ctx.update_config(|c| c.sandbox.profile.head = crate::config::SandboxMode::Off);
    let control = Controlled::install(f.fake.clone());
    let run = program(&f, false, "run:audit-replay");
    let marker = run.project_root.join("executions");
    let command = format!("printf x >> '{}'; printf audit-replay-output", marker.display());
    let args = json!({"command":command,"sandbox":"host"});
    let mut replies = f.send_tool(&run, "bash", args.clone(), 9409, escalation_dispatch).await;
    held(&mut replies).await;
    let request = control.request(0).await;
    control.answer(&request, "allow_once");
    let result = late(&f, &run, 9409).await;
    assert!(result["content"][0]["text"].as_str().unwrap().contains("audit-replay-output"));
    let replay = audit_replay(&f, &run, args).await;
    assert!(replay.to_string().contains("audit-replay-output"), "{replay}");
    let conflict = audit_replay(&f, &run, json!({"command":"printf malicious","sandbox":"host"})).await;
    assert!(conflict.to_string().contains("call_key_conflict"), "{conflict}");
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "x");
    assert_eq!(f.fake.calls("consent.request").len(), 1);
}

#[test]
fn audit_schema25_additive_migration_preserves_all_ledger_columns_and_refuses_future_schema() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aft.db");
    let db = crate::db::open(&path).unwrap();
    let key = ledger::Key { carrier:"audit".into(), call_key:"migration".into() };
    let scope = ledger::ScopeIdentity { owner:"reserved:prefrontal-core".into(), scope_ref:"run:audit".into(), scope_epoch:7 };
    ledger::admit_with_tool_result_source(&db, &key, "kept", Some(&scope), ledger::State::DispatchStarted, true).unwrap();
    let frame = ledger::RecordedFrame { ty:FrameType::Response, body:serde_json::to_vec(&json!({"content":[{"type":"text","text":"retained sensitive output"}]})).unwrap() };
    ledger::settle_with_tool_result_source(&db, &key, &frame, "ok", None, ledger::now_ms(), Some(b"source")).unwrap();
    let before = ledger::get_for_replay(&db, &key).unwrap().unwrap();
    db.execute_batch("DROP TABLE call_ledger_updates; UPDATE schema_version SET version=24;").unwrap();
    drop(db);
    let db = crate::db::open(&path).unwrap();
    let after = ledger::get_for_replay(&db, &key).unwrap().unwrap();
    assert_eq!(format!("{before:?}"), format!("{after:?}"));
    assert_eq!(db.query_row("SELECT version FROM schema_version", [], |r| r.get::<_,u32>(0)).unwrap(), 25);
    db.execute_batch("UPDATE schema_version SET version=26;").unwrap();
    drop(db);
    assert!(crate::db::open(&path).is_err(), "a future schema must not be downgraded");
    let raw = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(raw.query_row("SELECT version FROM schema_version", [], |r| r.get::<_,u32>(0)).unwrap(), 26);
    assert_eq!(raw.query_row("SELECT count(*) FROM call_ledger WHERE call_key='migration'", [], |r| r.get::<_,u32>(0)).unwrap(), 1);
}
