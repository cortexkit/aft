use super::super::test_support::*;
use super::tests::{caller_route, BackgroundCompletionGate};
use super::*;
use crate::bash_background::{persistence, TaskIdentity};
use crate::protocol::RawRequest;

struct Fixture {
    _root: tempfile::TempDir,
    storage: tempfile::TempDir,
    root: ProjectRootId,
    ctx: Arc<AppContext>,
    routes: HashMap<RouteChannel, RouteIdentity>,
    worker: RouteIdentity,
}

fn fixture() -> Fixture {
    let (root_dir, root) = test_root("codemode-worker-background");
    let storage = tempfile::tempdir().unwrap();
    let mut carrier = caller_route(&root, "reserved:prefrontal-core", "runner");
    let mut worker = caller_route(&root, "reserved:broca", "runner");
    let mut run = caller_route(&root, "reserved:prefrontal-core", "runner");
    for (route, session) in [
        (&mut carrier, "worker-session"),
        (&mut worker, "worker-session"),
        (&mut run, "program-session"),
    ] {
        let data = Arc::get_mut(&mut route.0).unwrap();
        data.session = session.into();
        if let AuthenticatedPrincipal::RouteBind { session_id, .. } = &mut data.spawn_principal {
            *session_id = session.into();
        }
    }
    let scope = serde_json::from_value(json!({
        "owner":{"kind":"reserved","module_id":"prefrontal-core"},
        "ref":"worker-a", "scope_epoch":3, "kind":"worker", "owner_authorized":true,
    }))
    .unwrap();
    Arc::get_mut(&mut carrier.0).unwrap().scope = Some(scope);
    Arc::get_mut(&mut worker.0).unwrap().scope = carrier.scope.clone();
    let data = Arc::get_mut(&mut run.0).unwrap();
    data.call_mode = subc_protocol::call_mode::CallMode::Codemode;
    data.held_reply = true;
    data.tool_result_body = true;
    data.scope = Some(serde_json::from_value(json!({
        "owner":{"kind":"reserved","module_id":"prefrontal-core"},
        "ref":"program", "scope_epoch":1, "kind":"ephemeral", "owner_authorized":true,
        "attributes":{"on_behalf_of_agent":"head-agent","run_id":"worker-program"},
        "parent":{"owner":{"kind":"reserved","module_id":"prefrontal-core"},"ref":"worker-a","scope_epoch":3},
        "parent_state":"linked",
    })).unwrap());
    let ctx = test_ctx();
    ctx.update_config(|config| {
        config.project_root = Some(root.as_path().into());
        config.storage_dir = Some(storage.path().into());
        config.experimental_bash_background = true;
        config.sandbox.profile.head = crate::config::SandboxMode::Off;
        config.sandbox.profile.worker = crate::config::SandboxMode::Off;
    });
    ctx.set_db(Arc::new(std::sync::Mutex::new(
        crate::db::open(&storage.path().join("aft.db")).unwrap(),
    )));
    ctx.finish_database_runtime(Ok(()));
    Fixture {
        _root: root_dir,
        storage,
        root,
        ctx,
        routes: HashMap::from([(route_key(1, 1), carrier), (route_key(9, 1), run)]),
        worker,
    }
}

fn save_catalog(f: &Fixture, route: &RouteIdentity) {
    remote_policy::catalog(
        json!({"op":"tool.catalog","preset":"worker","params":{"remote_exec":{"enabled":true}}}),
        route,
        &f.ctx,
    )
    .unwrap();
}

fn admit(f: &Fixture) -> codemode::Admission {
    admit_tool(f, "bash")
}

fn admit_tool(f: &Fixture, tool: &str) -> codemode::Admission {
    let body = json!({"name":tool,"arguments":{"command":"echo worker-output","background":true}});
    let candidates = vec![(f.routes[&route_key(1, 1)].clone(), f.ctx.clone())];
    let policies = codemode::SavedPolicies::read_for_call(
        candidates,
        codemode::launches_shell_task(Some(&body)),
    );
    codemode::admit(&f.routes, route_key(9, 1), Some(&body), |identity| {
        policies.get(&f.ctx, identity)
    })
}

#[test]
fn codemode_worker_missing_catalog_binding_refuses_with_saved_policy_error() {
    let f = fixture();
    let codemode::Admission::Refused(error) = admit(&f) else {
        panic!("missing worker catalog must refuse task ownership");
    };
    assert_eq!(error.code, remote_policy::policy_unavailable().code);
    assert_eq!(error.message, remote_policy::policy_unavailable().message);
}

#[test]
fn codemode_worker_conflicting_catalog_bindings_refuse_with_saved_policy_error() {
    let f = fixture();
    let mut other_worker = caller_route(&f.root, "test:another-worker-runner", "runner");
    let data = Arc::get_mut(&mut other_worker.0).unwrap();
    data.session = f.worker.session.clone();
    data.scope = f.worker.scope.clone();
    if let AuthenticatedPrincipal::RouteBind { session_id, .. } = &mut data.spawn_principal {
        *session_id = f.worker.session.clone();
    }
    save_catalog(&f, &f.worker);
    save_catalog(&f, &other_worker);
    let codemode::Admission::Refused(error) = admit(&f) else {
        panic!("conflicting catalog principals must not pick a task owner");
    };
    assert_eq!(error.code, remote_policy::policy_unavailable().code);
    assert_eq!(error.message, remote_policy::policy_unavailable().message);
    let codemode::Admission::Owner(reader) = admit_tool(&f, "read") else {
        panic!("conflicting task ownership must not refuse a non-shell call");
    };
    assert_eq!(
        reader.task_identity(),
        f.routes[&route_key(1, 1)].task_identity()
    );
}

#[test]
fn codemode_worker_scope_owner_catalog_fetch_does_not_conflict_with_session_host() {
    let f = fixture();
    save_catalog(&f, &f.routes[&route_key(1, 1)]);
    save_catalog(&f, &f.worker);
    let codemode::Admission::Owner(effective) = admit(&f) else {
        panic!("the scope owner's pre-launch fetch must not compete with the session host");
    };
    assert_eq!(effective.task_identity(), f.worker.task_identity());
}

#[test]
fn codemode_worker_scope_owner_only_catalog_binding_refuses_with_saved_policy_error() {
    let f = fixture();
    save_catalog(&f, &f.routes[&route_key(1, 1)]);
    let codemode::Admission::Refused(error) = admit(&f) else {
        panic!("the scope owner's fetch does not establish a worker-session binding");
    };
    assert_eq!(error.code, remote_policy::policy_unavailable().code);
    assert_eq!(error.message, remote_policy::policy_unavailable().message);
}

#[test]
fn codemode_worker_read_without_catalog_binding_runs_with_equivalent_owner_routes() {
    let mut f = fixture();
    let path = f.root.as_path().join("worker-input.txt");
    std::fs::write(&path, "worker-readable").unwrap();
    for duplicate_owner in [false, true] {
        if duplicate_owner {
            f.routes
                .insert(route_key(3, 1), f.routes[&route_key(1, 1)].clone());
        }
        let codemode::Admission::Owner(effective) = admit_tool(&f, "read") else {
            panic!("a worker read must not need a shell-task catalog binding");
        };
        let response = with_route(&effective, || {
            crate::commands::read::handle_read(
                &request(&effective, "read", json!({"file":path})),
                &f.ctx,
            )
        });
        assert!(response.success, "{response:?}");
        assert!(
            response.data.to_string().contains("worker-readable"),
            "{response:?}"
        );
    }
}

fn request(route: &RouteIdentity, command: &str, params: Value) -> RawRequest {
    let mut params = params.as_object().unwrap().clone();
    params.insert("worker_session".into(), json!(true));
    RawRequest {
        id: "worker-observation".into(),
        command: command.into(),
        lsp_hints: None,
        session_id: Some(route.session.clone()),
        params: Value::Object(params),
    }
}

fn with_route<T>(route: &RouteIdentity, run: impl FnOnce() -> T) -> T {
    let _identity = crate::bash_background::task_identity_scope(Some(route.task_identity()));
    crate::sandbox_spawn::with_authenticated_principal(route.spawn_principal.clone(), run)
}

#[tokio::test]
async fn codemode_worker_background_task_is_visible_only_to_catalog_binding_and_completes() {
    let mut f = fixture();
    let completion_gate = BackgroundCompletionGate::new(f.root.as_path());
    save_catalog(&f, &f.worker);
    let effective = match admit(&f) {
        codemode::Admission::Owner(identity) => identity,
        codemode::Admission::Refused(error) => panic!("worker run refused: {error:?}"),
        _ => panic!("worker run must resolve its owner"),
    };
    let expected = TaskIdentity {
        principal: Some("reserved:broca".into()),
        harness: "runner".into(),
        session: "worker-session".into(),
    };
    assert_eq!(effective.task_identity(), expected);
    assert_eq!(
        effective.spawn_principal,
        f.routes[&route_key(1, 1)].spawn_principal,
        "catalog binding must not change shell authority"
    );
    let (notice_tx, mut notice_rx) = mpsc::unbounded_channel();
    let sender: ProgressSender = Arc::new(Box::new(move |frame| {
        let _ = notice_tx.send(frame);
    }) as Box<dyn Fn(PushFrame) + Send + Sync>);
    f.ctx.set_progress_sender(Some(sender));
    let executor = Arc::new(Executor::new());
    assert!(executor.register_actor(f.root.clone(), f.ctx.clone()));
    let (done_tx, mut done_rx) = mpsc::channel(4);
    let (touch_tx, _touch_rx) = mpsc::channel(4);
    {
        let _identity = secret_consent::SecretIdentityGuard::install(effective.clone());
        bash::submit_deferred_bash(
            false,
            &executor,
            &done_tx,
            &touch_tx,
            &Arc::new(DispatchPathMetrics::new()),
            |request, ctx| crate::commands::bash::handle(&request, ctx),
            f.root.clone(),
            f.root.as_path().into(),
            effective.session.clone(),
            "worker-background".into(),
            route_key(9, 1),
            1,
            control_flags(),
            PROTOCOL_VERSION,
            json!({"command":completion_gate.command("worker-output"),"background":true,"compressed":false,"timeout":60_000}),
            crate::subc_format::FormatContext::default(),
            bash::BashWaitCancel {
                keepalive: None,
                speech_seed: None,
                run_scope: None,
                scope: effective.scope.clone(),
                late_results: true,
                ledger_carrier: Some("reserved:prefrontal-core".into()),
                connection: PersistentCancelSignal::new(),
                route: PersistentCancelSignal::new(),
                drain: drain::ModuleDrainWindow::default(),
            },
            BindTrust::FirstParty,
            effective.spawn_principal.clone(),
            None,
            Some("worker-background".into()),
            None,
            None,
            None,
            false,
            false,
            false,
            true,
            remote_policy::RemoteSource::None,
            Instant::now() + Duration::from_secs(10),
        );
    }
    let done = tokio::time::timeout(Duration::from_secs(10), done_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let response = done.response_for_test();
    assert!(response.success, "{response:?}");
    assert_eq!(response.data["status"], "running", "{response:?}");
    assert!(
        response.data.get("output_preview").is_none(),
        "launch guidance must not consume terminal output: {response:?}"
    );
    let task_id = response.data["task_id"]
        .as_str()
        .expect("background task id")
        .to_owned();
    let capture = super::super::runner_notices::test_endpoint::capture(&task_id, false);
    completion_gate.release();
    let completion = loop {
        let frame = tokio::time::timeout(Duration::from_secs(10), notice_rx.recv())
            .await
            .unwrap()
            .unwrap();
        if matches!(&frame, PushFrame::BashCompleted(frame) if frame.task_id == task_id) {
            break frame;
        }
    };
    let task = persistence::resolve_task_layout(
        &persistence::session_tasks_dir(&f.storage.path().join("runner"), "worker-session"),
        &task_id,
    )
    .unwrap();
    let metadata = persistence::read_task_at(&task).unwrap();
    assert!(
        !metadata.completion_delivered,
        "background launch must leave completion pending: {response:?}"
    );
    assert_eq!(metadata.launching_identity, Some(expected.clone()));
    assert_eq!(metadata.scope, f.worker.scope);
    assert_ne!(metadata.scope, effective.scope);
    assert_eq!(
        metadata.call_key.unwrap().requester,
        "reserved:prefrontal-core"
    );

    let codemode::Admission::Owner(program_observer) = admit_tool(&f, "bash_status") else {
        panic!("the same program must be able to observe the task it launched");
    };
    assert_eq!(program_observer.task_identity(), expected);
    let mut other_session = caller_route(&f.root, "reserved:broca", "runner");
    let mut other_principal = caller_route(&f.root, "test:another-runner", "runner");
    for (route, session) in [
        (&mut other_session, "other-worker"),
        (&mut other_principal, "worker-session"),
    ] {
        let data = Arc::get_mut(&mut route.0).unwrap();
        data.session = session.into();
        if let AuthenticatedPrincipal::RouteBind { session_id, .. } = &mut data.spawn_principal {
            *session_id = session.into();
        }
    }
    for route in [
        &program_observer,
        &f.worker,
        &other_session,
        &other_principal,
        &f.routes[&route_key(1, 1)],
    ] {
        let own = route.task_identity() == expected;
        with_route(route, || {
            let status = crate::commands::bash_status::handle(
                &request(route, "bash_status", json!({"task_id":task_id})),
                &f.ctx,
            );
            assert_eq!(status.success, own, "{status:?}");
            let read = crate::commands::read::handle_read(
                &request(route, "read", json!({"file":task.paths.stdout})),
                &f.ctx,
            );
            assert_eq!(read.success, own, "{read:?}");
            if own {
                assert!(read.data.to_string().contains("worker-output"), "{read:?}");
            } else {
                assert_eq!(status.data["code"], "task_not_found", "{status:?}");
                // The protected-storage boundary (storage floor) answers first when
                // the capture path sits under it; otherwise the artifact lookup answers.
                match read.data["code"].as_str() {
                    Some("denied_floor") => {}
                    Some("artifact_refused") => assert!(
                        read.data["message"]
                            .as_str()
                            .is_some_and(|message| message.contains("task_not_found")),
                        "{read:?}"
                    ),
                    _ => panic!("unexpected capture read refusal: {read:?}"),
                }
                assert!(!read.data.to_string().contains("worker-output"), "{read:?}");
            }
        });
        // bash_watch is offered only to Classic routes, not code-mode programs.
        if route.call_mode == subc_protocol::call_mode::CallMode::Codemode {
            continue;
        }
        let wake = crate::response_finalize::DeferredResponseWake::default();
        let _wake = wake.install();
        let watch = with_route(route, || {
            crate::commands::bash_watch::handle_deferred(
                &request(
                    route,
                    "bash_watch",
                    json!({"task_id":task_id,"timeout_ms":1000}),
                ),
                f.ctx.clone(),
            )
        });
        let watched = match watch {
            crate::response_finalize::DispatchOutcome::Immediate(response) => response,
            crate::response_finalize::DispatchOutcome::Deferred(mut pending) => {
                tokio::time::timeout(Duration::from_secs(5), wake.notified())
                    .await
                    .unwrap();
                with_route(route, || (pending.poll)(&f.ctx).expect("watch reply ready"))
            }
        };
        assert_eq!(watched.success, own, "{watched:?}");
        if !own {
            assert_eq!(watched.data["code"], "task_not_found", "{watched:?}");
        }
    }

    f.routes.insert(route_key(2, 1), f.worker.clone());
    f.routes.insert(route_key(3, 1), other_session);
    f.routes.insert(route_key(4, 1), other_principal);
    let root_channels = HashMap::from([(f.root.clone(), f.routes.keys().copied().collect())]);
    let (writer_tx, mut writer_rx) = mpsc::channel::<WriterFrame>(8);
    let result = fan_out_reliable_push_frame(
        &writer_tx,
        &DispatchPathMetrics::new(),
        &f.routes,
        &root_channels,
        &HashMap::new(),
        &mut RetryBuffer::new(),
        &mut HashMap::new(),
        &f.root,
        &completion,
        None,
    );
    assert_eq!(result.matched_channels, 0);
    let notice_metadata = f
        .ctx
        .bash_background()
        .notice_metadata(&task_id)
        .expect("notice task must remain registered");
    assert!(
        !notice_metadata.completion_delivered,
        "task observations or fan-out consumed completion before notice offer"
    );
    assert!(super::super::runner_notices::enqueue(
        f.ctx.bash_background().clone(),
        completion.clone()
    ));
    let notice = capture
        .receiver
        .recv_timeout(Duration::from_secs(3))
        .unwrap();
    assert_eq!(notice.session, "worker-session");
    assert_eq!(
        notice.request["params"]["delivery"],
        json!(["tool_result", "steer"])
    );
    assert!(
        writer_rx.try_recv().is_err(),
        "runner and foreign routes receive no completion frames"
    );
    let subs = HashMap::from([
        (
            (f.root.clone(), expected.clone()),
            HashSet::from([route_key(2, 1)]),
        ),
        (
            (f.root.clone(), f.routes[&route_key(4, 1)].task_identity()),
            HashSet::from([route_key(4, 1)]),
        ),
    ]);
    let mut wakes = HashMap::new();
    arm_durable_bg_wake(
        &DispatchPathMetrics::new(),
        &subs,
        &mut wakes,
        &mut HashMap::new(),
        f.root.clone(),
        expected.session.clone(),
        Some(expected),
    );
    assert!(wakes.contains_key(&route_key(2, 1)));
    assert!(!wakes.contains_key(&route_key(4, 1)));
    f.ctx.bash_background().shutdown();
}
