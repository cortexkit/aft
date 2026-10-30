use super::*;

#[tokio::test]
async fn bind_publication_rejects_missing_or_mismatched_applied_snapshot() {
    let (_dir, root) = test_support::test_root("prepared-witness-guard");
    for applied in [None, Some(8)] {
        let route = route_key(7, 1);
        let (writer, mut frames) = mpsc::channel(8);
        let (done, _completed) = mpsc::channel(8);
        let a0 = A0Egress::new(writer.clone(), done);
        let completion = RouteBindCompletion {
            expected_snapshot_id: Some(9),
            applied_snapshot_id: applied,
            route,
            identity: test_support::route_identity(&root, "witness"),
            bind_root_id: root.clone(),
            inserted_new_actor: false,
            configure_response: Response::success("witness", json!({})),
            diagnostics_on_edit: false,
            ver: PROTOCOL_VERSION,
            corr: 891,
            flags: control_flags(),
        };
        let mut pending = HashMap::from([(
            route,
            PendingBind {
                ack_publication: None,
                bind_root_id: root.clone(),
                inserted_new_actor: false,
                cancelled: false,
                configure_request_id: "witness".into(),
                started_at: Instant::now(),
                warned_half_deadline: false,
                deadline_reported: false,
                corr: 891,
                ver: PROTOCOL_VERSION,
                flags: control_flags(),
                cancellation: JobCancellation::new(),
                session: None,
            },
        )]);
        let mut routes = HashMap::new();
        handle_route_bind_completion(
            &writer,
            completion,
            false,
            &a0,
            &mut routes,
            &mut HashMap::new(),
            &mut HashMap::new(),
            &mut HashMap::new(),
            &mut HashMap::new(),
            &mut pending,
            &mut HashMap::from([(7, 1)]),
            &Arc::new(Executor::new()),
            &Arc::new(Notify::new()),
            &Arc::new(DispatchPathMetrics::new()),
            None,
        )
        .await
        .unwrap();
        let frame = frames.recv().await.unwrap();
        assert_eq!(
            frame.header.ty,
            FrameType::Error,
            "an unapplied snapshot must not produce an ACK"
        );
        assert_eq!(frame.header.corr, 891);
        assert_eq!(
            serde_json::from_slice::<Value>(&frame.body).unwrap()["code"],
            "config_unknown"
        );
        assert!(routes.is_empty());
    }
}

#[tokio::test]
async fn real_bind_dispatch_scope_survives_config_edits_before_executor_admission() {
    let (dir, root) = test_support::test_root("prepared-production-bind");
    let user = dir.path().join("user.jsonc");
    let project = root.as_path().join(".pi/aft.jsonc");
    std::fs::create_dir_all(project.parent().unwrap()).unwrap();
    std::fs::write(&user, r#"{"indexes":{"trigram":false,"semantic":false,"callgraph":false},"views":{"enabled":false},"url_fetch_allow_private":true}"#).unwrap();
    std::fs::write(&project, r#"{"callgraph_chunk_size":7}"#).unwrap();
    let app = App::default_shared();
    let ctx = Arc::new(crate::context::AppContext::from_app(
        app.clone(),
        crate::config::Config {
            storage_dir: Some(dir.path().join("storage")),
            ..Default::default()
        },
    ));
    let executor = Arc::new(Executor::new());
    assert!(executor.register_actor(root.clone(), ctx.clone()));
    let (entered, entering) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let holder = executor.submit_async(
        root.clone(),
        Lane::Mutating,
        "prepared-bind-holder".into(),
        Box::new(move |_| {
            entered.send(()).unwrap();
            let _ = released.recv_timeout(Duration::from_secs(30));
            Response::success("prepared-bind-holder", json!({}))
        }),
    );
    entering.recv_timeout(Duration::from_secs(5)).unwrap();
    let (writer, _frames) = mpsc::channel(8);
    let (done, mut completed) = mpsc::channel(8);
    let a0 = A0Egress::new(writer.clone(), done.clone());
    let metrics = Arc::new(DispatchPathMetrics::new());
    let (lossy_tx, _lossy) = mpsc::channel(8);
    let (reliable_tx, _reliable) = mpsc::unbounded_channel();
    let senders = PushSenders {
        lossy_tx,
        reliable_tx,
        lossy_overflow: Arc::default(),
        lossy_seq: Arc::new(AtomicU64::new(0)),
        fleet_status_client: FleetStatusClient::channel(1).0,
    };
    let request = ModuleControlRequest::RouteBind {
        route_channel: 7,
        epoch: 1,
        target: RouteTarget::ToolProvider {
            module_id: "aft".into(),
        },
        identity: subc_protocol::BindIdentity::new(root.as_path(), "pi", "prepared-bind"),
        principal: Some(subc_protocol::Principal::Direct),
        consumer_capabilities: None,
        admission_facts: Default::default(),
        scope: None,
        role_versions: None,
        call_mode: None,
    };
    let frame = Frame::build(
        FrameType::Request,
        control_flags(),
        0,
        0,
        891,
        serde_json::to_vec(&request).unwrap(),
    )
    .unwrap();
    let admitted = handle_control_request(
        &writer,
        &writer,
        &a0,
        &frame,
        &app,
        &executor,
        &Arc::new(standing::StandingActor::new(app.clone(), executor.clone())),
        &mut HashMap::new(),
        &mut HashMap::new(),
        &mut HashMap::new(),
        &mut HashMap::new(),
        &mut HashMap::new(),
        &mut HashMap::new(),
        &mut HashMap::new(),
        &mut HashMap::new(),
        &mut HashMap::new(),
        &mut HashMap::new(),
        &mut HashMap::new(),
        &Arc::new(StdMutex::new(HashMap::new())),
        &mut PendingSubcResponses::default(),
        &mut HashMap::new(),
        &mut HashMap::new(),
        &Arc::new(Notify::new()),
        &done,
        &metrics,
        None,
        &HealthRollupCache::new(),
        &senders,
        |request, ctx| crate::commands::configure::handle_configure(&request, ctx),
        Some(&user),
        usize::MAX,
        None,
        &dir.path().join("storage"),
    )
    .await;
    std::fs::write(&user, r#"{"url_fetch_allow_private":false}"#).unwrap();
    let canonical = root.as_path().join(".cortexkit/aft.jsonc");
    std::fs::create_dir_all(canonical.parent().unwrap()).unwrap();
    std::fs::write(canonical, r#"{"callgraph_chunk_size":9}"#).unwrap();
    let _ = release.send(());
    let _ = holder.await;
    admitted.unwrap();
    let ControlCompletion::ConfigureReady(completion) =
        tokio::time::timeout(Duration::from_secs(15), completed.recv())
            .await
            .unwrap()
            .unwrap()
    else {
        panic!("configure completion expected");
    };
    assert!(
        completion.configure_response.success,
        "{}",
        completion.configure_response.data
    );
    assert!(completion.expected_snapshot_id.is_some());
    assert_eq!(
        completion.expected_snapshot_id,
        completion.applied_snapshot_id
    );
    assert_eq!(ctx.config().callgraph_chunk_size, 7);
    assert!(ctx.config().url_fetch_allow_private);
    assert_eq!(
        ctx.config_live().sources().unwrap().project.path.as_deref(),
        Some(project.as_path())
    );
}
