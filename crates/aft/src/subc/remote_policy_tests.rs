use super::*;
use sha2::{Digest, Sha256};

fn identity(root: &Path, session: &str, epoch: u64, scoped: bool) -> RouteIdentity {
    let scope=serde_json::from_value(json!({"owner":{"kind":"reserved","module_id":"prefrontal-core"},"ref":"worker","scope_epoch":epoch,"kind":"worker","owner_authorized":true})).unwrap();
    RouteIdentity(Arc::new(RouteIdentityData {
        root: ProjectRootId::from_path(root).unwrap(),
        project_root: root.into(),
        harness: "runner".into(),
        session: session.into(),
        role: tool_provider::RouteRole::ToolProviderV1,
        trust: BindTrust::FirstParty,
        spawn_principal: AuthenticatedPrincipal::RouteBind {
            trust: crate::sandbox_spawn::PrincipalTrust::FirstParty,
            route_channel: 41,
            route_epoch: 1,
            project_root: root.into(),
            harness: "runner".into(),
            session_id: session.into(),
            principal_id: Some("reserved:broca".into()),
        },
        consumer_elicitation_capable: false,
        disabled_tools: Arc::new(vec![]),
        scope: scoped.then_some(scope),
        made_tool_call: AtomicBool::new(false),
    }))
}

fn context(root: &Path, storage: &Path, connection: Option<PathBuf>) -> AppContext {
    let mut config = crate::config::Config::default();
    config.project_root = Some(root.into());
    config.storage_dir = Some(storage.into());
    config.experimental_bash_background = true;
    config.sandbox.enabled = false;
    config.semantic.subc_connection_file = connection;
    let ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
    ctx.set_db(Arc::new(StdMutex::new(
        crate::db::open(&storage.join("aft.db")).unwrap(),
    )));
    ctx
}

fn plan(name: &str) -> Value {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/exec_remote/fixtures/plans");
    let jcs = std::fs::read(root.join(format!("{name}.jcs"))).unwrap();
    assert_eq!(
        format!("{:x}", Sha256::digest(&jcs)),
        std::fs::read_to_string(root.join(format!("{name}.sha256"))).unwrap()
    );
    let value: Value = serde_json::from_slice(&jcs).unwrap();
    assert_eq!(
        value,
        serde_json::from_slice::<Value>(&std::fs::read(root.join(format!("{name}.json"))).unwrap())
            .unwrap()
    );
    value
}

fn fetch(plan: &Value) -> Value {
    json!({"op":"tool.catalog","preset":"worker","params":plan["tool_items"][0]["params"],"composition":plan["composition"]})
}

#[test]
fn exec_remote_catalog_freezes_verbatim_plan_and_isolates_scope_epoch_and_session() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context(root.path(), storage.path(), None);
    let first = identity(root.path(), "one", 7, true);
    let second = identity(root.path(), "two", 7, true);
    let enabled = plan("broca-worker");
    let disabled = plan("broca-worker-policy-disabled");
    catalog(fetch(&enabled), &first, &ctx).unwrap();
    catalog(fetch(&disabled), &second, &ctx).unwrap();
    assert!(
        lookup(&ctx, key(&first, Some("worker")).as_ref())
            .unwrap()
            .params
            .remote_exec
            .unwrap()
            .enabled
    );
    assert!(
        !lookup(&ctx, key(&second, Some("worker")).as_ref())
            .unwrap()
            .params
            .remote_exec
            .unwrap()
            .enabled
    );
    // A second route under the same bind sees the policy, but another epoch
    // or an unscoped call cannot borrow it.
    let new_epoch = identity(root.path(), "one", 8, true);
    assert!(lookup(&ctx, key(&new_epoch, Some("worker")).as_ref()).is_none());
    let unscoped = identity(root.path(), "one", 7, false);
    assert!(lookup(&ctx, key(&unscoped, Some("worker")).as_ref()).is_none());
    assert!(lookup(&ctx, key(&first, Some("head")).as_ref()).is_none());
    assert!(lookup(
        &ctx,
        key(
            &identity(root.path(), "not-fetched", 7, true),
            Some("worker")
        )
        .as_ref()
    )
    .is_none());
    // Freeze is immutable within one bind/scope identity.
    catalog(fetch(&disabled), &first, &ctx).unwrap();
    assert!(
        lookup(&ctx, key(&first, Some("worker")).as_ref())
            .unwrap()
            .params
            .remote_exec
            .unwrap()
            .enabled
    );
}

#[test]
fn exec_remote_catalog_validates_vocabulary_and_disables_malformed_routing_only() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context(root.path(), storage.path(), None);
    let bind = identity(root.path(), "one", 7, true);
    for (name, values) in [
        ("behavior", vec!["autonomous", "interactive"]),
        ("tool_descs", vec!["concise", "full"]),
        ("scope", vec!["all", "readwrite"]),
        ("host", vec!["broca", "other-host"]),
    ] {
        for value in values {
            catalog(
                json!({"preset":"worker","params":{name:value}}),
                &bind,
                &ctx,
            )
            .unwrap();
        }
        let error =
            catalog(json!({"preset":"worker","params":{name:42}}), &bind, &ctx).unwrap_err();
        assert!(serde_json::to_string(&error).unwrap().contains(name));
    }
    for (preset, value, accepted) in [
        ("reader", "read", true),
        ("head", "all", true),
        ("head", "readwrite", true),
        ("worker", "read", false),
        ("reader", "all", false),
    ] {
        assert_eq!(
            catalog(
                json!({"preset":preset,"params":{"scope":value}}),
                &bind,
                &ctx
            )
            .is_ok(),
            accepted
        );
    }
    let error = catalog(
        json!({"preset":"worker","params":{"not_a_key":true}}),
        &bind,
        &ctx,
    )
    .unwrap_err();
    assert!(serde_json::to_string(&error).unwrap().contains("not_a_key"));
    for (n, malformed) in [
        json!({"remote_exec":{"enabled":"yes"}}),
        json!({"remote_exec":{"enabled":true,"unexpected":1}}),
        json!({"siblings":[4]}),
    ]
    .into_iter()
    .enumerate()
    {
        let bind = identity(root.path(), &format!("malformed-{n}"), 7, true);
        catalog(json!({"preset":"worker","params":malformed}), &bind, &ctx).unwrap();
        assert!(lookup(&ctx, key(&bind, Some("worker")).as_ref())
            .unwrap()
            .params
            .remote_exec
            .is_none());
    }
}

#[test]
fn exec_remote_catalog_paramless_and_unscoped_parity() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context(root.path(), storage.path(), None);
    let bind = identity(root.path(), "one", 7, false);
    for preset in ["head", "worker", "reader"] {
        let body = json!({"preset":preset});
        assert_eq!(
            serde_json::to_vec(&catalog(body.clone(), &bind, &ctx).unwrap()).unwrap(),
            serde_json::to_vec(
                &tool_provider::catalog(body, &[], crate::bash_background::powershell_available())
                    .unwrap()
            )
            .unwrap()
        );
    }
    assert!(
        catalog(fetch(&plan("broca-worker")), &bind, &ctx).is_err(),
        "unscoped params retain their existing refusal"
    );
}

#[tokio::test]
async fn exec_remote_catalog_route_fetch_then_call_after_restart_routes() {
    let daemon = crate::exec_remote::wire_tests::daemon(
        crate::exec_remote::wire_tests::Script::Utf8,
        "exec-remote/v1",
    )
    .await;
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let bind = identity(root.path(), "one", 7, true);
    let root_id = bind.root.clone();
    let ctx = context(root.path(), storage.path(), Some(daemon.connection.clone()));
    let executor = Arc::new(Executor::new());
    assert!(executor.register_actor(root_id.clone(), Arc::new(ctx)));
    let (writer, mut replies) = mpsc::channel(8);
    let frame = Frame::build(FrameType::Request, control_flags(), 41, 1, 7, vec![]).unwrap();
    submit_provider_read(
        &writer,
        &frame,
        bind.clone(),
        &executor,
        &Arc::default(),
        &Arc::new(DispatchPathMetrics::new()),
        "tool.catalog",
        fetch(&plan("broca-worker")),
    )
    .await
    .unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(10), replies.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reply.header.ty, FrameType::Response);
    // Simulate losing every in-memory route/context. The new call route opens
    // no catalog fetch; only the original persisted policy can route it.
    drop(executor);
    let ctx = context(root.path(), storage.path(), Some(daemon.connection.clone()));
    let executor = Arc::new(Executor::new());
    assert!(executor.register_actor(root_id.clone(), Arc::new(ctx)));
    let routes = HashMap::from([(route_key(42, 1), bind)]);
    let call = Frame::build(
        FrameType::Request,
        control_flags(),
        42,
        1,
        8,
        serde_json::to_vec(
            &json!({"name":"bash","preset":"worker","arguments":{"command":"cargo test"}}),
        )
        .unwrap(),
    )
    .unwrap();
    let (bash_tx, mut bash_rx) = mpsc::channel(8);
    let (touch_tx, _touch_rx) = mpsc::channel(8);
    let (deferred_tx, _deferred_rx) = mpsc::unbounded_channel();
    handle_tool_call(
        &writer,
        &call,
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
    let done = tokio::time::timeout(Duration::from_secs(15), bash_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        done.response_for_test().data["output"]
            .as_str()
            .is_some_and(|s| s.contains("ran remotely on ck-motor")),
        "{:?}",
        done.response_for_test()
    );
    let log = daemon.log.lock().unwrap();
    assert_eq!(
        log.iter()
            .filter(|(_, b)| b["method"] == "exec.run")
            .count(),
        1
    );
    let request = &log
        .iter()
        .find(|(_, b)| b["method"] == "exec.run")
        .unwrap()
        .1;
    assert_eq!(
        request["params"]["siblings"],
        plan("broca-worker")["tool_items"][0]["params"]["siblings"]
    );
}
