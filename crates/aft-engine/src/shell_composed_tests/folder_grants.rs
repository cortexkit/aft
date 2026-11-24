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
    let granted = crate::policy_path::policy_path(storage.path()).join("granted");
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
