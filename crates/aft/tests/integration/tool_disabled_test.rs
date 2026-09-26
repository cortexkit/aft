//! A tool listed in `disabled_tools` is refused at dispatch for every caller,
//! not only left unregistered by the plugins. These drive the standalone
//! NDJSON `tool_call` path; `subc_bridge_test` drives the same refusal over a
//! bound subc route.

use std::path::Path;

use serde_json::{json, Value};

use super::helpers::AftProcess;

const SESSION_ID: &str = "tool-disabled-session";

struct Fixture {
    aft: AftProcess,
    _dir: tempfile::TempDir,
    project: std::path::PathBuf,
    user_config: std::path::PathBuf,
}

/// Start a bridge configured from a user config file holding `user_doc`
/// (`None` leaves the file absent).
fn fixture(user_doc: Option<&str>) -> Fixture {
    let dir = tempfile::tempdir().expect("tool-disabled tempdir");
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).expect("create project");
    let user_config = dir.path().join("user-aft.jsonc");
    if let Some(doc) = user_doc {
        std::fs::write(&user_config, doc).expect("write user config");
    }
    let mut aft = AftProcess::spawn();
    let response = send(
        &mut aft,
        json!({
            "id": "cfg",
            "command": "configure",
            "harness": "opencode",
            "session_id": SESSION_ID,
            "project_root": project.to_string_lossy(),
            "cortexkit_user_config_path": user_config.to_string_lossy(),
        }),
    );
    assert_eq!(response["success"], true, "configure failed: {response:#}");
    Fixture {
        aft,
        _dir: dir,
        project,
        user_config,
    }
}

fn send(aft: &mut AftProcess, request: Value) -> Value {
    aft.send(&serde_json::to_string(&request).expect("serialize request"))
}

fn tool_call(aft: &mut AftProcess, id: &str, name: &str, arguments: Value) -> Value {
    send(
        aft,
        json!({
            "id": id,
            "command": "tool_call",
            "session_id": SESSION_ID,
            "name": name,
            "arguments": arguments,
        }),
    )
}

fn assert_disabled(response: &Value, disabled_entry: &str, label: &str) {
    assert_eq!(response["success"], false, "{label}: {response:#}");
    assert_eq!(response["code"], "tool_disabled", "{label}: {response:#}");
    let message = response["message"].as_str().unwrap_or_default();
    assert!(
        message.contains(&format!(
            "remove \"{disabled_entry}\" from `disabled_tools`"
        )) && message.contains("~/.config/cortexkit/aft.jsonc")
            && message.contains("npx @cortexkit/aft setup")
            && message.contains("restart the host"),
        "{label}: refusal must name the tool and the fix: {response:#}"
    );
}

fn write_file(root: &Path, name: &str) -> std::path::PathBuf {
    let path = root.join(name);
    std::fs::write(&path, "content\n").expect("write fixture file");
    path
}

#[test]
fn standalone_default_user_config_refuses_move_and_delete_until_the_file_allows_them() {
    // A user file with no `disabled_tools` key gets the default: move and
    // delete stay off.
    let mut f = fixture(Some("{}"));
    let victim = write_file(&f.project, "victim.txt");
    let moved = f.project.join("moved.txt");

    let delete = tool_call(
        &mut f.aft,
        "delete-default",
        "delete",
        json!({ "files": ["victim.txt"] }),
    );
    assert_disabled(&delete, "aft_delete", "default delete");
    assert!(victim.exists(), "a refused delete must not touch the file");

    let mv = tool_call(
        &mut f.aft,
        "move-default",
        "move",
        json!({ "path": "victim.txt", "destination": "moved.txt" }),
    );
    assert_disabled(&mv, "aft_move", "default move");
    assert!(
        victim.exists() && !moved.exists(),
        "a refused move must not run"
    );

    // Mid-session edit, no reconfigure: the next call reads the new list.
    std::fs::write(&f.user_config, r#"{ "disabled_tools": [] }"#).expect("rewrite user config");
    let mv = tool_call(
        &mut f.aft,
        "move-allowed",
        "move",
        json!({ "path": "victim.txt", "destination": "moved.txt" }),
    );
    assert_eq!(mv["success"], true, "explicit [] allows move: {mv:#}");
    assert!(moved.exists() && !victim.exists());
    let delete = tool_call(
        &mut f.aft,
        "delete-allowed",
        "delete",
        json!({ "files": ["moved.txt"] }),
    );
    assert_eq!(
        delete["success"], true,
        "explicit [] allows delete: {delete:#}"
    );
    assert!(!moved.exists());

    // And back: re-disabling takes effect on the next call as well.
    std::fs::write(&f.user_config, r#"{ "disabled_tools": ["aft_delete"] }"#)
        .expect("rewrite user config");
    let again = write_file(&f.project, "again.txt");
    let delete = tool_call(
        &mut f.aft,
        "delete-redisabled",
        "delete",
        json!({ "files": ["again.txt"] }),
    );
    assert_disabled(&delete, "aft_delete", "re-disabled delete");
    assert!(again.exists());
    assert!(f.aft.shutdown().success());
}

#[test]
fn standalone_disabled_read_is_refused_under_its_hoisted_and_prefixed_names() {
    let mut f = fixture(Some(r#"{ "disabled_tools": ["read"] }"#));
    write_file(&f.project, "notes.txt");
    for name in ["read", "aft_read"] {
        let response = tool_call(
            &mut f.aft,
            &format!("read-as-{name}"),
            name,
            json!({ "filePath": "notes.txt" }),
        );
        assert_disabled(&response, "read", name);
    }
    // A tool that is not disabled still runs.
    let glob = tool_call(&mut f.aft, "glob-ok", "glob", json!({ "pattern": "*.txt" }));
    assert_eq!(glob["success"], true, "{glob:#}");
    assert!(f.aft.shutdown().success());
}

#[test]
fn standalone_disabled_bash_refuses_agent_calls_but_keeps_plumbing() {
    let mut f = fixture(Some(r#"{ "disabled_tools": ["bash"] }"#));
    let marker = f.project.join("ran.txt");
    let bash = tool_call(
        &mut f.aft,
        "bash-disabled",
        "bash",
        json!({ "command": format!("touch {}", marker.display()) }),
    );
    assert_disabled(&bash, "bash", "bash");
    assert!(!marker.exists(), "a refused bash must not run");

    // The agent's companion call (catalog `taskId` spelling) follows bash.
    let status = tool_call(
        &mut f.aft,
        "bash-status-agent",
        "bash_status",
        json!({ "taskId": "bgb-missing" }),
    );
    assert_disabled(&status, "bash", "agent bash_status");

    // The plugins' own completion plumbing keeps working.
    let drain = tool_call(&mut f.aft, "drain", "bash_drain_completions", json!({}));
    assert_eq!(drain["success"], true, "drain must keep working: {drain:#}");
    let ack = tool_call(
        &mut f.aft,
        "ack",
        "bash_ack_completions",
        json!({ "task_ids": [] }),
    );
    assert_eq!(ack["success"], true, "ack must keep working: {ack:#}");
    assert!(f.aft.shutdown().success());
}
