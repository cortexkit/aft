use super::helpers::AftProcess;
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

fn write_file(root: &Path, relative: &str, content: &str) -> PathBuf {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&path, content).unwrap();
    path
}

fn send(aft: &mut AftProcess, request: serde_json::Value) -> serde_json::Value {
    aft.send(&request.to_string())
}

fn send_remove_import(
    aft: &mut AftProcess,
    id: &str,
    file: &Path,
    module: &str,
    name: Option<&str>,
) -> serde_json::Value {
    let mut request = json!({
        "id": id,
        "command": "remove_import",
        "file": file,
        "module": module,
    });

    if let Some(name) = name {
        request["name"] = json!(name);
    }

    send(aft, request)
}

#[test]
fn remove_import_reports_not_removed_when_name_is_absent() {
    let dir = tempdir().unwrap();
    let file = write_file(
        dir.path(),
        "imports.ts",
        "import { useEffect } from 'react';\n\nexport function App() {}\n",
    );

    let before = fs::read_to_string(&file).unwrap();
    let mut aft = AftProcess::spawn();

    let resp = send_remove_import(
        &mut aft,
        "rm-missing-name",
        &file,
        "react",
        Some("useState"),
    );

    assert_eq!(resp["success"], true, "request should complete: {resp:?}");
    assert_eq!(resp["removed"], false, "name was not present: {resp:?}");
    assert_eq!(resp["reason"], "name_not_found");
    assert_eq!(resp["name"], "useState");
    assert_eq!(fs::read_to_string(&file).unwrap(), before);

    assert!(aft.shutdown().success());
}

#[test]
fn remove_import_reports_removed_when_name_is_present() {
    let dir = tempdir().unwrap();
    let file = write_file(
        dir.path(),
        "imports.ts",
        "import { useEffect, useState } from 'react';\n\nexport function App() {}\n",
    );

    let mut aft = AftProcess::spawn();

    let resp = send_remove_import(
        &mut aft,
        "rm-present-name",
        &file,
        "react",
        Some("useState"),
    );

    assert_eq!(resp["success"], true, "remove should succeed: {resp:?}");
    assert_eq!(resp["removed"], true, "file should change: {resp:?}");
    assert_eq!(resp["name"], "useState");

    let content = fs::read_to_string(&file).unwrap();
    assert!(
        content.contains("useEffect"),
        "remaining import should stay: {content}"
    );
    assert!(
        !content.contains("useState"),
        "removed import should be gone: {content}"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn outline_files_reports_unsupported_language_skips() {
    let dir = tempdir().unwrap();
    let supported = write_file(dir.path(), "src/app.ts", "export function app() {}\n");
    let unsupported = write_file(dir.path(), "notes.txt", "not outlineable\n");

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = send(
        &mut aft,
        json!({
            "id": "outline-unsupported",
            "command": "outline",
            "files": [supported, unsupported],
        }),
    );

    assert_eq!(resp["success"], true, "outline should complete: {resp:?}");
    let text = resp["text"].as_str().expect("outline text");
    assert!(
        text.contains("src/\n  app.ts"),
        "supported file should render: {text}"
    );

    let skipped = resp["skipped_files"]
        .as_array()
        .expect("skipped_files array");
    assert_eq!(skipped.len(), 1, "one file should be skipped: {resp:?}");
    assert_eq!(skipped[0]["file"], "notes.txt");
    assert_eq!(skipped[0]["reason"], "unsupported_language");

    assert!(aft.shutdown().success());
}

#[test]
fn outline_directory_reports_parse_error_skips() {
    let dir = tempdir().unwrap();
    write_file(dir.path(), "src/valid.ts", "export function valid() {}\n");
    write_file(dir.path(), "src/invalid.ts", "export function broken( {\n");

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = send(
        &mut aft,
        json!({
            "id": "outline-parse-error",
            "command": "outline",
            "directory": dir.path(),
        }),
    );

    assert_eq!(resp["success"], true, "outline should complete: {resp:?}");
    let text = resp["text"].as_str().expect("outline text");
    assert!(
        text.contains("valid.ts"),
        "valid file should render: {text}"
    );
    assert!(
        !text.contains("invalid.ts"),
        "parse-error file should not render: {text}"
    );

    let skipped = resp["skipped_files"]
        .as_array()
        .expect("skipped_files array");
    assert_eq!(skipped.len(), 1, "one file should be skipped: {resp:?}");
    assert_eq!(skipped[0]["file"], "src/invalid.ts");
    assert_eq!(skipped[0]["reason"], "parse_error");

    assert!(aft.shutdown().success());
}

#[test]
fn outline_directory_reports_walk_truncation() {
    let dir = tempdir().unwrap();
    for index in 0..201 {
        write_file(
            dir.path(),
            &format!("src/file_{index:03}.ts"),
            "export function item() {}\n",
        );
    }

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = send(
        &mut aft,
        json!({
            "id": "outline-truncated",
            "command": "outline",
            "directory": dir.path(),
        }),
    );

    assert_eq!(resp["success"], true, "outline should complete: {resp:?}");
    assert_eq!(resp["complete"], false, "response: {resp:?}");
    assert_eq!(resp["walk_truncated"], true, "response: {resp:?}");

    assert!(aft.shutdown().success());
}
