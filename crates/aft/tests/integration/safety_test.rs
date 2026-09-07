//! Integration tests for the safety & recovery system (undo, checkpoint, edit_history).
//!
//! Tests exercise the full round-trip through the binary's JSON protocol:
//! snapshot → checkpoint → modify → restore → verify file contents.

use super::helpers::AftProcess;
// Only the unix-gated symlink tests below take this route; on Windows every
// caller is compiled out, so an unconditional import trips deny-warnings.
#[cfg(unix)]
use super::helpers::user_config;
#[cfg(unix)]
use aft::commands::checkpoint::handle_checkpoint;
#[cfg(unix)]
use aft::commands::restore_checkpoint::handle_restore_checkpoint;
#[cfg(unix)]
use aft::config::Config;
#[cfg(unix)]
use aft::context::AppContext;
#[cfg(unix)]
use aft::language::StubProvider;
#[cfg(unix)]
use aft::protocol::RawRequest;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

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

fn configure_restricted(aft: &mut AftProcess, root: &std::path::Path, request_id: &str) {
    let user_config_path = root.join(format!(".aft-user-config-{request_id}.jsonc"));
    fs::write(&user_config_path, r#"{"restrict_to_project_root": true}"#).unwrap();
    let response = aft.send(
        &serde_json::to_string(&serde_json::json!({
            "id": request_id,
            "command": "configure",
            "harness": "opencode",
            "project_root": root.display().to_string(),
            "cortexkit_user_config_path": user_config_path.display().to_string(),
        }))
        .unwrap(),
    );
    assert_eq!(response["success"], true, "configure: {response:?}");
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
        r#"{{"id":"snap-a","command":"snapshot","file":{}}}"#,
        crate::helpers::json_string(&file_a.display())
    ));
    assert_eq!(resp["success"], true, "snapshot a: {:?}", resp);

    let resp = aft.send(&format!(
        r#"{{"id":"snap-b","command":"snapshot","file":{}}}"#,
        crate::helpers::json_string(&file_b.display())
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
fn checkpoint_explicit_gitignored_file_is_counted_stored_and_restored() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let draft_relative = std::path::Path::new(".cortexkit/alfonso/drafts/spec.md");
    let draft = root.join(draft_relative);
    let mut original = b"draft: hand-edited decision\n\x00byte-exact\n".to_vec();
    while original.len() < 90 * 1024 {
        original.extend_from_slice(b"weeks of hand-edited specification detail\n");
    }
    let mutated = b"draft: changed\n";

    fs::write(root.join(".gitignore"), ".cortexkit/\n").unwrap();
    fs::create_dir_all(draft.parent().unwrap()).unwrap();
    fs::write(&draft, &original).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(root)
            .status()
            .unwrap()
            .success(),
        "fixture must be a git repository"
    );
    assert!(
        Command::new("git")
            .args(["check-ignore", "--quiet", draft_relative.to_str().unwrap()])
            .current_dir(root)
            .status()
            .unwrap()
            .success(),
        "fixture draft must be gitignored"
    );
    assert!(
        !Command::new("git")
            .args([
                "ls-files",
                "--error-unmatch",
                draft_relative.to_str().unwrap()
            ])
            .current_dir(root)
            .output()
            .unwrap()
            .status
            .success(),
        "fixture draft must be untracked"
    );

    let storage = tempfile::tempdir().unwrap();
    let mut aft = AftProcess::spawn();
    configure_unrestricted_with_storage(
        &mut aft,
        root,
        storage.path(),
        "cfg-gitignored-checkpoint",
    );

    let checkpoint = aft.send(
        &serde_json::to_string(&serde_json::json!({
            "id": "gitignored-checkpoint",
            "command": "tool_call",
            "session_id": "gitignored-checkpoint-session",
            "name": "safety",
            "arguments": {
                "op": "checkpoint",
                "name": "gitignored-draft",
                "files": [draft_relative.display().to_string()],
            },
        }))
        .unwrap(),
    );
    assert_eq!(checkpoint["success"], true, "checkpoint: {checkpoint:?}");
    assert_eq!(checkpoint["file_count"], 1);
    assert!(
        checkpoint.get("skipped").is_none(),
        "checkpoint: {checkpoint:?}"
    );
    let storage_path = checkpoint["storage_path"]
        .as_str()
        .expect("checkpoint storage path");
    assert!(std::path::Path::new(storage_path).is_dir());
    assert_eq!(
        checkpoint["durability"],
        format!("durable on disk at {storage_path}; survives restarts")
    );
    assert!(checkpoint["text"]
        .as_str()
        .is_some_and(|text| text.contains("durable on disk at")));

    let paths = aft.send(
        r#"{"id":"gitignored-paths","command":"checkpoint_paths","session_id":"gitignored-checkpoint-session","name":"gitignored-draft"}"#,
    );
    assert_eq!(paths["success"], true, "checkpoint paths: {paths:?}");
    assert_eq!(paths["file_count"], 1);
    // Compare as paths, not strings: the fixture's `draft` was joined from a
    // forward-slash literal, which `display()` preserves on Windows while the
    // product re-joins components with native separators.
    let reported_paths = paths["paths"]
        .as_array()
        .expect("paths array")
        .iter()
        .map(|value| std::path::PathBuf::from(value.as_str().expect("path string")))
        .collect::<Vec<_>>();
    assert_eq!(
        reported_paths,
        vec![draft.clone()],
        "reported success must name the stored restore target"
    );

    fs::write(&draft, mutated).unwrap();
    assert_eq!(fs::read(&draft).unwrap(), mutated, "mutation control");

    assert!(aft.shutdown().success());

    let mut restarted = AftProcess::spawn();
    configure_unrestricted_with_storage(
        &mut restarted,
        root,
        storage.path(),
        "cfg-gitignored-checkpoint-restart",
    );
    let list = restarted.send(
        r#"{"id":"gitignored-list","command":"tool_call","session_id":"gitignored-checkpoint-session","name":"safety","arguments":{"op":"list"}}"#,
    );
    assert_eq!(list["success"], true, "list: {list:?}");
    assert_eq!(list["checkpoints"].as_array().unwrap().len(), 1);
    assert!(list["text"]
        .as_str()
        .is_some_and(|text| text.contains("hydrated from disk")));

    let restore = restarted.send(
        r#"{"id":"gitignored-restore","command":"tool_call","session_id":"gitignored-checkpoint-session","name":"safety","arguments":{"op":"restore","name":"gitignored-draft"}}"#,
    );
    assert_eq!(restore["success"], true, "restore: {restore:?}");
    assert_eq!(restore["file_count"], 1);
    assert_eq!(
        fs::read(&draft).unwrap(),
        original,
        "restart restore must be byte-exact"
    );

    assert!(restarted.shutdown().success());
}

#[test]
fn checkpoint_restart_hydrates_durable_checkpoint_and_explains_empty_session() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let root = project.path();
    let file = root.join("checkpoint-target.txt");
    fs::write(&file, "original\n").unwrap();

    let session = "checkpoint-restart-session";
    let mut first = AftProcess::spawn();
    configure_unrestricted_with_storage(
        &mut first,
        root,
        storage.path(),
        "cfg-checkpoint-restart-first",
    );
    let create = first.send(
        &serde_json::to_string(&serde_json::json!({
            "id": "checkpoint-restart-create",
            "command": "checkpoint",
            "session_id": session,
            "name": "restart-checkpoint",
            "files": [file.display().to_string()],
        }))
        .unwrap(),
    );
    assert_eq!(create["success"], true, "create: {create:?}");
    let storage_path = create["storage_path"].as_str().unwrap();
    assert!(std::path::Path::new(storage_path).is_dir());
    assert_eq!(
        create["durability"],
        format!("durable on disk at {storage_path}; survives restarts")
    );
    assert!(first.shutdown().success());

    fs::write(&file, "mutated\n").unwrap();
    let mut restarted = AftProcess::spawn();
    configure_unrestricted_with_storage(
        &mut restarted,
        root,
        storage.path(),
        "cfg-checkpoint-restart-second",
    );
    let list = restarted.send(
        &serde_json::to_string(&serde_json::json!({
            "id": "checkpoint-restart-list",
            "command": "tool_call",
            "session_id": session,
            "name": "safety",
            "arguments": { "op": "list" },
        }))
        .unwrap(),
    );
    assert_eq!(list["success"], true, "list: {list:?}");
    assert_eq!(list["checkpoints"].as_array().unwrap().len(), 1);
    assert_eq!(
        list["durability"],
        "durable checkpoints are hydrated from disk and survive restarts"
    );

    let restore = restarted.send(
        &serde_json::to_string(&serde_json::json!({
            "id": "checkpoint-restart-restore",
            "command": "tool_call",
            "session_id": session,
            "name": "safety",
            "arguments": { "op": "restore", "name": "restart-checkpoint" },
        }))
        .unwrap(),
    );
    assert_eq!(restore["success"], true, "restore: {restore:?}");
    assert_eq!(fs::read_to_string(&file).unwrap(), "original\n");

    let empty = restarted.send(
        &serde_json::to_string(&serde_json::json!({
            "id": "checkpoint-restart-empty-list",
            "command": "tool_call",
            "session_id": "empty-post-restart-session",
            "name": "safety",
            "arguments": { "op": "list" },
        }))
        .unwrap(),
    );
    assert_eq!(empty["success"], true, "empty list: {empty:?}");
    assert_eq!(empty["checkpoints"], serde_json::json!([]));
    assert_eq!(
        empty["durability"],
        "no durable checkpoints found on disk; in-memory checkpoints do not survive restarts"
    );
    assert!(empty["text"]
        .as_str()
        .is_some_and(|text| text.contains("in-memory checkpoints do not survive restarts")));
    assert!(restarted.shutdown().success());
}

#[test]
fn test_undo_restores_previous_version() {
    let dir = temp_dir("undo_restore");
    let file = dir.join("target.txt");

    fs::write(&file, "version-1").unwrap();

    let mut aft = AftProcess::spawn();

    // Snapshot the original
    let resp = aft.send(&format!(
        r#"{{"id":"snap-1","command":"snapshot","file":{}}}"#,
        crate::helpers::json_string(&file.display())
    ));
    assert_eq!(resp["success"], true);

    // Overwrite externally
    fs::write(&file, "version-2").unwrap();
    assert_eq!(fs::read_to_string(&file).unwrap(), "version-2");

    // Undo → should restore version-1
    let resp = aft.send(&format!(
        r#"{{"id":"undo-1","command":"undo","file":{}}}"#,
        crate::helpers::json_string(&file.display())
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
        r#"{{"id":"undo-after-edit","command":"undo","file":{}}}"#,
        crate::helpers::json_string(&file.display())
    ));
    assert_eq!(undo["success"], true, "undo should succeed: {undo:?}");
    assert_eq!(fs::read_to_string(&file).unwrap(), "hello world\n");

    let history = aft.send(&format!(
        r#"{{"id":"history-after-undo","command":"edit_history","file":{}}}"#,
        crate::helpers::json_string(&file.display())
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
    let file_a_key = fs::canonicalize(&file_a).unwrap();
    let file_b_key = fs::canonicalize(&file_b).unwrap();

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
    let restored = undo["restored"].as_array().unwrap();
    let mut restored_paths = restored
        .iter()
        .map(|entry| entry["path"].as_str().unwrap())
        .collect::<Vec<_>>();
    restored_paths.sort_unstable();
    let mut expected_paths = vec![file_a_key.to_str().unwrap(), file_b_key.to_str().unwrap()];
    expected_paths.sort_unstable();
    assert_eq!(restored_paths, expected_paths);
    assert!(
        restored.iter().all(|entry| entry["backup_id"].is_string()),
        "every content restore should retain its backup id: {undo:?}"
    );
    assert_eq!(fs::read_to_string(&file_a).unwrap(), "original-a");
    assert_eq!(fs::read_to_string(&file_b).unwrap(), "original-b");

    let status = aft.shutdown();
    assert!(status.success());
}

#[cfg(unix)]
#[test]
fn failed_delete_does_not_shadow_the_previous_undo_operation() {
    let dir = temp_dir("failed_delete_does_not_shadow_undo");
    let edited = dir.join("edited.txt");
    let protected_dir = dir.join("protected");
    let victim = protected_dir.join("victim.txt");
    fs::create_dir_all(&protected_dir).unwrap();
    fs::write(&edited, "before edit\n").unwrap();
    fs::write(&victim, "untouched victim\n").unwrap();

    let mut aft = AftProcess::spawn();
    let edit = serde_json::json!({
        "id": "edit-before-failed-delete",
        "command": "edit_match",
        "file": edited.display().to_string(),
        "match": "before edit",
        "replacement": "after edit",
    });
    let edit_resp = aft.send(&serde_json::to_string(&edit).unwrap());
    assert_eq!(edit_resp["success"], true, "edit: {edit_resp:?}");
    assert_eq!(fs::read_to_string(&edited).unwrap(), "after edit\n");

    fs::set_permissions(&protected_dir, fs::Permissions::from_mode(0o555)).unwrap();
    let delete = serde_json::json!({
        "id": "permission-denied-delete",
        "command": "delete_file",
        "file": victim.display().to_string(),
    });
    let delete_resp = aft.send(&serde_json::to_string(&delete).unwrap());
    assert_eq!(delete_resp["success"], false, "delete: {delete_resp:?}");
    assert_eq!(delete_resp["code"], "io_error");
    assert_eq!(fs::read_to_string(&victim).unwrap(), "untouched victim\n");

    let undo = aft.send(r#"{"id":"undo-after-failed-delete","command":"undo"}"#);
    assert_eq!(undo["success"], true, "undo: {undo:?}");
    assert_eq!(undo["restored_count"], 1);
    assert_eq!(fs::read_to_string(&edited).unwrap(), "before edit\n");
    assert_eq!(fs::read_to_string(&victim).unwrap(), "untouched victim\n");

    fs::set_permissions(&protected_dir, fs::Permissions::from_mode(0o755)).unwrap();
    let status = aft.shutdown();
    assert!(status.success());
}

#[cfg(unix)]
#[test]
fn batch_failed_delete_has_no_history_and_undo_restores_only_deleted_file() {
    let dir = temp_dir("batch_failed_delete_history");
    let deleted = dir.join("deleted.txt");
    let protected_dir = dir.join("protected");
    let victim = protected_dir.join("victim.txt");
    fs::create_dir_all(&protected_dir).unwrap();
    fs::write(&deleted, "restore me\n").unwrap();
    fs::write(&victim, "never deleted\n").unwrap();
    fs::set_permissions(&protected_dir, fs::Permissions::from_mode(0o555)).unwrap();

    let mut aft = AftProcess::spawn();
    let delete = serde_json::json!({
        "id": "mixed-delete",
        "command": "delete_file",
        "files": [victim.display().to_string(), deleted.display().to_string()],
    });
    let delete_resp = aft.send(&serde_json::to_string(&delete).unwrap());
    assert_eq!(delete_resp["success"], true, "delete: {delete_resp:?}");
    assert_eq!(delete_resp["complete"], false);
    assert_eq!(delete_resp["deleted"].as_array().unwrap().len(), 1);
    assert_eq!(delete_resp["skipped_files"].as_array().unwrap().len(), 1);
    assert!(!deleted.exists());
    assert_eq!(fs::read_to_string(&victim).unwrap(), "never deleted\n");

    let history = aft.send(&format!(
        r#"{{"id":"failed-delete-history","command":"edit_history","file":{}}}"#,
        crate::helpers::json_string(&victim.display())
    ));
    assert_eq!(history["success"], true, "history: {history:?}");
    assert!(history["entries"].as_array().unwrap().is_empty());

    let undo = aft.send(r#"{"id":"undo-mixed-delete","command":"undo"}"#);
    assert_eq!(undo["success"], true, "undo: {undo:?}");
    assert_eq!(undo["restored_count"], 1);
    assert_eq!(fs::read_to_string(&deleted).unwrap(), "restore me\n");
    assert_eq!(fs::read_to_string(&victim).unwrap(), "never deleted\n");

    fs::set_permissions(&protected_dir, fs::Permissions::from_mode(0o755)).unwrap();
    let status = aft.shutdown();
    assert!(status.success());
}

#[cfg(unix)]
#[test]
fn failed_recursive_delete_does_not_keep_backups_for_intact_files() {
    let dir = temp_dir("failed_recursive_delete_history");
    let tree = dir.join("tree");
    let protected_dir = tree.join("protected");
    let victim = protected_dir.join("victim.txt");
    fs::create_dir_all(&protected_dir).unwrap();
    fs::write(&victim, "never deleted\n").unwrap();
    fs::set_permissions(&protected_dir, fs::Permissions::from_mode(0o555)).unwrap();

    let mut aft = AftProcess::spawn();
    let delete = serde_json::json!({
        "id": "permission-denied-recursive-delete",
        "command": "delete_file",
        "file": tree.display().to_string(),
        "recursive": true,
    });
    let delete_resp = aft.send(&serde_json::to_string(&delete).unwrap());
    assert_eq!(delete_resp["success"], false, "delete: {delete_resp:?}");
    assert_eq!(delete_resp["code"], "io_error");
    assert_eq!(fs::read_to_string(&victim).unwrap(), "never deleted\n");

    let history = aft.send(&format!(
        r#"{{"id":"recursive-delete-history","command":"edit_history","file":{}}}"#,
        crate::helpers::json_string(&victim.display())
    ));
    assert_eq!(history["success"], true, "history: {history:?}");
    assert!(history["entries"].as_array().unwrap().is_empty());

    fs::set_permissions(&protected_dir, fs::Permissions::from_mode(0o755)).unwrap();
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
    let reported_path = fs::canonicalize(&file).unwrap();

    let undo = aft.send(r#"{"id":"undo-write-created","command":"undo"}"#);
    assert_eq!(undo["success"], true, "undo: {undo:?}");
    assert_eq!(undo["restored_count"], 1, "undo result: {undo:?}");
    assert_eq!(undo["restored"].as_array().unwrap().len(), 1);
    assert_eq!(
        undo["restored"][0]["path"],
        reported_path.display().to_string(),
        "undo should report the path it removed: {undo:?}"
    );
    assert!(undo["restored"][0]["backup_id"].is_string());
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
        "config": user_config(serde_json::json!({ "restrict_to_project_root": true })),
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
fn relative_symlink_file_delete_is_rejected_with_project_restriction() {
    let dir = temp_dir("delete_relative_symlink_restricted");
    let target = dir.join("target.txt");
    let symlink = dir.join("target-link.txt");

    fs::write(&target, "target content").unwrap();
    std::os::unix::fs::symlink(&target, &symlink).unwrap();

    let mut aft = AftProcess::spawn();
    configure_restricted(&mut aft, &dir, "cfg-delete-relative-symlink");

    let delete = serde_json::json!({
        "id": "delete-relative-symlink-restricted",
        "command": "delete_file",
        "file": "target-link.txt",
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
    assert!(
        std::fs::symlink_metadata(&symlink).is_ok(),
        "symlink should remain intact"
    );
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
fn batch_with_only_failed_recursive_delete_reports_failure() {
    let dir = temp_dir("delete_recursive_batch_all_failed");
    let target_dir = temp_dir("delete_recursive_batch_all_failed_target");
    let outside_file = target_dir.join("outside.txt");
    let symlink = dir.join("outside-link.txt");

    fs::write(&outside_file, "outside").unwrap();
    std::os::unix::fs::symlink(&outside_file, &symlink).unwrap();

    let mut aft = AftProcess::spawn();
    let delete = serde_json::json!({
        "id": "delete-file-batch-all-failed",
        "command": "delete_file",
        "files": [dir.display().to_string()],
        "recursive": true,
    });
    let resp = aft.send(&serde_json::to_string(&delete).unwrap());

    assert_eq!(resp["success"], false, "delete should fail: {resp:?}");
    assert_eq!(resp["code"], "delete_failed");
    assert_eq!(resp["all_failed"], true);
    assert_eq!(resp["complete"], false);
    assert_eq!(resp["skipped_files"].as_array().unwrap().len(), 1);
    let message = resp["message"].as_str().expect("failure message");
    assert!(message.contains("delete failed for all 1 file(s)"));
    assert!(message.contains(&dir.display().to_string()));
    assert!(message.contains("symlink"));
    assert!(dir.exists(), "directory should remain intact");
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
        r#"{{"id":"s1","command":"snapshot","file":{}}}"#,
        crate::helpers::json_string(&file.display())
    ));

    // Modify and snapshot v2
    fs::write(&file, "v2").unwrap();
    aft.send(&format!(
        r#"{{"id":"s2","command":"snapshot","file":{}}}"#,
        crate::helpers::json_string(&file.display())
    ));

    // Modify and snapshot v3
    fs::write(&file, "v3").unwrap();
    aft.send(&format!(
        r#"{{"id":"s3","command":"snapshot","file":{}}}"#,
        crate::helpers::json_string(&file.display())
    ));

    // Query edit history
    let resp = aft.send(&format!(
        r#"{{"id":"hist","command":"edit_history","file":{}}}"#,
        crate::helpers::json_string(&file.display())
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
        r#"{{"id":"cp1","command":"checkpoint","name":"first","files":[{}]}}"#,
        crate::helpers::json_string(&file_a.display())
    ));
    assert_eq!(resp["success"], true);

    // Create checkpoint with 2 files
    let resp = aft.send(&format!(
        r#"{{"id":"cp2","command":"checkpoint","name":"second","files":[{},{}]}}"#,
        crate::helpers::json_string(&file_a.display()),
        crate::helpers::json_string(&file_b.display())
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
        r#"{{"id":"undo-err","command":"undo","file":{}}}"#,
        crate::helpers::json_string(&file.display())
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
        r#"{{"id":"ow1","command":"checkpoint","name":"reusable","files":[{}]}}"#,
        crate::helpers::json_string(&file_a.display())
    ));
    assert_eq!(resp["success"], true);
    assert_eq!(resp["file_count"], 1);

    // Modify files
    fs::write(&file_a, "a-v2").unwrap();
    fs::write(&file_b, "b-v2").unwrap();

    // Overwrite checkpoint "reusable" with both files (different content now)
    let resp = aft.send(&format!(
        r#"{{"id":"ow2","command":"checkpoint","name":"reusable","files":[{},{}]}}"#,
        crate::helpers::json_string(&file_a.display()),
        crate::helpers::json_string(&file_b.display())
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
        r#"{{"id":"hist-cap","command":"edit_history","file":{}}}"#,
        crate::helpers::json_string(&file.display())
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
            r#"{{"id":"undo-{expected}","command":"undo","file":{}}}"#,
            crate::helpers::json_string(&file.display())
        ));
        assert_eq!(undo["success"], true, "undo {expected} failed: {undo:?}");
        assert_eq!(fs::read_to_string(&file).unwrap(), format!("v{expected}"));
    }

    let no_more_history = aft.send(&format!(
        r#"{{"id":"undo-empty","command":"undo","file":{}}}"#,
        crate::helpers::json_string(&file.display())
    ));
    assert_eq!(no_more_history["success"], false);
    assert_eq!(no_more_history["code"], "no_undo_history");
    assert_eq!(fs::read_to_string(&file).unwrap(), "v1");

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn undo_preview_reports_operation_paths_without_mutating() {
    let dir = temp_dir("undo_preview_operation");
    let file_a = dir.join("a.txt");
    let file_b = dir.join("b.txt");

    fs::write(&file_a, "original-a").unwrap();
    fs::write(&file_b, "original-b").unwrap();
    let expected_a = fs::canonicalize(&file_a).unwrap();
    let expected_b = fs::canonicalize(&file_b).unwrap();

    let mut aft = AftProcess::spawn();
    let delete = serde_json::json!({
        "id": "delete-for-preview",
        "command": "delete_file",
        "files": [file_a.display().to_string(), file_b.display().to_string()],
    });
    let delete_resp = aft.send(&serde_json::to_string(&delete).unwrap());
    assert_eq!(delete_resp["success"], true, "delete: {delete_resp:?}");
    assert!(!file_a.exists());
    assert!(!file_b.exists());

    let preview = aft.send(r#"{"id":"undo-preview-operation","command":"undo_preview"}"#);
    assert_eq!(preview["success"], true, "preview: {preview:?}");
    assert_eq!(preview["count"], 2);
    let paths: Vec<&str> = preview["paths"]
        .as_array()
        .expect("paths array")
        .iter()
        .map(|path| path.as_str().expect("path string"))
        .collect();
    assert!(paths.contains(&expected_a.to_str().unwrap()));
    assert!(paths.contains(&expected_b.to_str().unwrap()));
    assert!(!file_a.exists(), "preview must not restore file_a");
    assert!(!file_b.exists(), "preview must not restore file_b");

    let preview_again =
        aft.send(r#"{"id":"undo-preview-operation-again","command":"undo_preview"}"#);
    assert_eq!(
        preview_again["success"], true,
        "second preview: {preview_again:?}"
    );
    assert_eq!(preview_again["paths"], preview["paths"]);

    let undo = aft.send(r#"{"id":"undo-after-preview","command":"undo"}"#);
    assert_eq!(undo["success"], true, "undo: {undo:?}");
    assert_eq!(fs::read_to_string(&file_a).unwrap(), "original-a");
    assert_eq!(fs::read_to_string(&file_b).unwrap(), "original-b");

    let status = aft.shutdown();
    assert!(status.success());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn undo_preview_with_file_reports_path_without_mutating() {
    let dir = temp_dir("undo_preview_file");
    let file = dir.join("target.txt");
    fs::write(&file, "version-1").unwrap();
    let expected = fs::canonicalize(&file).unwrap();

    let mut aft = AftProcess::spawn();
    let snap = aft.send(&format!(
        r#"{{"id":"snap-preview-file","command":"snapshot","file":{}}}"#,
        crate::helpers::json_string(&file.display())
    ));
    assert_eq!(snap["success"], true, "snapshot: {snap:?}");

    fs::write(&file, "version-2").unwrap();

    let preview = aft.send(&format!(
        r#"{{"id":"undo-preview-file","command":"undo_preview","file":{}}}"#,
        crate::helpers::json_string(&file.display())
    ));
    assert_eq!(preview["success"], true, "preview: {preview:?}");
    assert_eq!(preview["count"], 1);
    assert_eq!(preview["paths"][0], expected.display().to_string());
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "version-2",
        "preview must not mutate file contents"
    );

    let undo = aft.send(&format!(
        r#"{{"id":"undo-after-file-preview","command":"undo","file":{}}}"#,
        crate::helpers::json_string(&file.display())
    ));
    assert_eq!(undo["success"], true, "undo: {undo:?}");
    assert_eq!(fs::read_to_string(&file).unwrap(), "version-1");

    let status = aft.shutdown();
    assert!(status.success());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn checkpoint_paths_reports_restore_targets_without_mutating() {
    let dir = temp_dir("checkpoint_paths_preview");
    let file_a = dir.join("a.txt");
    let file_b = dir.join("b.txt");
    fs::write(&file_a, "checkpoint-a").unwrap();
    fs::write(&file_b, "checkpoint-b").unwrap();

    let mut aft = AftProcess::spawn();
    let create = aft.send(&format!(
        r#"{{"id":"checkpoint-paths-create","command":"checkpoint","name":"paths","files":[{},{}]}}"#,
        crate::helpers::json_string(&file_a.display()),
        crate::helpers::json_string(&file_b.display())
    ));
    assert_eq!(create["success"], true, "checkpoint create: {create:?}");

    fs::write(&file_a, "modified-a").unwrap();
    fs::write(&file_b, "modified-b").unwrap();

    let preview =
        aft.send(r#"{"id":"checkpoint-paths","command":"checkpoint_paths","name":"paths"}"#);
    assert_eq!(preview["success"], true, "checkpoint paths: {preview:?}");
    assert_eq!(preview["name"], "paths");
    assert_eq!(preview["file_count"], 2);
    let paths: Vec<&str> = preview["paths"]
        .as_array()
        .expect("paths array")
        .iter()
        .map(|path| path.as_str().expect("path string"))
        .collect();
    assert!(paths.contains(&file_a.to_str().unwrap()));
    assert!(paths.contains(&file_b.to_str().unwrap()));
    assert_eq!(fs::read_to_string(&file_a).unwrap(), "modified-a");
    assert_eq!(fs::read_to_string(&file_b).unwrap(), "modified-b");

    let restore = aft
        .send(r#"{"id":"checkpoint-paths-restore","command":"restore_checkpoint","name":"paths"}"#);
    assert_eq!(restore["success"], true, "restore: {restore:?}");
    assert_eq!(fs::read_to_string(&file_a).unwrap(), "checkpoint-a");
    assert_eq!(fs::read_to_string(&file_b).unwrap(), "checkpoint-b");

    let status = aft.shutdown();
    assert!(status.success());
    let _ = fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn restricted_checkpoint_creation_preserves_in_root_final_symlink() {
    let container = tempfile::tempdir().unwrap();
    let root = container.path().join("project");
    let outside = container.path().join("outside.txt");
    fs::create_dir_all(&root).unwrap();
    let root = fs::canonicalize(root).unwrap();
    let target = root.join("target.txt");
    let link = root.join("link.txt");
    fs::write(&outside, "outside-control").unwrap();
    fs::write(&target, "checkpoint-target").unwrap();
    std::os::unix::fs::symlink("target.txt", &link).unwrap();

    let mut aft = AftProcess::spawn();
    configure_restricted(&mut aft, &root, "cfg-checkpoint-final-symlink");

    let control = aft.send(
        &serde_json::to_string(&serde_json::json!({
            "id": "read-outside-control",
            "command": "read",
            "file": outside.display().to_string(),
        }))
        .unwrap(),
    );
    assert_eq!(
        control["success"], false,
        "restriction control: {control:?}"
    );
    assert_eq!(control["code"], "path_outside_root");

    let create = aft.send(
        &serde_json::to_string(&serde_json::json!({
            "id": "checkpoint-final-symlink",
            "command": "checkpoint",
            "name": "final-symlink",
            "files": [link.display().to_string()],
        }))
        .unwrap(),
    );
    assert_eq!(create["success"], true, "checkpoint: {create:?}");

    let paths = aft.send(
        r#"{"id":"paths-final-symlink","command":"checkpoint_paths","name":"final-symlink"}"#,
    );
    assert_eq!(paths["success"], true, "checkpoint paths: {paths:?}");
    assert_eq!(
        paths["paths"],
        serde_json::json!([link.display().to_string()])
    );

    fs::remove_file(&link).unwrap();
    fs::write(&link, "replacement-file").unwrap();
    fs::write(&target, "modified-target").unwrap();

    let restore = aft.send(
        r#"{"id":"restore-final-symlink","command":"restore_checkpoint","name":"final-symlink"}"#,
    );
    assert_eq!(restore["success"], true, "restore: {restore:?}");
    assert!(fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(
        fs::read_link(&link).unwrap(),
        std::path::Path::new("target.txt")
    );
    assert_eq!(fs::read_to_string(&target).unwrap(), "modified-target");

    let status = aft.shutdown();
    assert!(status.success());
}

#[cfg(unix)]
#[test]
fn restricted_checkpoint_creation_rejects_symlinked_parent_escape() {
    let container = tempfile::tempdir().unwrap();
    let root = container.path().join("project");
    let outside = container.path().join("outside");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&outside).unwrap();
    let outside_file = outside.join("external.txt");
    fs::write(&outside_file, "external-original").unwrap();
    std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();

    let mut aft = AftProcess::spawn();
    configure_restricted(&mut aft, &root, "cfg-checkpoint-parent-escape");
    let create = aft.send(
        &serde_json::to_string(&serde_json::json!({
            "id": "checkpoint-parent-escape",
            "command": "checkpoint",
            "name": "parent-escape",
            "files": [root.join("escape/external.txt").display().to_string()],
        }))
        .unwrap(),
    );

    assert_eq!(create["success"], false, "checkpoint: {create:?}");
    assert_eq!(create["code"], "path_outside_root");
    assert_eq!(
        fs::read_to_string(&outside_file).unwrap(),
        "external-original"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn checkpoint_regular_file_keys_and_restores_identically_when_restricted() {
    let container = tempfile::tempdir().unwrap();
    let root = container.path().join("project");
    fs::create_dir_all(&root).unwrap();
    let root = fs::canonicalize(root).unwrap();
    let file = root.join("ordinary.txt");
    let mut checkpoint_paths = Vec::new();

    for (restricted, label) in [(false, "unrestricted"), (true, "restricted")] {
        fs::write(&file, "checkpoint-ordinary").unwrap();
        let mut aft = AftProcess::spawn();
        if restricted {
            configure_restricted(&mut aft, &root, "cfg-checkpoint-ordinary");
        }

        let create = aft.send(
            &serde_json::to_string(&serde_json::json!({
                "id": format!("checkpoint-ordinary-{label}"),
                "command": "checkpoint",
                "name": "ordinary",
                "files": [file.display().to_string()],
            }))
            .unwrap(),
        );
        assert_eq!(create["success"], true, "checkpoint {label}: {create:?}");

        let paths =
            aft.send(r#"{"id":"paths-ordinary","command":"checkpoint_paths","name":"ordinary"}"#);
        assert_eq!(
            paths["success"], true,
            "checkpoint paths {label}: {paths:?}"
        );
        checkpoint_paths.push(paths["paths"].clone());

        fs::write(&file, "modified-ordinary").unwrap();
        let restore = aft
            .send(r#"{"id":"restore-ordinary","command":"restore_checkpoint","name":"ordinary"}"#);
        assert_eq!(restore["success"], true, "restore {label}: {restore:?}");
        assert_eq!(fs::read_to_string(&file).unwrap(), "checkpoint-ordinary");

        let status = aft.shutdown();
        assert!(status.success());
    }

    assert_eq!(checkpoint_paths[0], checkpoint_paths[1]);
    assert_eq!(
        checkpoint_paths[0],
        serde_json::json!([file.display().to_string()])
    );
}

#[cfg(unix)]
#[test]
fn restricted_checkpoint_creation_preserves_external_target_symlink() {
    let container = tempfile::tempdir().unwrap();
    let root = container.path().join("project");
    let outside = container.path().join("external.txt");
    fs::create_dir_all(&root).unwrap();
    let root = fs::canonicalize(root).unwrap();
    let link = root.join("link.txt");
    fs::write(&outside, "external-original").unwrap();
    std::os::unix::fs::symlink(&outside, &link).unwrap();

    let mut aft = AftProcess::spawn();
    configure_restricted(&mut aft, &root, "cfg-checkpoint-external-link");
    let create = aft.send(
        &serde_json::to_string(&serde_json::json!({
            "id": "checkpoint-external-link",
            "command": "checkpoint",
            "name": "external-link",
            "files": [link.display().to_string()],
        }))
        .unwrap(),
    );
    assert_eq!(create["success"], true, "checkpoint: {create:?}");
    assert_eq!(fs::read_to_string(&outside).unwrap(), "external-original");

    let paths = aft.send(
        r#"{"id":"paths-external-link","command":"checkpoint_paths","name":"external-link"}"#,
    );
    assert_eq!(paths["success"], true, "checkpoint paths: {paths:?}");
    assert_eq!(
        paths["paths"],
        serde_json::json!([link.display().to_string()])
    );

    fs::remove_file(&link).unwrap();
    fs::write(&link, "replacement-file").unwrap();
    fs::write(&outside, "external-modified").unwrap();

    let restore = aft.send(
        r#"{"id":"restore-external-link","command":"restore_checkpoint","name":"external-link"}"#,
    );
    assert_eq!(restore["success"], true, "restore: {restore:?}");
    assert!(fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read_link(&link).unwrap(), outside);
    assert_eq!(fs::read_to_string(&outside).unwrap(), "external-modified");

    let status = aft.shutdown();
    assert!(status.success());
}

#[cfg(unix)]
#[test]
fn restricted_checkpoint_restore_replaces_in_root_symlink_without_changing_its_target() {
    let root = temp_dir("checkpoint_restore_in_root_symlink");
    let file = root.join("a.txt");
    let target = root.join("b.txt");
    fs::write(&file, "checkpoint-a").unwrap();
    fs::write(&target, "target-b").unwrap();

    let mut aft = AftProcess::spawn();
    configure_restricted(&mut aft, &root, "cfg-checkpoint-in-root-symlink");
    let create = aft.send(
        &serde_json::to_string(&serde_json::json!({
            "id": "checkpoint-in-root-symlink",
            "command": "checkpoint",
            "name": "in-root-symlink",
            "files": [file.display().to_string()],
        }))
        .unwrap(),
    );
    assert_eq!(create["success"], true, "checkpoint: {create:?}");

    fs::remove_file(&file).unwrap();
    std::os::unix::fs::symlink(&target, &file).unwrap();

    let restore = aft.send(
        r#"{"id":"restore-in-root-symlink","command":"restore_checkpoint","name":"in-root-symlink"}"#,
    );
    assert_eq!(restore["success"], true, "restore: {restore:?}");
    assert_eq!(restore["name"], "in-root-symlink");
    assert_eq!(restore["file_count"], 1);
    assert!(restore["created_at"].is_u64());
    assert!(!fs::symlink_metadata(&file)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read_to_string(&file).unwrap(), "checkpoint-a");
    assert_eq!(fs::read_to_string(&target).unwrap(), "target-b");

    let status = aft.shutdown();
    assert!(status.success());
    let _ = fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn force_restricted_checkpoint_restore_replaces_external_target_symlink() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::NamedTempFile::new().unwrap();
    fs::write(outside.path(), "outside-unchanged").unwrap();
    let file = root.path().join("a.txt");
    fs::write(&file, "checkpoint-a").unwrap();

    let ctx = AppContext::new(
        Box::new(StubProvider),
        Config {
            project_root: Some(root.path().to_path_buf()),
            restrict_to_project_root: false,
            ..Config::default()
        },
    );
    let create: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "force-checkpoint-external-target",
        "command": "checkpoint",
        "name": "external-target",
        "files": [file.display().to_string()],
    }))
    .unwrap();
    let create_response = ctx.with_force_restrict(&create.id, || handle_checkpoint(&create, &ctx));
    let create_value = serde_json::to_value(create_response).unwrap();
    assert_eq!(
        create_value["success"], true,
        "checkpoint: {create_value:?}"
    );

    fs::remove_file(&file).unwrap();
    std::os::unix::fs::symlink(outside.path(), &file).unwrap();

    let restore: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "force-restore-external-target",
        "command": "restore_checkpoint",
        "name": "external-target",
    }))
    .unwrap();
    let restore_response =
        ctx.with_force_restrict(&restore.id, || handle_restore_checkpoint(&restore, &ctx));
    let restore_value = serde_json::to_value(restore_response).unwrap();

    assert_eq!(restore_value["success"], true, "restore: {restore_value:?}");
    assert!(!fs::symlink_metadata(&file)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read_to_string(&file).unwrap(), "checkpoint-a");
    assert_eq!(
        fs::read_to_string(outside.path()).unwrap(),
        "outside-unchanged"
    );
}

#[cfg(unix)]
#[test]
fn restricted_checkpoint_restore_replaces_dangling_symlink() {
    let root = temp_dir("checkpoint_restore_dangling_symlink");
    let file = root.join("a.txt");
    let missing_target = root.join("missing.txt");
    fs::write(&file, "checkpoint-a").unwrap();

    let mut aft = AftProcess::spawn();
    configure_restricted(&mut aft, &root, "cfg-checkpoint-dangling-symlink");
    let create = aft.send(
        &serde_json::to_string(&serde_json::json!({
            "id": "checkpoint-dangling-symlink",
            "command": "checkpoint",
            "name": "dangling-symlink",
            "files": [file.display().to_string()],
        }))
        .unwrap(),
    );
    assert_eq!(create["success"], true, "checkpoint: {create:?}");

    fs::remove_file(&file).unwrap();
    std::os::unix::fs::symlink(&missing_target, &file).unwrap();

    let restore = aft.send(
        r#"{"id":"restore-dangling-symlink","command":"restore_checkpoint","name":"dangling-symlink"}"#,
    );
    assert_eq!(restore["success"], true, "restore: {restore:?}");
    assert!(!fs::symlink_metadata(&file)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read_to_string(&file).unwrap(), "checkpoint-a");
    assert!(!missing_target.exists());

    let status = aft.shutdown();
    assert!(status.success());
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn restricted_checkpoint_restore_rejects_stored_lexical_path_outside_root() {
    let container = tempfile::tempdir().unwrap();
    let root = container.path().join("project");
    let outside = container.path().join("outside.txt");
    fs::create_dir_all(&root).unwrap();
    fs::write(&outside, "checkpoint-outside").unwrap();

    let mut aft = AftProcess::spawn();
    let create = aft.send(
        &serde_json::to_string(&serde_json::json!({
            "id": "checkpoint-outside-before-restriction",
            "command": "checkpoint",
            "name": "outside-before-restriction",
            "files": [outside.display().to_string()],
        }))
        .unwrap(),
    );
    assert_eq!(create["success"], true, "checkpoint: {create:?}");
    fs::write(&outside, "modified-outside").unwrap();

    configure_restricted(&mut aft, &root, "cfg-checkpoint-outside-path");
    let restore = aft.send(
        r#"{"id":"restore-outside-path","command":"restore_checkpoint","name":"outside-before-restriction"}"#,
    );
    assert_eq!(restore["success"], false, "restore: {restore:?}");
    assert_eq!(restore["code"], "path_outside_root");
    assert_eq!(fs::read_to_string(&outside).unwrap(), "modified-outside");

    let status = aft.shutdown();
    assert!(status.success());
}

#[cfg(unix)]
#[test]
fn restricted_undo_preview_and_undo_preserve_symlinked_backup_key() {
    let root = temp_dir("undo_symlinked_backup_key");
    let file = root.join("a.txt");
    let target = root.join("b.txt");
    fs::write(&file, "original-a").unwrap();
    fs::write(&target, "target-b").unwrap();

    let mut aft = AftProcess::spawn();
    configure_restricted(&mut aft, &root, "cfg-undo-symlinked-key");
    let edit = aft.send(
        &serde_json::to_string(&serde_json::json!({
            "id": "edit-before-symlinked-undo",
            "command": "edit_match",
            "file": file.display().to_string(),
            "match": "original-a",
            "replacement": "modified-a",
        }))
        .unwrap(),
    );
    assert_eq!(edit["success"], true, "edit: {edit:?}");

    fs::remove_file(&file).unwrap();
    std::os::unix::fs::symlink(&target, &file).unwrap();

    let preview = aft.send(
        &serde_json::to_string(&serde_json::json!({
            "id": "preview-symlinked-undo",
            "command": "undo_preview",
            "file": file.display().to_string(),
        }))
        .unwrap(),
    );
    assert_eq!(preview["success"], true, "preview: {preview:?}");
    assert_eq!(preview["count"], 1);
    assert_eq!(
        preview["paths"][0],
        fs::canonicalize(&root)
            .unwrap()
            .join("a.txt")
            .display()
            .to_string()
    );

    let undo = aft.send(
        &serde_json::to_string(&serde_json::json!({
            "id": "undo-symlinked-key",
            "command": "undo",
            "file": file.display().to_string(),
        }))
        .unwrap(),
    );
    assert_eq!(undo["success"], true, "undo: {undo:?}");
    assert!(!fs::symlink_metadata(&file)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read_to_string(&file).unwrap(), "original-a");
    assert_eq!(fs::read_to_string(&target).unwrap(), "target-b");

    let status = aft.shutdown();
    assert!(status.success());
    let _ = fs::remove_dir_all(&root);
}

/// Configure a project root WITHOUT path restriction. This is the default
/// plugin posture and the exact configuration under which the relative-path
/// undo hole reproduced: a relative path passed to `canonicalize_key` is joined
/// against the daemon's cwd (not the bound project root), so the per-session
/// stack lookup misses and reports a false `no_undo_history`.
fn configure_unrestricted(aft: &mut AftProcess, root: &std::path::Path, request_id: &str) {
    let response = aft.send(
        &serde_json::to_string(&serde_json::json!({
            "id": request_id,
            "command": "configure",
            "harness": "opencode",
            "project_root": root.display().to_string(),
        }))
        .unwrap(),
    );
    assert_eq!(response["success"], true, "configure: {response:?}");
}

fn configure_unrestricted_with_storage(
    aft: &mut AftProcess,
    root: &std::path::Path,
    storage: &std::path::Path,
    request_id: &str,
) {
    let response = aft.send(
        &serde_json::to_string(&serde_json::json!({
            "id": request_id,
            "command": "configure",
            "harness": "opencode",
            "project_root": root.display().to_string(),
            "storage_dir": storage.display().to_string(),
        }))
        .unwrap(),
    );
    assert_eq!(response["success"], true, "configure: {response:?}");
}

#[test]
fn relative_path_undo_restores_after_edit() {
    // Regression: a relative `file` passed to `undo` must resolve against the
    // bound project root so the backup key matches the path the mutating tool
    // recorded. Before the fix it was joined against the daemon's cwd, the
    // stack lookup missed, and the user got a false `no_undo_history`.
    let dir = temp_dir("relative_undo_after_edit");
    let file = dir.join("target.txt");
    fs::write(&file, "hello world\n").unwrap();

    let mut aft = AftProcess::spawn();
    configure_unrestricted(&mut aft, &dir, "cfg-relative-undo");

    let edit = serde_json::json!({
        "id": "edit-relative-undo",
        "command": "edit_match",
        "file": file.display().to_string(),
        "match": "world",
        "replacement": "rust",
    });
    let edit_resp = aft.send(&serde_json::to_string(&edit).unwrap());
    assert_eq!(edit_resp["success"], true, "edit: {edit_resp:?}");
    assert_eq!(fs::read_to_string(&file).unwrap(), "hello rust\n");

    // Send `undo` with a relative `file` value, matching the input that exposed
    // the path-resolution bug (a relative path was joined against the daemon's
    // cwd instead of the bound project root).
    let undo = aft.send(r#"{"id":"undo-relative","command":"undo","file":"target.txt"}"#);
    assert_eq!(undo["success"], true, "undo: {undo:?}");
    assert_eq!(fs::read_to_string(&file).unwrap(), "hello world\n");

    let status = aft.shutdown();
    assert!(status.success());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn relative_path_undo_preview_and_history_see_same_stack_as_absolute() {
    let dir = temp_dir("relative_preview_history");
    let file = dir.join("tracked.txt");
    fs::write(&file, "v1").unwrap();

    let mut aft = AftProcess::spawn();
    configure_unrestricted(&mut aft, &dir, "cfg-relative-preview-history");

    // Snapshot v1, then modify and snapshot v2, then modify to v3. The stack is
    // [v1, v2]; the top backup holds v2's content, so undo restores v2.
    aft.send(&format!(
        r#"{{"id":"s1","command":"snapshot","file":{}}}"#,
        crate::helpers::json_string(&file.display())
    ));
    fs::write(&file, "v2").unwrap();
    aft.send(&format!(
        r#"{{"id":"s2","command":"snapshot","file":{}}}"#,
        crate::helpers::json_string(&file.display())
    ));
    fs::write(&file, "v3").unwrap();

    // Relative-path history must see the same stack as the absolute path.
    let abs_history = aft.send(&format!(
        r#"{{"id":"hist-abs","command":"edit_history","file":{}}}"#,
        crate::helpers::json_string(&file.display())
    ));
    assert_eq!(abs_history["success"], true, "abs history: {abs_history:?}");
    let rel_history =
        aft.send(r#"{"id":"hist-rel","command":"edit_history","file":"tracked.txt"}"#);
    assert_eq!(rel_history["success"], true, "rel history: {rel_history:?}");
    assert_eq!(
        rel_history["entries"], abs_history["entries"],
        "relative and absolute history must agree"
    );

    // Relative-path preview must see the same stack as the absolute path.
    let abs_preview = aft.send(&format!(
        r#"{{"id":"preview-abs","command":"undo_preview","file":{}}}"#,
        crate::helpers::json_string(&file.display())
    ));
    assert_eq!(abs_preview["success"], true, "abs preview: {abs_preview:?}");
    let rel_preview =
        aft.send(r#"{"id":"preview-rel","command":"undo_preview","file":"tracked.txt"}"#);
    assert_eq!(rel_preview["success"], true, "rel preview: {rel_preview:?}");
    assert_eq!(
        rel_preview["paths"], abs_preview["paths"],
        "relative and absolute preview must agree"
    );

    // Relative-path undo restores the top of the same stack (v2's content).
    let undo = aft.send(r#"{"id":"undo-rel","command":"undo","file":"tracked.txt"}"#);
    assert_eq!(undo["success"], true, "undo: {undo:?}");
    assert_eq!(fs::read_to_string(&file).unwrap(), "v2");

    let status = aft.shutdown();
    assert!(status.success());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn relative_path_escaping_root_still_fails_validation() {
    let container = tempfile::tempdir().unwrap();
    let root = container.path().join("project");
    fs::create_dir_all(&root).unwrap();
    let outside = container.path().join("outside.txt");
    fs::write(&outside, "outside-control").unwrap();

    let mut aft = AftProcess::spawn();
    configure_restricted(&mut aft, &root, "cfg-relative-escape");

    // A relative path that lexically escapes the root must still be rejected.
    let undo = aft.send(r#"{"id":"undo-escape","command":"undo","file":"../outside.txt"}"#);
    assert_eq!(undo["success"], false, "undo escape: {undo:?}");
    assert_eq!(undo["code"], "path_outside_root");

    let preview =
        aft.send(r#"{"id":"preview-escape","command":"undo_preview","file":"../outside.txt"}"#);
    assert_eq!(preview["success"], false, "preview escape: {preview:?}");
    assert_eq!(preview["code"], "path_outside_root");

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn temp_path_mutations_report_missing_undo_and_increment_status_counter() {
    let dir = tempfile::tempdir().unwrap();
    let deleted = dir.path().join("deleted.txt");
    let written = dir.path().join("written.txt");
    let edited = dir.path().join("edited.txt");
    fs::write(&deleted, "delete me").unwrap();
    fs::write(&written, "before write").unwrap();
    fs::write(&edited, "before edit").unwrap();

    let mut aft = AftProcess::spawn_with_env(&[
        ("AFT_TEST_DISABLE_FILE_WATCHER", std::ffi::OsStr::new("0")),
        ("AFT_TEST_ALLOW_TEMP_BACKUPS", std::ffi::OsStr::new("0")),
    ]);

    let delete = aft.send(
        &serde_json::to_string(&serde_json::json!({
            "id": "temp-delete",
            "command": "delete_file",
            "file": deleted,
        }))
        .unwrap(),
    );
    assert_eq!(delete["success"], true, "delete: {delete:?}");
    assert_eq!(delete["backup_skipped_reason"], "temp_path");

    let write = aft.send(
        &serde_json::to_string(&serde_json::json!({
            "id": "temp-write",
            "command": "tool_call",
            "name": "write",
            "arguments": { "filePath": written, "content": "after write" },
        }))
        .unwrap(),
    );
    assert_eq!(write["success"], true, "write: {write:?}");
    assert_eq!(write["backup_skipped_reason"], "temp_path");
    assert!(write["text"]
        .as_str()
        .is_some_and(|text| text.contains("Undo is unavailable for this change")));

    let edit = aft.send(
        &serde_json::to_string(&serde_json::json!({
            "id": "temp-edit",
            "command": "tool_call",
            "name": "edit",
            "arguments": {
                "path": edited.display().to_string(),
                "edits": [{ "oldString": "before edit", "newString": "after edit" }],
            },
        }))
        .unwrap(),
    );
    assert_eq!(edit["success"], true, "edit: {edit:?}");
    assert_eq!(edit["backup_skipped_reason"], "temp_path");
    assert!(edit["text"]
        .as_str()
        .is_some_and(|text| text.contains("Undo is unavailable for this change")));

    let preview = aft.send(
        &serde_json::to_string(&serde_json::json!({
            "id": "temp-undo-preview",
            "command": "undo_preview",
            "file": edited.display().to_string(),
        }))
        .unwrap(),
    );
    assert_eq!(preview["success"], true, "undo preview: {preview:?}");
    assert_eq!(preview["backup_skipped_reason"], "temp_path");

    let undo = aft.send(
        &serde_json::to_string(&serde_json::json!({
            "id": "temp-undo",
            "command": "undo",
            "file": edited.display().to_string(),
        }))
        .unwrap(),
    );
    assert_eq!(undo["success"], false, "undo: {undo:?}");
    assert_eq!(undo["backup_skipped_reason"], "temp_path");
    assert!(undo["message"]
        .as_str()
        .is_some_and(|message| message.contains("undo is unavailable")));
    assert_eq!(fs::read_to_string(&edited).unwrap(), "after edit");

    let status = aft.send(r#"{"id":"temp-status","command":"status"}"#);
    assert!(status["backup_skipped_temp_path_total"]
        .as_u64()
        .is_some_and(|count| count >= 3));
    assert!(status["backup_skipped_too_large_total"].is_u64());

    assert!(aft.shutdown().success());
}

#[test]
fn undo_miss_message_echoes_resolved_absolute_path() {
    // The miss message is a defect surface of its own: "no undo history for:
    // <input path>" is indistinguishable from a genuine no-backups state, and
    // the two want opposite agent responses. Echoing the RESOLVED absolute path
    // turns a silent wrong answer into a visibly wrong input.
    let dir = temp_dir("undo_miss_absolute");
    let file = dir.join("never_snapshotted.txt");
    fs::write(&file, "content").unwrap();

    let mut aft = AftProcess::spawn();
    configure_unrestricted(&mut aft, &dir, "cfg-undo-miss-absolute");

    let undo = aft.send(r#"{"id":"undo-miss","command":"undo","file":"never_snapshotted.txt"}"#);
    assert_eq!(undo["success"], false, "undo: {undo:?}");
    assert_eq!(undo["code"], "no_undo_history");
    let message = undo["message"].as_str().unwrap();
    // The message must contain the resolved absolute path (root-joined), not the
    // raw relative input, so a mis-resolution is visible in the error itself.
    // Compare against the non-canonicalized absolute path: the message echoes the
    // root-joined spelling, while `fs::canonicalize` would resolve macOS
    // `/var` → `/private/var` and diverge.
    let resolved = dir.join("never_snapshotted.txt");
    assert!(
        message.contains(&resolved.display().to_string()),
        "miss message should echo the resolved absolute path, got: {message}"
    );

    let status = aft.shutdown();
    assert!(status.success());
    let _ = fs::remove_dir_all(&dir);
}
