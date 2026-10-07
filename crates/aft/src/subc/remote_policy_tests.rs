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

// Remote routing runs only on Unix; the plan fixtures carry Unix sibling paths,
// which a Windows decode rightly rejects as not absolute.
#[cfg(unix)]
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
    let mut other_principal = identity(root.path(), "one", 7, true);
    if let AuthenticatedPrincipal::RouteBind { principal_id, .. } =
        &mut Arc::get_mut(&mut other_principal.0)
            .unwrap()
            .spawn_principal
    {
        *principal_id = Some("reserved:another-carrier".into());
    }
    assert!(lookup(&ctx, key(&other_principal, Some("worker")).as_ref()).is_none());
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
    // A session starter preflights its worker plan on an unscoped route. The
    // plan's params are validated and accepted there, the answer is the same
    // as a paramless fetch, and nothing is frozen without a scope.
    let planned_fetch = fetch(&plan("broca-worker"));
    let mut paramless_fetch = planned_fetch.clone();
    paramless_fetch.as_object_mut().unwrap().remove("params");
    let paramless = catalog(paramless_fetch, &bind, &ctx).unwrap();
    let planned = catalog(planned_fetch, &bind, &ctx).unwrap();
    assert_eq!(
        serde_json::to_vec(&planned).unwrap(),
        serde_json::to_vec(&paramless).unwrap()
    );
    assert!(lookup(&ctx, key(&bind, Some("worker")).as_ref()).is_none());
    let error = catalog(
        json!({"preset":"worker","params":{"not_a_key":true}}),
        &bind,
        &ctx,
    )
    .unwrap_err();
    assert!(serde_json::to_string(&error).unwrap().contains("not_a_key"));
    let error = catalog(
        json!({"preset":"head","params":{"remote_exec":{"enabled":true}}}),
        &bind,
        &ctx,
    )
    .unwrap_err();
    assert!(serde_json::to_string(&error)
        .unwrap()
        .contains("remote_exec"));
}

// Remote dispatch runs only on Unix.
#[cfg(unix)]
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
    ctx.app()
        .set_subc_connection_file(daemon.connection.clone());
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
    let ctx = context(root.path(), storage.path(), None);
    ctx.app()
        .set_subc_connection_file(daemon.connection.clone());
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
    let deferred_tx = DeferredResponseSender {
        entries: deferred_tx,
        wake: crate::response_finalize::DeferredResponseWake::default(),
    };
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

#[tokio::test]
async fn exec_remote_module_loop_captures_its_authenticated_daemon_endpoint() {
    let ctx = test_support::test_ctx();
    let app = ctx.app();
    let executor = Arc::new(Executor::new());
    let dir = tempfile::tempdir().unwrap();
    let endpoint = dir.path().join("module-connection.json");
    let (mut peer, module) = tokio::io::duplex(65536);
    let (read, write) = tokio::io::split(module);
    let peer_task = async {
        let hello = subc_transport::read_frame(&mut peer)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(hello.header.ty, FrameType::Hello);
        let ack = Frame::build(
            FrameType::HelloAck,
            control_flags(),
            0,
            0,
            hello.header.corr,
            serde_json::to_vec(&ModuleHelloAckBody {
                negotiated_ver: PROTOCOL_VERSION,
                subc_ops: vec![],
                subc_capabilities: vec![],
                storage: None,
                machine_id: None,
            })
            .unwrap(),
        )
        .unwrap();
        subc_transport::write_frame(&mut peer, &ack).await.unwrap();
        let goodbye = Frame::build(FrameType::Goodbye, control_flags(), 0, 0, 0, vec![]).unwrap();
        subc_transport::write_frame(&mut peer, &goodbye)
            .await
            .unwrap();
    };
    let module_task = run_module_loop(
        read,
        write,
        &endpoint,
        app.clone(),
        executor,
        |req, _| Response::success(req.id, json!({})),
        None,
        false,
        1024 * 1024,
        None,
        dir.path(),
        None,
        None,
    );
    let (result, _) = tokio::join!(module_task, peer_task);
    result.unwrap();
    assert_eq!(app.subc_connection_file(), Some(endpoint));
}

#[cfg(unix)]
#[tokio::test]
async fn exec_remote_scope_drain_detaches_but_explicit_cancel_kills() {
    for cancelled in [false, true] {
        let daemon = crate::exec_remote::wire_tests::daemon(
            crate::exec_remote::wire_tests::Script::Cancel,
            "exec-remote/v1",
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path(), dir.path(), Some(daemon.connection.clone()));
        let registry = ctx.bash_background().clone();
        let task = registry
            .spawn_remote(
                crate::bash_background::RemoteLaunch {
                    params: Default::default(),
                    connection_file: Some(daemon.connection.clone()),
                    harness: "runner".into(),
                    session: "session".into(),
                },
                crate::sandbox_spawn::SpawnPlan::Unsandboxed,
                "cargo test",
                crate::bash_background::registry::resolve_posix_shell(),
                "session".into(),
                dir.path().into(),
                HashMap::new(),
                crate::bash_background::HardKill::After(Duration::from_secs(30)),
                dir.path().into(),
                10,
                true,
                false,
                Some(dir.path().into()),
            )
            .unwrap();
        registry.begin_wait_mode_session("session", &task);
        bash::detach_held_bash_in_background(
            drain::BashDetachTarget {
                registry: registry.clone(),
                task_id: task.clone(),
                session_id: "session".into(),
                wait_mode: true,
                worker_session: true,
                server_completion: true,
                request_id: "remote-drain".into(),
                ver: PROTOCOL_VERSION,
                flags: control_flags(),
                format_context: Default::default(),
            },
            cancelled,
        );
        tokio::time::timeout(Duration::from_secs(10), async {
            while registry.active_wait_session_count() != 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let status = registry
            .observed_status(&task, "session", 0)
            .unwrap()
            .info
            .status;
        assert_eq!(
            status,
            if cancelled {
                crate::bash_background::BgTaskStatus::Killed
            } else {
                crate::bash_background::BgTaskStatus::Running
            }
        );
        let sent = daemon
            .log
            .lock()
            .unwrap()
            .iter()
            .any(|(_, b)| b["method"] == "exec.cancel");
        assert_eq!(sent, cancelled);
        if !cancelled {
            tokio::task::spawn_blocking(move || registry.kill(&task, "session").unwrap())
                .await
                .unwrap();
        }
    }
}
