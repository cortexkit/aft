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
        disabled_tools: Arc::new(vec![]),
        scope: Some(serde_json::from_value(scope).unwrap()),
        call_mode,
        run_scope: OnceLock::new(),
        made_tool_call: AtomicBool::new(false),
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
    check(|s| s["parent"] = Value::Null, "reserved:basal", "parent");
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
#[test]
fn flow_read_dead_parent() {
    let dir = tempfile::tempdir().unwrap();
    let head = head(dir.path());
    let stamp = head.scope.as_ref().unwrap();
    assert_eq!(
        live_parent(stamp, &reply(stamp, ScopeStatus::NotLive)),
        Err(Refusal::RouteNotAdmitted("parent"))
    );
}
#[test]
fn flow_read_ended_parent() {
    let dir = tempfile::tempdir().unwrap();
    let head = head(dir.path());
    let stamp = head.scope.as_ref().unwrap();
    assert_eq!(
        live_parent(stamp, &reply(stamp, ScopeStatus::Ended)),
        Err(Refusal::RouteNotAdmitted("parent"))
    );
}
#[test]
fn flow_read_parent_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let head = head(dir.path());
    let stamp = head.scope.as_ref().unwrap();
    let mut changed = stamp.clone();
    changed.scope_epoch += 1;
    assert_eq!(
        live_parent(stamp, &reply(&changed, ScopeStatus::Live)),
        Err(Refusal::RouteNotAdmitted("parent"))
    );
    changed = stamp.clone();
    changed.kind = ScopeKind::Worker;
    assert_eq!(
        live_parent(stamp, &reply(&changed, ScopeStatus::Live)),
        Err(Refusal::RouteNotAdmitted("parent"))
    );
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
    for status in [ScopeStatus::Live, ScopeStatus::NotLive, ScopeStatus::Ended] {
        let child = identity(&basal_root, stamp(), "reserved:basal");
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
        dispatch(
            &writer,
            &metrics,
            &frame,
            child,
            body,
            &routes,
            &executor,
            &active,
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
        assert!(pending.resolve(
            Frame::build(
                FrameType::Response,
                control_flags(),
                0,
                0,
                request.header.corr,
                serde_json::to_vec(&reply(parent.scope.as_ref().unwrap(), status.clone())).unwrap()
            )
            .unwrap()
        ));
        let answer = tokio::time::timeout(Duration::from_secs(5), output.recv())
            .await
            .unwrap()
            .unwrap();
        let body: Value = serde_json::from_slice(&answer.body).unwrap();
        if status == ScopeStatus::Live {
            assert_eq!(answer.header.ty, FrameType::Response);
            assert_eq!(
                body,
                json!({"bytes_base64":"YWJj", "size":3,"sha256":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"})
            );
        } else {
            assert_eq!(answer.header.ty, FrameType::Error);
            assert_eq!(body["message"], "Route not admitted: parent.");
        }
    }
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
        read_as_parent(&child, &head, &json!({"path":"script"}), &ctx).unwrap()["bytes_base64"],
        "YWJj"
    );
    assert_eq!(
        read_as_parent(&child, &head, &json!({"path":outside}), &ctx),
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
        read_as_parent(&child, &head, &json!({"path":outside}), &ctx),
        Err(Refusal::AccessDenied)
    );
    let mut ended = stamp();
    ended["expires_at_ms"] = json!(1);
    assert_eq!(
        read_as_parent(
            &identity(&basal_root, ended, "reserved:basal"),
            &head,
            &json!({"path":"script"}),
            &ctx
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
        read_as_parent(&child, &parent, &json!({"path":"secret"}), &basal_ctx),
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
            read_as_parent(&child, &parent, &body, &ctx),
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
        read_as_parent(&child, &parent, &json!({"path":"script"}), &ctx),
        Err(Refusal::IoError)
    );
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
        let answer =
            read_as_parent_with_hooks(&child, &parent, &json!({"path":"script"}), &ctx, hooks);
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
