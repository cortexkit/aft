//! Integration tests for the safety & recovery system (undo, checkpoint, edit_history).
//!
//! Tests exercise the full round-trip through the binary's JSON protocol:
//! snapshot → checkpoint → modify → restore → verify file contents.

use super::helpers::AftProcess;
use std::fs;

/// Helper: create a temp directory with a unique name for this test.
fn temp_dir(test_name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir()
        .join("aft_safety_tests")
        .join(test_name)
        .join(format!("{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

#[test]
fn test_checkpoint_create_restore_cycle() {
    let dir = temp_dir("checkpoint_cycle");
    let file_a = dir.join("a.txt");
    let file_b = dir.join("b.txt");

    fs::write(&file_a, "original-a").unwrap();
    fs::write(&file_b, "original-b").unwrap();

    let mut aft = AftProcess::spawn();

    // Snapshot both files (populates backup store + tracked files)
    let resp = aft.send(&format!(
        r#"{{"id":"snap-a","command":"snapshot","file":"{}"}}"#,
        file_a.display()
    ));
    assert_eq!(resp["success"], true, "snapshot a: {:?}", resp);

    let resp = aft.send(&format!(
        r#"{{"id":"snap-b","command":"snapshot","file":"{}"}}"#,
        file_b.display()
    ));
    assert_eq!(resp["success"], true, "snapshot b: {:?}", resp);

    // Create checkpoint (no explicit files → uses tracked files from backup store)
    let resp = aft.send(r#"{"id":"cp-create","command":"checkpoint","name":"safe-point"}"#);
    assert_eq!(resp["success"], true, "checkpoint create: {:?}", resp);
    assert_eq!(resp["name"], "safe-point");
    assert!(resp["file_count"].as_u64().unwrap() >= 2);

    // Modify files externally
    fs::write(&file_a, "modified-a").unwrap();
    fs::write(&file_b, "modified-b").unwrap();
    assert_eq!(fs::read_to_string(&file_a).unwrap(), "modified-a");
    assert_eq!(fs::read_to_string(&file_b).unwrap(), "modified-b");

    // Restore checkpoint
    let resp =
        aft.send(r#"{"id":"cp-restore","command":"restore_checkpoint","name":"safe-point"}"#);
    assert_eq!(resp["success"], true, "restore: {:?}", resp);
    assert_eq!(resp["name"], "safe-point");

    // Verify files match original content
    assert_eq!(
        fs::read_to_string(&file_a).unwrap(),
        "original-a",
        "file a should be restored"
    );
    assert_eq!(
        fs::read_to_string(&file_b).unwrap(),
        "original-b",
        "file b should be restored"
    );

    let status = aft.shutdown();
    assert!(status.success());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_undo_restores_previous_version() {
    let dir = temp_dir("undo_restore");
    let file = dir.join("target.txt");

    fs::write(&file, "version-1").unwrap();

    let mut aft = AftProcess::spawn();

    // Snapshot the original
    let resp = aft.send(&format!(
        r#"{{"id":"snap-1","command":"snapshot","file":"{}"}}"#,
        file.display()
    ));
    assert_eq!(resp["success"], true);

    // Overwrite externally
    fs::write(&file, "version-2").unwrap();
    assert_eq!(fs::read_to_string(&file).unwrap(), "version-2");

    // Undo → should restore version-1
    let resp = aft.send(&format!(
        r#"{{"id":"undo-1","command":"undo","file":"{}"}}"#,
        file.display()
    ));
    assert_eq!(resp["success"], true, "undo: {:?}", resp);
    assert!(resp["backup_id"].is_string());
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "version-1",
        "file should be restored to version-1"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn test_undo_restores_file_after_edit_command() {
    let dir = temp_dir("undo_after_edit_command");
    let file = dir.join("target.txt");

    fs::write(&file, "hello world\n").unwrap();

    let mut aft = AftProcess::spawn();

    let edit = serde_json::json!({
        "id": "edit-before-undo",
        "command": "edit_match",
        "file": file.display().to_string(),
        "match": "world",
        "replacement": "rust"
    });
    let edit_resp = aft.send(&serde_json::to_string(&edit).unwrap());
    assert_eq!(
        edit_resp["success"], true,
        "edit should succeed: {edit_resp:?}"
    );
    assert_eq!(fs::read_to_string(&file).unwrap(), "hello rust\n");

    let undo = aft.send(&format!(
        r#"{{"id":"undo-after-edit","command":"undo","file":"{}"}}"#,
        file.display()
    ));
    assert_eq!(undo["success"], true, "undo should succeed: {undo:?}");
    assert_eq!(fs::read_to_string(&file).unwrap(), "hello world\n");

    let history = aft.send(&format!(
        r#"{{"id":"history-after-undo","command":"edit_history","file":"{}"}}"#,
        file.display()
    ));
    assert_eq!(history["success"], true);
    assert!(history["entries"].as_array().unwrap().is_empty());

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn test_operation_undo_restores_multiple_deleted_files() {
    let dir = temp_dir("operation_undo_delete_many");
    let file_a = dir.join("a.txt");
    let file_b = dir.join("b.txt");

    fs::write(&file_a, "original-a").unwrap();
    fs::write(&file_b, "original-b").unwrap();

    let mut aft = AftProcess::spawn();
    let delete = serde_json::json!({
        "id": "delete-many",
        "command": "delete_file",
        "files": [file_a.display().to_string(), file_b.display().to_string()],
    });
    let delete_resp = aft.send(&serde_json::to_string(&delete).unwrap());
    assert_eq!(delete_resp["success"], true, "delete: {delete_resp:?}");
    assert!(!file_a.exists());
    assert!(!file_b.exists());

    let undo = aft.send(r#"{"id":"undo-operation","command":"undo"}"#);
    assert_eq!(undo["success"], true, "undo: {undo:?}");
    assert_eq!(undo["operation"], true);
    assert_eq!(undo["restored_count"], 2);
    assert_eq!(fs::read_to_string(&file_a).unwrap(), "original-a");
    assert_eq!(fs::read_to_string(&file_b).unwrap(), "original-b");

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn undo_after_write_created_file_deletes_it() {
    let dir = temp_dir("undo_write_created_file");
    let file = dir.join("created.txt");
    let mut aft = AftProcess::spawn();

    let write = serde_json::json!({
        "id": "write-created",
        "command": "write",
        "file": file.display().to_string(),
        "content": "new file\n",
    });
    let write_resp = aft.send(&serde_json::to_string(&write).unwrap());
    assert_eq!(write_resp["success"], true, "write: {write_resp:?}");
    assert_eq!(fs::read_to_string(&file).unwrap(), "new file\n");

    let undo = aft.send(r#"{"id":"undo-write-created","command":"undo"}"#);
    assert_eq!(undo["success"], true, "undo: {undo:?}");
    assert!(!file.exists(), "created file should be removed by undo");

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn undo_after_append_created_file_deletes_it() {
    let dir = temp_dir("undo_append_created_file");
    let file = dir.join("created-by-append.txt");
    let mut aft = AftProcess::spawn();

    let append = serde_json::json!({
        "id": "append-created",
        "command": "edit_match",
        "op": "append",
        "file": file.display().to_string(),
        "appendContent": "appended\n",
    });
    let append_resp = aft.send(&serde_json::to_string(&append).unwrap());
    assert_eq!(append_resp["success"], true, "append: {append_resp:?}");
    assert_eq!(fs::read_to_string(&file).unwrap(), "appended\n");

    let undo = aft.send(r#"{"id":"undo-append-created","command":"undo"}"#);
    assert_eq!(undo["success"], true, "undo: {undo:?}");
    assert!(
        !file.exists(),
        "created append file should be removed by undo"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn undo_after_transaction_created_file_deletes_it() {
    let dir = temp_dir("undo_transaction_created_file");
    let file = dir.join("created-by-transaction.txt");
    let mut aft = AftProcess::spawn();

    let tx = serde_json::json!({
        "id": "transaction-created",
        "command": "transaction",
        "operations": [{
            "command": "write",
            "file": file.display().to_string(),
            "content": "created in tx\n",
        }],
    });
    let tx_resp = aft.send(&serde_json::to_string(&tx).unwrap());
    assert_eq!(tx_resp["success"], true, "transaction: {tx_resp:?}");
    assert_eq!(fs::read_to_string(&file).unwrap(), "created in tx\n");

    let undo = aft.send(r#"{"id":"undo-transaction-created","command":"undo"}"#);
    assert_eq!(undo["success"], true, "undo: {undo:?}");
    assert!(
        !file.exists(),
        "transaction-created file should be removed by undo"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[cfg(unix)]
#[test]
fn symlink_file_delete_is_rejected_without_project_restriction() {
    let dir = temp_dir("delete_single_symlink_unrestricted");
    let target = dir.join("target.txt");
    let symlink = dir.join("target-link.txt");

    fs::write(&target, "target content").unwrap();
    std::os::unix::fs::symlink(&target, &symlink).unwrap();

    let mut aft = AftProcess::spawn();
    let delete = serde_json::json!({
        "id": "delete-single-symlink-unrestricted",
        "command": "delete_file",
        "file": symlink.display().to_string(),
    });
    let resp = aft.send(&serde_json::to_string(&delete).unwrap());

    assert_eq!(resp["success"], false, "delete should fail: {resp:?}");
    assert_eq!(resp["code"], "invalid_request");
    assert!(
        resp["message"]
            .as_str()
            .unwrap()
            .contains("refusing to delete symlink"),
        "message should explain symlink rejection: {resp:?}"
    );
    assert!(symlink.exists(), "symlink should remain intact");
    assert_eq!(fs::read_to_string(&target).unwrap(), "target content");

    let status = aft.shutdown();
    assert!(status.success());
}

#[cfg(unix)]
#[test]
fn symlink_file_delete_is_rejected_with_project_restriction() {
    let dir = temp_dir("delete_single_symlink_restricted");
    let target = dir.join("target.txt");
    let symlink = dir.join("target-link.txt");

    fs::write(&target, "target content").unwrap();
    std::os::unix::fs::symlink(&target, &symlink).unwrap();

    let mut aft = AftProcess::spawn();
    let configure = serde_json::json!({
        "id": "cfg-delete-single-symlink",
        "command": "configure",
            "harness": "opencode",
        "project_root": dir.display().to_string(),
        "restrict_to_project_root": true,
    });
    let cfg = aft.send(&serde_json::to_string(&configure).unwrap());
    assert_eq!(cfg["success"], true, "configure should succeed: {cfg:?}");

    let delete = serde_json::json!({
        "id": "delete-single-symlink-restricted",
        "command": "delete_file",
        "file": symlink.display().to_string(),
    });
    let resp = aft.send(&serde_json::to_string(&delete).unwrap());

    assert_eq!(resp["success"], false, "delete should fail: {resp:?}");
    assert_eq!(resp["code"], "invalid_request");
    assert!(
        resp["message"]
            .as_str()
            .unwrap()
            .contains("refusing to delete symlink"),
        "message should explain symlink rejection: {resp:?}"
    );
    assert!(symlink.exists(), "symlink should remain intact");
    assert_eq!(fs::read_to_string(&target).unwrap(), "target content");

    let status = aft.shutdown();
    assert!(status.success());
}

#[cfg(unix)]
#[test]
fn symlink_to_outside_file_blocks_recursive_delete() {
    let dir = temp_dir("delete_recursive_blocks_file_symlink");
    let target_dir = temp_dir("delete_recursive_blocks_file_symlink_target");
    let real_file = dir.join("real.txt");
    let outside_file = target_dir.join("outside.txt");
    let symlink = dir.join("outside-link.txt");

    fs::write(&real_file, "inside").unwrap();
    fs::write(&outside_file, "outside").unwrap();
    std::os::unix::fs::symlink(&outside_file, &symlink).unwrap();

    let mut aft = AftProcess::spawn();
    let delete = serde_json::json!({
        "id": "delete-file-symlink-tree",
        "command": "delete_file",
        "file": dir.display().to_string(),
        "recursive": true,
    });
    let resp = aft.send(&serde_json::to_string(&delete).unwrap());

    assert_eq!(resp["success"], false, "delete should fail: {resp:?}");
    assert_eq!(resp["code"], "unsupported_directory_contents");
    assert!(
        resp["message"]
            .as_str()
            .unwrap()
            .contains(&symlink.display().to_string()),
        "message should mention symlink path: {resp:?}"
    );
    assert!(dir.exists(), "directory should remain intact");
    assert!(real_file.exists(), "regular file should remain intact");
    assert!(symlink.exists(), "symlink should remain intact");
    assert_eq!(fs::read_to_string(&outside_file).unwrap(), "outside");

    let status = aft.shutdown();
    assert!(status.success());
}

#[cfg(unix)]
#[test]
fn symlink_to_directory_blocks_recursive_delete() {
    let dir = temp_dir("delete_recursive_blocks_dir_symlink");
    let target_dir = temp_dir("delete_recursive_blocks_dir_symlink_target");
    let real_file = dir.join("real.txt");
    let symlink = dir.join("outside-dir-link");

    fs::write(&real_file, "inside").unwrap();
    fs::write(target_dir.join("outside.txt"), "outside").unwrap();
    std::os::unix::fs::symlink(&target_dir, &symlink).unwrap();

    let mut aft = AftProcess::spawn();
    let delete = serde_json::json!({
        "id": "delete-dir-symlink-tree",
        "command": "delete_file",
        "file": dir.display().to_string(),
        "recursive": true,
    });
    let resp = aft.send(&serde_json::to_string(&delete).unwrap());

    assert_eq!(resp["success"], false, "delete should fail: {resp:?}");
    assert_eq!(resp["code"], "unsupported_directory_contents");
    assert!(
        resp["message"]
            .as_str()
            .unwrap()
            .contains(&symlink.display().to_string()),
        "message should mention symlink path: {resp:?}"
    );
    assert!(dir.exists(), "directory should remain intact");
    assert!(real_file.exists(), "regular file should remain intact");
    assert!(symlink.exists(), "symlink should remain intact");
    assert!(
        target_dir.join("outside.txt").exists(),
        "symlink target should remain intact"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn empty_subdir_blocks_recursive_delete() {
    let dir = temp_dir("delete_recursive_blocks_empty_subdir");
    let content_file = dir.join("with_content.txt");
    let empty_subdir = dir.join("empty_subdir");

    fs::write(&content_file, "content").unwrap();
    fs::create_dir(&empty_subdir).unwrap();

    let mut aft = AftProcess::spawn();
    let delete = serde_json::json!({
        "id": "delete-empty-subdir-tree",
        "command": "delete_file",
        "file": dir.display().to_string(),
        "recursive": true,
    });
    let resp = aft.send(&serde_json::to_string(&delete).unwrap());

    assert_eq!(resp["success"], false, "delete should fail: {resp:?}");
    assert_eq!(resp["code"], "unsupported_directory_contents");
    assert!(
        resp["message"]
            .as_str()
            .unwrap()
            .contains(&empty_subdir.display().to_string()),
        "message should mention empty directory path: {resp:?}"
    );
    assert!(dir.exists(), "directory should remain intact");
    assert_eq!(fs::read_to_string(&content_file).unwrap(), "content");
    assert!(
        empty_subdir.exists(),
        "empty subdirectory should remain intact"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[cfg(unix)]
#[test]
fn unix_socket_blocks_recursive_delete() {
    use std::os::unix::net::UnixListener;

    let dir = std::env::temp_dir().join(format!("aft_sock_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let content_file = dir.join("with_content.txt");
    let socket_path = dir.join("socket.sock");
    fs::write(&content_file, "content").unwrap();
    let _listener = UnixListener::bind(&socket_path).unwrap();

    let mut aft = AftProcess::spawn();
    let delete = serde_json::json!({
        "id": "delete-socket-tree",
        "command": "delete_file",
        "file": dir.display().to_string(),
        "recursive": true,
    });
    let resp = aft.send(&serde_json::to_string(&delete).unwrap());

    assert_eq!(resp["success"], false, "delete should fail: {resp:?}");
    assert_eq!(resp["code"], "unsupported_directory_contents");
    assert!(
        resp["message"]
            .as_str()
            .unwrap()
            .contains(&socket_path.display().to_string()),
        "message should mention socket path: {resp:?}"
    );
    assert!(dir.exists(), "directory should remain intact");
    assert_eq!(fs::read_to_string(&content_file).unwrap(), "content");
    assert!(socket_path.exists(), "socket should remain intact");

    let status = aft.shutdown();
    assert!(status.success());
    drop(_listener);
    let _ = fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn hard_link_blocks_recursive_delete() {
    let dir = temp_dir("delete_recursive_blocks_hard_link");
    let file = dir.join("file.txt");
    let link = dir.join("file-hardlink.txt");
    fs::write(&file, "content").unwrap();
    fs::hard_link(&file, &link).unwrap();

    let mut aft = AftProcess::spawn();
    let delete = serde_json::json!({
        "id": "delete-hardlink-tree",
        "command": "delete_file",
        "file": dir.display().to_string(),
        "recursive": true,
    });
    let resp = aft.send(&serde_json::to_string(&delete).unwrap());

    assert_eq!(resp["success"], false, "delete should fail: {resp:?}");
    assert_eq!(resp["code"], "unsupported_directory_contents");
    assert!(dir.exists(), "directory should remain intact");
    assert_eq!(fs::read_to_string(&file).unwrap(), "content");
    assert_eq!(fs::read_to_string(&link).unwrap(), "content");

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn regular_tree_with_files_works_after_validation() {
    let dir = temp_dir("delete_recursive_regular_tree");
    let nested = dir.join("nested");
    let file_a = dir.join("a.txt");
    let file_b = nested.join("b.txt");

    fs::create_dir(&nested).unwrap();
    fs::write(&file_a, "root file").unwrap();
    fs::write(&file_b, "nested file").unwrap();

    let mut aft = AftProcess::spawn();
    let delete = serde_json::json!({
        "id": "delete-regular-tree",
        "command": "delete_file",
        "file": dir.display().to_string(),
        "recursive": true,
    });
    let delete_resp = aft.send(&serde_json::to_string(&delete).unwrap());
    assert_eq!(delete_resp["success"], true, "delete: {delete_resp:?}");
    assert_eq!(delete_resp["is_directory"], true);
    assert_eq!(delete_resp["files_deleted"], 2);
    assert!(!dir.exists(), "directory should be removed");

    let undo = aft.send(r#"{"id":"undo-regular-tree","command":"undo"}"#);
    assert_eq!(undo["success"], true, "undo: {undo:?}");
    assert_eq!(undo["operation"], true);
    assert_eq!(undo["restored_count"], 2);
    assert_eq!(fs::read_to_string(&file_a).unwrap(), "root file");
    assert_eq!(fs::read_to_string(&file_b).unwrap(), "nested file");

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn test_edit_history_returns_stack() {
    let dir = temp_dir("edit_history");
    let file = dir.join("tracked.txt");

    fs::write(&file, "v1").unwrap();

    let mut aft = AftProcess::spawn();

    // Snapshot v1
    aft.send(&format!(
        r#"{{"id":"s1","command":"snapshot","file":"{}"}}"#,
        file.display()
    ));

    // Modify and snapshot v2
    fs::write(&file, "v2").unwrap();
    aft.send(&format!(
        r#"{{"id":"s2","command":"snapshot","file":"{}"}}"#,
        file.display()
    ));

    // Modify and snapshot v3
    fs::write(&file, "v3").unwrap();
    aft.send(&format!(
        r#"{{"id":"s3","command":"snapshot","file":"{}"}}"#,
        file.display()
    ));

    // Query edit history
    let resp = aft.send(&format!(
        r#"{{"id":"hist","command":"edit_history","file":"{}"}}"#,
        file.display()
    ));
    assert_eq!(resp["success"], true, "edit_history: {:?}", resp);

    let entries = resp["entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 3, "should have 3 history entries");

    // Most recent first (reversed from stack order)
    for entry in entries {
        assert!(entry["backup_id"].is_string());
        assert!(entry["timestamp"].is_u64());
        assert!(entry["description"].is_string());
    }

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn test_list_checkpoints() {
    let dir = temp_dir("list_checkpoints");
    let file_a = dir.join("a.txt");
    let file_b = dir.join("b.txt");

    fs::write(&file_a, "data-a").unwrap();
    fs::write(&file_b, "data-b").unwrap();

    let mut aft = AftProcess::spawn();

    // Create checkpoint with 1 file
    let resp = aft.send(&format!(
        r#"{{"id":"cp1","command":"checkpoint","name":"first","files":["{}"]}}"#,
        file_a.display()
    ));
    assert_eq!(resp["success"], true);

    // Create checkpoint with 2 files
    let resp = aft.send(&format!(
        r#"{{"id":"cp2","command":"checkpoint","name":"second","files":["{}","{}"]}}"#,
        file_a.display(),
        file_b.display()
    ));
    assert_eq!(resp["success"], true);

    // List checkpoints
    let resp = aft.send(r#"{"id":"list","command":"list_checkpoints"}"#);
    assert_eq!(resp["success"], true, "list_checkpoints: {:?}", resp);

    let checkpoints = resp["checkpoints"].as_array().expect("checkpoints array");
    assert_eq!(checkpoints.len(), 2);

    let names: Vec<&str> = checkpoints
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"first"));
    assert!(names.contains(&"second"));

    // Verify file counts
    let first = checkpoints.iter().find(|c| c["name"] == "first").unwrap();
    let second = checkpoints.iter().find(|c| c["name"] == "second").unwrap();
    assert_eq!(first["file_count"], 1);
    assert_eq!(second["file_count"], 2);

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn test_undo_no_history_error() {
    let dir = temp_dir("undo_no_history");
    let file = dir.join("never_snapshotted.txt");
    fs::write(&file, "content").unwrap();

    let mut aft = AftProcess::spawn();

    // Undo with no prior snapshots → error
    let resp = aft.send(&format!(
        r#"{{"id":"undo-err","command":"undo","file":"{}"}}"#,
        file.display()
    ));
    assert_eq!(resp["success"], false, "undo should fail: {:?}", resp);
    assert_eq!(resp["code"], "no_undo_history");
    assert!(resp["message"]
        .as_str()
        .unwrap()
        .contains(&file.display().to_string())
        .then_some(true)
        .or_else(|| Some(
            resp["message"]
                .as_str()
                .unwrap()
                .contains("no undo history")
        ))
        .unwrap());

    // Process should still be alive
    let resp = aft.send(r#"{"id":"alive-1","command":"ping"}"#);
    assert_eq!(resp["success"], true, "process should survive error");

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn test_restore_nonexistent_checkpoint() {
    let mut aft = AftProcess::spawn();

    // Restore a checkpoint that doesn't exist → error
    let resp = aft.send(r#"{"id":"rc-err","command":"restore_checkpoint","name":"ghost"}"#);
    assert_eq!(resp["success"], false, "restore should fail: {:?}", resp);
    assert_eq!(resp["code"], "checkpoint_not_found");
    assert!(resp["message"].as_str().unwrap().contains("ghost"));

    // Process should still be alive
    let resp = aft.send(r#"{"id":"alive-2","command":"ping"}"#);
    assert_eq!(resp["success"], true, "process should survive error");

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn test_checkpoint_overwrite() {
    let dir = temp_dir("checkpoint_overwrite");
    let file_a = dir.join("a.txt");
    let file_b = dir.join("b.txt");

    fs::write(&file_a, "a-v1").unwrap();
    fs::write(&file_b, "b-v1").unwrap();

    let mut aft = AftProcess::spawn();

    // Create checkpoint "reusable" with file_a
    let resp = aft.send(&format!(
        r#"{{"id":"ow1","command":"checkpoint","name":"reusable","files":["{}"]}}"#,
        file_a.display()
    ));
    assert_eq!(resp["success"], true);
    assert_eq!(resp["file_count"], 1);

    // Modify files
    fs::write(&file_a, "a-v2").unwrap();
    fs::write(&file_b, "b-v2").unwrap();

    // Overwrite checkpoint "reusable" with both files (different content now)
    let resp = aft.send(&format!(
        r#"{{"id":"ow2","command":"checkpoint","name":"reusable","files":["{}","{}"]}}"#,
        file_a.display(),
        file_b.display()
    ));
    assert_eq!(resp["success"], true);
    assert_eq!(resp["file_count"], 2);

    // Modify files again
    fs::write(&file_a, "a-v3").unwrap();
    fs::write(&file_b, "b-v3").unwrap();

    // Restore → should get v2 content (the second checkpoint), not v1
    let resp = aft.send(r#"{"id":"ow-restore","command":"restore_checkpoint","name":"reusable"}"#);
    assert_eq!(resp["success"], true, "restore: {:?}", resp);

    assert_eq!(fs::read_to_string(&file_a).unwrap(), "a-v2");
    assert_eq!(fs::read_to_string(&file_b).unwrap(), "b-v2");

    // Process should still be alive after all this
    let resp = aft.send(r#"{"id":"alive-3","command":"ping"}"#);
    assert_eq!(resp["success"], true);

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn test_edit_history_caps_at_twenty_entries_per_file() {
    let dir = temp_dir("history_cap");
    let file = dir.join("history_cap.txt");
    fs::write(&file, "v0").unwrap();

    let mut aft = AftProcess::spawn();

    for i in 1..=21 {
        let req = serde_json::json!({
            "id": format!("edit-{i}"),
            "command": "edit_match",
            "file": file.display().to_string(),
            "match": format!("v{}", i - 1),
            "replacement": format!("v{i}")
        });
        let resp = aft.send(&serde_json::to_string(&req).unwrap());
        assert_eq!(resp["success"], true, "edit {i} failed: {resp:?}");
    }

    assert_eq!(fs::read_to_string(&file).unwrap(), "v21");

    let history = aft.send(&format!(
        r#"{{"id":"hist-cap","command":"edit_history","file":"{}"}}"#,
        file.display()
    ));
    assert_eq!(history["success"], true, "history failed: {:?}", history);

    let entries = history["entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 20, "history should be capped: {:?}", entries);
    assert_eq!(entries[0]["description"], "edit_match: v20");
    assert_eq!(entries[19]["description"], "edit_match: v1");
    assert!(!entries
        .iter()
        .any(|entry| entry["description"] == "edit_match: v0"));

    for expected in (1..=20).rev() {
        let undo = aft.send(&format!(
            r#"{{"id":"undo-{expected}","command":"undo","file":"{}"}}"#,
            file.display()
        ));
        assert_eq!(undo["success"], true, "undo {expected} failed: {undo:?}");
        assert_eq!(fs::read_to_string(&file).unwrap(), format!("v{expected}"));
    }

    let no_more_history = aft.send(&format!(
        r#"{{"id":"undo-empty","command":"undo","file":"{}"}}"#,
        file.display()
    ));
    assert_eq!(no_more_history["success"], false);
    assert_eq!(no_more_history["code"], "no_undo_history");
    assert_eq!(fs::read_to_string(&file).unwrap(), "v1");

    let status = aft.shutdown();
    assert!(status.success());
}
