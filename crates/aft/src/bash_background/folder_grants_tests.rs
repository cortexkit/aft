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
        super::scope_end_label(stopped.info.status_reason.as_deref()),
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
