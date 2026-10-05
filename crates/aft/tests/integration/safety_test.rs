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

/// Helper: create a directory for this test inside its private scratch dir,
/// which is removed when the test ends.
fn temp_dir(test_name: &str) -> std::path::PathBuf {
    let dir = crate::helpers::thread_scratch_dir()
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
fn checkpoint_restore_honors_one_file_scope() {
    let dir = tempfile::tempdir().unwrap();
    let file_a = dir.path().join("a.txt");
    let file_b = dir.path().join("b.txt");
    let file_c = dir.path().join("c.txt");
    fs::write(&file_a, "original-a").unwrap();
    fs::write(&file_b, "original-b").unwrap();
    fs::write(&file_c, "original-c").unwrap();

    let mut aft = AftProcess::spawn();
    let create = serde_json::json!({
        "id": "scoped-create",
        "command": "checkpoint",
        "name": "scoped",
        "files": [file_a, file_b, file_c],
    });
    let response = aft.send(&create.to_string());
    assert_eq!(response["success"], true, "checkpoint: {response:?}");

    fs::write(&file_a, "modified-a").unwrap();
    fs::write(&file_b, "modified-b").unwrap();
    let restore = serde_json::json!({
        "id": "scoped-restore",
        "command": "restore_checkpoint",
        "name": "scoped",
        "file": file_a,
    });
    let response = aft.send(&restore.to_string());
    assert_eq!(response["success"], true, "restore: {response:?}");
    assert_eq!(fs::read_to_string(&file_a).unwrap(), "original-a");
    assert_eq!(
        fs::read_to_string(&file_b).unwrap(),
        "modified-b",
        "a scoped restore must not overwrite another checkpoint file"
    );
    assert_eq!(fs::read_to_string(&file_c).unwrap(), "original-c");
    assert_eq!(response["file_count"], 1);
    assert_eq!(response["paths"], serde_json::json!([file_a]));

    assert!(aft.shutdown().success());
}

#[test]
fn checkpoint_restore_rejects_a_scope_path_absent_from_checkpoint_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let file_a = dir.path().join("a.txt");
    let file_b = dir.path().join("b.txt");
    let absent = dir.path().join("not-checkpointed.txt");
    fs::write(&file_a, "original-a").unwrap();
    fs::write(&file_b, "original-b").unwrap();
    fs::write(&absent, "outside-checkpoint").unwrap();

    let mut aft = AftProcess::spawn();
    let create = serde_json::json!({
        "id": "missing-scope-create",
        "command": "checkpoint",
        "name": "missing-scope",
        "files": [file_a, file_b],
    });
    let response = aft.send(&create.to_string());
    assert_eq!(response["success"], true, "checkpoint: {response:?}");

    fs::write(&file_a, "modified-a").unwrap();
    fs::write(&file_b, "modified-b").unwrap();
    let restore = serde_json::json!({
        "id": "missing-scope-restore",
        "command": "restore_checkpoint",
        "name": "missing-scope",
        "files": [file_a, absent],
    });
    let response = aft.send(&restore.to_string());
    assert_eq!(response["success"], false, "restore: {response:?}");
    assert_eq!(response["code"], "invalid_request");
    assert!(
        response["message"]
            .as_str()
            .is_some_and(|message| message.contains(absent.to_str().unwrap())),
        "missing path should be named: {response:?}"
    );
    assert_eq!(fs::read_to_string(&file_a).unwrap(), "modified-a");
    assert_eq!(fs::read_to_string(&file_b).unwrap(), "modified-b");
    assert_eq!(fs::read_to_string(&absent).unwrap(), "outside-checkpoint");

    assert!(aft.shutdown().success());
}

#[test]
fn checkpoint_restore_without_scope_restores_every_checkpoint_file() {
    let dir = tempfile::tempdir().unwrap();
    let files = [
        dir.path().join("a.txt"),
        dir.path().join("b.txt"),
        dir.path().join("c.txt"),
    ];
    for (index, file) in files.iter().enumerate() {
        fs::write(file, format!("original-{index}")).unwrap();
    }

    let mut aft = AftProcess::spawn();
    let create = serde_json::json!({
        "id": "all-create",
        "command": "checkpoint",
        "name": "all",
        "files": files,
    });
    let response = aft.send(&create.to_string());
    assert_eq!(response["success"], true, "checkpoint: {response:?}");
    for file in &files {
        fs::write(file, "modified").unwrap();
    }

    let response = aft.send(
        &serde_json::json!({
            "id": "all-restore",
            "command": "restore_checkpoint",
            "name": "all",
        })
        .to_string(),
    );
    assert_eq!(response["success"], true, "restore: {response:?}");
    assert_eq!(response["file_count"], 3);
    let restored_paths = response["paths"].as_array().expect("restore paths");
    for (index, file) in files.iter().enumerate() {
        assert!(restored_paths.contains(&serde_json::json!(file)));
        assert_eq!(
            fs::read_to_string(file).unwrap(),
            format!("original-{index}")
        );
    }

    assert!(aft.shutdown().success());
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
fn send_json(aft: &mut AftProcess, request: serde_json::Value) -> serde_json::Value {
    aft.send(&serde_json::to_string(&request).unwrap())
}

#[cfg(unix)]
fn undo_operation(aft: &mut AftProcess, id: &str) -> serde_json::Value {
    send_json(aft, serde_json::json!({ "id": id, "command": "undo" }))
}

#[cfg(unix)]
fn inode(path: &std::path::Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    fs::symlink_metadata(path).unwrap().ino()
}

/// Deleting a symlink removes the link, never its target, and undo recreates
/// the link with its exact target text. (This replaced a test that asserted
/// the symlink delete was refused, from before undo could restore symlinks.)
#[cfg(unix)]
#[test]
fn symlink_file_delete_is_undoable_without_project_restriction() {
    let dir = temp_dir("delete_single_symlink_unrestricted");
    let target = dir.join("target.txt");
    let symlink = dir.join("target-link.txt");

    fs::write(&target, "target content").unwrap();
    std::os::unix::fs::symlink(&target, &symlink).unwrap();

    let mut aft = AftProcess::spawn();
    let resp = send_json(
        &mut aft,
        serde_json::json!({
            "id": "delete-single-symlink-unrestricted",
            "command": "delete_file",
            "file": symlink.display().to_string(),
        }),
    );
    assert_eq!(resp["success"], true, "delete: {resp:?}");
    assert!(fs::symlink_metadata(&symlink).is_err(), "link removed");
    assert_eq!(fs::read_to_string(&target).unwrap(), "target content");

    let undo = undo_operation(&mut aft, "undo-single-symlink");
    assert_eq!(undo["success"], true, "undo: {undo:?}");
    assert_eq!(fs::read_link(&symlink).unwrap(), target);
    assert_eq!(fs::read_to_string(&target).unwrap(), "target content");

    let status = aft.shutdown();
    assert!(status.success());
}

/// Same as above with the project-root restriction on. (Replaced a test that
/// asserted the refusal.)
#[cfg(unix)]
#[test]
fn symlink_file_delete_is_undoable_with_project_restriction() {
    let dir = temp_dir("delete_single_symlink_restricted");
    let target = dir.join("target.txt");
    let symlink = dir.join("target-link.txt");

    fs::write(&target, "target content").unwrap();
    std::os::unix::fs::symlink(&target, &symlink).unwrap();

    let mut aft = AftProcess::spawn();
    let cfg = send_json(
        &mut aft,
        serde_json::json!({
            "id": "cfg-delete-single-symlink",
            "command": "configure",
            "harness": "opencode",
            "project_root": dir.display().to_string(),
            "config": user_config(serde_json::json!({ "restrict_to_project_root": true })),
        }),
    );
    assert_eq!(cfg["success"], true, "configure should succeed: {cfg:?}");

    let resp = send_json(
        &mut aft,
        serde_json::json!({
            "id": "delete-single-symlink-restricted",
            "command": "delete_file",
            "file": symlink.display().to_string(),
        }),
    );
    assert_eq!(resp["success"], true, "delete: {resp:?}");
    assert!(fs::symlink_metadata(&symlink).is_err(), "link removed");
    assert_eq!(fs::read_to_string(&target).unwrap(), "target content");

    let undo = undo_operation(&mut aft, "undo-single-symlink-restricted");
    assert_eq!(undo["success"], true, "undo: {undo:?}");
    assert_eq!(fs::read_link(&symlink).unwrap(), target);

    let status = aft.shutdown();
    assert!(status.success());
}

/// A relative path to a symlink resolves against the project root and the
/// link itself is deleted and restored. (Replaced a test that asserted the
/// refusal.)
#[cfg(unix)]
#[test]
fn relative_symlink_file_delete_is_undoable_with_project_restriction() {
    let dir = temp_dir("delete_relative_symlink_restricted");
    let target = dir.join("target.txt");
    let symlink = dir.join("target-link.txt");

    fs::write(&target, "target content").unwrap();
    std::os::unix::fs::symlink("target.txt", &symlink).unwrap();

    let mut aft = AftProcess::spawn();
    configure_restricted(&mut aft, &dir, "cfg-delete-relative-symlink");

    let resp = send_json(
        &mut aft,
        serde_json::json!({
            "id": "delete-relative-symlink-restricted",
            "command": "delete_file",
            "file": "target-link.txt",
        }),
    );
    assert_eq!(resp["success"], true, "delete: {resp:?}");
    assert!(fs::symlink_metadata(&symlink).is_err(), "link removed");
    assert_eq!(fs::read_to_string(&target).unwrap(), "target content");

    let undo = undo_operation(&mut aft, "undo-relative-symlink");
    assert_eq!(undo["success"], true, "undo: {undo:?}");
    assert_eq!(
        fs::read_link(&symlink).unwrap(),
        std::path::PathBuf::from("target.txt"),
        "the relative target text must come back unchanged"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

/// A recursive delete removes symlinks without following them, and undo
/// recreates each one exactly: an absolute link to a file outside the tree, a
/// relative link, and a dangling link. (Replaced a test that asserted a
/// symlink to an outside file blocked the delete.)
#[cfg(unix)]
#[test]
fn recursive_delete_undo_recreates_symlinks_exactly() {
    let dir = temp_dir("delete_recursive_symlinks");
    let target_dir = temp_dir("delete_recursive_symlinks_target");
    let tree = dir.join("tree");
    fs::create_dir_all(&tree).unwrap();
    let real_file = tree.join("real.txt");
    let outside_file = target_dir.join("outside.txt");
    fs::write(&real_file, "inside").unwrap();
    fs::write(&outside_file, "outside").unwrap();
    std::os::unix::fs::symlink(&outside_file, tree.join("outside-link.txt")).unwrap();
    std::os::unix::fs::symlink("real.txt", tree.join("relative-link.txt")).unwrap();
    std::os::unix::fs::symlink("../nowhere/missing", tree.join("dangling")).unwrap();

    let mut aft = AftProcess::spawn();
    let resp = send_json(
        &mut aft,
        serde_json::json!({
            "id": "delete-symlink-tree",
            "command": "delete_file",
            "file": tree.display().to_string(),
            "recursive": true,
        }),
    );
    assert_eq!(resp["success"], true, "delete: {resp:?}");
    assert!(fs::symlink_metadata(&tree).is_err(), "tree removed");
    assert_eq!(fs::read_to_string(&outside_file).unwrap(), "outside");

    let undo = undo_operation(&mut aft, "undo-symlink-tree");
    assert_eq!(undo["success"], true, "undo: {undo:?}");
    assert_eq!(fs::read_to_string(&real_file).unwrap(), "inside");
    assert_eq!(
        fs::read_link(tree.join("outside-link.txt")).unwrap(),
        outside_file
    );
    assert_eq!(
        fs::read_link(tree.join("relative-link.txt")).unwrap(),
        std::path::PathBuf::from("real.txt")
    );
    assert_eq!(
        fs::read_link(tree.join("dangling")).unwrap(),
        std::path::PathBuf::from("../nowhere/missing")
    );
    assert!(!tree.join("dangling").exists(), "still dangling");
    assert_eq!(fs::read_to_string(&outside_file).unwrap(), "outside");

    let status = aft.shutdown();
    assert!(status.success());
}

/// A tree whose only failure is a refused entry (a FIFO) reports the batch
/// as all-failed and deletes nothing. (This used a symlink to force the
/// refusal before symlinks were supported.)
#[cfg(unix)]
#[test]
fn batch_with_only_failed_recursive_delete_reports_failure() {
    let dir = temp_dir("delete_recursive_batch_all_failed");
    let fifo = dir.join("pipe");
    let made = Command::new("mkfifo").arg(&fifo).status().unwrap();
    assert!(made.success());
    fs::write(dir.join("file.txt"), "kept").unwrap();

    let mut aft = AftProcess::spawn();
    let resp = send_json(
        &mut aft,
        serde_json::json!({
            "id": "delete-file-batch-all-failed",
            "command": "delete_file",
            "files": [dir.display().to_string()],
            "recursive": true,
        }),
    );

    assert_eq!(resp["success"], false, "delete should fail: {resp:?}");
    assert_eq!(resp["code"], "delete_failed");
    assert_eq!(resp["all_failed"], true);
    assert_eq!(resp["complete"], false);
    assert_eq!(resp["skipped_files"].as_array().unwrap().len(), 1);
    let message = resp["message"].as_str().expect("failure message");
    assert!(message.contains("delete failed for all 1 file(s)"));
    assert!(message.contains(&dir.display().to_string()));
    assert!(message.contains("named pipe"), "{message}");
    assert!(fs::symlink_metadata(&fifo).is_ok(), "fifo should remain");
    assert_eq!(fs::read_to_string(dir.join("file.txt")).unwrap(), "kept");

    let status = aft.shutdown();
    assert!(status.success());
}

/// A symlink to a directory outside the tree is removed as a link: the
/// directory it points at, and its contents, are untouched, and undo
/// recreates the link. (Replaced a test that asserted the delete was refused.)
#[cfg(unix)]
#[test]
fn recursive_delete_never_follows_a_symlink_to_a_directory() {
    let dir = temp_dir("delete_recursive_dir_symlink");
    let target_dir = temp_dir("delete_recursive_dir_symlink_target");
    let tree = dir.join("tree");
    fs::create_dir_all(&tree).unwrap();
    fs::write(tree.join("real.txt"), "inside").unwrap();
    fs::write(target_dir.join("outside.txt"), "outside").unwrap();
    let symlink = tree.join("outside-dir-link");
    std::os::unix::fs::symlink(&target_dir, &symlink).unwrap();

    let mut aft = AftProcess::spawn();
    let resp = send_json(
        &mut aft,
        serde_json::json!({
            "id": "delete-dir-symlink-tree",
            "command": "delete_file",
            "file": tree.display().to_string(),
            "recursive": true,
        }),
    );
    assert_eq!(resp["success"], true, "delete: {resp:?}");
    assert_eq!(resp["files_deleted"], 2, "{resp:?}");
    assert!(fs::symlink_metadata(&tree).is_err());
    assert_eq!(
        fs::read_to_string(target_dir.join("outside.txt")).unwrap(),
        "outside",
        "the linked directory's contents must survive"
    );

    let undo = undo_operation(&mut aft, "undo-dir-symlink-tree");
    assert_eq!(undo["success"], true, "undo: {undo:?}");
    assert_eq!(fs::read_link(&symlink).unwrap(), target_dir);
    assert_eq!(fs::read_to_string(tree.join("real.txt")).unwrap(), "inside");

    let status = aft.shutdown();
    assert!(status.success());
}

/// Empty directories are recorded and recreated by undo, and every restored
/// directory gets its original mode back. (Replaced a test that asserted an
/// empty subdirectory blocked the delete.)
#[cfg(unix)]
#[test]
fn recursive_delete_undo_recreates_empty_directories_and_modes() {
    let dir = temp_dir("delete_recursive_empty_dirs");
    let tree = dir.join("tree");
    let empty = tree.join("empty");
    let nested_empty = tree.join("outer").join("inner");
    let locked = tree.join("locked");
    fs::create_dir_all(&empty).unwrap();
    fs::create_dir_all(&nested_empty).unwrap();
    fs::create_dir_all(&locked).unwrap();
    fs::write(tree.join("with_content.txt"), "content").unwrap();
    fs::write(tree.join("outer").join("in-outer.txt"), "in outer").unwrap();
    fs::set_permissions(&empty, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&tree.join("outer"), fs::Permissions::from_mode(0o750)).unwrap();
    // Read-only and empty: removable, and undo must apply this mode only
    // after nothing else needs to be created inside it.
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
    let mode = |path: &std::path::Path| fs::metadata(path).unwrap().permissions().mode() & 0o7777;
    let modes_before =
        [&tree, &empty, &tree.join("outer"), &nested_empty, &locked].map(|path| mode(path));

    let mut aft = AftProcess::spawn();
    let resp = send_json(
        &mut aft,
        serde_json::json!({
            "id": "delete-empty-subdir-tree",
            "command": "delete_file",
            "file": tree.display().to_string(),
            "recursive": true,
        }),
    );
    assert_eq!(resp["success"], true, "delete: {resp:?}");
    assert_eq!(resp["directories_deleted"], 5, "{resp:?}");
    assert!(fs::symlink_metadata(&tree).is_err());

    let undo = undo_operation(&mut aft, "undo-empty-subdir-tree");
    assert_eq!(undo["success"], true, "undo: {undo:?}");
    assert!(empty.is_dir() && fs::read_dir(&empty).unwrap().next().is_none());
    assert!(nested_empty.is_dir());
    assert_eq!(
        fs::read_to_string(tree.join("with_content.txt")).unwrap(),
        "content"
    );
    assert_eq!(
        fs::read_to_string(tree.join("outer").join("in-outer.txt")).unwrap(),
        "in outer"
    );
    let modes_after =
        [&tree, &empty, &tree.join("outer"), &nested_empty, &locked].map(|path| mode(path));
    assert_eq!(modes_after, modes_before);

    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    let status = aft.shutdown();
    assert!(status.success());
}

/// A socket is deleted and reported as not restorable; the rest of the tree
/// is restored by undo. (Replaced a test that asserted a socket blocked the
/// delete.)
#[cfg(unix)]
#[test]
fn recursive_delete_deletes_socket_and_reports_it_not_restored() {
    use std::os::unix::net::UnixListener;

    // Socket paths are limited to about 100 bytes, so this tree lives in a
    // short temp path; backups under temp paths are allowed in these tests.
    let dir = std::env::temp_dir().join(format!("aft_sock_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let content_file = dir.join("with_content.txt");
    let socket_path = dir.join("socket.sock");
    fs::write(&content_file, "content").unwrap();
    let listener = UnixListener::bind(&socket_path).unwrap();

    let mut aft = AftProcess::spawn();
    let resp = send_json(
        &mut aft,
        serde_json::json!({
            "id": "delete-socket-tree",
            "command": "delete_file",
            "file": dir.display().to_string(),
            "recursive": true,
        }),
    );
    assert_eq!(resp["success"], true, "delete: {resp:?}");
    let warnings = resp["warnings"].as_array().expect("warnings");
    assert!(
        warnings.iter().any(|warning| {
            let text = warning.as_str().unwrap();
            text.contains("socket.sock") && text.contains("not restorable")
        }),
        "{resp:?}"
    );
    assert!(fs::symlink_metadata(&dir).is_err());

    let undo = undo_operation(&mut aft, "undo-socket-tree");
    assert_eq!(undo["success"], true, "undo: {undo:?}");
    assert_eq!(fs::read_to_string(&content_file).unwrap(), "content");
    assert!(
        fs::symlink_metadata(&socket_path).is_err(),
        "socket is not recreated"
    );

    let status = aft.shutdown();
    assert!(status.success());
    drop(listener);
    let _ = fs::remove_dir_all(&dir);
}

/// Hard links inside the tree are relinked by undo, so the restored paths
/// share one file again. (Replaced a test that asserted a hard link blocked
/// the delete.)
#[cfg(unix)]
#[test]
fn recursive_delete_undo_relinks_hard_links() {
    let dir = temp_dir("delete_recursive_hard_link");
    let tree = dir.join("tree");
    fs::create_dir_all(tree.join("sub")).unwrap();
    let file = tree.join("file.txt");
    let link = tree.join("sub").join("file-hardlink.txt");
    fs::write(&file, "content").unwrap();
    fs::hard_link(&file, &link).unwrap();

    let mut aft = AftProcess::spawn();
    let resp = send_json(
        &mut aft,
        serde_json::json!({
            "id": "delete-hardlink-tree",
            "command": "delete_file",
            "file": tree.display().to_string(),
            "recursive": true,
        }),
    );
    assert_eq!(resp["success"], true, "delete: {resp:?}");
    assert!(resp.get("warnings").is_none(), "{resp:?}");
    assert!(fs::symlink_metadata(&tree).is_err());

    let undo = undo_operation(&mut aft, "undo-hardlink-tree");
    assert_eq!(undo["success"], true, "undo: {undo:?}");
    assert_eq!(fs::read_to_string(&file).unwrap(), "content");
    assert_eq!(fs::read_to_string(&link).unwrap(), "content");
    assert_eq!(inode(&file), inode(&link), "undo must relink, not copy");

    let status = aft.shutdown();
    assert!(status.success());
}

/// A file hard-linked to a path outside the tree cannot be relinked to it,
/// because undo does not know where that path is. The delete warns, the
/// outside file is untouched, and undo restores the content as a copy with a
/// warning.
#[cfg(unix)]
#[test]
fn recursive_delete_restores_outside_hard_link_as_copy_with_warning() {
    let dir = temp_dir("delete_recursive_outside_hard_link");
    let outside_dir = temp_dir("delete_recursive_outside_hard_link_target");
    let tree = dir.join("tree");
    fs::create_dir_all(&tree).unwrap();
    let outside = outside_dir.join("shared.txt");
    let inside = tree.join("shared.txt");
    fs::write(&outside, "shared").unwrap();
    fs::hard_link(&outside, &inside).unwrap();

    let mut aft = AftProcess::spawn();
    let resp = send_json(
        &mut aft,
        serde_json::json!({
            "id": "delete-outside-hardlink-tree",
            "command": "delete_file",
            "file": tree.display().to_string(),
            "recursive": true,
        }),
    );
    assert_eq!(resp["success"], true, "delete: {resp:?}");
    assert!(
        resp["warnings"][0]
            .as_str()
            .is_some_and(|warning| warning.contains("independent copy")),
        "{resp:?}"
    );
    assert_eq!(fs::read_to_string(&outside).unwrap(), "shared");

    let undo = undo_operation(&mut aft, "undo-outside-hardlink-tree");
    assert_eq!(undo["success"], true, "undo: {undo:?}");
    assert_eq!(fs::read_to_string(&inside).unwrap(), "shared");
    assert_ne!(inode(&inside), inode(&outside), "restored as a copy");
    assert!(
        undo["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning.as_str().unwrap().contains("independent copy")),
        "{undo:?}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

/// An entry created after the delete took its backups has no backup, so the
/// delete must not remove it: it stops partway, reports it, keeps the entry
/// and the directories holding it, and undo restores what was removed.
#[cfg(unix)]
#[test]
fn recursive_delete_stops_when_an_entry_appears_mid_delete() {
    let dir = temp_dir("delete_recursive_mid_delete_race");
    let tree = dir.join("tree");
    fs::create_dir_all(tree.join("a")).unwrap();
    fs::create_dir_all(tree.join("b")).unwrap();
    fs::write(tree.join("a/x.txt"), "x").unwrap();
    fs::write(tree.join("b/y.txt"), "y").unwrap();
    fs::write(tree.join("top.txt"), "top").unwrap();

    let mut aft = AftProcess::spawn_with_env(&[(
        "AFT_TEST_RECURSIVE_DELETE_INJECT",
        std::ffi::OsStr::new("a/late.txt"),
    )]);
    let resp = send_json(
        &mut aft,
        serde_json::json!({
            "id": "delete-racing-tree",
            "command": "delete_file",
            "file": tree.display().to_string(),
            "recursive": true,
        }),
    );
    assert_eq!(resp["success"], false, "delete must stop: {resp:?}");
    assert_eq!(resp["partial"], true, "{resp:?}");
    assert!(
        resp["message"]
            .as_str()
            .unwrap()
            .contains("created after the delete started"),
        "{resp:?}"
    );
    assert_eq!(
        fs::read_to_string(tree.join("a/late.txt")).unwrap(),
        "created during the delete",
        "the unbacked entry must survive"
    );

    let undo = undo_operation(&mut aft, "undo-racing-tree");
    assert_eq!(undo["success"], true, "undo: {undo:?}");
    assert_eq!(fs::read_to_string(tree.join("a/x.txt")).unwrap(), "x");
    assert_eq!(fs::read_to_string(tree.join("b/y.txt")).unwrap(), "y");
    assert_eq!(fs::read_to_string(tree.join("top.txt")).unwrap(), "top");
    assert_eq!(
        fs::read_to_string(tree.join("a/late.txt")).unwrap(),
        "created during the delete"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

/// If a directory of the deleted tree comes back as a symlink before undo,
/// undo must refuse instead of writing through it, and leave nothing behind.
#[cfg(unix)]
#[test]
fn undo_refuses_when_a_deleted_directory_came_back_as_a_symlink() {
    let dir = temp_dir("delete_recursive_undo_symlinked_ancestor");
    let elsewhere = temp_dir("delete_recursive_undo_symlinked_ancestor_elsewhere");
    let tree = dir.join("tree");
    fs::create_dir_all(tree.join("sub")).unwrap();
    fs::write(tree.join("sub/file.txt"), "content").unwrap();

    let mut aft = AftProcess::spawn();
    let resp = send_json(
        &mut aft,
        serde_json::json!({
            "id": "delete-before-replant",
            "command": "delete_file",
            "file": tree.display().to_string(),
            "recursive": true,
        }),
    );
    assert_eq!(resp["success"], true, "delete: {resp:?}");

    fs::create_dir(&tree).unwrap();
    std::os::unix::fs::symlink(&elsewhere, tree.join("sub")).unwrap();

    let undo = undo_operation(&mut aft, "undo-after-replant");
    assert_eq!(undo["success"], false, "undo must refuse: {undo:?}");
    assert!(
        undo["message"]
            .as_str()
            .unwrap()
            .contains("no longer a real directory"),
        "{undo:?}"
    );
    assert!(
        fs::read_dir(&elsewhere).unwrap().next().is_none(),
        "nothing may be written through the symlink"
    );
    assert_eq!(fs::read_link(tree.join("sub")).unwrap(), elsewhere);

    let status = aft.shutdown();
    assert!(status.success());
}

/// A recursive delete backs up every file before removing anything, and that
/// copy runs while the delete holds its root's write lane. A large tree (a
/// build output or a vendored dependency cache with thousands of small files)
/// would hold the root for minutes, so the delete must refuse up front, leave
/// the tree untouched, and say what it counted and what to do instead. The
/// fixture is larger than the file budget so the count also has to stop at
/// the cap instead of walking everything.
#[test]
fn recursive_delete_refuses_tree_over_backup_budget_without_deleting() {
    const FILES: usize = 2_500;
    let dir = temp_dir("delete_recursive_backup_budget");
    let tree = dir.join("node_modules");
    for index in 0..FILES {
        let package = tree.join(format!("pkg-{:03}", index / 25));
        fs::create_dir_all(&package).unwrap();
        fs::write(
            package.join(format!("file-{index}.js")),
            format!("module.exports = {index};\n"),
        )
        .unwrap();
    }

    let mut aft = AftProcess::spawn();
    let delete = serde_json::json!({
        "id": "delete-over-budget-tree",
        "command": "delete_file",
        "file": tree.display().to_string(),
        "recursive": true,
    });
    let resp = aft.send(&serde_json::to_string(&delete).unwrap());

    assert_eq!(resp["success"], false, "delete should be refused: {resp:?}");
    assert_eq!(resp["code"], "recursive_delete_backup_too_large");
    let message = resp["message"].as_str().unwrap();
    assert!(
        message.contains("at least 2001 entries"),
        "the count must stop just past the cap and say so: {message}"
    );
    assert!(
        message.contains("2000 entries"),
        "names the limit: {message}"
    );
    assert!(
        message.contains("rm -rf"),
        "names the no-undo alternative: {message}"
    );
    assert!(
        message.contains("smaller pieces"),
        "names the undo-preserving alternative: {message}"
    );

    let remaining = fs::read_dir(&tree)
        .unwrap()
        .map(|package| fs::read_dir(package.unwrap().path()).unwrap().count())
        .sum::<usize>();
    assert_eq!(remaining, FILES, "nothing may be deleted");

    let history = aft.send(
        &serde_json::to_string(&serde_json::json!({
            "id": "history-after-refusal",
            "command": "edit_history",
            "file": tree.join("pkg-000/file-0.js").display().to_string(),
        }))
        .unwrap(),
    );
    assert_eq!(
        history["entries"].as_array().map(Vec::len).unwrap_or(0),
        0,
        "a refused delete must not leave backups behind: {history:?}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

/// Under a system temp directory no entry is backed up, so there is no undo
/// whose shape an empty directory, symlink, hard link or socket could break.
/// The delete must go through, remove links without touching what they point
/// at, and report that undo is unavailable.
#[cfg(unix)]
#[test]
fn temp_tree_without_backups_deletes_links_empty_dirs_and_sockets() {
    use std::os::unix::net::UnixListener;

    let scratch = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let tree = scratch.path().join("tree");
    fs::create_dir_all(tree.join("empty")).unwrap();
    fs::write(tree.join("file.txt"), "content").unwrap();
    fs::hard_link(tree.join("file.txt"), tree.join("file-link.txt")).unwrap();
    let outside_file = outside.path().join("outside.txt");
    fs::write(&outside_file, "outside").unwrap();
    std::os::unix::fs::symlink(&outside_file, tree.join("to-outside")).unwrap();
    std::os::unix::fs::symlink(outside.path(), tree.join("to-outside-dir")).unwrap();
    std::os::unix::fs::symlink("missing-target", tree.join("dangling")).unwrap();
    let _listener = UnixListener::bind(tree.join("s.sock")).unwrap();

    let mut aft =
        AftProcess::spawn_with_env(&[("AFT_TEST_ALLOW_TEMP_BACKUPS", std::ffi::OsStr::new("0"))]);
    let resp = aft.send(
        &serde_json::to_string(&serde_json::json!({
            "id": "temp-tree-delete",
            "command": "delete_file",
            "file": tree.display().to_string(),
            "recursive": true,
        }))
        .unwrap(),
    );
    assert_eq!(resp["success"], true, "delete: {resp:?}");
    assert_eq!(resp["backup_skipped_reason"], "temp_path", "{resp:?}");
    assert!(fs::symlink_metadata(&tree).is_err(), "tree must be gone");
    assert_eq!(fs::read_to_string(&outside_file).unwrap(), "outside");
    assert!(outside.path().is_dir(), "a linked directory must survive");

    let status = aft.shutdown();
    assert!(status.success());
}

/// A single symlink under a system temp directory has no undo to protect
/// either; deleting it removes the link and leaves its target alone.
#[cfg(unix)]
#[test]
fn temp_symlink_without_backups_is_deleted_without_touching_its_target() {
    let scratch = tempfile::tempdir().unwrap();
    let target = scratch.path().join("target.txt");
    let link = scratch.path().join("link.txt");
    fs::write(&target, "target").unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let mut aft =
        AftProcess::spawn_with_env(&[("AFT_TEST_ALLOW_TEMP_BACKUPS", std::ffi::OsStr::new("0"))]);
    let resp = aft.send(
        &serde_json::to_string(&serde_json::json!({
            "id": "temp-symlink-delete",
            "command": "delete_file",
            "file": link.display().to_string(),
        }))
        .unwrap(),
    );
    assert_eq!(resp["success"], true, "delete: {resp:?}");
    assert_eq!(resp["backup_skipped_reason"], "temp_path");
    assert!(fs::symlink_metadata(&link).is_err());
    assert_eq!(fs::read_to_string(&target).unwrap(), "target");

    let status = aft.shutdown();
    assert!(status.success());
}

/// The backup budget bounds how much a delete copies into the undo store. A
/// tree under a system temp directory copies nothing, so it has no budget.
#[test]
fn temp_tree_without_backups_is_not_limited_by_the_backup_budget() {
    let scratch = tempfile::tempdir().unwrap();
    let tree = scratch.path().join("node_modules");
    for index in 0..2_100 {
        let package = tree.join(format!("pkg-{:03}", index / 50));
        fs::create_dir_all(&package).unwrap();
        fs::write(package.join(format!("f{index}.js")), "x").unwrap();
    }

    let mut aft =
        AftProcess::spawn_with_env(&[("AFT_TEST_ALLOW_TEMP_BACKUPS", std::ffi::OsStr::new("0"))]);
    let resp = aft.send(
        &serde_json::to_string(&serde_json::json!({
            "id": "temp-tree-over-budget",
            "command": "delete_file",
            "file": tree.display().to_string(),
            "recursive": true,
        }))
        .unwrap(),
    );
    assert_eq!(resp["success"], true, "delete: {resp:?}");
    assert_eq!(resp["files_deleted"], 2_100);
    assert_eq!(resp["backup_skipped_reason"], "temp_path");
    assert!(!tree.exists());

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
    // Two files and the two directories holding them.
    assert_eq!(undo["restored_count"], 4);
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
        crate::context_storage::isolate(Config {
            project_root: Some(root.path().to_path_buf()),
            restrict_to_project_root: false,
            ..Config::default()
        }),
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
