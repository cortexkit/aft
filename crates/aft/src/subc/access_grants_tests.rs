use super::*;

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

fn worker_at_parent_epoch(root: &Path, parent: &str, epoch: u64) -> RouteIdentity {
    let mut worker = identity(root, "worker", "worker", Some(parent));
    Arc::get_mut(&mut worker.0)
        .unwrap()
        .scope
        .as_mut()
        .unwrap()
        .parent
        .as_mut()
        .unwrap()
        .scope_epoch = epoch;
    worker
}

#[test]
fn grants_head_policy_survives_an_unregistered_parent_epoch() {
    let (_dir, ctx, head, worker) = fixture();
    let worker = worker_at_parent_epoch(&worker.project_root, "head", 12);
    let target = head
        .project_root
        .join(".cortexkit")
        .join("parent-inputs")
        .join("setup-process-preview-original-r1");
    remote_policy::catalog(
        catalog_request(json!([{"path":target,"mode":"read"}])),
        &worker,
        &ctx,
    )
    .unwrap();
    let grants = operation(json!({}), "grants.list", &worker, &ctx).unwrap();
    assert_eq!(grants["grants"].as_array().unwrap().len(), 1);
    assert_eq!(grants["grants"][0]["path"], json!(target));
    assert_eq!(grants["grants"][0]["mode"], "read");
}

#[test]
fn grants_head_policy_ended_epoch_blocks_older_live_policy() {
    let (_dir, ctx, head, worker) = fixture();
    let mut ended = scope_key(&head);
    ended.epoch = 10;
    store::end_scope_grants(&ctx.db().unwrap().lock().unwrap(), &ended).unwrap();
    for epoch in [10, 12] {
        let worker = worker_at_parent_epoch(&worker.project_root, "head", epoch);
        let error = remote_policy::catalog(
            catalog_request(json!([{"path":head.project_root,"mode":"read"}])),
            &worker,
            &ctx,
        )
        .unwrap_err();
        assert_eq!(error.detail.unwrap()["reason"], "head_policy_unknown");
    }
}

#[test]
fn grants_head_policy_does_not_cross_scope_refs() {
    let (_dir, ctx, head, worker) = fixture();
    let worker = worker_at_parent_epoch(&worker.project_root, "different-head", 12);
    let error = remote_policy::catalog(
        catalog_request(json!([{"path":head.project_root,"mode":"read"}])),
        &worker,
        &ctx,
    )
    .unwrap_err();
    assert_eq!(error.detail.unwrap()["reason"], "head_policy_unknown");
}

#[test]
fn grants_head_policy_does_not_cross_owners_or_read_future_epochs() {
    let (_dir, ctx, head, worker) = fixture();
    let mut other_owner = worker_at_parent_epoch(&worker.project_root, "head", 12);
    Arc::get_mut(&mut other_owner.0)
        .unwrap()
        .scope
        .as_mut()
        .unwrap()
        .parent
        .as_mut()
        .unwrap()
        .owner =
        serde_json::from_value(json!({"kind":"reserved","module_id":"another-owner"})).unwrap();
    let earlier = worker_at_parent_epoch(&worker.project_root, "head", 6);
    for caller in [&other_owner, &earlier] {
        let error = remote_policy::catalog(
            catalog_request(json!([{"path":head.project_root,"mode":"read"}])),
            caller,
            &ctx,
        )
        .unwrap_err();
        assert_eq!(error.detail.unwrap()["reason"], "head_policy_unknown");
    }
}

#[test]
fn grants_head_policy_prefers_the_latest_restrictions() {
    let (_dir, ctx, head, worker) = fixture();
    let mut newer_head = identity(&worker.project_root, "head", "head", None);
    Arc::get_mut(&mut newer_head.0)
        .unwrap()
        .scope
        .as_mut()
        .unwrap()
        .scope_epoch = 10;
    register(&newer_head, &ctx).unwrap();
    store::set_mode(
        &ctx.db().unwrap().lock().unwrap(),
        &scope_key(&newer_head),
        crate::access_policy::ApprovalMode::Ask,
    )
    .unwrap();
    let worker = worker_at_parent_epoch(&worker.project_root, "head", 12);
    let error = remote_policy::catalog(
        catalog_request(json!([{"path":head.project_root,"mode":"read"}])),
        &worker,
        &ctx,
    )
    .unwrap_err();
    assert_eq!(error.detail.unwrap()["reason"], "head_not_held");
}

#[test]
fn grants_head_policy_rechecks_revocation_at_source_epoch() {
    let (_dir, ctx, head, worker) = fixture();
    let worker = worker_at_parent_epoch(&worker.project_root, "head", 12);
    let target = worker.project_root.join("shared");
    store::insert(
        &ctx.db().unwrap().lock().unwrap(),
        &scope_key(&head),
        &AccessGrant {
            path: target.clone(),
            kind: GrantKind::Subtree,
            operation: AccessOperation::Read,
        },
    )
    .unwrap();
    remote_policy::catalog(
        catalog_request(json!([{"path":target,"mode":"read"}])),
        &worker,
        &ctx,
    )
    .unwrap();
    operation(json!({"path":target}), "grants.revoke", &head, &ctx).unwrap();
    let error = remote_policy::catalog(json!({"preset":"worker"}), &worker, &ctx).unwrap_err();
    assert_eq!(error.detail.unwrap()["reason"], "head_not_held");
}

#[test]
fn grants_head_policy_missing_is_unknown_not_not_held() {
    let (_dir, _ctx, head, _worker) = fixture();
    let error = validate(
        &json!([{"path":head.project_root,"mode":"read"}]),
        None,
        None,
    )
    .unwrap_err();
    assert_eq!(error.code, "plan_param_refused");
    assert_eq!(error.detail.unwrap()["reason"], "head_policy_unknown");
}

#[test]
fn grants_allow_paths_refusals_log_entry_and_head_at_info() {
    const CHILD: &str = "AFT_TEST_ALLOW_PATHS_LOG_CHILD";
    if std::env::var_os(CHILD).is_none() {
        // A separate test process keeps the global logger independent of other tests.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "subc::access_grants::tests::grants_allow_paths_refusals_log_entry_and_head_at_info",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    static LINES: StdMutex<Vec<String>> = StdMutex::new(Vec::new());
    struct Logger;
    impl log::Log for Logger {
        fn enabled(&self, _: &log::Metadata<'_>) -> bool {
            true
        }
        fn log(&self, record: &log::Record<'_>) {
            LINES
                .lock()
                .unwrap()
                .push(format!("{} {}", record.level(), record.args()));
        }
        fn flush(&self) {}
    }
    log::set_logger(&Logger).unwrap();
    log::set_max_level(log::LevelFilter::Info);
    let (_dir, ctx, head, worker) = fixture();
    let unknown = worker_at_parent_epoch(&worker.project_root, "missing-head", 12);
    for (caller, raw, index, reason, path, head_ref, epoch) in [
        (
            &head,
            json!([{"path":head.project_root,"mode":"read"},{"path":"relative","mode":"read"}]),
            1,
            "not_absolute",
            json!("relative"),
            "head",
            7,
        ),
        (
            &unknown,
            json!([{"path":head.project_root,"mode":"read"}]),
            0,
            "head_policy_unknown",
            json!(head.project_root),
            "missing-head",
            12,
        ),
        (&head, json!({}), 0, "malformed", Value::Null, "head", 7),
    ] {
        remote_policy::catalog(catalog_request(raw), caller, &ctx).unwrap_err();
        let lines: Vec<_> = LINES
            .lock()
            .unwrap()
            .drain(..)
            .filter(|line| line.contains("allow_paths refused:"))
            .collect();
        assert_eq!(lines.len(), 1, "{lines:?}");
        let line = &lines[0];
        for expected in [
            "INFO allow_paths refused:".to_owned(),
            format!("index={index}"),
            format!("reason={reason}"),
            format!("path={path}"),
            "prefrontal-core".to_owned(),
            format!("scope_ref: \"{head_ref}\""),
            format!("epoch: {epoch}"),
        ] {
            assert!(line.contains(&expected), "missing {expected:?} in {line}");
        }
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
