use super::*;
use crate::access_policy::{AccessGrant, AccessOperation, GrantKind};


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
