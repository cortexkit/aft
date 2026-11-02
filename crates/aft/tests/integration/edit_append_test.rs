use std::fs;
use std::path::Path;

use serde_json::json;

#[cfg(unix)]
use super::helpers::{link_cached_executable, warm_executable};
use super::helpers::{user_config, AftProcess};

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
            "config": user_config(serde_json::json!({ "restrict_to_project_root": true })),
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

fn tool_call(
    aft: &mut AftProcess,
    id: &str,
    name: &str,
    arguments: serde_json::Value,
) -> serde_json::Value {
    aft.send(
        &json!({ "id": id, "command": "tool_call", "name": name, "arguments": arguments })
            .to_string(),
    )
}

#[test]
fn append_creates_file_when_missing() {
    let dir = crate::test_helpers::tempdir();
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
    let dir = crate::test_helpers::tempdir();
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
    let dir = crate::test_helpers::tempdir();
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
    let dir = crate::test_helpers::tempdir();
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
fn append_tool_reports_nested_markdown_creation_and_identical_write() {
    let dir = crate::test_helpers::tempdir();
    let target = dir.path().join(
        ".cortexkit/internal-docs/investigations/new-investigation/permission-routing-correction.md",
    );
    let content = "# Permission routing\n\nCorrection recorded.\n";
    let mut aft = AftProcess::spawn();
    configure(&mut aft, dir.path());

    let response = tool_call(
        &mut aft,
        "append-tool-create",
        "edit",
        json!({ "path": target, "appendContent": content }),
    );
    assert_eq!(fs::read_to_string(&target).unwrap(), content);

    let write_response = tool_call(
        &mut aft,
        "write-tool-identical",
        "write",
        json!({ "path": target, "content": content }),
    );
    assert_eq!(write_response["code"], "no_change", "{write_response:#}");
    assert_eq!(fs::read_to_string(&target).unwrap(), content);
    assert_eq!(
        response["success"], true,
        "append wrote the file but failed: {response:#}"
    );
    assert_eq!(response["created"], true, "{response:#}");
    assert_eq!(response["syntax_valid"], true, "{response:#}");
    assert!(
        response["text"]
            .as_str()
            .unwrap()
            .starts_with("Created file ("),
        "{response:#}"
    );
    assert!(response.get("warnings").is_none(), "{response:#}");
    assert!(aft.shutdown().success());
}

#[test]
fn append_tool_reports_markdown_creation_with_existing_parent() {
    let dir = crate::test_helpers::tempdir();
    let target = dir.path().join("new.md");
    let mut aft = AftProcess::spawn();
    configure(&mut aft, dir.path());
    let response = tool_call(
        &mut aft,
        "append-tool-new-file",
        "edit",
        json!({ "path": target, "appendContent": "# New file\n" }),
    );
    assert_eq!(response["success"], true, "{response:#}");
    assert_eq!(response["created"], true, "{response:#}");
    assert_eq!(response["syntax_valid"], true, "{response:#}");
    assert_eq!(response["bytes_written"], "# New file\n".len());
    assert_eq!(fs::read_to_string(&target).unwrap(), "# New file\n");
    assert!(aft.shutdown().success());
}

#[test]
fn append_tool_reports_existing_markdown_append() {
    let dir = crate::test_helpers::tempdir();
    let target = dir.path().join("existing.md");
    fs::write(&target, "# Existing\n").unwrap();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, dir.path());
    let response = tool_call(
        &mut aft,
        "append-tool-existing",
        "edit",
        json!({ "path": target, "appendContent": "\nMore text.\n" }),
    );
    assert_eq!(response["success"], true, "{response:#}");
    assert_eq!(response["created"], false, "{response:#}");
    assert_eq!(response["syntax_valid"], true, "{response:#}");
    assert_eq!(response["bytes_written"], "\nMore text.\n".len());
    assert_eq!(
        fs::read_to_string(&target).unwrap(),
        "# Existing\n\nMore text.\n"
    );
    assert!(aft.shutdown().success());
}

#[test]
fn append_tool_undo_removes_created_markdown() {
    let dir = crate::test_helpers::tempdir();
    let target = dir.path().join("nested/undo/created.md");
    let mut aft = AftProcess::spawn();
    configure(&mut aft, dir.path());
    let response = tool_call(
        &mut aft,
        "append-tool-before-undo",
        "edit",
        json!({ "path": target, "appendContent": "# Undo creation\n" }),
    );
    assert_eq!(response["success"], true, "{response:#}");
    assert_eq!(fs::read_to_string(&target).unwrap(), "# Undo creation\n");
    let undo = tool_call(
        &mut aft,
        "append-tool-undo",
        "safety",
        json!({ "op": "undo" }),
    );
    assert_eq!(undo["success"], true, "{undo:#}");
    assert!(
        !target.exists(),
        "undo must delete the created file: {undo:#}"
    );
    assert!(aft.shutdown().success());
}

#[test]
fn append_tool_write_and_batch_creation_contracts() {
    let dir = crate::test_helpers::tempdir();
    let target = dir.path().join("nested/write/created.md");
    let missing = dir.path().join("nested/batch/missing.md");
    let mut aft = AftProcess::spawn();
    configure(&mut aft, dir.path());
    let response = tool_call(
        &mut aft,
        "append-tool-write-check",
        "write",
        json!({ "path": target, "content": "# Written\n" }),
    );
    assert_eq!(response["success"], true, "{response:#}");
    assert_eq!(response["created"], true, "{response:#}");
    assert_eq!(response["syntax_valid"], true, "{response:#}");
    assert_eq!(fs::read_to_string(&target).unwrap(), "# Written\n");

    let rejected = tool_call(
        &mut aft,
        "append-tool-batch-missing",
        "edit",
        json!({ "path": missing, "edits": [{ "oldString": "# Missing", "newString": "# Not created\n" }] }),
    );
    assert_eq!(rejected["success"], false, "{rejected:#}");
    assert_eq!(rejected["code"], "file_not_found", "{rejected:#}");
    assert!(
        !missing.exists(),
        "edits[] must not create a missing target"
    );
    let edited = tool_call(
        &mut aft,
        "append-tool-batch-existing",
        "edit",
        json!({ "path": target, "edits": [{ "oldString": "Written", "newString": "Edited" }] }),
    );
    assert_eq!(edited["success"], true, "{edited:#}");
    assert_eq!(fs::read_to_string(&target).unwrap(), "# Edited\n");
    assert!(aft.shutdown().success());
}

#[test]
#[cfg(unix)]
fn append_tool_reports_written_bytes_when_post_format_read_fails() {
    let dir = crate::test_helpers::tempdir();
    let root = dir.path();
    fs::write(root.join("biome.json"), "{}\n").unwrap();
    let bin = root.join("node_modules/.bin");
    fs::create_dir_all(&bin).unwrap();
    let formatter = bin.join("biome");
    link_cached_executable(
        &formatter,
        br##"#!/bin/sh
if [ "$1" = "--version" ]; then echo "biome 2.0.0"; exit 0; fi
for file in "$@"; do :; done
cp "$file" "$file.observed"
rm "$file"
"##,
    )
    .unwrap();
    warm_executable(&formatter, &["--version"]);
    let mut paths = vec![bin];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let path = std::env::join_paths(paths).unwrap();
    let mut aft = AftProcess::spawn_with_env(&[("PATH", path.as_os_str())]);
    let configured = aft.send(
        &json!({
            "id": "append-post-format-config",
            "command": "configure",
            "harness": "opencode",
            "project_root": root,
            "config": user_config(json!({
                "format_on_edit": true,
                "formatter": { "typescript": "biome" }
            })),
        })
        .to_string(),
    );
    assert_eq!(configured["success"], true, "{configured:#}");
    let target = root.join("created.ts");
    let content = "export const value = 1;\n";
    let response = tool_call(
        &mut aft,
        "append-post-format",
        "edit",
        json!({ "path": target, "appendContent": content }),
    );
    assert_eq!(
        fs::read_to_string(root.join("created.ts.observed")).unwrap_or_else(|error| panic!(
            "formatter did not capture bytes: {error}; {response:#}"
        )),
        content
    );
    assert!(!target.exists());
    assert_eq!(response["success"], true, "{response:#}");
    assert_eq!(response["bytes_written"], content.len(), "{response:#}");
    assert!(response["syntax_valid"].is_null(), "{response:#}");
    assert!(response.get("diff").is_none(), "{response:#}");
    let warning = response["warnings"][0]
        .as_str()
        .expect("post-write warning");
    assert!(warning.contains("Append wrote 24 bytes"), "{warning}");
    assert!(
        warning.contains("reading the file after formatting failed"),
        "{warning}"
    );
    assert!(
        response["text"].as_str().unwrap().contains(warning),
        "{response:#}"
    );
    assert!(aft.shutdown().success());
}

#[test]
fn append_with_create_dirs_false_rejects_missing_parent() {
    let dir = crate::test_helpers::tempdir();
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
    let dir = crate::test_helpers::tempdir();
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
    let dir = crate::test_helpers::tempdir();
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
    let dir = crate::test_helpers::tempdir();
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
    let dir = crate::test_helpers::tempdir();
    let target = dir.path().join("diff.txt");
    fs::write(&target, "line1\n").unwrap();

    let mut aft = AftProcess::spawn();
    configure(&mut aft, dir.path());

    // `include_diff` is the compact, agent-facing default: counts only, NEVER
    // full before/after content (which would scale the payload with file size).
    let mut req = append_request("append-diff", &target, "line2\n");
    req["include_diff"] = json!(true);
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_eq!(resp["success"], true, "append failed: {resp:?}");

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
    // Counts-only contract: no before/after content echoed to the agent.
    assert!(
        diff.get("before").is_none() && diff.get("after").is_none(),
        "include_diff must be counts-only (no before/after): {resp:?}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn append_include_diff_content_returns_full_before_after() {
    // The UI/plugin path opts into full content with `include_diff_content`.
    let dir = crate::test_helpers::tempdir();
    let target = dir.path().join("diff-content.txt");
    fs::write(&target, "line1\n").unwrap();

    let mut aft = AftProcess::spawn();
    configure(&mut aft, dir.path());

    let mut req = append_request("append-diff-content", &target, "line2\n");
    req["include_diff_content"] = json!(true);
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_eq!(resp["success"], true, "append failed: {resp:?}");
    let diff = resp
        .get("diff")
        .expect("include_diff_content should include diff");
    assert_eq!(diff["additions"], 1);
    assert_eq!(diff["deletions"], 0);
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
    let dir = crate::test_helpers::tempdir();
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
    let dir = crate::test_helpers::tempdir();
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
