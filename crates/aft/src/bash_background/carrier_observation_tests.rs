use super::*;
use crate::bash_background::persistence::create_task_layout;
use crate::{context::AppContext, protocol::RawRequest};

fn request(task: &str, session: &str) -> RawRequest {
    serde_json::from_value(serde_json::json!({
        "id":"observe", "command":"bash_status", "session_id":session,
        "worker_session":true, "params":{"task_id":task}
    }))
    .unwrap()
}

#[test]
fn carrier_status_never_renews_a_registered_workers_hard_kill() {
    let _storage_env = crate::test_env::without_storage_override();
    let (_dir, ctx, task) = default_kill_tests::wait_fixture(None);
    let before = ctx
        .bash_background()
        .hard_kill_deadline(&task.task_id, &task.session_id)
        .unwrap()
        .limit_ms;
    let req = request(&task.task_id, &task.session_id);
    for _ in 0..2 {
        let response = crate::commands::bash_status::handle_with_observation_only(&req, &ctx, true);
        assert!(response.success, "{:?}", response.data);
    }
    assert_eq!(
        ctx.bash_background()
            .hard_kill_deadline(&task.task_id, &task.session_id)
            .unwrap()
            .limit_ms,
        before
    );
    let response = crate::commands::bash_status::handle(&req, &ctx);
    assert!(response.success);
    assert!(
        ctx.bash_background()
            .hard_kill_deadline(&task.task_id, &task.session_id)
            .unwrap()
            .limit_ms
            > before,
        "ordinary worker status is the renewal positive control"
    );
}

#[test]
fn carrier_status_hides_erased_watch_tombstones_while_agent_keeps_task_erased() {
    let _storage_env = crate::test_env::without_storage_override();
    let dir = tempfile::tempdir().unwrap();
    let ctx = AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            storage_dir: Some(dir.path().into()),
            project_root: Some(dir.path().into()),
            ..Default::default()
        },
    );
    let db = Arc::new(Mutex::new(
        crate::db::open(&dir.path().join("aft.db")).unwrap(),
    ));
    ctx.set_db(db.clone());
    let registry = ctx.bash_background();
    registry.set_db_pool(db.clone());
    registry.set_harness(crate::harness::Harness::Opencode);
    let (id, _task) = tests::insert_terminal_piped_task(registry, &dir, "true", "done", "", false);
    registry
        .inner
        .watch_registry
        .lock()
        .unwrap()
        .register(id.clone(), WatchPattern::Substring("NEVER".into()), false)
        .unwrap();
    crate::db::bash_tasks::delete_bash_task(&db.lock().unwrap(), "opencode", "session", &id)
        .unwrap();
    assert!(registry.has_erased_watch_reference(&id));
    let req = request(&id, "session");
    let agent = crate::commands::bash_status::handle(&req, &ctx);
    assert_eq!(agent.data["code"], "task_erased");
    let carrier = crate::commands::bash_status::handle_with_observation_only(&req, &ctx, true);
    assert_eq!(carrier.data["code"], "task_not_found");
}

#[test]
fn carrier_status_skips_head_worker_and_unscoped_project_fallbacks() {
    let _storage_env = crate::test_env::without_storage_override();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let ctx = AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(root.clone()),
            storage_dir: Some(root.clone()),
            restrict_to_project_root: false,
            ..Default::default()
        },
    );
    let db = Arc::new(Mutex::new(crate::db::open(&root.join("aft.db")).unwrap()));
    ctx.set_db(db.clone());
    ctx.bash_background().set_db_pool(db.clone());
    ctx.bash_background()
        .set_harness(crate::harness::Harness::Opencode);
    let head: subc_protocol::scope::ScopeStamp = serde_json::from_value(serde_json::json!({
        "owner":{"kind":"reserved","module_id":"prefrontal-core"},"ref":"head", "scope_epoch":1,
        "kind":"head","owner_authorized":true
    }))
    .unwrap();
    let head = crate::db::access_grants::ScopeKey::from_stamp(&head);
    crate::db::access_grants::register_scope(&db.lock().unwrap(), &head, None, &ctx.config())
        .unwrap();
    crate::db::access_grants::register_agent(&db.lock().unwrap(), &head, "head-agent", true)
        .unwrap();
    let layout = create_task_layout(&root, "worker-session", "bash-0000000000000091").unwrap();
    let mut metadata = crate::db::rich_ui::terminal_task_metadata_for_test(None);
    metadata.task_id = layout.paths.task_id.clone();
    metadata.session_id = "worker-session".into();
    metadata.project_root = Some(root.clone());
    metadata.workdir = root.clone();
    metadata.completion_delivered = true;
    metadata.scope = Some(serde_json::from_value(serde_json::json!({
        "owner":{"kind":"reserved","module_id":"prefrontal-core"},"ref":"worker","scope_epoch":1,
        "kind":"worker","owner_authorized":true,
        "parent":{"owner":{"kind":"reserved","module_id":"prefrontal-core"},"ref":"head","scope_epoch":1}
    })).unwrap());
    write_task_at(&layout, &metadata).unwrap();
    fs::write(&layout.paths.stdout, "worker-output").unwrap();
    fs::write(&layout.paths.stderr, "").unwrap();
    let row = metadata
        .to_bash_task_row("opencode", &layout.paths)
        .unwrap();
    crate::db::rich_ui::upsert_task_row_with_view(&db.lock().unwrap(), &row, &metadata).unwrap();
    for scope in [Some(head), None] {
        if scope.is_none() {
            // Relaxed project lookup applies only to unstamped tasks, not to
            // the scoped worker used by the head's positive control above.
            let plain =
                create_task_layout(&root, "other-session", "bash-0000000000000092").unwrap();
            metadata.task_id = plain.paths.task_id.clone();
            metadata.session_id = "other-session".into();
            metadata.scope = None;
            write_task_at(&plain, &metadata).unwrap();
            fs::write(&plain.paths.stdout, "project-output").unwrap();
            fs::write(&plain.paths.stderr, "").unwrap();
            let row = metadata.to_bash_task_row("opencode", &plain.paths).unwrap();
            crate::db::rich_ui::upsert_task_row_with_view(&db.lock().unwrap(), &row, &metadata)
                .unwrap();
        }
        let req = request(&metadata.task_id, "head-session");
        crate::access_policy::with_scope(scope, || {
            let agent = crate::commands::bash_status::handle(&req, &ctx);
            assert!(
                agent.success,
                "ordinary head/project fallback must read worker (scoped={}): {:?}",
                crate::access_policy::current_scope().is_some(),
                agent.data
            );
            let carrier =
                crate::commands::bash_status::handle_with_observation_only(&req, &ctx, true);
            assert_eq!(carrier.data["code"], "task_not_found");
        });
    }
}
