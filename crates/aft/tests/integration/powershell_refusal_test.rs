//! A PowerShell request on a host without `pwsh` is refused, and the refusal
//! names both ways forward instead of quietly running the command under bash.

use std::path::Path;

use serde_json::{json, Value};

use super::helpers::AftProcess;

const SESSION_ID: &str = "powershell-refusal-session";

#[test]
fn powershell_shell_without_pwsh_is_refused_with_the_fix() {
    // An empty PATH guarantees `pwsh` cannot resolve on any host, including CI
    // machines that do have PowerShell installed.
    let empty_path = tempfile::tempdir().expect("empty PATH dir");
    let project = tempfile::tempdir().expect("powershell temp project");
    let mut aft = AftProcess::spawn_with_env(&[("PATH", empty_path.path().as_os_str())]);
    configure_project(&mut aft, project.path(), "cfg-no-pwsh");
    for (label, name, arguments) in [
        (
            "bash-shell-powershell",
            "bash",
            json!({
                "command": "Get-ChildItem",
                "shell": "powershell",
                "foreground_orchestrate": true,
                "block_to_completion": true
            }),
        ),
        (
            "powershell-tool",
            "powershell",
            json!({ "command": "Get-ChildItem" }),
        ),
    ] {
        let response = send_json(
            &mut aft,
            json!({
                "id": format!("tool-call-{label}"),
                "command": "tool_call",
                "session_id": SESSION_ID,
                "name": name,
                "arguments": arguments,
            }),
        );
        assert_eq!(response["success"], false, "{label}: {response:#}");
        assert_eq!(
            response["code"], "powershell_not_installed",
            "{label}: {response:#}"
        );
        let message = response["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("PowerShell (pwsh) is not installed")
                && message.contains("was not run")
                && message.contains("bash tool without `shell: \"powershell\"`")
                && message.contains("https://aka.ms/powershell"),
            "{label}: refusal must name the fix: {response:#}"
        );
    }
    assert!(aft.shutdown().success());
}

fn configure_project(aft: &mut AftProcess, root: &Path, id: &str) {
    let response = send_json(
        aft,
        json!({
            "id": id,
            "command": "configure",
            "harness": "opencode",
            "project_root": root.to_string_lossy(),
            "config": crate::helpers::user_config(json!({
                "search_index": false,
                "semantic_search": false,
                "callgraph_store": false
            })),
        }),
    );
    assert_eq!(response["success"], true, "configure failed: {response:#}");
}

fn send_json(aft: &mut AftProcess, request: Value) -> Value {
    aft.send(&serde_json::to_string(&request).expect("serialize request"))
}
