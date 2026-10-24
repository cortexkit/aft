use super::*;
use crate::access_policy::{AccessDecision, AccessDenial};

#[cfg(unix)]
#[path = "access_grants_restore_tests.rs"]
mod slow_restore;

fn identity(root: &Path, name: &str, kind: &str, parent: Option<&str>) -> RouteIdentity {
    let stamp = serde_json::from_value(json!({"owner":{"kind":"reserved","module_id":"prefrontal-core"},"ref":name,"scope_epoch":7,"kind":kind,"parent":parent.map(|name|json!({"owner":{"kind":"reserved","module_id":"prefrontal-core"},"ref":name,"scope_epoch":7})),"owner_authorized":true,"attributes":{"agent_id":match kind {"head"=>Some(name),"ephemeral" if parent.is_none()=>Some("head"),_=>None}}})).unwrap();
    RouteIdentity(Arc::new(RouteIdentityData {
        root: ProjectRootId::from_path(root).unwrap(),
        project_root: root.into(),
        harness: "runner".into(),
        session: name.into(),
        role: tool_provider::RouteRole::ToolProviderV1,
        trust: BindTrust::FirstParty,
        spawn_principal: AuthenticatedPrincipal::RouteBind {
            trust: crate::sandbox_spawn::PrincipalTrust::FirstParty,
            route_channel: 41,
            route_epoch: 1,
            project_root: root.into(),
            harness: "runner".into(),
            session_id: name.into(),
            principal_id: Some("reserved:broca".into()),
        },
        consumer_elicitation_capable: false,
        disabled_tools: Arc::new(vec![]),
        scope: Some(stamp),
        call_mode: subc_protocol::call_mode::CallMode::Classic,
        run_scope: OnceLock::new(),
        made_tool_call: AtomicBool::new(false),
        run_owner: None,
    }))
}
fn context(root: &Path, storage: &Path) -> AppContext {
    let mut config = crate::config::Config::default();
    config.project_root = Some(root.into());
    config.storage_dir = Some(storage.into());
    let ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
    ctx.set_db(Arc::new(StdMutex::new(
        crate::db::open(&storage.join("aft.db")).unwrap(),
    )));
    ctx
}
fn default_fixture() -> (tempfile::TempDir, AppContext, RouteIdentity, RouteIdentity) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("head");
    let work = dir.path().join("worker");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    // Resolve macOS's /var -> /private/var alias, but remove Windows's verbatim
    // `\\?\` prefix: launch params are plain paths, where `?` denotes a glob.
    let root = policy_path(&root);
    let work = policy_path(&work);
    let ctx = context(&root, &dir.path().join("store"));
    let head = identity(&root, "head", "head", None);
    let worker = identity(&work, "worker", "worker", Some("head"));
    register(&head, &ctx).unwrap();
    register(&worker, &ctx).unwrap();
    (dir, ctx, head, worker)
}
fn fixture() -> (tempfile::TempDir, AppContext, RouteIdentity, RouteIdentity) {
    let result = default_fixture();
    remote_policy::catalog(
        json!({"preset":"head","params":{"mode":"ask"}}),
        &result.2,
        &result.1,
    )
    .unwrap();
    result
}
fn catalog_request(raw: Value) -> Value {
    json!({"preset":"worker","params":{"allow_paths":raw}})
}

fn worker_at_parent_epoch(worker: &RouteIdentity, parent: &str, epoch: u64) -> RouteIdentity {
    let mut data = copied_identity(worker);
    let parent_stamp = data.scope.as_mut().unwrap().parent.as_mut().unwrap();
    parent_stamp.scope_ref = parent.into();
    parent_stamp.scope_epoch = epoch;
    RouteIdentity(Arc::new(data))
}

#[test]
fn grants_head_policy_survives_an_unregistered_parent_epoch() {
    let (_dir, ctx, head, worker) = fixture();
    let worker = worker_at_parent_epoch(&worker, "head", 12);
    remote_policy::catalog(
        catalog_request(json!([{"path":head.project_root,"mode":"read"}])),
        &worker,
        &ctx,
    )
    .unwrap();
    assert_eq!(
        operation(json!({}), "grants.list", &worker, &ctx).unwrap()["grants"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn grants_head_policy_never_crosses_identity_future_or_end_boundaries() {
    let (_dir, ctx, head, worker) = fixture();
    let different = worker_at_parent_epoch(&worker, "different-head", 12);
    let future = worker_at_parent_epoch(&worker, "head", 6);
    let mut other_owner = copied_identity(&worker_at_parent_epoch(&worker, "head", 12));
    other_owner
        .scope
        .as_mut()
        .unwrap()
        .parent
        .as_mut()
        .unwrap()
        .owner =
        serde_json::from_value(json!({"kind":"reserved","module_id":"another-owner"})).unwrap();
    for caller in [different, future, RouteIdentity(Arc::new(other_owner))] {
        let error = remote_policy::catalog(
            catalog_request(json!([{"path":head.project_root,"mode":"read"}])),
            &caller,
            &ctx,
        )
        .unwrap_err();
        assert_eq!(error.detail.unwrap()["reason"], "head_policy_unknown");
    }
    let mut ended = scope_key(&head);
    ended.epoch = 10;
    store::end_scope_grants(&ctx.db().unwrap().lock().unwrap(), &ended).unwrap();
    for epoch in [10, 12] {
        let caller = worker_at_parent_epoch(&worker, "head", epoch);
        let error = remote_policy::catalog(
            catalog_request(json!([{"path":head.project_root,"mode":"read"}])),
            &caller,
            &ctx,
        )
        .unwrap_err();
        assert_eq!(error.detail.unwrap()["reason"], "head_policy_unknown");
    }
}

#[test]
fn grants_head_policy_tombstone_returns_no_scope_and_refuses_surviving_authority() {
    let (dir, ctx, head, worker) = fixture();
    let latest = at_epoch(&head, 10);
    register(&latest, &ctx).unwrap();
    let older_target = policy_path(&dir.path().join("older-head-grant"));
    let ended_target = policy_path(&dir.path().join("ended-head-grant"));
    let db = ctx.db().unwrap();
    {
        let conn = db.lock().unwrap();
        store::set_mode(
            &conn,
            &scope_key(&latest),
            crate::access_policy::ApprovalMode::Ask,
        )
        .unwrap();
        for (identity, path) in [(&head, &older_target), (&latest, &ended_target)] {
            store::insert(
                &conn,
                &scope_key(identity),
                &AccessGrant {
                    path: path.clone(),
                    kind: GrantKind::Subtree,
                    operation: AccessOperation::Read,
                },
            )
            .unwrap();
            let policy = policy_for_scope(&conn, &scope_key(identity), &ctx)
                .unwrap()
                .unwrap();
            validate(&json!([{"path":path,"mode":"read"}]), Some(&policy), None).unwrap();
        }
        // Leave configuration and stale grants in place deliberately. Ordinary
        // scope-end cleanup also deletes grants; authorization must not depend
        // on that deletion to reject an ended policy.
        let key = scope_key(&latest);
        assert_eq!(
            conn.execute(
                "UPDATE access_scopes SET ended=1 WHERE owner=?1 AND scope_ref=?2 AND epoch=?3",
                rusqlite::params![key.owner, key.scope_ref, key.epoch.to_string()],
            )
            .unwrap(),
            1
        );
        assert!(store::scope_config(&conn, &key).unwrap().is_none());
        assert!(policy_snapshot(&conn, &key).unwrap().is_none());
        let retained_config: String = conn
            .query_row(
                "SELECT config FROM access_scopes WHERE owner=?1 AND scope_ref=?2 AND epoch=?3",
                rusqlite::params![key.owner, key.scope_ref, key.epoch.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        let retained_config: crate::config::Config =
            serde_json::from_str(&retained_config).unwrap();
        assert_eq!(
            retained_config.project_root,
            Some(head.project_root.clone())
        );
        for identity in [&head, &latest] {
            let key = scope_key(identity);
            let retained_grants: usize = conn
                .query_row(
                    "SELECT COUNT(*) FROM access_grants WHERE owner=?1 AND scope_ref=?2 AND epoch=?3",
                    rusqlite::params![key.owner, key.scope_ref, key.epoch.to_string()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(retained_grants, 1);
        }
    }
    for epoch in [10, 12] {
        let caller = worker_at_parent_epoch(&worker, "head", epoch);
        for path in [&older_target, &ended_target, &head.project_root] {
            let error = remote_policy::catalog(
                catalog_request(json!([{"path":path,"mode":"read"}])),
                &caller,
                &ctx,
            )
            .unwrap_err();
            assert_eq!(error.code, "plan_param_refused");
            assert_eq!(error.message, "allow_paths[0]: head_policy_unknown");
            assert_eq!(
                error.detail,
                Some(json!({
                    "param":"allow_paths", "index":0, "path":path, "reason":"head_policy_unknown"
                }))
            );
            assert!(store::list(&db.lock().unwrap(), &scope_key(&caller))
                .unwrap()
                .is_empty());
        }
        let mut parent = scope_key(&latest);
        parent.epoch = epoch;
        assert_eq!(
            store::head_policy_scope(&db.lock().unwrap(), &parent).unwrap(),
            None,
            "a tombstone must resolve to no policy scope, not an ended key"
        );
    }
}

#[test]
fn grants_head_policy_rechecks_revocation_at_source_epoch() {
    let (_dir, ctx, head, worker) = fixture();
    let caller = worker_at_parent_epoch(&worker, "head", 12);
    let target = worker.project_root.join("shared");
    let db = ctx.db().unwrap();
    store::insert(
        &db.lock().unwrap(),
        &scope_key(&head),
        &AccessGrant {
            path: policy_path(&target),
            kind: GrantKind::Subtree,
            operation: AccessOperation::Read,
        },
    )
    .unwrap();
    remote_policy::catalog(
        catalog_request(json!([{"path":target,"mode":"read"}])),
        &caller,
        &ctx,
    )
    .unwrap();
    operation(json!({"path":target}), "grants.revoke", &head, &ctx).unwrap();
    let error = remote_policy::catalog(json!({"preset":"worker"}), &caller, &ctx).unwrap_err();
    assert_eq!(error.detail.unwrap()["reason"], "head_not_held");
}

#[test]
fn grants_head_policy_uses_latest_observed_restrictions() {
    let (_dir, ctx, head, worker) = fixture();
    let target = worker.project_root.join("shared");
    let db = ctx.db().unwrap();
    store::insert(
        &db.lock().unwrap(),
        &scope_key(&head),
        &AccessGrant {
            path: policy_path(&target),
            kind: GrantKind::Subtree,
            operation: AccessOperation::Read,
        },
    )
    .unwrap();
    let newer = at_epoch(&head, 10);
    register(&newer, &ctx).unwrap();
    store::set_mode(
        &db.lock().unwrap(),
        &scope_key(&newer),
        crate::access_policy::ApprovalMode::Ask,
    )
    .unwrap();
    let caller = worker_at_parent_epoch(&worker, "head", 12);
    let error = remote_policy::catalog(
        catalog_request(json!([{"path":target,"mode":"read"}])),
        &caller,
        &ctx,
    )
    .unwrap_err();
    assert_eq!(error.detail.unwrap()["reason"], "head_not_held");
}

fn copied_identity(identity: &RouteIdentity) -> RouteIdentityData {
    RouteIdentityData {
        root: identity.root.clone(),
        project_root: identity.project_root.clone(),
        harness: identity.harness.clone(),
        session: identity.session.clone(),
        role: identity.role,
        trust: identity.trust,
        spawn_principal: identity.spawn_principal.clone(),
        consumer_elicitation_capable: identity.consumer_elicitation_capable,
        disabled_tools: identity.disabled_tools.clone(),
        scope: identity.scope.clone(),
        call_mode: identity.call_mode.clone(),
        run_scope: OnceLock::new(),
        made_tool_call: AtomicBool::new(false),
        run_owner: None,
    }
}

fn at_epoch(identity: &RouteIdentity, epoch: u64) -> RouteIdentity {
    let mut data = copied_identity(identity);
    data.scope.as_mut().unwrap().scope_epoch = epoch;
    RouteIdentity(Arc::new(data))
}

#[cfg(unix)]
#[test]
fn grants_revived_launch_symlink_changes_never_widen_authority() {
    let (_dir, ctx, head, worker) = fixture();
    let original = head.project_root.join("original");
    let other = head.project_root.join("other");
    let alias = head.project_root.join("alias");
    std::fs::create_dir(&original).unwrap();
    std::fs::create_dir(&other).unwrap();
    std::os::unix::fs::symlink(&original, &alias).unwrap();
    remote_policy::catalog(
        catalog_request(json!([{"path":alias,"mode":"read"}])),
        &worker,
        &ctx,
    )
    .unwrap();
    std::fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink(&other, &alias).unwrap();
    let revived = at_epoch(&worker, 8);
    restore_launch(&revived, &ctx, "worker").unwrap();
    let db = ctx.db().unwrap();
    assert_eq!(
        store::list(&db.lock().unwrap(), &scope_key(&revived)).unwrap(),
        vec![AccessGrant {
            path: original.clone(),
            kind: GrantKind::Subtree,
            operation: AccessOperation::Read,
        }]
    );
    std::fs::remove_dir(&original).unwrap();
    std::os::unix::fs::symlink(&other, &original).unwrap();
    let changed = at_epoch(&worker, 9);
    restore_launch(&changed, &ctx, "worker").unwrap();
    assert!(store::list(&db.lock().unwrap(), &scope_key(&changed))
        .unwrap()
        .is_empty());
}

async fn first_read(ctx: Arc<AppContext>, identity: RouteIdentity, preset: &str) {
    let body = first_read_response(ctx, identity, preset).await;
    assert_eq!(body["structuredContent"]["success"], true, "{body}");
}

async fn first_read_response(ctx: Arc<AppContext>, identity: RouteIdentity, preset: &str) -> Value {
    first_read_path_response(ctx, identity, preset, None).await
}

async fn first_read_path_response(
    ctx: Arc<AppContext>,
    identity: RouteIdentity,
    preset: &str,
    target: Option<PathBuf>,
) -> Value {
    ctx.update_config(|config| config.project_root = Some(identity.project_root.clone()));
    let target = target.unwrap_or_else(|| identity.project_root.join("first-call.txt"));
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(&target, "first call").unwrap();
    let executor = Arc::new(Executor::new());
    executor.register_actor(identity.root.clone(), ctx.clone());
    let confinement = remote_policy::CarriedConfinement::read(&ctx, &identity);
    let routes = HashMap::from([(route_key(41, 1), identity)]);
    let frame = Frame::build(
        FrameType::Request,
        control_flags(),
        41,
        1,
        7,
        serde_json::to_vec(&json!({
            "op":"tool.call", "name":"read", "preset":preset,
            "arguments":{"filePath":target}
        }))
        .unwrap(),
    )
    .unwrap();
    let decoded = DecodedFrame {
        parsed_body: Default::default(),
        confinement: None,
        launch_restore: None,
        codemode_policies: None,
        frame,
        phase_trace: PhaseTrace::new(Instant::now()),
    };
    let mut waits = persistence::DatabaseWaits::default();
    let decoded = match waits.restore_launch(decoded, &routes, &executor) {
        Ok(()) => waits.next().await,
        Err(decoded) => decoded,
    };
    let frame = decoded.frame;
    let (writer, mut replies) = mpsc::channel(8);
    let (bash_tx, _bash_rx) = mpsc::channel(8);
    let (touch_tx, _touch_rx) = mpsc::channel(8);
    let (entries, _deferred_rx) = mpsc::unbounded_channel();
    handle_tool_call(
        &writer,
        &frame,
        None,
        Some(confinement),
        PhaseTrace::new(Instant::now()),
        &routes,
        &HashMap::new(),
        &ReclaimedRoutes::default(),
        &mut HashMap::new(),
        &executor,
        &Arc::default(),
        &Arc::new(PendingDeferredSetups::default()),
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
        |request, ctx| crate::commands::read::handle_read(&request, ctx),
        &DeferredResponseSender {
            entries,
            wake: crate::response_finalize::DeferredResponseWake::default(),
        },
        false,
        1024 * 1024,
        &drain::ModuleDrainWindow::default(),
        None,
    )
    .await
    .unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(5), replies.recv())
        .await
        .expect("first read must terminate")
        .unwrap();
    let body: Value = serde_json::from_slice(&reply.frame.body).unwrap();
    body
}

#[tokio::test]
async fn grants_revived_launch_first_call_restores_without_catalog() {
    let (_dir, ctx, head, worker) = fixture();
    let worker = at_epoch(&worker, 1);
    let target = head.project_root.join("shared");
    std::fs::create_dir_all(&target).unwrap();
    remote_policy::catalog(
        catalog_request(json!([{"path":target,"mode":"write"}])),
        &worker,
        &ctx,
    )
    .unwrap();
    let revived = at_epoch(&worker, 2);
    let ctx = Arc::new(ctx);
    first_read(ctx.clone(), revived.clone(), "worker").await;
    let conn = ctx.db().unwrap();
    let conn = conn.lock().unwrap();
    let expected = vec![AccessGrant {
        path: policy_path(&target),
        kind: GrantKind::Subtree,
        operation: AccessOperation::Write,
    }];
    assert_eq!(store::list(&conn, &scope_key(&revived)).unwrap(), expected);
    assert_eq!(store::list(&conn, &scope_key(&worker)).unwrap(), expected);
    let markers: i64 = conn
        .query_row("SELECT COUNT(*) FROM access_materialized", [], |r| r.get(0))
        .unwrap();
    assert_eq!(markers, 2, "old-epoch history must remain intact");
}

#[test]
fn grants_future_targets_restore_only_while_launch_parent_exists() {
    let (_dir, ctx, head, worker) = fixture();
    let parent = head.project_root.join("existing-parent");
    std::fs::create_dir(&parent).unwrap();
    let future = parent.join("future").join("nested");
    let created = parent.join("created-later");
    remote_policy::catalog(
        catalog_request(json!([
            {"path":future,"mode":"write"},
            {"path":created,"mode":"write"}
        ])),
        &worker,
        &ctx,
    )
    .unwrap();
    std::fs::remove_dir(&parent).unwrap();
    let revived = at_epoch(&worker, 8);
    let refused = restore_launch(&revived, &ctx, "worker").unwrap_err();
    assert_eq!(refused.detail.unwrap()["retryable"], true);
    std::fs::create_dir_all(&created).unwrap();
    restore_launch(&revived, &ctx, "worker").unwrap();
    let db = ctx.db().unwrap();
    let restored = store::list(&db.lock().unwrap(), &scope_key(&revived)).unwrap();
    assert_eq!(restored.len(), 2);
    for target in [&future, &created] {
        assert!(restored
            .iter()
            .any(|grant| &grant.path == target && grant.operation == AccessOperation::Write));
    }
    assert!(!future.exists());
}

#[test]
fn grants_revived_launch_failed_restore_refuses_and_retries() {
    let (_dir, ctx, head, worker) = fixture();
    let worker = at_epoch(&worker, 1);
    let target = head.project_root.join("shared");
    remote_policy::catalog(
        catalog_request(json!([{"path":target,"mode":"write"}])),
        &worker,
        &ctx,
    )
    .unwrap();
    let revived = at_epoch(&worker, 2);
    let ctx = Arc::new(ctx);
    let db = ctx.db().unwrap();
    db.lock().unwrap().execute_batch("CREATE TRIGGER fail_restore BEFORE INSERT ON access_grants WHEN NEW.epoch='2' BEGIN SELECT RAISE(ABORT,'injected restore failure'); END;").unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (_, lines) = crate::logging::capture_log_lines(|| {
        let body = runtime.block_on(first_read_path_response(
            ctx.clone(),
            revived.clone(),
            "worker",
            Some(target.join("first-call.txt")),
        ));
        let refused = &body["structuredContent"];
        assert_eq!(refused["code"], "grants_unavailable", "{body}");
        assert_eq!(refused["detail"]["retryable"], true, "{body}");
        assert_eq!(refused["detail"]["path"], json!(target), "{body}");
    });
    assert!(
        lines
            .iter()
            .any(|line| line.contains("launch_grant_restore_failed:")
                && line.contains(&format!("owner={}", scope_key(&revived).owner))
                && line.contains("scope_ref=worker epoch=2 code=grants_unavailable")
                && line.contains("injected restore failure")),
        "{lines:?}"
    );
    assert_eq!(ctx.launch_restore_attempts(), 1);
    assert!(store::list(&db.lock().unwrap(), &scope_key(&revived))
        .unwrap()
        .is_empty());
    crate::access_policy::with_scope(Some(scope_key(&revived)), || {
        assert_ne!(
            ctx.access_policy("read")
                .unwrap()
                .decide(AccessOperation::Read, &target, false),
            AccessDecision::Allow
        );
    });
    db.lock()
        .unwrap()
        .execute_batch("DROP TRIGGER fail_restore;")
        .unwrap();
    runtime.block_on(first_read(ctx.clone(), revived.clone(), "worker"));
    assert_eq!(
        ctx.launch_restore_attempts(),
        2,
        "failed restores must retry"
    );
    assert_eq!(
        store::list(&db.lock().unwrap(), &scope_key(&revived)).unwrap(),
        vec![AccessGrant {
            path: policy_path(&target),
            kind: GrantKind::Subtree,
            operation: AccessOperation::Write,
        }]
    );
}

#[tokio::test]
async fn grants_revived_launch_memo_skips_restore_database_on_second_call() {
    let (_dir, ctx, head, worker) = fixture();
    let worker = at_epoch(&worker, 1);
    std::fs::create_dir_all(head.project_root.join("shared")).unwrap();
    remote_policy::catalog(
        catalog_request(json!([{"path":head.project_root.join("shared"),"mode":"read"}])),
        &worker,
        &ctx,
    )
    .unwrap();
    let ctx = Arc::new(ctx);
    for epoch in [1, 2] {
        let identity = at_epoch(&worker, epoch);
        first_read(ctx.clone(), identity.clone(), "worker").await;
        assert_eq!(ctx.launch_restore_attempts(), epoch as usize);
        // Separately constructed routes for the same scope reuse the cached
        // launch result instead of repeating its database check.
        first_read(ctx.clone(), at_epoch(&worker, epoch), "worker").await;
        assert_eq!(
            ctx.launch_restore_attempts(),
            epoch as usize,
            "second call must not enter the restore database path"
        );
    }
}

#[tokio::test]
async fn grants_revived_launch_no_params_is_not_memoized() {
    let (_dir, ctx, head, worker) = fixture();
    let worker = at_epoch(&worker, 1);
    let revived = at_epoch(&worker, 2);
    let ctx = Arc::new(ctx);
    first_read(ctx.clone(), revived.clone(), "worker").await;
    first_read(ctx.clone(), revived.clone(), "worker").await;
    assert_eq!(
        ctx.launch_restore_attempts(),
        2,
        "no params is not a definite materialization"
    );
    let target = head.project_root.join("late-launch");
    std::fs::create_dir_all(&target).unwrap();
    remote_policy::catalog(
        catalog_request(json!([{"path":target,"mode":"read"}])),
        &worker,
        &ctx,
    )
    .unwrap();
    first_read(ctx.clone(), revived.clone(), "worker").await;
    assert_eq!(ctx.launch_restore_attempts(), 3);
    let db = ctx.db().unwrap();
    assert_eq!(
        store::list(&db.lock().unwrap(), &scope_key(&revived)).unwrap(),
        vec![AccessGrant {
            path: policy_path(&target),
            kind: GrantKind::Subtree,
            operation: AccessOperation::Read,
        }]
    );
    first_read(ctx.clone(), revived, "worker").await;
    assert_eq!(ctx.launch_restore_attempts(), 3);
}

#[tokio::test]
async fn grants_revived_launch_does_not_copy_ask_earned_grants() {
    let (_dir, ctx, head, worker) = fixture();
    let worker = at_epoch(&worker, 1);
    remote_policy::catalog(json!({"preset":"worker"}), &worker, &ctx).unwrap();
    let earned = AccessGrant {
        path: policy_path(&head.project_root.join("consented")),
        kind: GrantKind::Subtree,
        operation: AccessOperation::Write,
    };
    {
        let db = ctx.db().unwrap();
        // Consent installs standing grants, not launch parameters.
        store::insert(&db.lock().unwrap(), &scope_key(&worker), &earned).unwrap();
    }
    let revived = at_epoch(&worker, 2);
    let ctx = Arc::new(ctx);
    first_read(ctx.clone(), revived.clone(), "worker").await;
    let db = ctx.db().unwrap();
    let conn = db.lock().unwrap();
    assert!(store::list(&conn, &scope_key(&revived)).unwrap().is_empty());
    assert_eq!(
        store::list(&conn, &scope_key(&worker)).unwrap(),
        vec![earned]
    );
}

#[test]
fn grants_revived_launch_drops_and_logs_revoked_head_and_floor() {
    let (_dir, ctx, head, worker) = fixture();
    let worker = at_epoch(&worker, 1);
    let held = head.project_root.join("held");
    let revoked = worker.project_root.join("revoked");
    let denied = head.project_root.join("denied");
    for path in [&held, &revoked, &denied] {
        std::fs::create_dir_all(path).unwrap();
    }
    let grant = AccessGrant {
        path: policy_path(&revoked),
        kind: GrantKind::Subtree,
        operation: AccessOperation::Read,
    };
    {
        let db = ctx.db().unwrap();
        store::insert(&db.lock().unwrap(), &scope_key(&head), &grant).unwrap();
    }
    let raw = json!([
        {"path":held,"mode":"write"},
        {"path":revoked,"mode":"read"},
        {"path":denied,"mode":"read"}
    ]);
    remote_policy::catalog(catalog_request(raw.clone()), &worker, &ctx).unwrap();
    operation(json!({"path":revoked}), "grants.revoke", &head, &ctx).unwrap();
    ctx.update_config(|config| config.sandbox.read_deny.push(denied.clone()));
    register(&head, &ctx).unwrap();
    let revived = at_epoch(&worker, 2);
    let (result, lines) =
        crate::logging::capture_log_lines(|| restore_launch(&revived, &ctx, "worker"));
    result.unwrap();
    assert_eq!(lines.len(), 2, "{lines:?}");
    for (path, reason) in [(&revoked, "head_not_held"), (&denied, "denied_floor")] {
        assert!(
            lines.iter().any(|line| {
                line.contains(&format!("path={}", json!(path)))
                    && line.contains("mode=\"read\"")
                    && line.contains(&format!("reason=\"{reason}\""))
            }),
            "missing named drop for {path:?}: {lines:?}"
        );
    }
    let db = ctx.db().unwrap();
    {
        let conn = db.lock().unwrap();
        assert_eq!(
            store::list(&conn, &scope_key(&revived)).unwrap(),
            vec![AccessGrant {
                path: policy_path(&held),
                kind: GrantKind::Subtree,
                operation: AccessOperation::Write,
            }]
        );
        assert_eq!(store::list(&conn, &scope_key(&worker)).unwrap().len(), 3);
        let mut saved = store::launch_params(&conn, &launch_key(&revived, "worker").unwrap())
            .unwrap()
            .unwrap();
        for entry in saved.as_array_mut().unwrap() {
            assert_eq!(entry["target"]["state"], "existing");
            assert!(entry.as_object_mut().unwrap().remove("target").is_some());
        }
        assert_eq!(saved, raw);
        store::revoke(&conn, &scope_key(&revived), &policy_path(&held)).unwrap();
    }
    let (result, lines) =
        crate::logging::capture_log_lines(|| restore_launch(&revived, &ctx, "worker"));
    result.unwrap();
    assert!(
        lines.is_empty(),
        "repeat call must not revalidate: {lines:?}"
    );
    assert!(store::list(&db.lock().unwrap(), &scope_key(&revived))
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn grants_revived_launch_isolates_ref_owner_session_and_preset() {
    for dimension in [
        "ref",
        "owner",
        "session",
        "preset",
        "principal",
        "harness",
        "root",
    ] {
        let (dir, ctx, head, worker) = fixture();
        let worker = at_epoch(&worker, 1);
        remote_policy::catalog(
            catalog_request(json!([{"path":head.project_root.join("shared"),"mode":"read"}])),
            &worker,
            &ctx,
        )
        .unwrap();
        let mut data = copied_identity(&at_epoch(&worker, 2));
        let mut preset = "worker";
        match dimension {
            "ref" => data.scope.as_mut().unwrap().scope_ref = "other".into(),
            "owner" => {
                data.scope.as_mut().unwrap().owner =
                    serde_json::from_value(json!({"kind":"direct"})).unwrap()
            }
            "session" => data.session = "other".into(),
            "preset" => preset = "head",
            "principal" => {
                if let AuthenticatedPrincipal::RouteBind { principal_id, .. } =
                    &mut data.spawn_principal
                {
                    *principal_id = Some("reserved:other".into());
                }
            }
            "harness" => data.harness = "other".into(),
            _ => {
                data.project_root = dir.path().join("other-root");
                std::fs::create_dir(&data.project_root).unwrap();
                data.root = ProjectRootId::from_path(&data.project_root).unwrap();
            }
        }
        let other = RouteIdentity(Arc::new(data));
        let ctx = Arc::new(ctx);
        first_read(ctx.clone(), other.clone(), preset).await;
        let db = ctx.db().unwrap();
        assert!(
            store::list(&db.lock().unwrap(), &scope_key(&other))
                .unwrap()
                .is_empty(),
            "{dimension}"
        );
    }
}

#[test]
fn grants_allow_paths_denied_floor_names_module_and_credential_rules() {
    let dir = tempfile::tempdir().unwrap();
    let home = policy_path(dir.path());
    let policy = AccessPolicy::from_config(
        &crate::config::Config::default(),
        &home.join("aft-store"),
        vec![home.join("project")],
        &home,
    );
    // Built one component at a time: the policy names the floor with the
    // platform's own separators, so a "/"-joined literal differs on Windows.
    for (target, floor, rule, description) in [
        (
            home.join(".local/share/cortexkit/synapse/hy-memory"),
            home.join(".local").join("share").join("cortexkit"),
            "cortexkit_module_data",
            "module data store under ~/.local/share/cortexkit/<module>",
        ),
        (
            home.join(".ssh/keys"),
            home.join(".ssh"),
            "credential_path",
            "credential path",
        ),
    ] {
        let error = validate(
            &json!([{"path":target,"mode":"write"}]),
            Some(&policy),
            None,
        )
        .unwrap_err();
        assert_eq!(error.code, "plan_param_refused");
        assert_eq!(
            error.detail,
            Some(json!({
                "param":"allow_paths", "index":0, "path":target, "reason":"denied_floor",
                "floor_rule":rule, "floor_path":floor,
            }))
        );
        assert_eq!(
            error.message,
            format!(
                "allow_paths[0]: denied_floor ({description}: {})",
                floor.display()
            )
        );
    }
}

#[test]
fn grants_allow_paths_accepts_a_literal_tilde_inside_an_absolute_path() {
    // Windows short names (C:\Users\RUNNER~1) put `~` inside absolute paths.
    let (_dir, ctx, head, _worker) = fixture();
    let target = head.project_root.join("SHORT~1");
    let raw = json!([{"path":target,"mode":"read"}]);
    remote_policy::catalog(catalog_request(raw), &head, &ctx).unwrap();
}

#[test]
fn grants_allow_paths_preflight_fetch_revoke_restart_reclaim() {
    let (dir, ctx, head, worker) = fixture();
    let target = head.project_root.join("shared");
    let raw = json!([{"path":target,"mode":"read"},{"path":target,"mode":"write"}]);
    remote_policy::catalog(catalog_request(raw.clone()), &head, &ctx).unwrap();
    assert!(
        operation(json!({}), "grants.list", &head, &ctx).unwrap()["grants"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        operation(json!({"scope":"worker"}), "grants.list", &head, &ctx).unwrap()["grants"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    remote_policy::catalog(catalog_request(raw.clone()), &worker, &ctx).unwrap();
    let granted = operation(json!({"scope":"worker"}), "grants.list", &head, &ctx).unwrap();
    assert_eq!(granted["grants"].as_array().unwrap().len(), 1);
    assert_eq!(granted["grants"][0]["mode"], "write");
    operation(
        json!({"scope":"worker","path":target}),
        "grants.revoke",
        &head,
        &ctx,
    )
    .unwrap();
    let storage = ctx.storage_dir();
    drop(ctx);
    std::fs::remove_dir(&worker.project_root).unwrap();
    std::fs::create_dir(&worker.project_root).unwrap();
    let restarted = context(&worker.project_root, &storage);
    remote_policy::catalog(catalog_request(raw), &worker, &restarted).unwrap();
    remote_policy::catalog(json!({"preset":"worker"}), &worker, &restarted).unwrap();
    assert!(
        operation(json!({}), "grants.list", &worker, &restarted).unwrap()["grants"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let db = restarted.db().unwrap();
    let conn = db.lock().unwrap();
    let policy = policy_for_scope(&conn, &scope_key(&worker), &restarted)
        .unwrap()
        .unwrap();
    assert_eq!(
        policy.decide(AccessOperation::Write, &target, false),
        AccessDecision::Ask
    );
    assert!(dir.path().exists());
}

#[test]
fn grants_allow_paths_atomic_validation_and_named_refusals() {
    let (_dir, ctx, head, worker) = fixture();
    for (bad, reason) in [
        (json!({"path":"relative","mode":"read"}), "not_absolute"),
        (json!({"path":"~/outside","mode":"read"}), "not_absolute"),
        (json!({"path":"/tmp/../outside","mode":"read"}), "malformed"),
        (json!({"path":"/tmp/*","mode":"read"}), "malformed"),
        (json!({"path":"/tmp/outside"}), "malformed"),
        (json!({"path":"/tmp/outside","mode":"execute"}), "malformed"),
        (
            json!({"path":worker.project_root.join("outside"),"mode":"read"}),
            "head_not_held",
        ),
    ] {
        let raw = json!([{"path":head.project_root.join("ok"),"mode":"read"},bad]);
        for caller in [&head, &worker] {
            let error =
                remote_policy::catalog(catalog_request(raw.clone()), caller, &ctx).unwrap_err();
            assert_eq!(error.code, "plan_param_refused");
            assert_eq!(error.detail.as_ref().unwrap()["index"], 1);
            assert_eq!(error.detail.as_ref().unwrap()["param"], "allow_paths");
            assert_eq!(error.detail.as_ref().unwrap()["reason"], reason);
        }
        assert!(
            operation(json!({}), "grants.list", &worker, &ctx).unwrap()["grants"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let conn = ctx.db().unwrap();
        assert!(store::launch_params(
            &conn.lock().unwrap(),
            &remote_policy::key(&worker, Some("worker")).unwrap()
        )
        .unwrap()
        .is_none());
    }
    assert_eq!(
        validate(&json!({}), None, None)
            .unwrap_err()
            .detail
            .unwrap()["reason"],
        "malformed"
    );
    let oversized = Value::Array(
        (0..65)
            .map(|_| json!({"path":head.project_root,"mode":"read"}))
            .collect(),
    );
    assert_eq!(
        validate(&oversized, None, None)
            .unwrap_err()
            .detail
            .unwrap()["index"],
        64
    );
}

#[test]
fn grants_catalog_refetch_rechecks_head_grant_and_floor() {
    let (_dir, ctx, head, worker) = fixture();
    let target = worker.project_root.join("allowed");
    let grant = AccessGrant {
        path: crate::access_policy::policy_path(&target),
        kind: GrantKind::Subtree,
        operation: AccessOperation::Read,
    };
    {
        let db = ctx.db().unwrap();
        store::insert(&db.lock().unwrap(), &scope_key(&head), &grant).unwrap();
    }
    let raw = json!([{"path":target,"mode":"read"}]);
    remote_policy::catalog(catalog_request(raw), &worker, &ctx).unwrap();
    operation(json!({"path":target}), "grants.revoke", &head, &ctx).unwrap();
    let error = remote_policy::catalog(json!({"preset":"worker"}), &worker, &ctx).unwrap_err();
    assert_eq!(error.detail.unwrap()["reason"], "head_not_held");
    ctx.update_config(|config| {
        config
            .sandbox
            .read_deny
            .push(head.project_root.join("secret"))
    });
    register(&head, &ctx).unwrap();
    let error = remote_policy::catalog(
        catalog_request(json!([{"path":head.project_root,"mode":"write"}])),
        &head,
        &ctx,
    )
    .unwrap_err();
    assert_eq!(error.detail.unwrap()["reason"], "denied_floor");
}

#[test]
fn grants_operations_refuse_other_scopes_and_end_drops_grants() {
    let (_dir, ctx, head, worker) = fixture();
    let other = identity(&worker.project_root, "other", "worker", Some("head"));
    register(&other, &ctx).unwrap();
    for op in ["grants.list", "grants.revoke"] {
        for target in ["head", "other", "unknown"] {
            assert_eq!(
                operation(
                    json!({"scope":target,"path":"/tmp/outside"}),
                    op,
                    &worker,
                    &ctx
                )
                .unwrap_err()
                .code,
                "grants_scope_refused"
            );
        }
    }
    assert!(operation(json!({"scope":"worker"}), "grants.list", &head, &ctx).is_ok());
    remote_policy::catalog(
        catalog_request(json!([{"path":head.project_root.join("shared"),"mode":"read"}])),
        &worker,
        &ctx,
    )
    .unwrap();
    {
        let db = ctx.db().unwrap();
        store::end_scope_grants(&db.lock().unwrap(), &scope_key(&worker)).unwrap();
    }
    assert_eq!(
        operation(json!({}), "grants.list", &worker, &ctx)
            .unwrap_err()
            .code,
        "grants_scope_refused"
    );
    let db = ctx.db().unwrap();
    assert!(store::list(&db.lock().unwrap(), &scope_key(&worker))
        .unwrap()
        .is_empty());
}

#[test]
fn grants_policy_reads_revocation_on_the_next_check() {
    let (_dir, ctx, head, worker) = fixture();
    ctx.update_config(|config| config.restrict_to_project_root = true);
    let path = head.project_root.join("shared");
    std::fs::write(&path, "protected").unwrap();
    remote_policy::catalog(
        catalog_request(json!([{"path":path,"mode":"write"}])),
        &worker,
        &ctx,
    )
    .unwrap();
    ctx.update_config(|config| config.project_root = Some(worker.project_root.clone()));
    crate::access_policy::with_scope(Some(scope_key(&worker)), || {
        assert_eq!(
            ctx.access_policy("check").unwrap().decide(
                AccessOperation::Read,
                &crate::access_policy::policy_path(&path),
                false
            ),
            AccessDecision::Allow
        );
        assert!(ctx.validate_read_path("check", "worker", &path).is_ok());
        operation(json!({"path":path}), "grants.revoke", &worker, &ctx).unwrap();
        assert_eq!(
            ctx.access_policy("check").unwrap().decide(
                AccessOperation::Read,
                &crate::access_policy::policy_path(&path),
                false
            ),
            AccessDecision::Deny(AccessDenial::OutsideRoots)
        );
        assert!(ctx.validate_read_path("check", "worker", &path).is_err());
    });
}

#[test]
fn scoped_read_lists_load_policy_and_grants_once_per_call() {
    let _storage_override = crate::test_env::without_storage_override();
    let (_dir, ctx, head, worker) = fixture();
    let shared = head.project_root.join("shared");
    std::fs::write(&shared, "shared").unwrap();
    remote_policy::catalog(
        catalog_request(json!([{"path":shared,"mode":"read"}])),
        &worker,
        &ctx,
    )
    .unwrap();
    ctx.update_config(|config| config.project_root = Some(worker.project_root.clone()));
    let src = worker.project_root.join("src");
    std::fs::create_dir(&src).unwrap();
    for index in 0..160 {
        std::fs::write(src.join(format!("item{index:03}.rs")), "pub fn item() {}\n").unwrap();
    }
    *ctx.search_index().write().unwrap() = Some(crate::search_index::SearchIndex::build(
        &worker.project_root,
    ));
    crate::access_policy::with_scope(Some(scope_key(&worker)), || {
        assert_eq!(
            ctx.access_policy("scope-sanity").unwrap().decide(
                AccessOperation::Read,
                &policy_path(&shared),
                false
            ),
            AccessDecision::Allow
        );
        for (tool, params) in [
            ("glob", json!({"path":"src", "pattern":"**/*.rs"})),
            ("outline", json!({"directory":src})),
            ("outline", json!({"target":src,"files":true})),
        ] {
            let req = crate::protocol::RawRequest {
                id: format!("scoped-policy-once-{tool}"),
                command: tool.into(),
                session_id: Some(worker.session.clone()),
                lsp_hints: None,
                params,
            };
            for _ in 0..2 {
                let probe = crate::context::policy_load_probe::Probe::new(&req.id);
                let response = if tool == "glob" {
                    crate::commands::glob::handle_glob(&req, &ctx)
                } else {
                    crate::commands::outline::handle_outline(&req, &ctx)
                };
                assert!(response.success, "{response:?}");
                let loads = probe.counts();
                assert_eq!((loads.policies, loads.grants), (1, 1), "{loads:?}");
            }
        }
    });
}

fn read_policy_boundary_fixture() -> (
    tempfile::TempDir,
    AppContext,
    RouteIdentity,
    RouteIdentity,
    PathBuf,
    PathBuf,
) {
    let (dir, ctx, head, scope_a) = fixture();
    let root_b = dir.path().join("worker-b");
    std::fs::create_dir(&root_b).unwrap();
    let scope_b = identity(&policy_path(&root_b), "worker-b", "worker", Some("head"));
    register(&scope_b, &ctx).unwrap();
    let path_a = head.project_root.join("grant-a.txt");
    let path_b = head.project_root.join("grant-b.txt");
    std::fs::write(&path_a, "scope A").unwrap();
    std::fs::write(&path_b, "scope B").unwrap();
    for (route, path) in [(&scope_a, &path_a), (&scope_b, &path_b)] {
        remote_policy::catalog(
            catalog_request(json!([{"path":path,"mode":"read"}])),
            route,
            &ctx,
        )
        .unwrap();
    }
    ctx.update_config(|config| {
        config.project_root = Some(scope_a.project_root.clone());
        config.restrict_to_project_root = true;
    });
    (dir, ctx, scope_a, scope_b, path_a, path_b)
}

fn assert_boundary_grants(
    policy: &crate::access_policy::AccessPolicy,
    allowed: &Path,
    denied: &Path,
    label: &str,
) {
    assert_eq!(
        policy.decide(AccessOperation::Read, &policy_path(allowed), false),
        AccessDecision::Allow,
        "{label}: wrong grants for allowed path {allowed:?}: {:?}",
        policy.grants
    );
    assert_ne!(
        policy.decide(AccessOperation::Read, &policy_path(denied), false),
        AccessDecision::Allow,
        "{label}: inherited another scope's grant for {denied:?}: {:?}",
        policy.grants
    );
}

#[test]
fn read_policy_snapshot_scope_boundary_uses_current_scope_grants() {
    let _storage_override = crate::test_env::without_storage_override();
    let (_dir, ctx, scope_a, scope_b, path_a, path_b) = read_policy_boundary_fixture();
    let id = "same-request-scope-boundary";
    let probe = crate::context::policy_load_probe::Probe::new(id);
    crate::access_policy::with_scope(Some(scope_key(&scope_a)), || {
        ctx.with_read_policy_snapshot(id, || {
            let policy_a = ctx.access_policy(id).unwrap();
            assert_boundary_grants(&policy_a, &path_a, &path_b, "scope A");
            let initial = probe.counts();
            crate::access_policy::with_scope(Some(scope_key(&scope_b)), || {
                let policy_b = ctx.access_policy(id).unwrap();
                assert_boundary_grants(
                    &policy_b,
                    &path_b,
                    &path_a,
                    "scope B in A's live request cache",
                );
                assert!(probe.counts().policies > initial.policies);
                ctx.with_read_policy_snapshot(id, || {
                    let nested_b = ctx.access_policy(id).unwrap();
                    assert_boundary_grants(&nested_b, &path_b, &path_a, "nested scope B");
                });
                // Dropping the inner cache restores A's entry, not B's authority.
                let after_nested = ctx.access_policy(id).unwrap();
                assert_boundary_grants(
                    &after_nested,
                    &path_b,
                    &path_a,
                    "scope B after nested cache drop",
                );
            });
        });
    });
    // Reusing both identifiers on this thread after the call must start fresh.
    crate::access_policy::with_scope(Some(scope_key(&scope_b)), || {
        let after_call = ctx.access_policy(id).unwrap();
        assert_boundary_grants(&after_call, &path_b, &path_a, "scope B after call return");
    });
}

#[test]
fn read_policy_snapshot_route_generation_rebuilds_changed_grants() {
    let _storage_override = crate::test_env::without_storage_override();
    let (_dir, ctx, scope_a, _scope_b, path_a, path_b) = read_policy_boundary_fixture();
    let id = "same-request-route-boundary";
    crate::access_policy::with_scope(Some(scope_key(&scope_a)), || {
        ctx.with_read_policy_snapshot(id, || {
            assert_boundary_grants(
                &ctx.access_policy(id).unwrap(),
                &path_a,
                &path_b,
                "initial route",
            );
            let generation = crate::access_policy::ask_route_generation();
            {
                let db = ctx.db().unwrap();
                let conn = db.lock().unwrap();
                assert_eq!(
                    crate::db::access_grants::revoke(
                        &conn,
                        &scope_key(&scope_a),
                        &policy_path(&path_a)
                    )
                    .unwrap(),
                    1
                );
                crate::db::access_grants::insert(
                    &conn,
                    &scope_key(&scope_a),
                    &crate::access_policy::AccessGrant {
                        path: policy_path(&path_b),
                        kind: crate::access_policy::GrantKind::File,
                        operation: AccessOperation::Read,
                    },
                )
                .unwrap();
            }
            assert_eq!(crate::access_policy::ask_route_generation(), generation);
            assert_boundary_grants(
                &ctx.access_policy(id).unwrap(),
                &path_a,
                &path_b,
                "same-route snapshot",
            );
            let _route = crate::access_policy::ask_route_guard(Some(
                crate::access_policy::AskRoute::Lowered,
            ));
            assert_ne!(crate::access_policy::ask_route_generation(), generation);
            assert_boundary_grants(
                &ctx.access_policy(id).unwrap(),
                &path_b,
                &path_a,
                "changed route generation",
            );
        });
    });
}

#[test]
fn read_policy_snapshot_nested_scope_does_not_resurrect_revoked_grants() {
    let _storage_override = crate::test_env::without_storage_override();
    let (_dir, ctx, scope_a, scope_b, path_a, path_b) = read_policy_boundary_fixture();
    let id = "same-request-nested-scope-boundary";
    crate::access_policy::with_scope(Some(scope_key(&scope_a)), || {
        ctx.with_read_policy_snapshot(id, || {
            assert_boundary_grants(
                &ctx.access_policy(id).unwrap(),
                &path_a,
                &path_b,
                "outer scope A",
            );
            crate::access_policy::with_scope(Some(scope_key(&scope_b)), || {
                ctx.with_read_policy_snapshot(id, || {
                    assert_boundary_grants(
                        &ctx.access_policy(id).unwrap(),
                        &path_b,
                        &path_a,
                        "inner scope B",
                    );
                    let db = ctx.db().unwrap();
                    let conn = db.lock().unwrap();
                    assert_eq!(
                        crate::db::access_grants::revoke(
                            &conn,
                            &scope_key(&scope_a),
                            &policy_path(&path_a)
                        )
                        .unwrap(),
                        1
                    );
                });
            });
            let after_nested = ctx.access_policy(id).unwrap();
            assert_ne!(
                after_nested.decide(AccessOperation::Read, &policy_path(&path_a), false),
                AccessDecision::Allow,
                "scope A resurrected revoked grants after scope B: {:?}",
                after_nested.grants
            );
            assert_ne!(
                after_nested.decide(AccessOperation::Read, &policy_path(&path_b), false),
                AccessDecision::Allow
            );
        });
    });
}

#[test]
fn grants_ordinary_detach_and_reclamation_keep_standing_capabilities() {
    let (dir, ctx, _head, worker) = fixture();
    let grant = AccessGrant {
        path: crate::access_policy::policy_path(&dir.path().join("shared")),
        kind: GrantKind::Subtree,
        operation: AccessOperation::Read,
    };
    {
        let db = ctx.db().unwrap();
        store::insert(&db.lock().unwrap(), &scope_key(&worker), &grant).unwrap();
    }
    let storage = ctx.storage_dir();
    let scope = scope_key(&worker);
    drop(worker);
    drop(ctx);
    std::fs::remove_dir(dir.path().join("worker")).unwrap();
    std::fs::create_dir(dir.path().join("worker")).unwrap();
    let rebound = context(&dir.path().join("worker"), &storage);
    let db = rebound.db().unwrap();
    assert_eq!(
        store::list(&db.lock().unwrap(), &scope).unwrap(),
        vec![grant]
    );
}

#[test]
fn grants_empty_launch_is_materialized_and_cannot_be_widened_on_refetch() {
    let (_dir, ctx, head, worker) = fixture();
    remote_policy::catalog(json!({"preset":"worker"}), &worker, &ctx).unwrap();
    remote_policy::catalog(
        catalog_request(json!([{"path":head.project_root.join("shared"),"mode":"write"}])),
        &worker,
        &ctx,
    )
    .unwrap();
    assert!(
        operation(json!({}), "grants.list", &worker, &ctx).unwrap()["grants"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn grants_deferred_delete_carries_scope() {
    let (_dir, ctx, head, worker) = fixture();
    ctx.backup().lock().set_policy(crate::backup::BackupPolicy {
        enabled: false,
        ..Default::default()
    });
    let target = head.project_root.join("shared");
    std::fs::write(&target, "protected").unwrap();
    remote_policy::catalog(
        catalog_request(json!([{"path":target,"mode":"write"}])),
        &worker,
        &ctx,
    )
    .unwrap();
    ctx.update_config(|config| {
        config.project_root = Some(worker.project_root.clone());
        config.restrict_to_project_root = true;
    });
    let ctx = Arc::new(ctx);
    let request: RawRequest = serde_json::from_value(
        json!({"id":"deferred-grant","command":"delete_file","session_id":"worker","file":target}),
    )
    .unwrap();
    let outcome = crate::access_policy::with_scope(Some(scope_key(&worker)), || {
        crate::commands::delete_file::handle_delete_deferred_with_restriction(
            &request,
            Arc::clone(&ctx),
            false,
        )
    });
    let crate::response_finalize::DispatchOutcome::Deferred(mut pending) = outcome else {
        panic!("external temp-file delete must defer");
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    let response = loop {
        if let Some(response) = (pending.poll)(&ctx) {
            break response;
        }
        assert!(
            Instant::now() < deadline,
            "deferred delete did not terminate"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(response.success, "{}", response.data);
    assert!(!target.exists());
}

#[test]
fn grants_exact_file_authority_reaches_preserved_final_write_checks() {
    let (_dir, ctx, head, worker) = fixture();
    let path = head.project_root.join("file");
    std::fs::write(&path, "protected").unwrap();
    let grant = AccessGrant {
        path: crate::access_policy::policy_path(&path),
        kind: GrantKind::File,
        operation: AccessOperation::Write,
    };
    {
        let db = ctx.db().unwrap();
        store::insert(&db.lock().unwrap(), &scope_key(&worker), &grant).unwrap();
    }
    ctx.update_config(|config| {
        config.project_root = Some(worker.project_root.clone());
        config.restrict_to_project_root = true;
    });
    crate::access_policy::with_scope(Some(scope_key(&worker)), || {
        assert!(ctx.validate_write_location("file-check", &path).is_ok());
        assert!(ctx
            .validate_write_location("file-check", &path.with_file_name("sibling"))
            .is_err());
        operation(json!({"path":path}), "grants.revoke", &worker, &ctx).unwrap();
        assert!(ctx.validate_write_location("file-check", &path).is_err());
    });
}

#[test]
fn grants_codemode_run_uses_agent_policy_and_revocation_without_copying() {
    let (_dir, ctx, head, worker) = fixture();
    let path = head.project_root.join("shared");
    std::fs::write(&path, "protected").unwrap();
    let grant = AccessGrant {
        path: crate::access_policy::policy_path(&path),
        kind: GrantKind::File,
        operation: AccessOperation::Write,
    };
    let mut run = identity(&worker.project_root, "run", "ephemeral", Some("worker"));
    Arc::get_mut(&mut run.0).unwrap().call_mode = subc_protocol::call_mode::CallMode::Codemode;
    register(&run, &ctx).unwrap();
    ctx.update_config(|config| {
        config.project_root = Some(worker.project_root.clone());
        config.restrict_to_project_root = true;
    });
    let worker_key = scope_key(&worker);
    let run_key = scope_key(&run);
    assert_ne!(run_key, worker_key);
    let db = ctx.db().unwrap();
    store::insert(&db.lock().unwrap(), &worker_key, &grant).unwrap();
    crate::access_policy::with_scope(Some(policy_scope_key(&run)), || {
        assert!(ctx.validate_read_path("run-check", "worker", &path).is_ok());
        operation(json!({"path":path}), "grants.revoke", &worker, &ctx).unwrap();
        assert!(ctx
            .validate_read_path("run-check", "worker", &path)
            .is_err());
    });
    let conn = db.lock().unwrap();
    assert!(store::list(&conn, &run_key).unwrap().is_empty());
    store::insert(&conn, &worker_key, &grant).unwrap();
    store::end_scope_grants(&conn, &run_key).unwrap();
    assert_eq!(store::list(&conn, &worker_key).unwrap(), vec![grant]);
}

#[test]
fn modes_unstored_head_defaults_to_full_access_but_worker_still_asks() {
    use crate::access_policy::ApprovalMode;
    let (dir, ctx, head, worker) = default_fixture();
    let outside = policy_path(&dir.path().join("outside"));
    let db = ctx.db().unwrap();
    let conn = db.lock().unwrap();
    assert_eq!(
        store::mode(&conn, &scope_key(&head)).unwrap(),
        ApprovalMode::FullAccess
    );
    assert_eq!(
        store::mode(&conn, &scope_key(&worker)).unwrap(),
        ApprovalMode::Ask
    );
    let head_policy = policy_for_scope(&conn, &scope_key(&head), &ctx)
        .unwrap()
        .unwrap();
    let worker_policy = policy_for_scope(&conn, &scope_key(&worker), &ctx)
        .unwrap()
        .unwrap();
    assert_eq!(
        head_policy.decide(AccessOperation::Read, &outside, false),
        AccessDecision::Allow
    );
    assert_eq!(
        worker_policy.decide(AccessOperation::Read, &outside, false),
        AccessDecision::Ask
    );
    assert!(matches!(
        ask_route(&head),
        crate::access_policy::AskRoute::Consent { .. }
    ));
}

#[test]
fn modes_policy_mapping_keeps_floor_and_read_only_outside_write_limit() {
    use crate::access_policy::ApprovalMode;
    let (dir, ctx, head, worker) = fixture();
    let outside = dir.path().join("outside");
    let secret = worker.project_root.join("secret");
    ctx.update_config(|config| config.sandbox.read_deny.push(secret.clone()));
    register(&worker, &ctx).unwrap();
    let db = ctx.db().unwrap();
    let conn = db.lock().unwrap();
    for mode in [
        ApprovalMode::Ask,
        ApprovalMode::FullAccess,
        ApprovalMode::ReadOnly,
    ] {
        store::set_mode(&conn, &scope_key(&head), mode).unwrap();
        let policy = policy_for_scope(&conn, &scope_key(&worker), &ctx)
            .unwrap()
            .unwrap();
        assert_eq!(policy.mode, mode);
        assert_eq!(
            policy.decide(AccessOperation::Read, &outside, false),
            if mode == ApprovalMode::FullAccess {
                AccessDecision::Allow
            } else {
                AccessDecision::Ask
            }
        );
        assert_eq!(
            policy.decide(AccessOperation::Write, &outside, false),
            match mode {
                ApprovalMode::FullAccess => AccessDecision::Allow,
                ApprovalMode::Ask => AccessDecision::Ask,
                ApprovalMode::ReadOnly => AccessDecision::Deny(AccessDenial::ReadOnly),
            }
        );
        assert_eq!(
            policy.decide(
                AccessOperation::Write,
                &worker.project_root.join("ok"),
                false
            ),
            AccessDecision::Allow
        );
        for operation in [AccessOperation::Read, AccessOperation::Write] {
            assert_eq!(
                policy.decide(operation, &secret, false),
                AccessDecision::Deny(AccessDenial::Floor)
            );
        }
    }
}

#[test]
fn modes_worker_inherits_live_head_until_plan_override_and_survives_restart() {
    use crate::access_policy::ApprovalMode;
    let (_dir, ctx, head, worker) = fixture();
    remote_policy::catalog(
        json!({"preset":"head","params":{"mode":"full-access"}}),
        &head,
        &ctx,
    )
    .unwrap();
    let head_run = identity(&head.project_root, "head-run", "ephemeral", None);
    register(&head_run, &ctx).unwrap();
    // The launching head validates a worker's plan before the worker starts.
    // That validation must not apply the worker's mode to the head itself.
    remote_policy::catalog(
        json!({"preset":"worker","params":{"mode":"read-only"}}),
        &head,
        &ctx,
    )
    .unwrap();
    {
        let db = ctx.db().unwrap();
        let conn = db.lock().unwrap();
        assert_eq!(
            store::mode(&conn, &scope_key(&head)).unwrap(),
            ApprovalMode::FullAccess
        );
        assert_eq!(
            store::mode(&conn, &scope_key(&head_run)).unwrap(),
            ApprovalMode::FullAccess
        );
        assert_eq!(
            store::mode(&conn, &scope_key(&worker)).unwrap(),
            ApprovalMode::FullAccess
        );
    }
    remote_policy::catalog(
        json!({"preset":"head","params":{"mode":"ask"}}),
        &head,
        &ctx,
    )
    .unwrap();
    {
        let db = ctx.db().unwrap();
        assert_eq!(
            store::mode(&db.lock().unwrap(), &scope_key(&worker)).unwrap(),
            ApprovalMode::Ask
        );
    }
    remote_policy::catalog(
        json!({"preset":"worker","params":{"mode":"read-only"}}),
        &worker,
        &ctx,
    )
    .unwrap();
    let storage = ctx.storage_dir();
    drop(ctx);
    let ctx = context(&worker.project_root, &storage);
    remote_policy::catalog(json!({"preset":"worker"}), &worker, &ctx).unwrap();
    let db = ctx.db().unwrap();
    let conn = db.lock().unwrap();
    assert_eq!(
        store::mode(&conn, &scope_key(&worker)).unwrap(),
        ApprovalMode::ReadOnly
    );
    let run = identity(&worker.project_root, "run", "ephemeral", Some("worker"));
    assert_eq!(
        store::mode(&conn, &policy_scope_key(&run)).unwrap(),
        ApprovalMode::ReadOnly
    );
    drop(conn);
    assert!(remote_policy::catalog(
        json!({"preset":"worker","params":{"mode":"unsafe"}}),
        &worker,
        &ctx
    )
    .is_err());
}

#[test]
fn modes_full_access_head_holds_outside_allow_paths_except_denied_floor() {
    use crate::access_policy::ApprovalMode;
    let (dir, ctx, head, worker) = default_fixture();
    let outside = dir.path().join("scratch-checkout");
    std::fs::create_dir(&outside).unwrap();
    let outside = policy_path(&outside);
    let raw = json!([{"path":outside,"mode":"write"}]);
    {
        let db = ctx.db().unwrap();
        assert_eq!(
            store::mode(&db.lock().unwrap(), &scope_key(&head)).unwrap(),
            ApprovalMode::FullAccess
        );
    }
    remote_policy::catalog(catalog_request(raw.clone()), &head, &ctx).unwrap();
    remote_policy::catalog(catalog_request(raw.clone()), &worker, &ctx).unwrap();
    {
        let db = ctx.db().unwrap();
        let conn = db.lock().unwrap();
        let grant = AccessGrant {
            path: outside.clone(),
            kind: GrantKind::Subtree,
            operation: AccessOperation::Write,
        };
        assert_eq!(
            policy_for_scope(&conn, &scope_key(&head), &ctx)
                .unwrap()
                .unwrap()
                .head_policy(&grant),
            HeadPolicy::Allow
        );
        assert_eq!(
            store::list(&conn, &scope_key(&worker)).unwrap(),
            vec![grant]
        );
    }
    ctx.update_config(|config| config.sandbox.read_deny.push(outside.join("secret")));
    register(&head, &ctx).unwrap();
    for caller in [&head, &worker] {
        let refused =
            remote_policy::catalog(catalog_request(raw.clone()), caller, &ctx).unwrap_err();
        assert_eq!(refused.detail.unwrap()["reason"], "denied_floor");
    }
}

#[cfg(windows)]
#[test]
fn grants_launch_fixture_roots_do_not_contain_windows_verbatim_glob_prefix() {
    let (_dir, _ctx, head, worker) = fixture();
    for root in [&head.project_root, &worker.project_root] {
        assert!(root.is_absolute());
        assert!(
            !root.to_string_lossy().contains('?'),
            "launch paths must be plain: {}",
            root.display()
        );
        assert_eq!(root, &policy_path(root));
    }
}

/// The launch check resolves the head's policy, which probes the filesystem.
/// It must do so with aft.db unlocked, so a slow or stalled filesystem never
/// keeps other routes (such as a flow read of the head's files) from the
/// database.
#[test]
fn grants_catalog_resolves_head_policy_with_database_unlocked() {
    let (_dir, ctx, head, worker) = fixture();
    let target = head.project_root.join("shared");
    let raw = json!([{"path":target,"mode":"read"}]);
    let db = ctx.db().unwrap();
    let probes = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let hook = {
        let probes = probes.clone();
        unlocked_hook::install(move |_| probes.borrow_mut().push(db.try_lock().is_ok()))
    };
    remote_policy::catalog(catalog_request(raw), &worker, &ctx).unwrap();
    drop(hook);
    let probes = probes.borrow();
    assert!(
        !probes.is_empty(),
        "the launch check resolved the head's policy"
    );
    assert!(
        probes.iter().all(|unlocked| *unlocked),
        "aft.db was locked during resolution: {probes:?}"
    );
    let granted = operation(json!({"scope":"worker"}), "grants.list", &head, &ctx).unwrap();
    assert_eq!(granted["grants"].as_array().unwrap().len(), 1, "{granted}");
}

/// A head's standing grant on an outside directory, which the head therefore
/// holds, and a worker launch asking to read that directory. Returns the
/// directory and the launch's `allow_paths`.
fn delegated_launch() -> (
    tempfile::TempDir,
    AppContext,
    RouteIdentity,
    RouteIdentity,
    PathBuf,
    Value,
) {
    let (dir, ctx, head, worker) = fixture();
    let outside = dir.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    let outside = policy_path(&outside);
    let raw = json!([{"path":outside,"mode":"read"}]);
    {
        let db = ctx.db().unwrap();
        let conn = db.lock().unwrap();
        store::insert(
            &conn,
            &scope_key(&head),
            &AccessGrant {
                path: outside.clone(),
                kind: GrantKind::Subtree,
                operation: AccessOperation::Read,
            },
        )
        .unwrap();
        // Against the head's policy as stored now, the launch is accepted.
        let policy = policy_for_scope(&conn, &scope_key(&head), &ctx)
            .unwrap()
            .unwrap();
        validate(&raw, Some(&policy), None).unwrap();
    }
    (dir, ctx, head, worker, outside, raw)
}

/// Installs a hook that records every unlocked step and, the first time the
/// catalog reaches `at`, runs `change` on the database as a concurrent route
/// would.
fn change_once_at(
    ctx: &AppContext,
    at: unlocked_hook::Point,
    change: impl Fn(&rusqlite::Connection) + 'static,
) -> (
    unlocked_hook::Installed,
    std::rc::Rc<std::cell::RefCell<Vec<unlocked_hook::Point>>>,
) {
    let db = ctx.db().unwrap();
    let points = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let seen = points.clone();
    let hook = unlocked_hook::install(move |point| {
        let first = !seen.borrow().contains(&point);
        seen.borrow_mut().push(point);
        if point == at && first {
            change(&db.lock().unwrap());
        }
    });
    (hook, points)
}

fn revoke_head_grant(head: &RouteIdentity, path: &Path) -> impl Fn(&rusqlite::Connection) {
    let (key, path) = (scope_key(head), path.to_path_buf());
    move |conn| assert_eq!(store::revoke(conn, &key, &path).unwrap(), 1)
}

fn worker_grants(head: &RouteIdentity, ctx: &AppContext) -> Vec<Value> {
    operation(json!({"scope":"worker"}), "grants.list", head, ctx).unwrap()["grants"]
        .as_array()
        .unwrap()
        .clone()
}

fn assert_head_not_held(error: &subc_protocol::ErrorBody, path: &Path) {
    assert_eq!(error.code, "plan_param_refused", "{error:?}");
    let detail = error.detail.as_ref().expect("refusal names the entry");
    assert_eq!(detail["reason"], "head_not_held", "{error:?}");
    assert_eq!(detail["path"], json!(path), "{error:?}");
}

/// The head stops holding the launch's directory while its policy is being
/// resolved. The launch must be judged by the head's new policy: refused, no
/// grant materialized, and the stored launch parameters never read on the
/// strength of the stale snapshot.
#[test]
fn grants_catalog_head_change_during_resolution_refuses_by_the_new_policy() {
    let (_dir, ctx, head, worker, outside, raw) = delegated_launch();
    let (hook, points) = change_once_at(
        &ctx,
        unlocked_hook::Point::Resolve,
        revoke_head_grant(&head, &outside),
    );
    let error = remote_policy::catalog(catalog_request(raw), &worker, &ctx).unwrap_err();
    drop(hook);
    assert_head_not_held(&error, &outside);
    let points = points.borrow();
    assert!(
        points
            .iter()
            .filter(|point| **point == unlocked_hook::Point::Resolve)
            .count()
            >= 2,
        "the changed policy was resolved again: {points:?}"
    );
    assert!(
        !points.contains(&unlocked_hook::Point::LaunchValidation),
        "stored launch parameters were read after a stale validation: {points:?}"
    );
    assert!(worker_grants(&head, &ctx).is_empty());
}

#[test]
fn grants_revived_launch_rechecks_head_changes_after_unlocked_resolution() {
    let (_dir, ctx, head, worker, outside, raw) = delegated_launch();
    remote_policy::catalog(catalog_request(raw), &worker, &ctx).unwrap();
    let revived = at_epoch(&worker, 8);
    let (hook, points) = change_once_at(
        &ctx,
        unlocked_hook::Point::Resolve,
        revoke_head_grant(&head, &outside),
    );
    restore_launch(&revived, &ctx, "worker").unwrap();
    drop(hook);
    let db = ctx.db().unwrap();
    assert!(
        store::list(&db.lock().unwrap(), &scope_key(&revived))
            .unwrap()
            .is_empty(),
        "the stale head snapshot must not install a revoked grant"
    );
    assert!(
        points
            .borrow()
            .iter()
            .filter(|point| **point == unlocked_hook::Point::Resolve)
            .count()
            >= 2,
        "the changed policy must be resolved again"
    );
}

/// The head stops holding the launch's directory after the launch read its
/// stored parameters and before it materializes them. Nothing may be
/// materialized against the stale snapshot; the new policy refuses.
#[test]
fn grants_catalog_head_change_before_materialize_refuses_by_the_new_policy() {
    let (_dir, ctx, head, worker, outside, raw) = delegated_launch();
    let (hook, points) = change_once_at(
        &ctx,
        unlocked_hook::Point::LaunchValidation,
        revoke_head_grant(&head, &outside),
    );
    let error = remote_policy::catalog(catalog_request(raw), &worker, &ctx).unwrap_err();
    drop(hook);
    assert_head_not_held(&error, &outside);
    let points = points.borrow();
    assert!(
        points
            .iter()
            .filter(|point| **point == unlocked_hook::Point::Resolve)
            .count()
            >= 2,
        "the changed policy was resolved again: {points:?}"
    );
    assert!(worker_grants(&head, &ctx).is_empty());
}

/// Another launch of the same worker scope stores different parameters
/// between this launch's read of them and its materialize step. This launch
/// must then validate the stored parameters, not the ones it read earlier:
/// here the stored ones name a path the head does not hold, so it refuses
/// and names that path.
#[test]
fn grants_catalog_validates_params_stored_by_a_concurrent_launch() {
    let (dir, ctx, head, worker, _outside, raw) = delegated_launch();
    let unheld = policy_path(&dir.path().join("unheld"));
    let key = launch_key(&worker, "worker").unwrap();
    let other = json!([{"path":unheld,"mode":"read"}]);
    let (hook, _points) =
        change_once_at(&ctx, unlocked_hook::Point::LaunchValidation, move |conn| {
            store::materialize(conn, &key, &other, &[]).unwrap()
        });
    let error = remote_policy::catalog(catalog_request(raw), &worker, &ctx).unwrap_err();
    drop(hook);
    assert_head_not_held(&error, &unheld);
    assert!(worker_grants(&head, &ctx).is_empty());
}
