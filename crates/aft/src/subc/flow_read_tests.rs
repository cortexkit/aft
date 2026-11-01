use super::*;

fn identity(root: &Path, scope: Value, principal: &str) -> RouteIdentity {
    let call_mode = if scope["ref"]
        .as_str()
        .is_some_and(|name| name.starts_with("codemode/"))
    {
        subc_protocol::call_mode::CallMode::Codemode
    } else {
        subc_protocol::call_mode::CallMode::Classic
    };
    RouteIdentity(Arc::new(RouteIdentityData {
        root: ProjectRootId::from_path(root).unwrap(),
        project_root: root.into(),
        harness: "runner".into(),
        session: "head".into(),
        role: tool_provider::RouteRole::ToolProviderV1,
        trust: BindTrust::FirstParty,
        spawn_principal: AuthenticatedPrincipal::RouteBind {
            trust: PrincipalTrust::FirstParty,
            route_channel: 41,
            route_epoch: 1,
            project_root: root.into(),
            harness: "runner".into(),
            session_id: "head".into(),
            principal_id: Some(principal.into()),
        },
        consumer_elicitation_capable: false,
        held_reply: false,
        held_update: false,
        call_keepalive: false,
        tool_result_body: false,
        disabled_tools: Arc::new(vec![]),
        scope: Some(serde_json::from_value(scope).unwrap()),
        call_mode,
        run_scope: OnceLock::new(),
        made_tool_call: AtomicBool::new(false),
        run_owner: None,
    }))
}
fn stamp() -> Value {
    json!({"owner":{"kind":"reserved","module_id":"prefrontal-core"},"ref":"flow-read/call", "scope_epoch":7,"kind":"ephemeral", "owner_authorized":true,
        "expires_at_ms":crate::db::call_ledger::now_ms() + 60000,
        "parent":{"owner":{"kind":"reserved","module_id":"prefrontal-core"},"ref":"head","scope_epoch":7}})
}
fn check(mut change: impl FnMut(&mut Value), principal: &str, expected: &'static str) {
    let dir = tempfile::tempdir().unwrap();
    let mut scope = stamp();
    change(&mut scope);
    assert_eq!(
        route_checks(&identity(dir.path(), scope, principal)).unwrap_err(),
        Refusal::RouteNotAdmitted(expected)
    );
}
#[test]
fn flow_read_principal() {
    check(|_| {}, "reserved:broca", "principal");
}
#[test]
fn flow_read_owner() {
    check(
        |s| s["owner"]["module_id"] = json!("basal"),
        "reserved:basal",
        "owner",
    );
}
#[test]
fn flow_read_owner_authorized() {
    check(
        |s| s["owner_authorized"] = json!(false),
        "reserved:basal",
        "owner",
    );
}
#[test]
fn flow_read_kind() {
    check(|s| s["kind"] = json!("head"), "reserved:basal", "kind");
}
#[test]
fn flow_read_prefix() {
    check(
        |s| s["ref"] = json!("other/call"),
        "reserved:basal",
        "scope_ref",
    );
}
#[test]
fn flow_read_codemode_ephemeral() {
    check(
        |s| s["ref"] = json!("codemode/call"),
        "reserved:basal",
        "scope_ref",
    );
}
#[test]
fn flow_read_missing_parent() {
    let dir = tempfile::tempdir().unwrap();
    let mut scope = stamp();
    scope["parent"] = Value::Null;
    assert_parent_refusal(
        route_checks(&identity(dir.path(), scope, "reserved:basal")).unwrap_err(),
        "the flow-read scope carries no parent scope",
    );
}
#[test]
fn flow_read_expired() {
    check(
        |s| s["expires_at_ms"] = json!(1),
        "reserved:basal",
        "expiry",
    );
}
#[test]
fn flow_read_missing_expiry() {
    check(
        |s| s["expires_at_ms"] = Value::Null,
        "reserved:basal",
        "expiry",
    );
}

#[test]
fn flow_read_management_route_refused() {
    let stamp: ScopeStamp = serde_json::from_value(stamp()).unwrap();
    let error = admit_management(Some(&stamp)).unwrap_err();
    assert_eq!(error.code, "file_read_exact_route_not_admitted");
    assert_eq!(error.message, "Route not admitted: operation.");
    assert!(admit_management(None).is_ok());
}

fn head(root: &Path) -> RouteIdentity {
    let mut scope = stamp();
    scope["kind"] = json!("head");
    scope["ref"] = json!("head");
    scope["parent"] = Value::Null;
    scope["expires_at_ms"] = Value::Null;
    identity(root, scope, "reserved:broca")
}
fn reply(stamp: &ScopeStamp, status: ScopeStatus) -> ModuleControlResponseToModule {
    ModuleControlResponseToModule::ScopeDescribe {
        status,
        scope_epoch: Some(stamp.scope_epoch),
        scope: Some(stamp.clone()),
        daemon_incarnation: "test".into(),
        owner_synced: true,
        owner_configured: true,
    }
}
fn assert_parent_refusal(error: Refusal, reason: &str) {
    let error = error.error();
    assert_eq!(error.code, "file_read_exact_route_not_admitted");
    assert_eq!(
        error.message,
        format!("Route not admitted: parent: {reason}.")
    );
}
#[test]
fn flow_read_dead_parent() {
    let dir = tempfile::tempdir().unwrap();
    let head = head(dir.path());
    let stamp = head.scope.as_ref().unwrap();
    assert_parent_refusal(
        live_parent(stamp, &reply(stamp, ScopeStatus::NotLive)).unwrap_err(),
        "the daemon reports the parent scope is not live",
    );
}
#[test]
fn flow_read_ended_parent() {
    let dir = tempfile::tempdir().unwrap();
    let head = head(dir.path());
    let stamp = head.scope.as_ref().unwrap();
    assert_parent_refusal(
        live_parent(stamp, &reply(stamp, ScopeStatus::Ended)).unwrap_err(),
        "the daemon reports the parent scope ended",
    );
}
#[test]
fn flow_read_parent_unexpected_response() {
    let dir = tempfile::tempdir().unwrap();
    let head = head(dir.path());
    assert_parent_refusal(
        live_parent(
            head.scope.as_ref().unwrap(),
            &ModuleControlResponseToModule::CatalogUpdate {},
        )
        .unwrap_err(),
        "the daemon did not return a scope.describe response for the parent scope",
    );
}
#[test]
fn flow_read_parent_epoch_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let head = head(dir.path());
    let stamp = head.scope.as_ref().unwrap();
    let mut changed = stamp.clone();
    changed.scope_epoch += 1;
    assert_parent_refusal(
        live_parent(stamp, &reply(&changed, ScopeStatus::Live)).unwrap_err(),
        "the daemon reports the parent scope is live at epoch 8, not 7",
    );
    let mut response = reply(stamp, ScopeStatus::Live);
    if let ModuleControlResponseToModule::ScopeDescribe { scope_epoch, .. } = &mut response {
        *scope_epoch = None;
    }
    assert_parent_refusal(
        live_parent(stamp, &response).unwrap_err(),
        "the daemon reports the parent scope is live without an epoch (expected 7)",
    );
    let mut response = reply(&changed, ScopeStatus::Live);
    if let ModuleControlResponseToModule::ScopeDescribe { scope_epoch, .. } = &mut response {
        *scope_epoch = Some(7);
    }
    assert_parent_refusal(
        live_parent(stamp, &response).unwrap_err(),
        "the daemon's parent scope stamp has epoch 8, not 7",
    );
}
#[test]
fn flow_read_parent_stamp_checks() {
    let dir = tempfile::tempdir().unwrap();
    let head = head(dir.path());
    let stamp = head.scope.as_ref().unwrap();
    let mut changed = stamp.clone();
    changed.kind = ScopeKind::Worker;
    assert_parent_refusal(
        live_parent(stamp, &reply(&changed, ScopeStatus::Live)).unwrap_err(),
        "the daemon's parent scope is not a head scope",
    );
    changed = stamp.clone();
    changed.owner_authorized = false;
    assert_parent_refusal(
        live_parent(stamp, &reply(&changed, ScopeStatus::Live)).unwrap_err(),
        "the daemon's parent scope is not owner-authorized",
    );
    changed = stamp.clone();
    changed.expires_at_ms = Some(1);
    assert_parent_refusal(
        live_parent(stamp, &reply(&changed, ScopeStatus::Live)).unwrap_err(),
        "the daemon's parent scope has expired",
    );
    changed = stamp.clone();
    changed.scope_ref = "another-head".into();
    assert_parent_refusal(
        live_parent(stamp, &reply(&changed, ScopeStatus::Live)).unwrap_err(),
        "the daemon's parent scope reference does not match the requested parent",
    );
    changed = stamp.clone();
    changed.owner = Principal::Reserved {
        module_id: "basal".into(),
    };
    assert_parent_refusal(
        live_parent(stamp, &reply(&changed, ScopeStatus::Live)).unwrap_err(),
        "the daemon's parent scope owner does not match the requested parent",
    );
    let mut response = reply(stamp, ScopeStatus::Live);
    if let ModuleControlResponseToModule::ScopeDescribe { scope, .. } = &mut response {
        *scope = None;
    }
    assert_parent_refusal(
        live_parent(stamp, &response).unwrap_err(),
        "the daemon reports the parent scope is live without a scope stamp",
    );
}

#[test]
fn flow_read_parent_route_reasons() {
    let dir = tempfile::tempdir().unwrap();
    let child: ScopeStamp = serde_json::from_value(stamp()).unwrap();
    let reason = "no open AFT route for the parent head scope head at epoch 7 (the head must have an AFT route open at the epoch core stamped)";
    assert_parent_refusal(parent_route(&child, &HashMap::new()).unwrap_err(), reason);
    let mut missing_parent = child.clone();
    missing_parent.parent = None;
    assert_parent_refusal(
        parent_route(&missing_parent, &HashMap::new()).unwrap_err(),
        "the flow-read scope carries no parent scope",
    );
    let mut routes = HashMap::new();
    for (channel, epoch) in [(42, 9), (43, 6), (44, 9)] {
        let mut scope = serde_json::to_value(head(dir.path()).scope.as_ref().unwrap()).unwrap();
        scope["scope_epoch"] = json!(epoch);
        routes.insert(
            route_key(channel, 1),
            identity(dir.path(), scope, "reserved:broca"),
        );
    }
    let mut unrelated = serde_json::to_value(head(dir.path()).scope.as_ref().unwrap()).unwrap();
    unrelated["ref"] = json!("unrelated");
    unrelated["scope_epoch"] = json!(99);
    routes.insert(
        route_key(45, 1),
        identity(dir.path(), unrelated, "reserved:broca"),
    );
    let mut other_owner = serde_json::to_value(head(dir.path()).scope.as_ref().unwrap()).unwrap();
    other_owner["owner"]["module_id"] = json!("basal");
    other_owner["scope_epoch"] = json!(100);
    routes.insert(
        route_key(46, 1),
        identity(dir.path(), other_owner, "reserved:basal"),
    );
    assert_parent_refusal(
        parent_route(&child, &routes).unwrap_err(),
        &format!("{reason}; open routes for that owner and scope reference have epochs [6, 9]"),
    );
    routes.insert(route_key(47, 1), head(dir.path()));
    assert!(parent_route(&child, &routes).is_ok());
}

#[tokio::test]
async fn flow_read_daemon_parent_confirmation() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("head");
    let basal_root = dir.path().join("basal");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&basal_root).unwrap();
    std::fs::write(root.join("script"), b"abc").unwrap();
    std::fs::write(basal_root.join("script"), b"wrong").unwrap();
    let parent = head(&root);
    let mut config = Config::default();
    config.project_root = Some(root);
    config.storage_dir = Some(dir.path().join("store"));
    let ctx = Arc::new(AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        config,
    ));
    ctx.set_db(Arc::new(StdMutex::new(
        crate::db::open(&ctx.storage_dir().join("aft.db")).unwrap(),
    )));
    access_grants::register(&parent, &ctx).unwrap();
    let executor = Arc::new(Executor::new());
    executor.register_actor(parent.root.clone(), ctx);
    for (status, refusal) in [
        (Some(ScopeStatus::Live), None),
        (Some(ScopeStatus::NotLive), Some("Route not admitted: parent: the daemon reports the parent scope is not live.")),
        (Some(ScopeStatus::Ended), Some("Route not admitted: parent: the daemon reports the parent scope ended.")),
        (None, Some("Route not admitted: parent: the daemon scope.describe request for the parent scope failed.")),
    ] {
        let child = identity(&basal_root, stamp(), "reserved:basal");
        let child_scope = child.scope.as_ref().unwrap();
        assert_eq!(child_scope.kind, ScopeKind::Ephemeral);
        assert!(child_scope.parent.is_some());
        assert!(child_scope.attributes.flow_id.is_none());
        let routes = HashMap::from([
            (route_key(41, 1), child.clone()),
            (route_key(42, 1), parent.clone()),
        ]);
        let (writer, mut output) = mpsc::channel(8);
        let (control_tx, mut requests) = mpsc::channel(8);
        let pending = readiness::PendingControlReplies::default();
        let metrics = Arc::new(DispatchPathMetrics::new());
        let control = Arc::new(readiness::DaemonControl::new(
            control_tx,
            metrics.clone(),
            PROTOCOL_VERSION,
            vec![subc_protocol::scope::SCOPE_DESCRIBE_OP.into()],
            vec![],
            pending.clone(),
            HELLO_CORR + 1,
        ));
        let body = json!({"op":"file.read_exact","path":"script"});
        let frame = Frame::build(
            FrameType::Request,
            control_flags(),
            41,
            1,
            1,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap();
        let active = Arc::default();
        let (bash, _) = mpsc::channel(8);
        let (touch, _) = mpsc::channel(8);
        let (entries, _) = mpsc::unbounded_channel();
        handle_tool_call(
            &writer,
            &frame,
            Some(Ok(body)),
            None,
            PhaseTrace::new(Instant::now()),
            &routes,
            &HashMap::new(),
            &ReclaimedRoutes::default(),
            &mut HashMap::new(),
            &executor,
            &active,
            &Arc::new(PendingDeferredSetups::default()),
            &Arc::new(Notify::new()),
            &PersistentCancelSignal::new(),
            &bash,
            &touch,
            &metrics,
            &mut HashMap::new(),
            &mut HashMap::new(),
            &mut 1,
            &mut HashMap::new(),
            &mut HashMap::new(),
            &mut HashMap::new(),
            &mut HashMap::new(),
            |_, _| panic!("flow-read must not dispatch a general tool"),
            &DeferredResponseSender { entries, wake: Default::default() },
            false,
            2 * 1024 * 1024,
            &drain::ModuleDrainWindow::default(),
            Some(&control),
        )
        .await
        .unwrap();
        let request = tokio::time::timeout(Duration::from_secs(5), requests.recv())
            .await
            .unwrap()
            .unwrap();
        let query: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(query["op"], "scope.describe");
        assert_eq!(query["ref"], "head");
        let (ty, response) = match &status {
            Some(status) => (
                FrameType::Response,
                serde_json::to_vec(&reply(parent.scope.as_ref().unwrap(), status.clone())).unwrap(),
            ),
            None => (
                FrameType::Error,
                serde_json::to_vec(&ErrorBody::new("internal", "secret daemon details")).unwrap(),
            ),
        };
        assert!(pending.resolve(
            Frame::build(
                ty,
                control_flags(),
                0,
                0,
                request.header.corr,
                response,
            )
            .unwrap()
        ));
        let answer = tokio::time::timeout(Duration::from_secs(5), output.recv())
            .await
            .unwrap()
            .unwrap();
        let body: Value = serde_json::from_slice(&answer.body).unwrap();
        if let Some(refusal) = refusal {
            assert_eq!(answer.header.ty, FrameType::Error);
            assert_eq!(body["code"], "file_read_exact_route_not_admitted");
            assert_eq!(body["message"], refusal);
        } else {
            assert_eq!(answer.header.ty, FrameType::Response);
            assert_eq!(
                body,
                json!({"bytes_base64":"YWJj", "size":3,"sha256":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"})
            );
        }
    }
}

#[tokio::test]
async fn flow_read_missing_daemon_control() {
    let dir = tempfile::tempdir().unwrap();
    let child = identity(dir.path(), stamp(), "reserved:basal");
    let routes = HashMap::from([(route_key(42, 1), head(dir.path()))]);
    let (writer, mut output) = mpsc::channel(8);
    let body = json!({"op":"file.read_exact","path":"script"});
    let frame = Frame::build(
        FrameType::Request,
        control_flags(),
        41,
        1,
        1,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap();
    dispatch(
        &writer,
        &Arc::new(DispatchPathMetrics::new()),
        &frame,
        child,
        body,
        &routes,
        &Arc::new(Executor::new()),
        &Arc::default(),
        None,
    )
    .await
    .unwrap();
    let answer = output.recv().await.unwrap();
    assert_eq!(answer.header.ty, FrameType::Error);
    let body: Value = serde_json::from_slice(&answer.body).unwrap();
    assert_eq!(body["code"], "file_read_exact_route_not_admitted");
    assert_eq!(
        body["message"],
        "Route not admitted: parent: cannot ask the daemon whether the parent scope is live."
    );
}

async fn wire(identity: RouteIdentity, body: Value) -> Value {
    let (writer, mut replies) = mpsc::channel(8);
    let (bash, _) = mpsc::channel(8);
    let (touch, _) = mpsc::channel(8);
    let (entries, _) = mpsc::unbounded_channel();
    let frame = Frame::build(
        FrameType::Request,
        control_flags(),
        41,
        1,
        1,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap();
    let routes = HashMap::from([(route_key(41, 1), identity)]);
    handle_tool_call(
        &writer,
        &frame,
        None,
        None,
        PhaseTrace::new(Instant::now()),
        &routes,
        &HashMap::new(),
        &ReclaimedRoutes::default(),
        &mut HashMap::new(),
        &Arc::new(Executor::new()),
        &Arc::default(),
        &Arc::new(PendingDeferredSetups::default()),
        &Arc::new(Notify::new()),
        &PersistentCancelSignal::new(),
        &bash,
        &touch,
        &Arc::new(DispatchPathMetrics::new()),
        &mut HashMap::new(),
        &mut HashMap::new(),
        &mut 1,
        &mut HashMap::new(),
        &mut HashMap::new(),
        &mut HashMap::new(),
        &mut HashMap::new(),
        |_, _| panic!("flow route must not dispatch a general tool"),
        &DeferredResponseSender {
            entries,
            wake: Default::default(),
        },
        false,
        2 * 1024 * 1024,
        &drain::ModuleDrainWindow::default(),
        None,
    )
    .await
    .unwrap();
    let answer = replies.recv().await.unwrap();
    assert_eq!(answer.header.ty, FrameType::Error);
    serde_json::from_slice(&answer.body).unwrap()
}

#[tokio::test]
async fn flow_read_other_operations_refused() {
    let dir = tempfile::tempdir().unwrap();
    for op in [
        json!({"op":"grants.revoke","path":"/tmp"}),
        json!({"op":"tool.catalog"}),
        json!({"op":"role.describe"}),
        json!({"op":"tool.call","name":"read","arguments":{"path":"x"}}),
        json!({"name":"read","arguments":{"path":"x"}}),
        json!({"op":"unknown"}),
    ] {
        let result = wire(identity(dir.path(), stamp(), "reserved:basal"), op).await;
        assert_eq!(result["code"], "file_read_exact_route_not_admitted");
        assert_eq!(result["message"], "Route not admitted: operation.");
    }
}
#[tokio::test]
async fn flow_read_normal_route_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut scope = stamp();
    scope["ref"] = json!("codemode/call");
    let result = wire(
        identity(dir.path(), scope, "reserved:basal"),
        json!({"op":"file.read_exact","path":"x"}),
    )
    .await;
    assert_eq!(result["code"], "file_read_exact_route_not_admitted");
    assert_eq!(result["message"], "Route not admitted: scope_ref.");
}

#[test]
fn flow_read_parent_root_and_grants() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("head");
    let basal_root = dir.path().join("basal");
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&basal_root).unwrap();
    std::fs::write(root.join("script"), "abc").unwrap();
    std::fs::write(basal_root.join("script"), "wrong").unwrap();
    std::fs::write(&outside, "abc").unwrap();
    let head = head(&root);
    let child = identity(&basal_root, stamp(), "reserved:basal");
    let mut config = Config::default();
    config.project_root = Some(root.clone());
    config.storage_dir = Some(dir.path().join("store"));
    let ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
    ctx.set_db(Arc::new(StdMutex::new(
        crate::db::open(&ctx.storage_dir().join("aft.db")).unwrap(),
    )));
    access_grants::register(&head, &ctx).unwrap();
    assert_eq!(
        read_as_parent(&child, &head, &json!({"path":"script"}), &ctx, limit()).unwrap()
            ["bytes_base64"],
        "YWJj"
    );
    assert_eq!(
        read_as_parent(&child, &head, &json!({"path":outside}), &ctx, limit()),
        Err(Refusal::AccessDenied)
    );
    let key = access_grants::scope_key(&head);
    {
        let db = ctx.db().unwrap();
        let conn = db.lock().unwrap();
        crate::db::access_grants::insert(
            &conn,
            &key,
            &crate::access_policy::AccessGrant {
                path: outside.canonicalize().unwrap(),
                kind: crate::access_policy::GrantKind::File,
                operation: crate::access_policy::AccessOperation::Read,
            },
        )
        .unwrap();
    }
    assert_eq!(
        read_as_parent(&child, &head, &json!({"path":outside}), &ctx, limit()),
        Err(Refusal::AccessDenied)
    );
    let mut ended = stamp();
    ended["expires_at_ms"] = json!(1);
    assert_eq!(
        read_as_parent(
            &identity(&basal_root, ended, "reserved:basal"),
            &head,
            &json!({"path":"script"}),
            &ctx,
            limit()
        ),
        Err(Refusal::RouteNotAdmitted("expiry"))
    );
}

#[test]
fn flow_read_parent_floor_from_stored_policy() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("head");
    std::fs::create_dir(&root).unwrap();
    let secret = root.join("secret");
    std::fs::write(&secret, "secret").unwrap();
    let parent = head(&root);
    let child = identity(&root, stamp(), "reserved:basal");
    let mut config = Config::default();
    config.project_root = Some(root.clone());
    config.storage_dir = Some(dir.path().join("store"));
    config.sandbox.read_deny.push(secret.clone());
    let head_ctx = AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        config.clone(),
    );
    let db = Arc::new(StdMutex::new(
        crate::db::open(&head_ctx.storage_dir().join("aft.db")).unwrap(),
    ));
    head_ctx.set_db(db.clone());
    access_grants::register(&parent, &head_ctx).unwrap();
    config.sandbox.read_deny.clear();
    let basal_ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
    basal_ctx.set_db(db);
    access_grants::register(&child, &basal_ctx).unwrap();
    assert_eq!(
        read_as_parent(
            &child,
            &parent,
            &json!({"path":"secret"}),
            &basal_ctx,
            limit()
        ),
        Err(Refusal::AccessDenied)
    );
}

#[test]
fn flow_read_bad_request() {
    let dir = tempfile::tempdir().unwrap();
    let parent = head(dir.path());
    let child = identity(dir.path(), stamp(), "reserved:basal");
    let ctx = AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        Config::default(),
    );
    for body in [json!({}), json!({"path": 123}), json!({"path": ""})] {
        assert_eq!(
            read_as_parent(&child, &parent, &body, &ctx, limit()),
            Err(Refusal::BadRequest)
        );
    }
}

#[test]
fn flow_read_persistence_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let parent = head(dir.path());
    let child = identity(dir.path(), stamp(), "reserved:basal");
    let ctx = AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        Config::default(),
    );
    assert_eq!(
        read_as_parent(&child, &parent, &json!({"path":"script"}), &ctx, limit()),
        Err(Refusal::IoError)
    );
}

/// The production time limit, for reads whose tests do not exercise it.
fn limit() -> Instant {
    Instant::now() + persistence::WAIT_BUDGET
}

fn read_fixture() -> (
    tempfile::TempDir,
    Arc<AppContext>,
    RouteIdentity,
    RouteIdentity,
) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("head");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("script"), "abc").unwrap();
    let parent = head(&root);
    let child = identity(&root, stamp(), "reserved:basal");
    let mut config = Config::default();
    config.project_root = Some(root);
    config.storage_dir = Some(dir.path().join("store"));
    let ctx = Arc::new(AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        config,
    ));
    ctx.set_db(Arc::new(StdMutex::new(
        crate::db::open(&ctx.storage_dir().join("aft.db")).unwrap(),
    )));
    access_grants::register(&parent, &ctx).unwrap();
    (dir, ctx, parent, child)
}

struct ReadPause {
    entered: std::sync::mpsc::Receiver<()>,
    resume: Option<std::sync::mpsc::SyncSender<()>>,
}

impl ReadPause {
    fn wait(&self) {
        self.entered
            .recv_timeout(Duration::from_secs(5))
            .expect("read reached the mid-read hook");
    }
    fn resume(&mut self) {
        if let Some(sender) = self.resume.take() {
            let _ = sender.send(());
        }
    }
}
impl Drop for ReadPause {
    fn drop(&mut self) {
        self.resume();
    }
}

fn pause_read() -> (file_read_exact::ReadHooks, ReadPause) {
    let (entered_tx, entered) = std::sync::mpsc::sync_channel(1);
    let (resume, resumed) = std::sync::mpsc::sync_channel(1);
    let resumed = StdMutex::new(resumed);
    let hooks = file_read_exact::ReadHooks {
        mid_read: Some(Arc::new(move || {
            let _ = entered_tx.send(());
            resumed
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10))
                .expect("test releases the paused read");
        })),
        ..file_read_exact::ReadHooks::default()
    };
    (
        hooks,
        ReadPause {
            entered,
            resume: Some(resume),
        },
    )
}

#[test]
fn flow_read_database_lock_released_during_read() {
    let (_dir, ctx, parent, child) = read_fixture();
    let (hooks, mut pause) = pause_read();
    let reader_ctx = ctx.clone();
    let worker = std::thread::spawn(move || {
        read_as_parent_with_hooks(
            &child,
            &parent,
            &json!({"path":"script"}),
            &reader_ctx,
            limit(),
            hooks,
        )
    });
    pause.wait();
    let db = ctx.db().unwrap();
    let conn = db
        .try_lock()
        .expect("another route can acquire aft.db during the paused read");
    assert_eq!(
        conn.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        1
    );
    pause.resume();
    drop(conn);
    assert_eq!(worker.join().unwrap().unwrap()["bytes_base64"], "YWJj");
}

#[test]
fn flow_read_revoked_between_snapshot_and_recheck() {
    use crate::access_policy::{AccessGrant, AccessOperation, ApprovalMode, GrantKind};
    use crate::db::access_grants as store;
    let (_dir, ctx, parent, child) = read_fixture();
    let key = access_grants::scope_key(&parent);
    // Grants are stored under their policy spelling, and grants.revoke
    // normalizes its argument the same way before deleting. A raw
    // canonicalize() result differs on Windows (a \\?\ verbatim prefix), so
    // the revocation below would match no row there.
    let path = crate::access_policy::policy_path(&parent.project_root.join("script"));
    {
        let db = ctx.db().unwrap();
        let conn = db.lock().unwrap();
        let mut config = ctx.config().as_ref().clone();
        // Without an implicit policy root, this in-checkout file depends on its
        // explicit grant; revocation removes its only read authority.
        config.project_root = None;
        store::register_scope(&conn, &key, None, &config).unwrap();
        store::set_mode(&conn, &key, ApprovalMode::Ask).unwrap();
        store::insert(
            &conn,
            &key,
            &AccessGrant {
                path: path.clone(),
                kind: GrantKind::File,
                operation: AccessOperation::Read,
            },
        )
        .unwrap();
    }
    let (hooks, mut pause) = pause_read();
    let reader_ctx = ctx.clone();
    let worker = std::thread::spawn(move || {
        read_as_parent_with_hooks(
            &child,
            &parent,
            &json!({"path":"script"}),
            &reader_ctx,
            limit(),
            hooks,
        )
    });
    pause.wait();
    {
        let db = ctx.db().unwrap();
        let conn = db
            .try_lock()
            .expect("grant revocation must not wait for the paused file read");
        assert_eq!(store::revoke(&conn, &key, &path).unwrap(), 1);
    }
    pause.resume();
    assert_eq!(worker.join().unwrap(), Err(Refusal::AccessDenied));
}

#[test]
fn flow_read_stalled_read_expires_without_late_success() {
    let (_dir, ctx, parent, _child) = read_fixture();
    let mut scope = stamp();
    scope["expires_at_ms"] = json!(crate::db::call_ledger::now_ms() + 1500);
    let child = identity(&parent.project_root, scope, "reserved:basal");
    let (hooks, mut pause) = pause_read();
    let (done, result) = std::sync::mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let answer = read_as_parent_with_hooks(
            &child,
            &parent,
            &json!({"path":"script"}),
            &ctx,
            limit(),
            hooks,
        );
        let _ = done.send(answer);
    });
    pause.wait();
    let answer = result
        .recv_timeout(Duration::from_secs(3))
        .expect("expiry returns while the filesystem read is still paused");
    assert!(
        matches!(
            answer,
            Err(Refusal::RouteNotAdmitted("expiry")) | Err(Refusal::IoError)
        ),
        "no late success: {answer:?}"
    );
    pause.resume();
    worker.join().unwrap();
}

/// A parent head root whose persistence the test controls: the database is
/// still opening (nothing staged, so only the test finishes it) and the head's
/// scope row is not registered yet, as just after the head's bind reply.
fn unregistered_parent() -> (
    tempfile::TempDir,
    Arc<AppContext>,
    Arc<Executor>,
    RouteIdentity,
    RouteIdentity,
) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("head");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("script"), "abc").unwrap();
    let parent = head(&root);
    let child = identity(&root, stamp(), "reserved:basal");
    let mut config = Config::default();
    config.project_root = Some(root);
    config.storage_dir = Some(dir.path().join("store"));
    let ctx = Arc::new(AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        config,
    ));
    ctx.mark_database_runtime_initializing_for_test();
    let executor = Arc::new(Executor::new());
    executor.register_actor(parent.root.clone(), ctx.clone());
    (dir, ctx, executor, parent, child)
}

/// Starts a flow read of the parent checkout's `script` file and answers the daemon's parent liveness
/// query as live, so what follows depends only on the parent's persistence.
async fn start_live_read(
    executor: &Arc<Executor>,
    parent: &RouteIdentity,
    child: RouteIdentity,
    parent_wait: Duration,
) -> mpsc::Receiver<WriterFrame> {
    let (output, pending, corr) = start_read(
        executor,
        parent,
        child,
        parent_wait,
        DispatchHooks::default(),
    )
    .await;
    confirm_live(&pending, corr, parent);
    output
}

async fn start_read(
    executor: &Arc<Executor>,
    parent: &RouteIdentity,
    child: RouteIdentity,
    parent_wait: Duration,
    hooks: DispatchHooks,
) -> (
    mpsc::Receiver<WriterFrame>,
    readiness::PendingControlReplies,
    u64,
) {
    start_dispatch(
        executor,
        parent,
        child,
        parent_wait,
        hooks,
        json!({"op":"file.read_exact","path":"script"}),
    )
    .await
}

async fn start_dispatch(
    executor: &Arc<Executor>,
    parent: &RouteIdentity,
    child: RouteIdentity,
    parent_wait: Duration,
    hooks: DispatchHooks,
    body: Value,
) -> (
    mpsc::Receiver<WriterFrame>,
    readiness::PendingControlReplies,
    u64,
) {
    let routes = HashMap::from([
        (route_key(41, 1), child.clone()),
        (route_key(42, 1), parent.clone()),
    ]);
    let (writer, output) = mpsc::channel(8);
    let (control_tx, mut requests) = mpsc::channel(8);
    let pending = readiness::PendingControlReplies::default();
    let metrics = Arc::new(DispatchPathMetrics::new());
    let control = Arc::new(readiness::DaemonControl::new(
        control_tx,
        metrics.clone(),
        PROTOCOL_VERSION,
        vec![subc_protocol::scope::SCOPE_DESCRIBE_OP.into()],
        vec![],
        pending.clone(),
        HELLO_CORR + 1,
    ));
    let frame = Frame::build(
        FrameType::Request,
        control_flags(),
        41,
        1,
        1,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap();
    let active = Arc::default();
    dispatch_with_wait(
        &writer,
        &metrics,
        &frame,
        child,
        body,
        &routes,
        executor,
        &active,
        Some(&control),
        parent_wait,
        hooks,
    )
    .await
    .unwrap();
    let request = tokio::time::timeout(Duration::from_secs(5), requests.recv())
        .await
        .unwrap()
        .unwrap();
    (output, pending, request.header.corr)
}

fn confirm_live(pending: &readiness::PendingControlReplies, corr: u64, parent: &RouteIdentity) {
    assert!(pending.resolve(
        Frame::build(
            FrameType::Response,
            control_flags(),
            0,
            0,
            corr,
            serde_json::to_vec(&reply(parent.scope.as_ref().unwrap(), ScopeStatus::Live)).unwrap()
        )
        .unwrap()
    ));
}

fn dispatch_limit(child: &mut RouteIdentity, expires: bool) -> Duration {
    if expires {
        Arc::get_mut(&mut child.0)
            .unwrap()
            .scope
            .as_mut()
            .unwrap()
            .expires_at_ms = Some(crate::db::call_ledger::now_ms() as u64 + 400);
        Duration::from_secs(10)
    } else {
        Duration::from_millis(400)
    }
}

async fn deadline_error(output: &mut mpsc::Receiver<WriterFrame>, expires: bool, started: Instant) {
    let answer = tokio::time::timeout_at(
        tokio::time::Instant::from_std(started + Duration::from_millis(1400)),
        output.recv(),
    )
    .await
    .expect("the read must answer at its deadline while its blocker is still held")
    .expect("one terminal frame");
    assert!(
        started.elapsed() >= Duration::from_millis(399),
        "the read must not time out before its deadline"
    );
    let body: Value = serde_json::from_slice(&answer.body).unwrap();
    assert_eq!(answer.header.ty, FrameType::Error, "{body}");
    if expires {
        assert_eq!(body["code"], "file_read_exact_route_not_admitted");
        assert_eq!(body["message"], "Route not admitted: expiry.");
    } else {
        assert_eq!(body["code"], "file_read_exact_io_error");
        assert_eq!(
            body["message"],
            "The parent head's access policy could not be read within the read's time limit."
        );
    }
}

async fn no_second_frame(output: &mut mpsc::Receiver<WriterFrame>) {
    assert!(
        tokio::time::timeout(Duration::from_secs(2), output.recv())
            .await
            .expect("the response task releases its writer after answering")
            .is_none(),
        "a read must send exactly one terminal frame"
    );
}

struct ReadRunProbe {
    ran: Arc<std::sync::atomic::AtomicBool>,
    dropped: Option<oneshot::Sender<()>>,
}

impl Drop for ReadRunProbe {
    fn drop(&mut self) {
        let _ = self.dropped.take().unwrap().send(());
    }
}

#[tokio::test]
async fn flow_read_queued_job_deadline_cancels_before_start_and_replies_once() {
    for lane in [Lane::Mutating, Lane::PureRead] {
        for expires in [false, true] {
            let (_dir, ctx, parent, mut child) = read_fixture();
            let executor = Arc::new(Executor::with_config(crate::executor::ExecutorConfig {
                pool_size: 2,
                read_cap: 1,
                actor_cap: 1,
                ..Default::default()
            }));
            executor.register_actor(parent.root.clone(), ctx);
            let (started, running) = oneshot::channel();
            let (release, held) = std::sync::mpsc::channel::<()>();
            let blocker = executor.submit_async(
                parent.root.clone(),
                lane,
                "blocker".into(),
                Box::new(move |_| {
                    let _ = started.send(());
                    let _ = held.recv();
                    Response::success("blocker", json!({}))
                }),
            );
            running.await.unwrap();
            let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (dropped, job_dropped) = oneshot::channel();
            let probe = ReadRunProbe {
                ran: ran.clone(),
                dropped: Some(dropped),
            };
            let (submitted, queued) = oneshot::channel();
            let started_at = Instant::now();
            let limit = dispatch_limit(&mut child, expires);
            let (mut output, pending, corr) = start_read(
                &executor,
                &parent,
                child,
                limit,
                DispatchHooks {
                    submitted: Some(submitted),
                    before_read: Some(Box::new(move || {
                        probe.ran.store(true, std::sync::atomic::Ordering::SeqCst);
                        drop(probe);
                    })),
                },
            )
            .await;
            confirm_live(&pending, corr, &parent);
            tokio::time::timeout(Duration::from_secs(2), queued)
                .await
                .expect("read submitted behind the blocker")
                .unwrap();
            deadline_error(&mut output, expires, started_at).await;
            release.send(()).unwrap();
            blocker.await.unwrap();
            tokio::time::timeout(Duration::from_secs(2), job_dropped)
                .await
                .expect("cancelled read closure must be dropped")
                .unwrap();
            assert!(
                !ran.load(std::sync::atomic::Ordering::SeqCst),
                "a read cancelled in the queue must never run after the blocker releases"
            );
            no_second_frame(&mut output).await;
        }
    }
}

#[tokio::test]
async fn flow_read_describe_deadline_replies_once_without_daemon_answer() {
    for expires in [false, true] {
        let (_dir, ctx, parent, mut child) = read_fixture();
        let executor = Arc::new(Executor::new());
        executor.register_actor(parent.root.clone(), ctx);
        let started_at = Instant::now();
        let limit = dispatch_limit(&mut child, expires);
        let (mut output, pending, corr) =
            start_read(&executor, &parent, child, limit, DispatchHooks::default()).await;
        deadline_error(&mut output, expires, started_at).await;
        // A late daemon reply cannot restart dispatch or send another terminal frame.
        let _ = pending.resolve(
            Frame::build(
                FrameType::Response,
                control_flags(),
                0,
                0,
                corr,
                serde_json::to_vec(&reply(parent.scope.as_ref().unwrap(), ScopeStatus::Live))
                    .unwrap(),
            )
            .unwrap(),
        );
        no_second_frame(&mut output).await;
    }
}

#[tokio::test]
async fn flow_read_running_job_deadline_discards_answer_without_cancelling() {
    for expires in [false, true] {
        let (_dir, ctx, parent, mut child) = read_fixture();
        let executor = Arc::new(Executor::new());
        executor.register_actor(parent.root.clone(), ctx);
        let (started, running) = oneshot::channel();
        let (release, held) = std::sync::mpsc::channel::<()>();
        let started_at = Instant::now();
        let limit = dispatch_limit(&mut child, expires);
        let (mut output, pending, corr) = start_read(
            &executor,
            &parent,
            child,
            limit,
            DispatchHooks {
                submitted: None,
                before_read: Some(Box::new(move || {
                    let _ = started.send(crate::executor::current_job_cancellation().unwrap());
                    let _ = held.recv();
                })),
            },
        )
        .await;
        confirm_live(&pending, corr, &parent);
        let cancellation = tokio::time::timeout(Duration::from_secs(2), running)
            .await
            .expect("read is executing before its deadline")
            .unwrap();
        deadline_error(&mut output, expires, started_at).await;
        assert!(
            !cancellation.cancel_already_requested(),
            "running reads finish normally"
        );
        release.send(()).unwrap();
        let drained = executor.submit_async(
            parent.root.clone(),
            Lane::Mutating,
            "drained".into(),
            Box::new(|_| Response::success("drained", json!({}))),
        );
        tokio::time::timeout(Duration::from_secs(2), drained)
            .await
            .expect("the late read releases its executor reservation")
            .unwrap();
        no_second_frame(&mut output).await;
    }
}

#[tokio::test]
async fn flow_read_dispatch_normal_read_succeeds_once() {
    let (_dir, ctx, parent, child) = read_fixture();
    let executor = Arc::new(Executor::new());
    executor.register_actor(parent.root.clone(), ctx);
    let mut output = start_live_read(&executor, &parent, child, Duration::from_secs(10)).await;
    let answer = tokio::time::timeout(Duration::from_secs(2), output.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(answer.header.ty, FrameType::Response);
    let body: Value = serde_json::from_slice(&answer.body).unwrap();
    assert_eq!(body["bytes_base64"], "YWJj");
    no_second_frame(&mut output).await;
}

/// No answer may arrive within `quiet`: the read is still waiting.
async fn assert_still_waiting(output: &mut mpsc::Receiver<WriterFrame>, quiet: Duration) {
    if let Ok(answer) = tokio::time::timeout(quiet, output.recv()).await {
        let answer = answer.expect("reply channel open");
        panic!(
            "read answered before the parent was registered: {}",
            String::from_utf8_lossy(&answer.body)
        );
    }
}

/// A read sent right after the parent head's bind reply waits for the
/// parent's database to open and its scope row to be registered, then
/// succeeds, instead of refusing a read the parent's policy allows.
#[tokio::test]
async fn flow_read_waits_for_parent_database_and_scope_registration() {
    let (_dir, ctx, executor, parent, child) = unregistered_parent();
    let mut output = start_live_read(&executor, &parent, child, Duration::from_secs(10)).await;
    assert_still_waiting(&mut output, Duration::from_millis(300)).await;

    ctx.set_db(Arc::new(StdMutex::new(
        crate::db::open(&ctx.storage_dir().join("aft.db")).unwrap(),
    )));
    ctx.finish_database_runtime(Ok(()));
    assert_still_waiting(&mut output, Duration::from_millis(300)).await;

    access_grants::register(&parent, &ctx).unwrap();
    let answer = tokio::time::timeout(Duration::from_secs(5), output.recv())
        .await
        .expect("read answers once the parent is registered")
        .unwrap();
    let body: Value = serde_json::from_slice(&answer.body).unwrap();
    assert_eq!(answer.header.ty, FrameType::Response, "{body}");
    assert_eq!(
        body,
        json!({"bytes_base64":"YWJj", "size":3,"sha256":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"})
    );
}

/// When the parent's scope row never arrives, the read refuses once the wait
/// bound passes, and not before; a database that fails to open ends the wait
/// at once.
#[tokio::test]
async fn flow_read_refuses_after_bound_when_parent_never_registers() {
    let bound = Duration::from_millis(400);
    let (_dir, ctx, executor, parent, child) = unregistered_parent();
    ctx.set_db(Arc::new(StdMutex::new(
        crate::db::open(&ctx.storage_dir().join("aft.db")).unwrap(),
    )));
    ctx.finish_database_runtime(Ok(()));
    let started = Instant::now();
    let mut output = start_live_read(&executor, &parent, child, bound).await;
    let answer = tokio::time::timeout(Duration::from_secs(5), output.recv())
        .await
        .expect("an unregistered parent is refused once the bound passes")
        .unwrap();
    assert!(started.elapsed() >= bound, "refused before the wait bound");
    let body: Value = serde_json::from_slice(&answer.body).unwrap();
    assert_eq!(answer.header.ty, FrameType::Error, "{body}");
    assert_eq!(body["code"], "file_read_exact_io_error");
    assert_eq!(
        body["message"],
        "The parent head's access policy could not be read within the read's time limit."
    );

    let (_dir, ctx, executor, parent, child) = unregistered_parent();
    let started = Instant::now();
    let mut output = start_live_read(&executor, &parent, child, Duration::from_secs(10)).await;
    ctx.finish_database_runtime_error("database cannot open".into(), false);
    let answer = tokio::time::timeout(Duration::from_secs(5), output.recv())
        .await
        .expect("a failed parent database is refused without waiting out the bound")
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));
    let body: Value = serde_json::from_slice(&answer.body).unwrap();
    assert_eq!(answer.header.ty, FrameType::Error, "{body}");
    assert_eq!(body["code"], "file_read_exact_io_error");
}

/// Another route can hold aft.db for a long time, for example while it
/// resolves paths on a stalled filesystem. A flow read must still answer
/// within its own time limit, with the policy timeout, and releasing the lock
/// afterwards must not send a second answer.
#[tokio::test]
async fn flow_read_policy_lock_held_past_limit_replies_once_with_timeout() {
    let (_dir, ctx, parent, child) = read_fixture();
    let executor = Arc::new(Executor::new());
    executor.register_actor(parent.root.clone(), ctx.clone());
    let db = ctx.db().unwrap();
    let (held_tx, held) = std::sync::mpsc::sync_channel(0);
    let (release, released) = std::sync::mpsc::sync_channel::<()>(0);
    let holder = std::thread::spawn(move || {
        let _conn = db.lock().unwrap();
        held_tx.send(()).unwrap();
        // Held until the test releases it, or until the test fails and drops
        // the sender.
        let _ = released.recv();
    });
    held.recv().unwrap();

    let read_limit = Duration::from_millis(400);
    let started = Instant::now();
    let mut output = start_live_read(&executor, &parent, child, read_limit).await;
    let answer = tokio::time::timeout(Duration::from_secs(5), output.recv())
        .await
        .expect("the read answers while aft.db is still held")
        .unwrap();
    let elapsed = started.elapsed();
    assert!(
        elapsed >= read_limit,
        "answered before its limit: {elapsed:?}"
    );
    assert!(
        elapsed < read_limit + Duration::from_secs(1),
        "answered long after its limit: {elapsed:?}"
    );
    let body: Value = serde_json::from_slice(&answer.body).unwrap();
    assert_eq!(answer.header.ty, FrameType::Error, "{body}");
    assert_eq!(body["code"], "file_read_exact_io_error");
    assert_eq!(
        body["message"],
        "The parent head's access policy could not be read within the read's time limit."
    );

    release.send(()).unwrap();
    holder.join().unwrap();
    if let Ok(Some(late)) = tokio::time::timeout(Duration::from_millis(500), output.recv()).await {
        panic!(
            "a second answer followed the lock's release: {}",
            String::from_utf8_lossy(&late.body)
        );
    }
}

#[tokio::test]
async fn perf_sweep_admission_only_the_basal_flow_route() {
    let dir = tempfile::tempdir().unwrap();
    for scope_ref in ["head/call", "codemode/call"] {
        let mut scope = stamp();
        scope["ref"] = json!(scope_ref);
        let error = wire(
            identity(dir.path(), scope, "reserved:basal"),
            json!({"op":"perf.sweep"}),
        )
        .await;
        assert_eq!(error["code"], "file_read_exact_route_not_admitted");
        assert_eq!(error["message"], "Route not admitted: scope_ref.");
    }
    let error = wire(
        identity(dir.path(), stamp(), "reserved:other"),
        json!({"op":"perf.sweep"}),
    )
    .await;
    assert_eq!(error["message"], "Route not admitted: principal.");
    let error = wire(
        identity(dir.path(), stamp(), "reserved:basal"),
        json!({"op":"tool.call", "name":"perf.sweep", "arguments":{}}),
    )
    .await;
    assert_eq!(error["message"], "Route not admitted: operation.");
    let error = wire(
        identity(dir.path(), stamp(), "reserved:basal"),
        json!({"op":"perf.sweep"}),
    )
    .await;
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("no open AFT route for the parent head scope"),
        "{error}"
    );
}

/// perf.sweep samples the daemon itself, not the parent project, so a long
/// sample must not occupy the project's executor lanes: a write on the same
/// root is admitted while the sweep is still sampling. The sweep still sends
/// exactly one answer at its deadline, and its busy guard stays held until the
/// sample itself ends.
#[tokio::test]
async fn perf_sweep_long_sample_does_not_block_project_writes() {
    let (_dir, ctx, parent, mut child) = read_fixture();
    let executor = Arc::new(Executor::new());
    executor.register_actor(parent.root.clone(), ctx);
    // The child scope expires while the sample is still held, so the sweep
    // answers at its deadline and never starts a real profiler child here.
    Arc::get_mut(&mut child.0)
        .unwrap()
        .scope
        .as_mut()
        .unwrap()
        .expires_at_ms = Some(crate::db::call_ledger::now_ms() as u64 + 3000);
    let (started, sampling) = oneshot::channel();
    let (release, held) = std::sync::mpsc::channel::<()>();
    let (mut output, pending, corr) = start_dispatch(
        &executor,
        &parent,
        child,
        super::super::perf_sweep::DISPATCH_BUDGET,
        DispatchHooks {
            submitted: None,
            before_read: Some(Box::new(move || {
                let _ = started.send(());
                // Stands in for the profiler's sample, which can last 35 s.
                let _ = held.recv();
            })),
        },
        json!({"op":"perf.sweep","profile_seconds":30}),
    )
    .await;
    confirm_live(&pending, corr, &parent);
    tokio::time::timeout(Duration::from_secs(2), sampling)
        .await
        .expect("the sweep is sampling")
        .unwrap();

    let write = executor.submit_async(
        parent.root.clone(),
        Lane::Mutating,
        "write".into(),
        Box::new(|_| Response::success("write", json!({}))),
    );
    tokio::time::timeout(Duration::from_secs(1), write)
        .await
        .expect("a write on the same root is admitted while the sweep samples")
        .unwrap();

    let answer = tokio::time::timeout(Duration::from_secs(5), output.recv())
        .await
        .expect("the sweep answers at its deadline while the sample is held")
        .expect("one terminal frame");
    let body: Value = serde_json::from_slice(&answer.body).unwrap();
    assert_eq!(answer.header.ty, FrameType::Error, "{body}");
    assert_eq!(body["message"], "Route not admitted: expiry.");
    match super::super::perf_sweep::admit(&json!({"op":"perf.sweep"})) {
        Ok(_) => panic!("a second sweep was admitted while the first still samples"),
        Err(error) => assert_eq!(error.code, "busy"),
    }

    release.send(()).unwrap();
    let released_by = Instant::now() + Duration::from_secs(2);
    while let Err(error) = super::super::perf_sweep::admit(&json!({"op":"perf.sweep"})) {
        assert!(
            Instant::now() < released_by,
            "the busy guard outlived the sample: {}",
            error.message
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    no_second_frame(&mut output).await;
}
