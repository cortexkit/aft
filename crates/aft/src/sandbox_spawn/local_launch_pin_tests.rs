use super::*;
use std::os::unix::fs::symlink;

#[test]
fn pinned_executable_refuses_legacy_local_fallback_to_task_launcher() {
    let storage = tempfile::tempdir().unwrap();
    let task = crate::bash_background::persistence::create_task_layout(
        storage.path(),
        "legacy-local-launch",
        "bash-0123456789abcdef",
    )
    .unwrap();
    let launcher_path = crate::pinned_executable::task_executable(&task.paths.io_dir);
    std::fs::create_dir_all(launcher_path.parent().unwrap()).unwrap();
    symlink("/bin/true", &launcher_path).unwrap();
    let snapshot = SavedLocalLaunch {
        policy: SavedSpawnPolicy::Launcher {
            profile: SandboxProfile::build(
                vec![],
                vec![],
                vec![],
                vec![],
                vec![],
                vec![],
                vec![],
                storage.path().to_owned(),
            )
            .unwrap(),
            launcher_path,
            launcher_identity: None,
        },
        root: storage.path().to_owned(),
        cwd: storage.path().to_owned(),
        principal: AuthenticatedPrincipal::FirstParty,
        shell_path: PathBuf::from("/bin/sh"),
        environment: Vec::new(),
        command: b"true".to_vec(),
        payload_digest: [0; 32],
        linux_scope: false,
        linux_scope_max_runtime_secs: default_scope_max_runtime_secs(),
        memory_limit_mib: None,
    };
    let mut legacy = serde_json::to_value(snapshot).unwrap();
    legacy["policy"]["Launcher"]
        .as_object_mut()
        .unwrap()
        .remove("launcher_identity")
        .expect("omit field as in the legacy snapshot");
    let bytes = serde_json::to_vec(&legacy).unwrap();
    crate::bash_background::persistence::create_control_file(
        &task.dirs,
        REMOTE_LOCAL_LAUNCH,
        &bytes,
    )
    .unwrap();
    let mut hash = blake3::Hasher::new();
    hash.update(task.paths.task_id.as_bytes());
    hash.update(&bytes);
    let error = restore_local_launch(&task, hash.finalize().to_hex().as_str())
        .expect_err("a verified legacy snapshot must not yield a plan to execute its task alias");
    assert_eq!(error, SNAPSHOT_DIGEST_MISMATCH);
}

#[test]
fn pinned_executable_local_fallback_rejects_replaced_pin_identity() {
    use super::super::launcher_image::LauncherIdentity;
    let directory = tempfile::tempdir().unwrap();
    let launcher = directory.path().join("aft");
    let replacement = directory.path().join("replacement");
    std::fs::write(&launcher, b"first image").unwrap();
    std::fs::write(&replacement, b"second image").unwrap();
    let identity = LauncherIdentity::capture(&launcher).unwrap();
    assert!(LauncherIdentity::validate_saved(Some(identity), &launcher).is_ok());
    std::fs::rename(replacement, &launcher).unwrap();
    assert!(LauncherIdentity::validate_saved(Some(identity), &launcher).is_err());
}
