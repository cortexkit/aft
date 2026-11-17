use super::*;
use aft::db::call_ledger::{self as ledger, State};

pub(super) async fn drive_bash_elicitation_goodbye_daemon(input: FakeDaemonInput) {
    let FakeDaemonSession {
        mut stream,
        root1,
        executor,
        ..
    } = open_fake_daemon_session(input).await;
    write_bind_project_config(&root1, &minimal_bind_doc());
    let mut bind = serde_json::to_value(ModuleControlRequest::RouteBind {
        route_channel: 1,
        epoch: 1,
        target: RouteTarget::ToolProvider {
            module_id: "aft".into(),
        },
        identity: BindIdentity::new("runner", "elicitation-session-1"),
        principal: Some(subc_mcp_principal()),
        consumer_capabilities: Some(vec!["elicitation".into()]),
        admission_facts: Default::default(),
        scope: None,
        role_versions: Some([("tool-provider".into(), "v1".into())].into()),
        call_mode: None,
    })
    .unwrap();
    aft::subc::stamp_bind_workdir_for_test(&mut bind, json!(root1));
    send_control_request(&mut stream, 101, serde_json::from_value(bind).unwrap()).await;
    expect_route_bind_ack(&mut stream, 101).await;
    let root_id = ProjectRootId::from_path(&root1).unwrap();
    let ctx = executor.actor_context(&root_id).expect("bound actor");
    let db = ctx.db().expect("call ledger");
    let key = ledger::Key {
        carrier: "reserved:subc-mcp".into(),
        call_key: "scoped-goodbye:held-bash".into(),
    };
    let touched = root1.join("elicitation-goodbye.txt");
    let call = json!({
        "op":"tool.call", "name":"bash", "preset":"head",
        "call_key":key.call_key,
        "schema_pin":"tp1:bash:019128a28f9f2724d2c086f93a1c74222b294ad95fee8b255fa799b257397a4f:2",
        "arguments":{ "command": touch_command(&touched), "compressed": false },
    });
    send_frame(
        &mut stream,
        Frame::build(
            FrameType::Request,
            Flags::new(false, Priority::Interactive, false),
            1,
            1,
            401,
            serde_json::to_vec(&call).unwrap(),
        )
        .unwrap(),
    )
    .await;
    let (_ask_corr, _) = expect_bash_elicitation_request(&mut stream, 1, "touch").await;
    assert_eq!(
        ledger::get(&db.lock().unwrap(), &key)
            .unwrap()
            .expect("keyed v1 admission")
            .state,
        State::Prepared,
    );
    send_route_goodbye(&mut stream, 1, 402).await;

    // A carrier close retains a scoped ask. The daemon must determine the
    // scope's lifetime before the held call can be settled as not started.
    let describe = read_frame_timeout(&mut stream, "Goodbye scope.describe").await;
    assert_eq!(describe.header.ty, FrameType::Request);
    assert_eq!(describe.header.channel, 0);
    let request: ModuleControlRequestFromModule = serde_json::from_slice(&describe.body).unwrap();
    assert!(matches!(
        request,
        ModuleControlRequestFromModule::ScopeDescribe { owner: Principal::Direct, scope_ref }
            if scope_ref == "elicitation-session-1"
    ));
    let held = ledger::get(&db.lock().unwrap(), &key)
        .unwrap()
        .expect("held call remains durable after Goodbye");
    assert_eq!(held.state, State::Prepared);
    assert!(held.task_id.is_none(), "a held ask must not spawn a task");
    assert!(
        !touched.exists(),
        "Goodbye alone must not start the command"
    );

    let ended = ModuleControlResponseToModule::ScopeDescribe {
        status: subc_protocol::scope::ScopeStatus::Ended,
        scope_epoch: Some(1),
        daemon_incarnation: "daemon-a".into(),
        owner_synced: true,
        owner_configured: true,
        scope: None,
    };
    send_frame(
        &mut stream,
        Frame::build(
            FrameType::Response,
            control_flags(),
            0,
            0,
            describe.header.corr,
            serde_json::to_vec(&ended).unwrap(),
        )
        .unwrap(),
    )
    .await;
    let settled = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let row = ledger::get(&db.lock().unwrap(), &key).unwrap().unwrap();
            if row.state == State::Settled {
                break row;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("confirmed scope end settles the held call");
    assert_eq!(settled.outcome.as_deref(), Some("not_started"));
    assert!(
        settled.task_id.is_none(),
        "scope denial must not spawn a task"
    );
    let frame = settled.frame.expect("durable scope-end denial");
    assert_eq!(frame.ty, FrameType::Error);
    let body: Value = serde_json::from_slice(&frame.body).unwrap();
    assert_eq!(body["code"], "not_started");
    assert_eq!(body["detail"]["reason"], "scope_ended");
    assert!(!touched.exists(), "scope-denied bash must never spawn");
    send_connection_goodbye(&mut stream).await;
}
