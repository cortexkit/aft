use super::*;

#[tokio::test]
async fn exec_remote_v1_bash_runs_through_real_route_without_invalid_selector() {
    let (dir, root) = test_support::test_root("v1-bash-selector");
    let ctx = Arc::new(AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(root.as_path().into()),
            storage_dir: Some(dir.path().join("storage")),
            sandbox: crate::config::SandboxConfig {
                enabled: false,
                ..Default::default()
            },
            ..Default::default()
        },
    ));
    let executor = Arc::new(Executor::new());
    assert!(executor.register_actor(root.clone(), ctx));
    let identity = RouteIdentity(Arc::new(RouteIdentityData {
        root: root.clone(),
        project_root: root.as_path().into(),
        harness: "runner".into(),
        session: "selector-test".into(),
        role: tool_provider::RouteRole::ToolProviderV1,
        trust: BindTrust::FirstParty,
        spawn_principal: AuthenticatedPrincipal::FirstParty,
        consumer_elicitation_capable: false,
        disabled_tools: Arc::new(vec![]),
        scope: None,
        made_tool_call: AtomicBool::new(false),
    }));
    let routes = HashMap::from([(route_key(41, 1), identity)]);
    let frame = Frame::build(
        FrameType::Request,
        control_flags(),
        41,
        1,
        7,
        serde_json::to_vec(&json!({"name":"bash","arguments":{"command":"printf v1-bash-proof"}}))
            .unwrap(),
    )
    .unwrap();
    let (writer, _replies) = mpsc::channel(8);
    let (bash_tx, mut bash_rx) = mpsc::channel(8);
    let (touch_tx, _touch_rx) = mpsc::channel(8);
    let (deferred_tx, _deferred_rx) = mpsc::unbounded_channel();
    let deferred_tx = super::DeferredResponseSender {
        entries: deferred_tx,
        wake: crate::response_finalize::DeferredResponseWake::default(),
    };
    handle_tool_call(
        &writer,
        &frame,
        PhaseTrace::new(Instant::now()),
        &routes,
        &HashMap::new(),
        &ReclaimedRoutes::default(),
        &mut HashMap::new(),
        &executor,
        &Arc::default(),
        &Arc::new(AtomicUsize::new(0)),
        &Arc::new(Notify::new()),
        &PersistentCancelSignal::new(),
        &bash_tx,
        &touch_tx,
        &Arc::new(DispatchPathMetrics::new()),
        &mut HashMap::new(),
        &mut HashMap::new(),
        &mut 1,
        &mut HashMap::new(),
        &mut HashMap::new(),
        &mut HashMap::new(),
        &mut HashMap::new(),
        |request, ctx| crate::commands::bash::handle(&request, ctx),
        &deferred_tx,
        false,
        1024 * 1024,
        &drain::ModuleDrainWindow::default(),
    )
    .await
    .unwrap();
    let done = tokio::time::timeout(Duration::from_secs(10), bash_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let response = done.response_for_test();
    assert!(response.success, "{response:?}");
    assert_eq!(response.data["exit_code"], 0, "{response:?}");
    assert_eq!(response.data["output"], "v1-bash-proof", "{response:?}");
}

#[test]
fn exec_remote_v1_powershell_keeps_its_only_supported_selector() {
    let mut args = serde_json::Map::new();
    provider_shell_selector("powershell", &mut args);
    assert_eq!(args.get("shell"), Some(&json!("powershell")));
}

#[cfg(unix)]
async fn v1_bash_wait_limit_case(worker: bool) {
    let (dir, root) = test_support::test_root("v1-bash-wait-limit");
    let mut config = crate::config::Config::default();
    config.project_root = Some(root.as_path().into());
    config.storage_dir = Some(dir.path().join("storage"));
    config.sandbox.enabled = false;
    config.bash.worker_wait_max_ms = 1_000;
    let ctx = Arc::new(AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        config,
    ));
    let executor = Arc::new(Executor::new());
    assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));
    let identity = RouteIdentity(Arc::new(RouteIdentityData {
        root: root.clone(),
        project_root: root.as_path().into(),
        harness: "runner".into(),
        session: "wait-limit-test".into(),
        role: tool_provider::RouteRole::ToolProviderV1,
        trust: BindTrust::FirstParty,
        spawn_principal: AuthenticatedPrincipal::FirstParty,
        consumer_elicitation_capable: false,
        disabled_tools: Arc::new(vec![]),
        scope: None,
        made_tool_call: AtomicBool::new(false),
    }));
    let mut call = json!({"name":"bash","arguments":{
        "command":"sleep 3; printf completed", "wait":true, "timeout":30_000
    }});
    call["preset"] = json!(if worker { "worker" } else { "head" });
    let frame = Frame::build(
        FrameType::Request,
        control_flags(),
        41,
        1,
        7,
        serde_json::to_vec(&call).unwrap(),
    )
    .unwrap();
    let (writer, _replies) = mpsc::channel(8);
    let (bash_tx, mut bash_rx) = mpsc::channel(8);
    let (touch_tx, _touch_rx) = mpsc::channel(8);
    let (deferred_tx, _deferred_rx) = mpsc::unbounded_channel();
    let deferred_tx = DeferredResponseSender {
        entries: deferred_tx,
        wake: crate::response_finalize::DeferredResponseWake::default(),
    };
    let started = Instant::now();
    handle_tool_call(
        &writer,
        &frame,
        PhaseTrace::new(started),
        &HashMap::from([(route_key(41, 1), identity)]),
        &HashMap::new(),
        &ReclaimedRoutes::default(),
        &mut HashMap::new(),
        &executor,
        &Arc::default(),
        &Arc::new(AtomicUsize::new(0)),
        &Arc::new(Notify::new()),
        &PersistentCancelSignal::new(),
        &bash_tx,
        &touch_tx,
        &Arc::new(DispatchPathMetrics::new()),
        &mut HashMap::new(),
        &mut HashMap::new(),
        &mut 1,
        &mut HashMap::new(),
        &mut HashMap::new(),
        &mut HashMap::new(),
        &mut HashMap::new(),
        |request, ctx| crate::commands::bash::handle(&request, ctx),
        &deferred_tx,
        false,
        1024 * 1024,
        &drain::ModuleDrainWindow::default(),
    )
    .await
    .unwrap();
    let done = tokio::time::timeout(Duration::from_secs(8), bash_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let elapsed = started.elapsed();
    let response = done.response_for_test();
    // Clean up before assertions so a failing handoff assertion leaves no child.
    if let Some(task_id) = response.data["task_id"].as_str() {
        let snapshot = ctx
            .bash_background()
            .status(task_id, "wait-limit-test", None, None, 0)
            .unwrap();
        let status = snapshot.info.status;
        let _ = ctx.bash_background().kill(task_id, "wait-limit-test");
        if worker {
            assert_eq!(status, crate::bash_background::BgTaskStatus::Running);
        }
    }
    assert!(response.success, "{response:?}");
    if worker {
        assert_eq!(response.data["status"], "running", "{response:?}");
        assert!(response.data["task_id"].as_str().is_some(), "{response:?}");
        assert!(
            elapsed >= Duration::from_millis(800) && elapsed < Duration::from_millis(2_500),
            "{elapsed:?}"
        );
        assert!(
            response.data["output"]
                .as_str()
                .unwrap()
                .contains("was not killed"),
            "{response:?}"
        );
        assert!(
            response.data["output"]
                .as_str()
                .unwrap()
                .contains("timeout"),
            "{response:?}"
        );
    } else {
        assert_eq!(response.data["exit_code"], 0, "{response:?}");
        assert_eq!(response.data["output"], "completed", "{response:?}");
        assert!(elapsed >= Duration::from_millis(2_800), "{elapsed:?}");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn tool_provider_v1_worker_bash_hands_off_at_configured_cap() {
    v1_bash_wait_limit_case(true).await;
}

#[cfg(unix)]
#[tokio::test]
async fn tool_provider_v1_head_bash_wait_is_not_worker_capped() {
    v1_bash_wait_limit_case(false).await;
}
