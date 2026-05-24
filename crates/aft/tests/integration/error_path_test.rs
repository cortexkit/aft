use std::fs;
use std::path::Path;

use super::helpers::AftProcess;

fn assert_error_code(resp: &serde_json::Value, code: &str) {
    assert_eq!(
        resp["success"], false,
        "expected failure response: {resp:?}"
    );
    assert_eq!(resp["code"], code, "unexpected error response: {resp:?}");
}

fn set_read_only(path: &Path, read_only: bool) {
    let mut perms = fs::metadata(path).expect("metadata").permissions();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mode = perms.mode();
        if read_only {
            perms.set_mode(mode & !0o222);
        } else {
            perms.set_mode(mode | 0o600);
        }
    }

    #[cfg(not(unix))]
    {
        perms.set_readonly(read_only);
    }

    fs::set_permissions(path, perms).expect("set permissions");
}

#[test]
fn write_fails_when_parent_directory_is_missing_and_create_dirs_is_false() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("missing").join("nested").join("new.txt");

    let req = serde_json::json!({
        "id": "write-missing-parent",
        "command": "write",
        "file": target.display().to_string(),
        "content": "hello",
        "create_dirs": false,
    });
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_error_code(&resp, "invalid_request");
    assert!(resp["message"]
        .as_str()
        .unwrap()
        .contains("failed to write file"));
    assert!(
        !target.exists(),
        "write should not create missing directories"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn write_rejects_paths_outside_the_configured_project_root() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let outside = dir.path().join("outside");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&outside).unwrap();

    // Must opt into path restriction (default is false)
    let configure = aft.send(
        &serde_json::to_string(&serde_json::json!({
            "id": "cfg",
            "command": "configure",
            "harness": "opencode",
            "project_root": root.display().to_string(),
            "restrict_to_project_root": true,
        }))
        .unwrap(),
    );
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:?}"
    );

    let target = outside.join("new.txt");
    let req = serde_json::json!({
        "id": "write-outside-root",
        "command": "write",
        "file": target.display().to_string(),
        "content": "hello",
    });
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_error_code(&resp, "path_outside_root");
    let message = resp["message"].as_str().unwrap();
    assert!(message.contains(&target.display().to_string()));
    assert!(message.contains(&root.display().to_string()));

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn write_fails_for_read_only_files() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("readonly.txt");
    fs::write(&target, "before").unwrap();
    set_read_only(&target, true);

    let req = serde_json::json!({
        "id": "write-read-only",
        "command": "write",
        "file": target.display().to_string(),
        "content": "after",
    });
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_error_code(&resp, "invalid_request");
    let message = resp["message"].as_str().unwrap().to_lowercase();
    assert!(message.contains("failed to write file"));
    assert!(message.contains("permission") || message.contains("denied"));

    set_read_only(&target, false);
    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn edit_match_rejects_empty_match_strings() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("sample.txt");
    fs::write(&target, "hello world\n").unwrap();

    let req = serde_json::json!({
        "id": "edit-match-empty",
        "command": "edit_match",
        "file": target.display().to_string(),
        "match": "",
        "replacement": "updated",
    });
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_error_code(&resp, "invalid_request");
    assert_eq!(
        resp["message"],
        "edit_match: 'match' must be a non-empty string"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn edit_match_returns_file_not_found_for_missing_files() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("missing.txt");

    let req = serde_json::json!({
        "id": "edit-match-missing-file",
        "command": "edit_match",
        "file": target.display().to_string(),
        "match": "hello",
        "replacement": "updated",
    });
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_error_code(&resp, "file_not_found");
    assert_eq!(
        resp["message"],
        format!("file not found: {}", target.display())
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn edit_match_rejects_occurrences_that_are_out_of_range() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("occurrence.txt");
    fs::write(&target, "hello world\n").unwrap();

    let req = serde_json::json!({
        "id": "edit-match-occurrence-range",
        "command": "edit_match",
        "file": target.display().to_string(),
        "match": "hello",
        "replacement": "updated",
        "occurrence": 5,
    });
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_error_code(&resp, "invalid_request");
    assert_eq!(
        resp["message"],
        "edit_match: occurrence 5 out of range, file has 1 occurrence(s)"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn delete_file_returns_file_not_found_for_missing_files() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("missing.txt");

    let req = serde_json::json!({
        "id": "delete-missing",
        "command": "delete_file",
        "file": target.display().to_string(),
    });
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_error_code(&resp, "file_not_found");
    assert_eq!(
        resp["message"],
        format!("delete_file: file not found: {}", target.display())
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn delete_file_rejects_paths_outside_the_configured_project_root() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let outside = dir.path().join("outside");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&outside).unwrap();

    let target = outside.join("delete-me.txt");
    fs::write(&target, "hello").unwrap();

    // Must opt into path restriction (default is false)
    let configure = aft.send(
        &serde_json::to_string(&serde_json::json!({
            "id": "cfg",
            "command": "configure",
            "harness": "opencode",
            "project_root": root.display().to_string(),
            "restrict_to_project_root": true,
        }))
        .unwrap(),
    );
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:?}"
    );

    let req = serde_json::json!({
        "id": "delete-outside-root",
        "command": "delete_file",
        "file": target.display().to_string(),
    });
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_error_code(&resp, "path_outside_root");
    let message = resp["message"].as_str().unwrap();
    assert!(message.contains(&target.display().to_string()));
    assert!(message.contains(&root.display().to_string()));
    assert!(
        target.exists(),
        "delete_file should not remove files outside the project root"
    );

    let status = aft.shutdown();
    assert!(status.success());
}
