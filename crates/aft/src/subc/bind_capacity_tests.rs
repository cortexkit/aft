use super::*;

async fn exhausted_bind(tool: bool, completion_capacity: bool) {
    let (dir, root) = test_support::test_root("bind-capacity-before-effects");
    let user = dir.path().join("must-not-migrate.jsonc");
    let original =
        "{\n  // my settings\n  \"search_index\": false,\n  \"hoist_builtin_tools\": false\n}\n";
    std::fs::write(&user, original).unwrap();
    let app = App::default_shared();
    let executor = Arc::new(Executor::new());
    let metrics = Arc::new(DispatchPathMetrics::new());
    let (writer, mut frames) = mpsc::channel(8);
    let (completed, _completions) = mpsc::channel(if completion_capacity {
        1
    } else {
        WRITER_QUEUE_CAPACITY + 1
    });
    let a0 = A0Egress::new(writer.clone(), completed.clone());
    let completion_permit =
        completion_capacity.then(|| completed.clone().try_reserve_owned().unwrap());
    let mut slots = Vec::new();
    if !completion_capacity {
        for channel in 10..10 + WRITER_QUEUE_CAPACITY as u16 {
            slots.push(
                a0.reserve(
                    route_key(channel, 1),
                    PROTOCOL_VERSION,
                    channel as u64,
                    control_flags(),
                    &metrics,
                )
                .unwrap(),
            );
        }
    }
    let request = ModuleControlRequest::RouteBind {
        route_channel: 7,
        epoch: 1,
        target: if tool {
            RouteTarget::ToolProvider {
                module_id: "aft".into(),
            }
        } else {
            RouteTarget::ManagementSurface {
                module_id: "aft".into(),
            }
        },
        identity: subc_protocol::BindIdentity::new(root.as_path(), "pi", "capacity"),
        principal: Some(Principal::Direct),
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
    let (lossy_tx, _lossy) = mpsc::channel(8);
    let (reliable_tx, _reliable) = mpsc::unbounded_channel();
    let senders = PushSenders {
        lossy_tx,
        reliable_tx,
        lossy_overflow: Arc::default(),
        lossy_seq: Arc::new(AtomicU64::new(0)),
        fleet_status_client: FleetStatusClient::channel(1).0,
    };
    let mut live = HashMap::new();
    let mut pending = HashMap::new();
    let mut installed = HashMap::new();
    let storage = tempfile::tempdir().unwrap();
    handle_control_request(
        &writer,
        &writer,
        &a0,
        &frame,
        &app,
        &executor,
        &Arc::new(standing::StandingActor::new(app.clone(), executor.clone())),
        &mut live,
        &mut pending,
        &mut installed,
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
        &completed,
        &metrics,
        None,
        &HealthRollupCache::new(),
        &senders,
        |_, _| panic!("configure must not run without reserved reply capacity"),
        Some(&user),
        usize::MAX,
        None,
        storage.path(),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(&user).unwrap(),
        original,
        "capacity refusal migrated configuration"
    );
    let config_files: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name.to_string_lossy().contains("must-not-migrate"))
        .collect();
    assert_eq!(
        config_files,
        vec![user.file_name().unwrap().to_os_string()],
        "capacity refusal left a migration backup or lock"
    );
    assert!(
        executor.actor_context(&root).is_none(),
        "capacity refusal registered an actor"
    );
    assert!(live.is_empty());
    assert!(pending.is_empty());
    assert!(installed.is_empty(), "capacity refusal installed an epoch");
    let terminal = frames.try_recv().expect("exactly one capacity refusal");
    assert_eq!(terminal.header.ty, FrameType::Error);
    assert_eq!(terminal.header.corr, 891);
    let error: Value = serde_json::from_slice(&terminal.body).unwrap();
    assert_eq!(error["code"], "busy");
    assert_eq!(error["message"], a0_egress::BIND_BUSY_MESSAGE);
    assert!(frames.try_recv().is_err());
    drop(slots);
    drop(completion_permit);
}

#[tokio::test]
async fn tool_bind_slot_exhaustion_has_no_configure_side_effects() {
    exhausted_bind(true, false).await;
}
#[tokio::test]
async fn tool_bind_completion_exhaustion_has_no_configure_side_effects() {
    exhausted_bind(true, true).await;
}
#[tokio::test]
async fn management_bind_slot_exhaustion_has_no_install_side_effects() {
    exhausted_bind(false, false).await;
}
#[tokio::test]
async fn management_bind_completion_exhaustion_has_no_install_side_effects() {
    exhausted_bind(false, true).await;
}
