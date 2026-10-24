use super::*;
use crate::bash_background::persistence::{
    create_task_layout, write_task, PersistedTask, TaskPaths,
};
use crate::bash_background::BgTaskStatus;
use crate::db::access_grants::{self as scopes, ScopeKey};
use std::sync::{Arc, Mutex};

struct Fixture {
    _dir: tempfile::TempDir,
    ctx: AppContext,
    head: ScopeKey,
    worker: TaskPaths,
    // Declared last so it drops after the context: `AFT_STORAGE_DIR` stays
    // cleared for the whole fixture lifetime.
    _storage_env: crate::test_env::StorageOverrideGuard,
}

fn stamp(
    name: &str,
    kind: &str,
    epoch: u64,
    parent: Option<&str>,
) -> subc_protocol::scope::ScopeStamp {
    serde_json::from_value(json!({
        "owner": {"kind": "reserved", "module_id": "prefrontal-core"},
        "ref": name, "kind": kind, "scope_epoch": epoch, "owner_authorized": true,
        "parent": parent.map(|name| json!({
            "owner": {"kind": "reserved", "module_id": "prefrontal-core"},
            "ref": name, "scope_epoch": epoch
        }))
    }))
    .unwrap()
}

fn record(
    ctx: &AppContext,
    session: &str,
    id: &str,
    scope: subc_protocol::scope::ScopeStamp,
) -> TaskPaths {
    let config = ctx.config();
    let root = config.project_root.clone().unwrap();
    let paths = create_task_layout(config.storage_dir.as_ref().unwrap(), session, id)
        .unwrap()
        .paths;
    let mut metadata = PersistedTask::starting(
        id.into(),
        session.into(),
        "printf worker-output".into(),
        root.clone(),
        Some(root),
        None,
        false,
        false,
    );
    metadata.scope = Some(scope);
    metadata.mark_terminal(BgTaskStatus::Completed, Some(0), None);
    metadata.completion_delivered = true;
    write_task(&paths.json, &metadata).unwrap();
    crate::db::bash_tasks::upsert_bash_task(
        &ctx.db().unwrap().lock().unwrap(),
        &metadata.to_bash_task_row("opencode", &paths).unwrap(),
    )
    .unwrap();
    std::fs::write(&paths.command, "printf worker-output\n").unwrap();
    std::fs::write(&paths.environment, "SECRET_CREDENTIAL=hidden-environment\n").unwrap();
    std::fs::write(&paths.stdout, "worker-output\n").unwrap();
    std::fs::write(&paths.stderr, "").unwrap();
    std::fs::write(&paths.exit, "0\n").unwrap();
    paths
}

fn fixture() -> Fixture {
    // The context resolves its storage folder on every call, and
    // `AFT_STORAGE_DIR` outranks the configured one below. Other tests in this
    // process set that variable while they run; if it changed under this
    // fixture, the storage floor would protect that other folder instead of
    // these task records, and a refusal the tests expect would become a
    // success. Clear it, and hold the process environment lock so no other
    // test can set it again, until the fixture is dropped.
    let storage_env = crate::test_env::without_storage_override();
    let dir = tempfile::tempdir().unwrap();
    // Grep opens resolved task paths. Keep the fixture and its open probes in
    // that same path form, including Windows prefixes and macOS temp aliases.
    let resolved = dir.path().canonicalize().unwrap();
    let root = resolved.join("project");
    let storage = resolved.join("store");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&storage).unwrap();
    let ctx = AppContext::new(
        Box::new(StubProvider),
        Config {
            project_root: Some(root),
            storage_dir: Some(storage.clone()),
            restrict_to_project_root: false,
            ..Config::default()
        },
    );
    let db = Arc::new(Mutex::new(
        crate::db::open(&storage.join("aft.db")).unwrap(),
    ));
    ctx.set_db(Arc::clone(&db));
    ctx.bash_background().set_db_pool(db);
    ctx.bash_background()
        .set_harness(crate::harness::Harness::Opencode);
    let head = ScopeKey::from_stamp(&stamp("head", "head", 9, None));
    {
        let db = ctx.db().unwrap();
        let conn = db.lock().unwrap();
        scopes::register_scope(&conn, &head, None, &ctx.config()).unwrap();
        scopes::register_agent(&conn, &head, "head-agent", true).unwrap();
    }
    let worker = record(
        &ctx,
        "worker-session",
        "bash-0123456789abcdef",
        stamp("worker", "worker", 1, Some("head")),
    );
    Fixture {
        _dir: dir,
        ctx,
        head,
        worker,
        _storage_env: storage_env,
    }
}

fn read_as(ctx: &AppContext, session: &str, path: &Path) -> crate::protocol::Response {
    let mut req = request("read", json!({"file": path}));
    req.session_id = Some(session.into());
    crate::commands::read::handle_read(&req, ctx)
}

fn assert_floor(response: crate::protocol::Response, path: &Path) {
    assert!(!response.success, "{:?}", response.data);
    assert_eq!(response.data["code"], "denied_floor", "{:?}", response.data);
    assert_eq!(
        response.data["message"],
        format!(
            "path '{}' is denied by the access policy floor",
            path.display()
        )
    );
}

#[test]
fn access_policy_head_reads_worker_task_records() {
    let f = fixture();
    crate::access_policy::with_scope(Some(f.head.clone()), || {
        for (path, expected) in [
            (&f.worker.command, "printf worker-output"),
            (&f.worker.json, "worker-session"),
            (&f.worker.stdout, "worker-output"),
            (&f.worker.exit, "0"),
        ] {
            let response = read_as(&f.ctx, "head-session", path);
            assert!(response.success, "{}: {:?}", path.display(), response.data);
            assert!(
                response.data.to_string().contains(expected),
                "{:?}",
                response.data
            );
        }
    });
}

#[test]
fn access_policy_ended_head_loses_cached_worker_task_access() {
    let f = fixture();
    crate::access_policy::with_scope(Some(f.head.clone()), || {
        assert!(read_as(&f.ctx, "head-session", &f.worker.command).success);
        scopes::end_scope_grants(&f.ctx.db().unwrap().lock().unwrap(), &f.head).unwrap();
        assert_floor(
            read_as(&f.ctx, "head-session", &f.worker.command),
            &f.worker.command,
        );
    });
}

#[test]
fn access_policy_replaced_head_registration_loses_cached_worker_task_access() {
    let f = fixture();
    crate::access_policy::with_scope(Some(f.head.clone()), || {
        assert!(read_as(&f.ctx, "head-session", &f.worker.command).success);
        scopes::register_agent(
            &f.ctx.db().unwrap().lock().unwrap(),
            &f.head,
            "head-agent",
            false,
        )
        .unwrap();
        assert_floor(
            read_as(&f.ctx, "head-session", &f.worker.command),
            &f.worker.command,
        );
    });
}

#[test]
fn access_policy_head_reads_revived_worker_task_records() {
    let f = fixture();
    let revived = record(
        &f.ctx,
        "revived-session",
        "bash-1123456789abcdef",
        stamp("worker", "worker", 8, Some("head")),
    );
    crate::access_policy::with_scope(Some(f.head.clone()), || {
        for path in [&f.worker.command, &revived.command] {
            let response = read_as(&f.ctx, "head-session", path);
            assert!(response.success, "{:?}", response.data);
        }
        let mut req = request("bash_status", json!({"task_id": revived.task_id}));
        req.session_id = Some("head-session".into());
        let response = crate::commands::bash_status::handle(&req, &f.ctx);
        assert!(response.success, "{:?}", response.data);
    });
}

#[test]
fn access_policy_other_head_cannot_read_worker_task_records() {
    let f = fixture();
    let mut other = f.head.clone();
    other.scope_ref = "other-head".into();
    {
        let db = f.ctx.db().unwrap();
        let conn = db.lock().unwrap();
        scopes::register_scope(&conn, &other, None, &f.ctx.config()).unwrap();
        scopes::register_agent(&conn, &other, "other-agent", true).unwrap();
    }
    crate::access_policy::with_scope(Some(other), || {
        assert_floor(
            read_as(&f.ctx, "other-head-session", &f.worker.command),
            &f.worker.command,
        )
    });
    let mut other_owner = f.head.clone();
    other_owner.owner = "another-owner".into();
    {
        let db = f.ctx.db().unwrap();
        let conn = db.lock().unwrap();
        scopes::register_scope(&conn, &other_owner, None, &f.ctx.config()).unwrap();
        scopes::register_agent(&conn, &other_owner, "foreign-agent", true).unwrap();
    }
    crate::access_policy::with_scope(Some(other_owner), || {
        assert_floor(
            read_as(&f.ctx, "other-head-session", &f.worker.stdout),
            &f.worker.stdout,
        )
    });
    assert_floor(
        read_as(&f.ctx, "unscoped-session", &f.worker.stdout),
        &f.worker.stdout,
    );
}

#[test]
fn access_policy_worker_cannot_read_sibling_or_head_task_records() {
    let f = fixture();
    let sibling = ScopeKey::from_stamp(&stamp("sibling", "worker", 1, Some("head")));
    let head_task = record(
        &f.ctx,
        "head-session",
        "bash-2123456789abcdef",
        stamp("head", "head", 9, None),
    );
    crate::access_policy::with_scope(Some(sibling), || {
        assert_floor(
            read_as(&f.ctx, "sibling-session", &f.worker.stdout),
            &f.worker.stdout,
        );
        assert_floor(
            read_as(&f.ctx, "sibling-session", &head_task.command),
            &head_task.command,
        );
    });
}

#[test]
fn access_policy_head_cannot_read_task_environment() {
    let f = fixture();
    crate::access_policy::with_scope(Some(f.head.clone()), || {
        assert_floor(
            read_as(&f.ctx, "head-session", &f.worker.environment),
            &f.worker.environment,
        )
    });
}

#[test]
fn access_policy_session_cannot_read_task_environment() {
    let f = fixture();
    assert_floor(
        read_as(&f.ctx, "worker-session", &f.worker.environment),
        &f.worker.environment,
    );
    f.ctx
        .update_config(|config| config.project_root = Some(f.worker.dir.clone()));
    assert_floor(
        read_as(&f.ctx, "worker-session", &f.worker.environment),
        &f.worker.environment,
    );
}

#[test]
fn access_policy_indexed_task_entries_exclude_environment() {
    let f = fixture();
    let filters = crate::search_index::PathFilters::default();
    assert!(filters.matches(&f.worker.dir, &f.worker.command));
    assert!(!filters.matches(&f.worker.dir, &f.worker.environment));
}

#[test]
fn access_policy_head_cannot_write_worker_task_records() {
    let f = fixture();
    crate::access_policy::with_scope(Some(f.head.clone()), || {
        let mut req = request(
            "write",
            json!({"file": f.worker.command, "content": "changed"}),
        );
        req.session_id = Some("head-session".into());
        assert_floor(
            crate::commands::write::handle_write(&req, &f.ctx),
            &f.worker.command,
        );
    });
    assert_eq!(
        std::fs::read_to_string(&f.worker.command).unwrap(),
        "printf worker-output\n"
    );
}

#[test]
fn access_policy_own_task_io_folder_grep_and_glob() {
    let f = fixture();
    let mut req = request(
        "grep",
        json!({"path": f.worker.io_dir, "pattern": "worker-output"}),
    );
    req.session_id = Some("worker-session".into());
    let response = crate::commands::grep::handle_grep(&req, &f.ctx);
    assert!(response.success, "{:?}", response.data);
    assert_eq!(response.data["total_matches"], 1, "{:?}", response.data);
    req.command = "glob".into();
    req.params = json!({"path": f.worker.io_dir, "pattern": "*"});
    let response = crate::commands::glob::handle_glob(&req, &f.ctx);
    assert!(response.success, "{:?}", response.data);
    assert!(
        response.data.to_string().contains("stdout"),
        "{:?}",
        response.data
    );
    assert_floor(
        read_as(&f.ctx, "worker-session", &f.worker.command),
        &f.worker.command,
    );
}

#[test]
fn explicit_task_io_grep_opens_only_registered_nonsecret_artifacts() {
    let f = fixture();
    for name in ["fixture.db", "fixture.db-shm", "unregistered.txt"] {
        std::fs::write(f.worker.io_dir.join(name), "worker-output\n").unwrap();
    }
    let probe = crate::project_content::Probe::new(&f.worker.io_dir);
    for (session, scope) in [
        ("worker-session", None),
        ("head-session", Some(f.head.clone())),
    ] {
        crate::access_policy::with_scope(scope, || {
            let mut req = request(
                "grep",
                json!({"path": f.worker.io_dir, "pattern": "worker-output"}),
            );
            req.session_id = Some(session.into());
            let response = crate::commands::grep::handle_grep(&req, &f.ctx);
            assert!(response.success, "{:?}", response.data);
            assert_eq!(response.data["total_matches"], 1, "{:?}", response.data);
        });
    }
    let mut expected = vec![
        f.worker.stdout.clone(),
        f.worker.stderr.clone(),
        f.worker.exit.clone(),
    ];
    expected.sort();
    let mut opened = probe.paths();
    opened.sort();
    opened.dedup();
    assert_eq!(opened, expected);
    let mut req = request("glob", json!({"path": f.worker.io_dir, "pattern": "*"}));
    req.session_id = Some("worker-session".into());
    let response = crate::commands::glob::handle_glob(&req, &f.ctx);
    assert!(response.success, "{:?}", response.data);
    assert_eq!(response.data["total"], 3, "{:?}", response.data);
    let mut req = request(
        "grep",
        json!({"path": f.worker.io_dir, "pattern": "worker-output"}),
    );
    req.session_id = Some("unrelated-session".into());
    assert_floor(
        crate::commands::grep::handle_grep(&req, &f.ctx),
        &f.worker.io_dir,
    );
    let mut opened = probe.paths();
    opened.sort();
    opened.dedup();
    assert_eq!(opened, expected);
}

// `AFT_STORAGE_DIR` outranks a configured storage folder, and other tests in
// this process set it while they run. If it were set while a fixture is alive,
// the storage floor would cover that other folder instead of the fixture's task
// records, and the unrelated session's grep below would succeed instead of
// being refused. Holding the variable set here shows the fixture clears it.
#[test]
fn fixture_floor_ignores_a_storage_override_set_by_another_test() {
    let _env = crate::test_env::process_env_lock();
    struct Restore(Option<std::ffi::OsString>);
    impl Drop for Restore {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => std::env::set_var("AFT_STORAGE_DIR", value),
                None => std::env::remove_var("AFT_STORAGE_DIR"),
            }
        }
    }
    let _restore = Restore(std::env::var_os("AFT_STORAGE_DIR"));
    let other_storage = tempfile::tempdir().unwrap();
    std::env::set_var("AFT_STORAGE_DIR", other_storage.path());
    let f = fixture();
    let mut req = request(
        "grep",
        json!({"path": f.worker.io_dir, "pattern": "worker-output"}),
    );
    req.session_id = Some("unrelated-session".into());
    assert_floor(
        crate::commands::grep::handle_grep(&req, &f.ctx),
        &f.worker.io_dir,
    );
}

#[test]
fn explicit_task_io_grep_rejects_registered_sqlite_names() {
    let f = fixture();
    std::fs::remove_file(&f.worker.stdout).unwrap();
    // A registered artifact name is not permission to read a live database,
    // even on platforms without device/inode identities or a SQLite suffix.
    let _connection = crate::db::file_identity::IdentityConnection::new(
        rusqlite::Connection::open(&f.worker.stdout).unwrap(),
        "task artifact test",
    );
    let probe = crate::project_content::Probe::new(&f.worker.io_dir);
    let mut req = request(
        "grep",
        json!({"path": f.worker.io_dir, "pattern": "worker-output"}),
    );
    req.session_id = Some("worker-session".into());
    let response = crate::commands::grep::handle_grep(&req, &f.ctx);
    assert!(response.success, "{:?}", response.data);
    assert_eq!(response.data["total_matches"], 0, "{:?}", response.data);
    let mut opened = probe.paths();
    opened.sort();
    opened.dedup();
    let mut expected = vec![f.worker.stderr, f.worker.exit];
    expected.sort();
    assert_eq!(opened, expected);
}

// Hard-link alias checks need Unix device/inode identities. The all-platform
// registered-name test above covers Windows' name-based refusal instead.
#[cfg(unix)]
#[test]
fn explicit_task_io_grep_rejects_stdout_alias_to_live_sqlite() {
    let f = fixture();
    std::fs::remove_file(&f.worker.stdout).unwrap();
    std::fs::hard_link(
        f.ctx.config().storage_dir.as_ref().unwrap().join("aft.db"),
        &f.worker.stdout,
    )
    .unwrap();
    let probe = crate::project_content::Probe::new(&f.worker.io_dir);
    let mut req = request(
        "grep",
        json!({"path": f.worker.io_dir, "pattern": "worker-output"}),
    );
    req.session_id = Some("worker-session".into());
    let response = crate::commands::grep::handle_grep(&req, &f.ctx);
    assert!(response.success, "{:?}", response.data);
    assert_eq!(response.data["total_matches"], 0, "{:?}", response.data);
    let mut opened = probe.paths();
    opened.sort();
    opened.dedup();
    let mut expected = vec![f.worker.stderr, f.worker.exit];
    expected.sort();
    assert_eq!(opened, expected);
}

#[test]
fn explicit_task_artifacts_keep_filters_and_do_not_widen_project_walks() {
    let f = fixture();
    std::fs::write(&f.worker.stderr, "stderr-output\n").unwrap();
    let mut req = request(
        "grep",
        json!({"path": f.worker.io_dir, "pattern": "output", "include": ["stderr"]}),
    );
    req.session_id = Some("worker-session".into());
    let response = crate::commands::grep::handle_grep(&req, &f.ctx);
    assert!(response.success, "{:?}", response.data);
    assert_eq!(response.data["total_matches"], 1, "{:?}", response.data);
    assert!(response.data["matches"][0]["file"]
        .as_str()
        .unwrap()
        .ends_with("stderr"));
    f.ctx
        .update_config(|config| config.project_root = Some(f._dir.path().to_path_buf()));
    req.params = json!({"pattern": "worker-output|stderr-output"});
    let response = crate::commands::grep::handle_grep(&req, &f.ctx);
    assert!(response.success, "{:?}", response.data);
    assert_eq!(response.data["total_matches"], 0, "{:?}", response.data);
}

#[test]
fn access_policy_head_status_is_read_only_for_worker_tasks() {
    let f = fixture();
    crate::access_policy::with_scope(Some(f.head.clone()), || {
        let mut req = request(
            "bash_status",
            json!({"task_id": f.worker.task_id, "output_offset": 0}),
        );
        req.session_id = Some("head-session".into());
        let status = crate::commands::bash_status::handle(&req, &f.ctx);
        assert!(status.success, "{:?}", status.data);
        assert_eq!(status.data["exit_code"], 0);
        assert_eq!(status.data["output_chunk_base64"], "d29ya2VyLW91dHB1dAo=");
        req.command = "bash_kill".into();
        let kill = crate::commands::bash_kill::handle(&req, &f.ctx);
        assert_eq!(kill.data["code"], "task_not_found", "{:?}", kill.data);
    });
}

#[test]
fn access_policy_head_recursive_task_reads_exclude_environment() {
    let f = fixture();
    crate::access_policy::with_scope(Some(f.head.clone()), || {
        let mut req = request(
            "grep",
            json!({"path": f.worker.dir, "pattern": "worker-output|hidden-environment"}),
        );
        req.session_id = Some("head-session".into());
        let response = crate::commands::grep::handle_grep(&req, &f.ctx);
        assert!(response.success, "{:?}", response.data);
        assert_eq!(response.data["total_matches"], 3, "{:?}", response.data);
        assert!(!response.data.to_string().contains("hidden-environment"));
        req.command = "glob".into();
        req.params = json!({"path": f.worker.dir, "pattern": "**/*"});
        let response = crate::commands::glob::handle_glob(&req, &f.ctx);
        assert!(response.success, "{:?}", response.data);
        assert!(response.data.to_string().contains("metadata.json"));
        assert!(!response.data.to_string().contains("environment.bin"));
    });
}

#[test]
fn access_policy_foreign_worker_status_and_controls_are_denied() {
    let f = fixture();
    for session in ["other-head-session", "sibling-session", "unscoped-session"] {
        let mut req = request("bash_status", json!({"task_id": f.worker.task_id}));
        req.session_id = Some(session.into());
        let response = crate::commands::bash_status::handle(&req, &f.ctx);
        assert_eq!(
            response.data["code"], "task_not_found",
            "{:?}",
            response.data
        );
    }
    crate::access_policy::with_scope(Some(f.head.clone()), || {
        let mut req = request(
            "bash_write",
            json!({"task_id": f.worker.task_id, "input": "x"}),
        );
        req.session_id = Some("head-session".into());
        let response = crate::commands::bash_write::handle(&req, &f.ctx);
        assert_eq!(
            response.data["code"], "task_not_found",
            "{:?}",
            response.data
        );
    });
}

#[test]
fn access_policy_task_folder_cache_is_identity_checked_and_shared_between_files() {
    let f = fixture();
    crate::access_policy::with_scope(Some(f.head.clone()), || {
        assert!(read_as(&f.ctx, "head-session", &f.worker.command).success);
        std::fs::write(&f.worker.json, "invalid metadata").unwrap();
        assert!(read_as(&f.ctx, "head-session", &f.worker.stdout).success);
        // Cached permissions retain identities, not handles. Renaming is allowed,
        // but a new directory at the same path must not inherit those permissions.
        std::fs::rename(&f.worker.dir, f.worker.dir.with_extension("old")).unwrap();
        std::fs::create_dir_all(&f.worker.control_dir).unwrap();
        std::fs::create_dir_all(&f.worker.io_dir).unwrap();
        std::fs::write(&f.worker.command, "replacement").unwrap();
        assert_floor(
            read_as(&f.ctx, "head-session", &f.worker.command),
            &f.worker.command,
        );
    });
}

#[test]
fn access_policy_head_status_reads_worker_task_from_another_project() {
    let f = fixture();
    let mut metadata = crate::bash_background::persistence::read_task(&f.worker.json).unwrap();
    metadata.project_root = Some(f._dir.path().join("worker-project"));
    std::fs::create_dir_all(metadata.project_root.as_ref().unwrap()).unwrap();
    write_task(&f.worker.json, &metadata).unwrap();
    let mut req = request(
        "bash_status",
        json!({"task_id": f.worker.task_id, "output_offset": 0, "stderr_offset": 0}),
    );
    req.session_id = Some("head-session".into());
    let head = crate::access_policy::with_scope(Some(f.head.clone()), || {
        crate::commands::bash_status::handle(&req, &f.ctx)
    });
    assert!(head.success, "{:?}", head.data);
    f.ctx
        .update_config(|config| config.project_root = metadata.project_root.clone());
    req.session_id = Some("worker-session".into());
    let worker = crate::commands::bash_status::handle(&req, &f.ctx);
    assert!(worker.success, "{:?}", worker.data);
    assert_eq!(head.data, worker.data);
}

#[cfg(unix)]
#[test]
fn access_policy_head_cannot_control_live_worker_task() {
    let f = fixture();
    let config = f.ctx.config();
    let registry = f.ctx.bash_background();
    let task = crate::bash_background::registry::with_scope_spawn(
        Some(stamp("worker", "worker", 1, Some("head"))),
        || {
            registry
                .spawn(
                    crate::sandbox_spawn::SpawnPlan::Unsandboxed,
                    "exec sleep 30",
                    "worker-session".into(),
                    config.project_root.clone().unwrap(),
                    std::collections::HashMap::new(),
                    crate::bash_background::HardKill::After(std::time::Duration::from_secs(30)),
                    None,
                    config.storage_dir.clone().unwrap(),
                    10,
                    true,
                    false,
                    config.project_root.clone(),
                )
                .unwrap()
        },
    );
    struct Stop(crate::bash_background::BgTaskRegistry, String);
    impl Drop for Stop {
        fn drop(&mut self) {
            let _ = self.0.kill(&self.1, "worker-session");
        }
    }
    let _stop = Stop(registry.clone(), task.clone());
    let mut req = request("bash_status", json!({"task_id": task}));
    req.session_id = Some("head-session".into());
    crate::access_policy::with_scope(Some(f.head.clone()), || {
        let status = crate::commands::bash_status::handle(&req, &f.ctx);
        assert!(status.success, "{:?}", status.data);
        assert_eq!(status.data["status"], "running");
        req.command = "bash_kill".into();
        let kill = crate::commands::bash_kill::handle(&req, &f.ctx);
        assert_eq!(kill.data["code"], "task_not_found", "{:?}", kill.data);
    });
    assert_eq!(
        registry
            .status(&task, "worker-session", None, None, 0)
            .unwrap()
            .info
            .status,
        BgTaskStatus::Running
    );
    req.session_id = Some("worker-session".into());
    assert!(crate::commands::bash_kill::handle(&req, &f.ctx).success);
}

#[test]
fn access_policy_status_owner_lookup_has_bounded_filesystem_work() {
    let f = fixture();
    let registry = f.ctx.bash_background();
    let storage = f.ctx.config().storage_dir.clone().unwrap();
    registry.replay_session(&storage, "worker-session").unwrap();
    registry.reset_task_access_work();
    let mut req = request("bash_status", json!({"task_id": f.worker.task_id}));
    req.session_id = Some("worker-session".into());
    let worker_scope = ScopeKey::from_stamp(&stamp("worker", "worker", 1, Some("head")));
    crate::access_policy::with_scope(Some(worker_scope), || {
        for _ in 0..3 {
            let response = crate::commands::bash_status::handle(&req, &f.ctx);
            assert!(response.success, "{:?}", response.data);
        }
    });
    assert_eq!(
        registry.task_access_work(),
        (0, 0, 0),
        "own status must not perform worker-owner lookups, namespace reads or task-folder probes"
    );

    let cold = fixture();
    let storage = cold.ctx.config().storage_dir.clone().unwrap();
    for index in 0..5100 {
        std::fs::create_dir_all(
            storage
                .join("bash-tasks")
                .join(format!("unrelated-{index:04}")),
        )
        .unwrap();
    }
    cold.ctx.bash_background().reset_task_access_work();
    req.session_id = Some("head-session".into());
    crate::access_policy::with_scope(Some(cold.head.clone()), || {
        let response = crate::commands::bash_status::handle(&req, &cold.ctx);
        assert!(response.success, "{:?}", response.data);
    });
    assert_eq!(
        cold.ctx.bash_background().task_access_work(),
        (1, 0, 1),
        "a cold head lookup must use the persisted task row and probe only its one task folder"
    );
    crate::access_policy::with_scope(Some(cold.head.clone()), || {
        let response = crate::commands::bash_status::handle(&req, &cold.ctx);
        assert!(response.success, "{:?}", response.data);
    });
    assert_eq!(
        cold.ctx.bash_background().task_access_work(),
        (2, 0, 1),
        "a repeated head lookup must use the live task and cached folder record"
    );
}

#[test]
fn access_policy_status_missing_row_or_non_head_never_probes_foreign_folders() {
    let f = fixture();
    let registry = f.ctx.bash_background();
    registry.reset_task_access_work();
    let mut req = request("bash_status", json!({"task_id": f.worker.task_id}));
    req.session_id = Some("sibling-session".into());
    let sibling = ScopeKey::from_stamp(&stamp("sibling", "worker", 1, Some("head")));
    crate::access_policy::with_scope(Some(sibling), || {
        let response = crate::commands::bash_status::handle(&req, &f.ctx);
        assert_eq!(
            response.data["code"], "task_not_found",
            "{:?}",
            response.data
        );
    });
    assert_eq!(registry.task_access_work(), (1, 0, 0));
    f.ctx
        .db()
        .unwrap()
        .lock()
        .unwrap()
        .execute("DELETE FROM bash_tasks", [])
        .unwrap();
    registry.reset_task_access_work();
    req.session_id = Some("head-session".into());
    crate::access_policy::with_scope(Some(f.head.clone()), || {
        let response = crate::commands::bash_status::handle(&req, &f.ctx);
        assert_eq!(
            response.data["code"], "task_not_found",
            "{:?}",
            response.data
        );
    });
    assert_eq!(
        registry.task_access_work(),
        (1, 0, 0),
        "an existing task bundle without its task row must not trigger filesystem discovery"
    );
}

#[test]
#[ignore = "manual 50k-file path-filter cost measurement"]
fn access_policy_ordinary_project_path_filter_cost() {
    use std::hint::black_box;
    use std::time::Instant;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project/src/generated/modules");
    let mut paths = Vec::with_capacity(50_000);
    for group in 0..250 {
        let group = root.join(format!("group-{group:03}"));
        std::fs::create_dir_all(&group).unwrap();
        for file in 0..200 {
            let path = group.join(format!("source-{file:03}.rs"));
            std::fs::write(&path, "").unwrap();
            paths.push(path);
        }
    }
    let filters = crate::search_index::PathFilters::default();
    let measure = |probe: &dyn Fn(&Path) -> bool| {
        let start = Instant::now();
        for _ in 0..10 {
            for path in &paths {
                assert!(black_box(probe(black_box(path))));
            }
        }
        start.elapsed().as_secs_f64() * 1000.0 / 10.0
    };
    let baseline = measure(&|_| true);
    let entry_filter = measure(&crate::bash_background::registry::safe_task_entry);
    let indexed_filter = measure(&|path| filters.matches(&root, path));
    println!("TASK_PATH_FILTER_COST files=50000 baseline_ms={baseline:.3} safe_task_entry_ms={entry_filter:.3} path_filters_matches_ms={indexed_filter:.3}");
}
