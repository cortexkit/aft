use std::fs;
use std::path::Path;

use serde_json::json;

use super::helpers::AftProcess;

fn configure(aft: &mut AftProcess, root: &Path) {
    let resp = aft.configure(root);
    assert_eq!(resp["success"], true, "configure failed: {resp:?}");
}

fn configure_restricted(aft: &mut AftProcess, root: &Path) {
    let resp = aft.send(
        &serde_json::to_string(&json!({
            "id": "cfg-restricted",
            "command": "configure",
            "harness": "opencode",
            "project_root": root.display().to_string(),
            "restrict_to_project_root": true,
        }))
        .unwrap(),
    );
    assert_eq!(resp["success"], true, "configure failed: {resp:?}");
}

fn append_request(id: &str, file: &Path, append_content: &str) -> serde_json::Value {
    json!({
        "id": id,
        "command": "edit_match",
        "op": "append",
        "file": file.display().to_string(),
        "appendContent": append_content,
    })
}

#[test]
fn append_creates_file_when_missing() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("created.txt");

    let mut aft = AftProcess::spawn();
    configure(&mut aft, dir.path());

    let req = append_request("append-create", &target, "hello append");
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_eq!(resp["success"], true, "append failed: {resp:?}");
    assert_eq!(resp["created"], true);
    assert_eq!(fs::read_to_string(&target).unwrap(), "hello append");

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn append_appends_to_existing_file() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("existing.txt");
    fs::write(&target, "line1\n").unwrap();

    let mut aft = AftProcess::spawn();
    configure(&mut aft, dir.path());

    let req = append_request("append-existing", &target, "line2\n");
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_eq!(resp["success"], true, "append failed: {resp:?}");
    assert_eq!(resp["created"], false);
    assert_eq!(fs::read_to_string(&target).unwrap(), "line1\nline2\n");

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn append_reports_invalid_syntax() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("broken.ts");
    fs::write(&target, "export function broken() {\n").unwrap();

    let mut aft = AftProcess::spawn();
    configure(&mut aft, dir.path());

    let req = append_request("append-invalid-syntax", &target, "const value = ;\n");
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_eq!(resp["success"], true, "append failed: {resp:?}");
    assert_eq!(resp["syntax_valid"], false, "response: {resp:?}");

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn append_creates_parent_directories() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("newdir/subdir/file.txt");

    let mut aft = AftProcess::spawn();
    configure(&mut aft, dir.path());

    let req = append_request("append-create-dirs", &target, "nested content");
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_eq!(resp["success"], true, "append failed: {resp:?}");
    assert!(dir.path().join("newdir/subdir").is_dir());
    assert_eq!(fs::read_to_string(&target).unwrap(), "nested content");

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn append_with_create_dirs_false_rejects_missing_parent() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("newdir/subdir/file.txt");

    let mut aft = AftProcess::spawn();
    configure(&mut aft, dir.path());

    let mut req = append_request("append-no-create-dirs", &target, "nested content");
    req["createDirs"] = json!(false);
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_eq!(resp["success"], false, "append should fail: {resp:?}");
    assert_eq!(resp["code"], "write_error");
    assert!(!target.exists());

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
#[allow(non_snake_case)]
fn append_rejects_missing_appendContent() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("missing-content.txt");

    let mut aft = AftProcess::spawn();
    configure(&mut aft, dir.path());

    let req = json!({
        "id": "append-missing-content",
        "command": "edit_match",
        "op": "append",
        "file": target.display().to_string(),
    });
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_eq!(resp["success"], false, "append should fail: {resp:?}");
    assert_eq!(resp["code"], "invalid_request");
    let message = resp["message"].as_str().unwrap();
    assert!(
        message.contains("appendContent") || message.contains("append_content"),
        "unexpected message: {message}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn append_rejects_path_outside_project_root_when_restricted() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    fs::create_dir_all(&root).unwrap();
    let outside = dir.path().join("outside.txt");

    let mut aft = AftProcess::spawn();
    configure_restricted(&mut aft, &root);

    let req = append_request("append-outside-root", &outside, "blocked");
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_eq!(resp["success"], false, "append should fail: {resp:?}");
    assert_eq!(resp["code"], "path_outside_root");
    assert!(!outside.exists());

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn append_creates_backup_for_existing_file() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("backup.txt");
    fs::write(&target, "before\n").unwrap();

    let mut aft = AftProcess::spawn();
    configure(&mut aft, dir.path());

    let req = append_request("append-backup", &target, "after\n");
    let resp = aft.send(&serde_json::to_string(&req).unwrap());
    assert_eq!(resp["success"], true, "append failed: {resp:?}");

    let history = aft.send(
        &serde_json::to_string(&json!({
            "id": "append-backup-history",
            "command": "edit_history",
            "file": target.display().to_string(),
        }))
        .unwrap(),
    );
    assert_eq!(history["success"], true, "history failed: {history:?}");
    let entries = history["entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 1, "unexpected history: {entries:?}");
    assert_eq!(entries[0]["description"], "edit_match: append");
    assert!(entries[0]["backup_id"].is_string());

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn append_includes_diff_when_requested() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("diff.txt");
    fs::write(&target, "line1\n").unwrap();

    let mut aft = AftProcess::spawn();
    configure(&mut aft, dir.path());

    let mut req = append_request("append-diff", &target, "line2\n");
    req["include_diff"] = json!(true);
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_eq!(resp["success"], true, "append failed: {resp:?}");

    // Append now honors include_diff like the other write-style handlers.
    // Diff is computed against the file's pre-append content, so additions
    // reflect the lines added and deletions stay 0.
    let diff = resp
        .get("diff")
        .expect("append should include diff when include_diff: true");
    assert_eq!(
        diff["additions"], 1,
        "expected one added line for 'line2\\n': {resp:?}"
    );
    assert_eq!(
        diff["deletions"], 0,
        "append never deletes existing content: {resp:?}"
    );
    assert_eq!(diff["before"], "line1\n");
    assert_eq!(diff["after"], "line1\nline2\n");

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn append_diff_omitted_when_not_requested() {
    // Inverse case: ensure that omitting include_diff still keeps the
    // response shape lean. Some plugins parse `diff` defensively, so
    // missing-vs-empty matters for response-size tests.
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("no-diff.txt");

    let mut aft = AftProcess::spawn();
    configure(&mut aft, dir.path());

    let req = append_request("append-no-diff", &target, "hello\n");
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_eq!(resp["success"], true, "append failed: {resp:?}");
    assert!(
        resp.get("diff").is_none(),
        "append should omit diff when include_diff is absent: {resp:?}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn append_accepts_snake_case_param() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("snake-case.txt");

    let mut aft = AftProcess::spawn();
    configure(&mut aft, dir.path());

    let req = json!({
        "id": "append-snake-case",
        "command": "edit_match",
        "op": "append",
        "file": target.display().to_string(),
        "append_content": "snake content",
    });
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_eq!(resp["success"], true, "append failed: {resp:?}");
    assert_eq!(resp["created"], true);
    assert_eq!(fs::read_to_string(&target).unwrap(), "snake content");

    let status = aft.shutdown();
    assert!(status.success());
}
