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
