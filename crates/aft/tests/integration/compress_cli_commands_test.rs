use std::fs;

use serde_json::json;
use tempfile::tempdir;

use super::test_helpers::AftProcess;

fn configure(aft: &mut AftProcess, project: &std::path::Path, storage: &std::path::Path) {
    let response = aft.send(
        &json!({
            "id": "cfg",
            "command": "configure",
            "harness": "opencode",
            "project_root": project,
            "storage_dir": storage,
            "experimental_bash_compress": true,
            "search_index": false,
            "semantic_search": false
        })
        .to_string(),
    );
    assert_eq!(response["success"], true, "configure failed: {response}");
}

fn write_filter(dir: &std::path::Path, name: &str, body: &str) {
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join(format!("{name}.toml")), body).unwrap();
}

#[test]
fn list_filters_reports_builtin_user_project_and_invalid_untrusted() {
    let project = tempdir().unwrap();
    let storage = tempdir().unwrap();
    write_filter(
        &storage.path().join("filters"),
        "user-build",
        r#"[filter]
matches = ["user-build"]
description = "User build filter"
"#,
    );
    write_filter(
        &project.path().join(".aft/filters"),
        "internal-test",
        r#"[filter]
matches = ["internal-test"]
description = "Internal test filter"
"#,
    );
    write_filter(
        &project.path().join(".aft/filters"),
        "bad",
        "not valid = toml = at all =",
    );

    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path(), storage.path());
    let response = aft.send(&json!({ "id": "list", "command": "list_filters" }).to_string());

    assert_eq!(response["success"], true, "list failed: {response}");
    let filters = response["filters"].as_array().unwrap();
    assert!(filters
        .iter()
        .any(|f| f["source"] == "builtin" && f["name"] == "make"));
    assert!(filters
        .iter()
        .any(|f| f["source"] == "user" && f["name"] == "user-build"));
    let project_filter = filters
        .iter()
        .find(|f| f["source"] == "project" && f["name"] == "internal-test")
        .unwrap();
    assert_eq!(project_filter["trusted"], false);
    let invalid = filters
        .iter()
        .find(|f| f["source"] == "project_invalid" && f["name"] == "bad")
        .unwrap();
    assert!(invalid["error"].as_str().unwrap().contains("invalid TOML"));
}

#[test]
fn trust_project_marks_current_project_trusted() {
    let project = tempdir().unwrap();
    let storage = tempdir().unwrap();
    write_filter(
        &project.path().join(".aft/filters"),
        "printf",
        r#"[filter]
matches = ["printf"]
description = "Replace printf output"

[shortcircuit]
when = "hello"
replacement = "project filter active"
"#,
    );
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path(), storage.path());

    let trust = aft.send(
        &json!({ "id": "trust", "command": "trust_filter_project", "project_root": project.path() })
            .to_string(),
    );
    assert_eq!(trust["success"], true, "trust failed: {trust}");
    assert_eq!(trust["trusted"], true);

    let list = aft.send(&json!({ "id": "list", "command": "list_filters" }).to_string());
    let project_filter = list["filters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|filter| filter["name"] == "printf")
        .unwrap();
    assert_eq!(project_filter["trusted"], true);
    assert_eq!(list["trusted_projects"].as_array().unwrap().len(), 1);
}

#[test]
fn untrust_project_removes_trusted_path() {
    let project = tempdir().unwrap();
    let storage = tempdir().unwrap();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path(), storage.path());

    let trust = aft.send(
        &json!({ "id": "trust", "command": "trust_filter_project", "project_root": project.path() })
            .to_string(),
    );
    assert_eq!(trust["success"], true);
    let untrust = aft.send(
        &json!({ "id": "untrust", "command": "untrust_filter_project", "project_root": project.path() })
            .to_string(),
    );
    assert_eq!(untrust["success"], true, "untrust failed: {untrust}");
    assert_eq!(untrust["trusted"], false);

    let list = aft.send(&json!({ "id": "list", "command": "list_filters" }).to_string());
    assert_eq!(list["trusted_projects"].as_array().unwrap().len(), 0);
}

#[test]
fn trust_project_errors_when_path_missing() {
    let project = tempdir().unwrap();
    let storage = tempdir().unwrap();
    let missing = project.path().join("missing");
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path(), storage.path());

    let response = aft.send(
        &json!({ "id": "trust", "command": "trust_filter_project", "project_root": missing })
            .to_string(),
    );
    assert_eq!(response["success"], false);
    assert_eq!(response["code"], "path_not_found");
}
