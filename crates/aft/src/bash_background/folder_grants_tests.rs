use super::*;
use crate::access_policy::{AccessGrant, AccessOperation, GrantKind};

fn scope(name: &str, epoch: u64) -> ScopeKey {
    ScopeKey {
        owner: "owner".into(),
        scope_ref: name.into(),
        epoch,
    }
}

fn grants(name: &str, paths: &[&str]) -> TaskFolderGrants {
    TaskFolderGrants {
        owner: "owner".into(),
        scope_ref: name.into(),
        epoch: 3,
        paths: paths.iter().map(PathBuf::from).collect(),
    }
}

#[test]
fn folder_grants_record_only_saved_grants_the_profile_allows() {
    let temp = tempfile::tempdir().unwrap();
    let mut profile = crate::sandbox_profile::SandboxProfile::build(
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        temp.path().to_path_buf(),
    )
    .unwrap();
    profile
        .data_policy
        .write_allow
        .push(PathBuf::from("/granted/write"));
    profile
        .data_policy
        .read_allow
        .push(PathBuf::from("/granted/read"));
    let saved = |path: &str, operation| AccessGrant {
        path: PathBuf::from(path),
        kind: GrantKind::Subtree,
        operation,
    };
    let granted = [
        saved("/granted/write", AccessOperation::Write),
        saved("/granted/read", AccessOperation::Read),
        // A grant the shell profile left out (for example a folder that no
        // longer exists) never reached the sandbox.
        saved("/granted/missing", AccessOperation::Read),
    ];
    let recorded = for_profile(Some(scope("worker", 3)), &granted, Some(&profile)).unwrap();
    assert_eq!(
        recorded,
        grants("worker", &["/granted/read", "/granted/write"])
    );
    assert_eq!(for_profile(None, &granted, Some(&profile)), None);
    assert_eq!(for_profile(Some(scope("worker", 3)), &granted, None), None);
    assert_eq!(
        for_profile(Some(scope("worker", 3)), &granted[2..], Some(&profile)),
        None
    );
}

#[cfg(unix)]
#[test]
fn folder_revoke_stops_only_commands_whose_sandbox_held_the_folder() {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::new(Arc::new(Mutex::new(None)));
    let session = "folder-revoke";
    let spawn = |recorded: Option<TaskFolderGrants>| {
        let _record = spawn_guard(recorded);
        registry
            .spawn(
                crate::sandbox_spawn::SpawnPlan::Unsandboxed,
                "sleep 30",
                session.to_string(),
                project.path().to_path_buf(),
                HashMap::new(),
                crate::bash_background::HardKill::After(Duration::from_secs(60)),
                None,
                storage.path().to_path_buf(),
                10,
                false,
                false,
                Some(project.path().to_path_buf()),
            )
            .unwrap()
    };
    let held = spawn(Some(grants("worker", &["/granted", "/other"])));
    let other_folder = spawn(Some(grants("worker", &["/other"])));
    let other_holder = spawn(Some(grants("head", &["/granted"])));
    let unrecorded = spawn(None);
    assert_eq!(for_spawn(), None, "the record ends with its spawn");

    let found = registry.folder_grant_tasks(&scope("worker", 3), Path::new("/granted"));
    assert_eq!(
        found,
        vec![HeldTask {
            task_id: held.clone(),
            session_id: session.into(),
        }]
    );
    assert!(registry.kill_for_folder_revocation(&found[0]).unwrap());
    let stopped = registry.status(&held, session, None, None, 0).unwrap();
    assert_eq!(stopped.info.status, BgTaskStatus::Killed);
    assert_eq!(
        stopped.info.status_reason.as_deref(),
        Some(FOLDER_ACCESS_REVOKED_REASON)
    );
    assert_eq!(
        crate::bash_background::registry::scope_end_label(stopped.info.status_reason.as_deref()),
        Some("folder access revoked")
    );
    assert!(registry
        .folder_grant_tasks(&scope("worker", 3), Path::new("/granted"))
        .is_empty());
    for task in [other_folder, other_holder, unrecorded] {
        let running = registry.status(&task, session, None, None, 0).unwrap();
        assert!(
            !running.info.status.is_terminal(),
            "{task}: {:?}",
            running.info.status
        );
        registry.kill(&task, session).unwrap();
    }
}

/// Wait for a command to end and return its snapshot.
#[cfg(unix)]
fn wait_terminal(
    registry: &BgTaskRegistry,
    task_id: &str,
    session: &str,
) -> super::super::BgTaskSnapshot {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let snapshot = registry.status(task_id, session, None, None, 0).unwrap();
        if snapshot.info.status.is_terminal() {
            return snapshot;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{task_id} still {:?}",
            snapshot.info.status
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// aft.db with a registered worker scope holding a saved grant for each of
/// `paths`.
#[cfg(unix)]
fn grant_database(
    dir: &Path,
    scope: &ScopeKey,
    paths: &[&Path],
) -> Arc<Mutex<crate::db::TrackedConnection>> {
    std::fs::create_dir_all(dir).unwrap();
    let conn = crate::db::open(&dir.join("aft.db")).unwrap();
    crate::db::access_grants::register_scope(&conn, scope, None, &crate::config::Config::default())
        .unwrap();
    crate::db::access_grants::register_worker(&conn, scope).unwrap();
    for path in paths {
        crate::db::access_grants::insert(
            &conn,
            scope,
            &AccessGrant {
                path: path.to_path_buf(),
                kind: GrantKind::Subtree,
                operation: AccessOperation::Read,
            },
        )
        .unwrap();
    }
    Arc::new(Mutex::new(conn))
}

/// A revoke deletes the grant and then scans the running commands. A command
/// whose sandbox was built before the delete but which registers after the
/// scan is stopped by the spawn's own check once it is registered; a command
/// spawned the same way without the revoke keeps running.
#[cfg(unix)]
#[test]
fn folder_revoke_racing_a_spawn_stops_the_command_once_registered() {
    use std::time::Duration;
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let granted = crate::access_policy::policy_path(storage.path()).join("granted");
    std::fs::create_dir(&granted).unwrap();
    let sandbox_temp = storage.path().join("sandbox-temp");
    std::fs::create_dir(&sandbox_temp).unwrap();
    // A stand-in launcher: `/bin/sh sandbox-launch <launcher args> -- <command>`
    // runs only the command, so the test needs no real sandbox backend.
    crate::executable_cache::write_executable_with_mode(
        &project.path().join("sandbox-launch"),
        b"while [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = -- ]; then\n    shift\n    exec \"$@\"\n  fi\n  shift\ndone\nexit 78\n",
        0o700,
    )
    .unwrap();
    let mut profile = crate::sandbox_profile::SandboxProfile::build(
        vec![project.path().to_path_buf()],
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        sandbox_temp,
    )
    .unwrap();
    profile.data_policy.read_allow.push(granted.clone());
    let plan = crate::sandbox_spawn::SpawnPlan::launcher_for_test(profile, "/bin/sh".into());

    let worker = scope("worker", 3);
    let db = grant_database(storage.path(), &worker, &[&granted]);
    let ctx = crate::context::AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(project.path().to_path_buf()),
            storage_dir: Some(storage.path().to_path_buf()),
            experimental_bash_background: true,
            ..Default::default()
        },
    );
    ctx.update_config(|config| {
        config.sandbox.profile.head = crate::config::SandboxMode::Off;
        config.sandbox.profile.worker = crate::config::SandboxMode::Off;
    });
    ctx.set_db(Arc::clone(&db));
    let session = "folder-race";
    let spawn = || {
        let _scope = crate::access_policy::scope_guard(Some(worker.clone()));
        crate::sandbox_spawn::with_spawn_plan_for_test(plan.clone(), || {
            crate::bash_background::spawn(
                "folder-race",
                session,
                "sleep 30",
                crate::bash_background::BashShell::Bash,
                crate::bash_background::registry::resolve_posix_shell(),
                Some(project.path().to_path_buf()),
                None,
                crate::bash_background::HardKill::After(Duration::from_secs(60)),
                &ctx,
                true,
                false,
                false,
                false,
                24,
                80,
                Vec::new(),
                false,
                None,
                None,
            )
        })
    };
    let task_id = |response: &crate::protocol::Response| {
        assert!(response.success, "{:?}", response.data);
        response.data["task_id"].as_str().unwrap().to_owned()
    };

    let kept = task_id(&spawn());
    let registry = ctx.bash_background();
    let recorded = registry
        .task_for_session(&kept, session)
        .unwrap()
        .lock_state()
        .unwrap()
        .metadata
        .folder_grants
        .clone();
    assert_eq!(
        recorded,
        Some(TaskFolderGrants {
            owner: worker.owner.clone(),
            scope_ref: worker.scope_ref.clone(),
            epoch: worker.epoch,
            paths: vec![granted.clone()],
        })
    );

    let revoke_db = Arc::clone(&db);
    after_record_for_test(move || {
        let conn = revoke_db.lock().unwrap();
        let id = crate::db::access_grant_rows::live_rows(&conn).unwrap()[0]
            .grant_id
            .clone();
        crate::db::access_grant_rows::revoke_live(&conn, &id)
            .unwrap()
            .expect("the grant was live");
    });
    let raced = task_id(&spawn());
    let stopped = wait_terminal(registry, &raced, session);
    assert_eq!(stopped.info.status, BgTaskStatus::Killed);
    assert_eq!(
        stopped.info.status_reason.as_deref(),
        Some(FOLDER_ACCESS_REVOKED_REASON)
    );

    let running = registry.status(&kept, session, None, None, 0).unwrap();
    assert!(
        !running.info.status.is_terminal(),
        "{:?}",
        running.info.status
    );
    registry.kill(&kept, session).unwrap();
}

/// A grant revoked while AFT was down is checked when the command is
/// restored: the command holding it stops, one whose grant is still saved
/// keeps running.
#[cfg(unix)]
#[test]
fn folder_revoke_while_aft_was_down_stops_the_command_on_restore() {
    use std::collections::HashMap;
    use std::time::Duration;
    let storage = tempfile::tempdir().unwrap();
    let project = storage.path().to_path_buf();
    let session = "folder-restore";
    let original = BgTaskRegistry::new(Arc::new(Mutex::new(None)));
    let spawn = |paths: &[&str]| {
        let _record = spawn_guard(Some(grants("worker", paths)));
        original
            .spawn(
                crate::sandbox_spawn::SpawnPlan::Unsandboxed,
                "sleep 30",
                session.to_string(),
                project.clone(),
                HashMap::new(),
                crate::bash_background::HardKill::After(Duration::from_secs(60)),
                None,
                project.clone(),
                10,
                false,
                false,
                Some(project.clone()),
            )
            .unwrap()
    };
    let revoked = spawn(&["/revoked"]);
    let kept = spawn(&["/kept"]);
    original.detach();

    // While AFT was down the person revoked `/revoked`; `/kept` is still saved.
    let db = grant_database(
        &storage.path().join("db"),
        &scope("worker", 3),
        &[Path::new("/kept")],
    );
    let restarted = BgTaskRegistry::new(Arc::new(Mutex::new(None)));
    restarted.set_db_pool(db);
    restarted.replay_session(&project, session).unwrap();

    let stopped = wait_terminal(&restarted, &revoked, session);
    assert_eq!(stopped.info.status, BgTaskStatus::Killed);
    assert_eq!(
        stopped.info.status_reason.as_deref(),
        Some(FOLDER_ACCESS_REVOKED_REASON)
    );
    let running = restarted.status(&kept, session, None, None, 0).unwrap();
    assert!(
        !running.info.status.is_terminal(),
        "{:?}",
        running.info.status
    );
    restarted.kill(&kept, session).unwrap();
}
