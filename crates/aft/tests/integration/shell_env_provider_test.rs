#![cfg(unix)]

use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use serde_json::{json, Value};

use super::helpers::{user_config, AftProcess};

fn configure(aft: &mut AftProcess, project: &Path, storage: &Path, command: Vec<String>) {
    let result = aft.send(
        &json!({
            "id":"provider-config", "command":"configure", "harness":"opencode",
            "project_root":project, "storage_dir":storage, "bash_permissions":true,
            "config":user_config(json!({
                "git":{"co_author":"off"},
                "bash":{"background":true},
                "sandbox":{"profile":{"head":"strict", "worker":"strict"}},
                "shell":{"env_provider":{"kind":"custom", "command":command}}
            }))
        })
        .to_string(),
    );
    assert_eq!(result["success"], true, "{result}");
}

fn call(aft: &mut AftProcess, id: &str, params: Value) -> Value {
    aft.send(
        &json!({"id":id, "method":"bash", "session_id":"provider-session", "params":params})
            .to_string(),
    )
}

fn fixture() -> (tempfile::TempDir, AftProcess) {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    for directory in ["project", "storage", "home/toolchain/bin"] {
        std::fs::create_dir_all(root.join(directory)).unwrap();
    }
    let tool = root.join("home/toolchain/bin/project-node");
    std::fs::write(&tool, "#!/bin/sh\nprintf 'project-node20\\n'\n").unwrap();
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    let home = root.join("home");
    let mut aft = AftProcess::spawn_with_env(&[("HOME", home.as_os_str())]);
    let export = json!({"PATH":format!("{}:/usr/bin:/bin", root.join("home/toolchain/bin").display()), "GIT_AUTHOR_NAME":"Sandbox Author", "REMOVE":null}).to_string();
    configure(
        &mut aft,
        &root.join("project"),
        &root.join("storage"),
        vec![
            "/bin/sh".into(),
            "-c".into(),
            format!(
                "printf 'run\\n' >> provider-runs; printf '%s' '{}'",
                export.replace('\'', "'\\''")
            ),
        ],
    );
    (fixture, aft)
}

#[test]
fn shell_env_provider_native_shell_sees_path_and_removes_variables() {
    let (_fixture, mut aft) = fixture();
    let result = call(
        &mut aft,
        "provider-native",
        json!({
            "command":"project-node; printf '%s|%s|%s' \"$GIT_AUTHOR_NAME\" \"${REMOVE-unset}\" \"${AFT_INTERNAL_SHELL_ENV_REMOVALS-unset}\"",
            "env":{"REMOVE":"base"}, "foreground_orchestrate":true, "compressed":false,
        }),
    );
    assert_eq!(result["status"], "completed", "{result}");
    let output = result["output"].as_str().unwrap();
    assert!(output.contains("project-node20"), "{result}");
    assert!(output.contains("Sandbox Author|unset|unset"), "{result}");
    assert!(aft.shutdown().success());
}

#[test]
fn shell_env_provider_host_approval_freezes_environment_and_refuses_changed_input() {
    let (fixture, mut aft) = fixture();
    let params = json!({"command":"project-node; printf '%s' \"$GIT_AUTHOR_NAME\"", "sandbox":"host", "foreground_orchestrate":true, "compressed":false});
    let asked = call(&mut aft, "provider-host-ask", params.clone());
    assert_eq!(asked["code"], "permission_required", "{asked}");
    let grant = asked["asks"][0]["grant_id"].as_str().unwrap();
    let mut retry = params.clone();
    retry["permissions_granted"] = json!([grant]);
    let ran = call(&mut aft, "provider-host-retry", retry);
    assert_eq!(ran["status"], "completed", "{ran}");
    assert!(
        ran["output"].as_str().unwrap().contains("project-node20"),
        "{ran}"
    );
    assert!(
        ran["output"].as_str().unwrap().contains("Sandbox Author"),
        "{ran}"
    );
    let count = || {
        std::fs::read_to_string(fixture.path().join("project/provider-runs"))
            .unwrap()
            .lines()
            .count()
    };
    assert_eq!(
        count(),
        1,
        "the retry must use the approved environment without rerunning custom provider"
    );
    let asked = call(&mut aft, "provider-host-ask-changed", params.clone());
    let grant = asked["asks"][0]["grant_id"].as_str().unwrap();
    let mut changed = params;
    changed["permissions_granted"] = json!([grant]);
    changed["env"] = json!({"EXTRA":"changed after approval"});
    let refused = call(&mut aft, "provider-host-changed", changed);
    assert_eq!(refused["code"], "sandbox_escalation_denied", "{refused}");
    assert_eq!(
        count(),
        2,
        "a changed retry must not reload the provider either"
    );
    assert!(aft.shutdown().success());
}

#[test]
fn shell_env_provider_failure_notice_precedes_existing_task_tail() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    let mut aft = AftProcess::spawn_with_env(&[("HOME", OsStr::new(root))]);
    let configured = aft.send(&json!({"id":"failure-config", "command":"configure", "harness":"opencode", "project_root":root,
        "config":user_config(json!({"git":{"co_author":"off"}, "bash":{"background":true}, "sandbox":{"profile":{"head":"off"}}, "shell":{"env_provider":{"kind":"custom", "command":["/bin/sh", "-c", "echo first-stderr-line >&2; exit 1"]}}}))
    }).to_string());
    assert_eq!(configured["success"], true, "{configured}");
    let result = call(
        &mut aft,
        "provider-failure",
        json!({"command":"printf 'shell-ran\\n'", "foreground_orchestrate":true, "compressed":false}),
    );
    assert_eq!(result["status"], "completed", "{result}");
    let output = result["output"].as_str().unwrap();
    let notice = "Project environment not loaded (exit 1: first-stderr-line); this command used AFT's default PATH.";
    assert!(output.contains("shell-ran"), "{result}");
    assert!(output.contains(&format!("{notice}\n[task ")), "{result}");
    assert!(aft.shutdown().success());
}
