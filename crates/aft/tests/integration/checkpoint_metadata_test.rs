#[cfg(unix)]
use super::helpers::AftProcess;
#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[cfg(unix)]
#[test]
fn checkpoint_restore_preserves_permissions_and_symlinks_through_protocol() {
    let dir = crate::test_helpers::tempdir();
    let file = dir.path().join("mode.txt");
    let target = dir.path().join("target.txt");
    let link = dir.path().join("link.txt");

    fs::write(&file, "original mode\n").unwrap();
    let mut mode = fs::metadata(&file).unwrap().permissions();
    mode.set_mode(0o600);
    fs::set_permissions(&file, mode).unwrap();

    fs::write(&target, "target content\n").unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let mut aft = AftProcess::spawn();
    let checkpoint = serde_json::json!({
        "id": "checkpoint-metadata",
        "command": "checkpoint",
        "name": "metadata",
        "files": [file.display().to_string(), link.display().to_string()],
    });
    let resp = aft.send(&checkpoint.to_string());
    assert_eq!(resp["success"], true, "checkpoint: {resp:?}");

    fs::write(&file, "changed mode\n").unwrap();
    let mut changed_mode = fs::metadata(&file).unwrap().permissions();
    changed_mode.set_mode(0o644);
    fs::set_permissions(&file, changed_mode).unwrap();
    fs::remove_file(&link).unwrap();
    fs::write(&link, "plain file\n").unwrap();

    let restore = serde_json::json!({
        "id": "restore-metadata",
        "command": "restore_checkpoint",
        "name": "metadata",
    });
    let resp = aft.send(&restore.to_string());
    assert_eq!(resp["success"], true, "restore: {resp:?}");

    assert_eq!(fs::read_to_string(&file).unwrap(), "original mode\n");
    assert_eq!(
        fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read_link(&link).unwrap(), target);

    let status = aft.shutdown();
    assert!(status.success());
}
