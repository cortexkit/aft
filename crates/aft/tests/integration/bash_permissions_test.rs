use super::helpers::AftProcess;

use serde_json::json;
use tempfile::TempDir;

fn configure(aft: &mut AftProcess, root: &TempDir) {
    let response = aft.send(
        &serde_json::to_string(&json!({
            "id": "cfg",
            "command": "configure",
            "harness": "opencode",
            "project_root": root.path(),
            "bash_permissions": true,
        }))
        .unwrap(),
    );
    assert_eq!(response["success"], true, "configure failed: {response:?}");
}

fn configure_path(aft: &mut AftProcess, root: &std::path::Path) {
    let response = aft.send(
        &serde_json::to_string(&json!({
            "id": "cfg",
            "command": "configure",
            "harness": "opencode",
            "project_root": root,
            "bash_permissions": true,
        }))
        .unwrap(),
    );
    assert_eq!(response["success"], true, "configure failed: {response:?}");
}

#[cfg(unix)]
fn create_dir_symlink(src: &std::path::Path, dst: &std::path::Path) {
    std::os::unix::fs::symlink(src, dst).expect("create symlink");
}

#[cfg(windows)]
fn create_dir_symlink(src: &std::path::Path, dst: &std::path::Path) {
    std::os::windows::fs::symlink_dir(src, dst).expect("create symlink");
}

fn bash(aft: &mut AftProcess, id: &str, command: &str) -> serde_json::Value {
    aft.send(
        &serde_json::to_string(&json!({
            "id": id,
            "method": "bash",
            "params": {
                "command": command,
                "permissions_requested": true,
            },
        }))
        .unwrap(),
    )
}

#[test]
fn simple_echo_requires_bash_permission() {
    // Regression: previously AFT silently skipped `echo` from bash
    // permission asks, which let any command starting with `echo`
    // bypass the user's `bash: { "*": deny, ... }` rules even though
    // OpenCode's built-in bash tool always asks for echo.
    // See packages/opencode/src/tool/bash.ts (which only excludes the
    // CWD set: cd / push-location / set-location).
    let root = TempDir::new().unwrap();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let response = bash(&mut aft, "echo", "echo hello");
    assert_eq!(response["success"], false, "response: {response:?}");
    assert_eq!(response["code"], "permission_required");
    let asks = response["asks"]
        .as_array()
        .expect("asks should be an array");
    assert!(
        asks.iter().any(|ask| {
            ask["kind"] == "bash"
                && ask["patterns"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|p| p == "echo hello")
        }),
        "expected a bash ask with pattern 'echo hello': {response:?}"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn rm_outside_project_root_requires_external_directory() {
    let root = TempDir::new().unwrap();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let response = bash(&mut aft, "rm", "rm /tmp/foo.txt");
    assert_eq!(response["success"], false, "response: {response:?}");
    assert_eq!(response["code"], "permission_required");
    assert!(
        response["asks"].as_array().unwrap().iter().any(|ask| {
            ask["kind"] == "external_directory"
                && ask["patterns"].as_array().unwrap().iter().any(|p| {
                    p.as_str()
                        .is_some_and(|p| p.contains("tmp/") && p.ends_with("/*"))
                })
        }),
        "response: {response:?}"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn bash_permission_scan_collects_redirect_target() {
    let root = TempDir::new().unwrap();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let response = bash(&mut aft, "redirect", "echo hi > /tmp/aft-redirect-out");
    assert_eq!(response["success"], false, "response: {response:?}");
    assert!(
        response["asks"].as_array().unwrap().iter().any(|ask| {
            ask["kind"] == "external_directory"
                && ask["patterns"].as_array().unwrap().iter().any(|p| {
                    p.as_str()
                        .is_some_and(|p| p.contains("tmp/") && p.ends_with("/*"))
                })
        }),
        "expected external_directory ask for redirect target: {response:?}"
    );

    let dynamic = bash(&mut aft, "dynamic-redirect", "echo hi > $OUTFILE");
    assert_eq!(dynamic["success"], false, "response: {dynamic:?}");
    assert!(dynamic["asks"].as_array().unwrap().iter().any(|ask| {
        ask["kind"] == "external_directory"
            && ask["patterns"].as_array().unwrap().iter().any(|p| p == "*")
    }));

    assert!(aft.shutdown().success());
}

#[test]
fn bash_permission_scan_handles_source() {
    let root = TempDir::new().unwrap();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let response = bash(&mut aft, "source", "source /tmp/aft-source.sh");
    assert_eq!(response["success"], false, "response: {response:?}");
    assert!(
        response["asks"].as_array().unwrap().iter().any(|ask| {
            ask["kind"] == "external_directory"
                && ask["patterns"].as_array().unwrap().iter().any(|p| {
                    p.as_str()
                        .is_some_and(|p| p.contains("tmp/") && p.ends_with("/*"))
                })
        }),
        "expected external_directory ask for source target: {response:?}"
    );

    let dot = bash(&mut aft, "dot-source", ". /tmp/aft-dot-source.sh");
    assert_eq!(dot["success"], false, "response: {dot:?}");
    assert!(dot["asks"].as_array().unwrap().iter().any(|ask| {
        ask["kind"] == "external_directory"
            && ask["patterns"].as_array().unwrap().iter().any(|p| {
                p.as_str()
                    .is_some_and(|p| p.contains("tmp/") && p.ends_with("/*"))
            })
    }));

    assert!(aft.shutdown().success());
}

#[test]
fn symlink_path_resolving_outside_project_requires_permission() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("project");
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    create_dir_symlink(&outside, &root.join("link"));
    std::fs::write(outside.join("secret.txt"), "secret").unwrap();

    let mut aft = AftProcess::spawn();
    configure_path(&mut aft, &root);

    let response = bash(&mut aft, "symlink-outside", "rm link/secret.txt");
    assert_eq!(response["success"], false, "response: {response:?}");
    assert_eq!(response["code"], "permission_required");
    assert!(response["asks"].as_array().unwrap().iter().any(|ask| {
        ask["kind"] == "external_directory"
            && ask["patterns"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p.as_str().is_some_and(|p| p.contains("outside")))
    }));

    assert!(aft.shutdown().success());
}

#[test]
fn chained_cd_then_rm_uses_subcommand_directory() {
    let root = TempDir::new().unwrap();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let response = bash(&mut aft, "chain", "cd /tmp && rm foo");
    assert_eq!(response["success"], false, "response: {response:?}");
    assert!(
        response["asks"].as_array().unwrap().iter().any(|ask| {
            ask["kind"] == "external_directory"
                && ask["patterns"].as_array().unwrap().iter().any(|p| {
                    p.as_str()
                        .is_some_and(|p| p.contains("tmp/") && p.ends_with("/*"))
                })
        }),
        "response: {response:?}"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn git_status_returns_bash_ask_with_stable_prefix() {
    let root = TempDir::new().unwrap();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let response = bash(&mut aft, "git", "git status");
    assert_eq!(response["success"], false, "response: {response:?}");
    let asks = response["asks"].as_array().unwrap();
    assert!(asks.iter().any(|ask| {
        ask["kind"] == "bash"
            && ask["patterns"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p == "git status")
            && ask["always"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p == "git status *")
    }));

    assert!(aft.shutdown().success());
}

#[test]
fn pipe_returns_asks_for_each_subcommand() {
    let root = TempDir::new().unwrap();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let response = bash(&mut aft, "pipe", "find . | xargs grep foo");
    assert_eq!(response["success"], false, "response: {response:?}");
    let asks = response["asks"].as_array().unwrap();
    assert!(asks.iter().any(|ask| ask["kind"] == "bash"
        && ask["patterns"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p == "find .")));
    assert!(asks.iter().any(|ask| ask["kind"] == "bash"
        && ask["patterns"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p == "grep foo")));

    assert!(aft.shutdown().success());
}

#[test]
fn granted_permission_allows_git_status_short() {
    let root = TempDir::new().unwrap();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let response = aft.send(
        &serde_json::to_string(&json!({
            "id": "grant",
            "method": "bash",
            "params": {
                "command": "git status --short",
                "permissions_requested": true,
                "permissions_granted": ["git status *"],
            },
        }))
        .unwrap(),
    );
    assert_ne!(
        response["code"], "permission_required",
        "response: {response:?}"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn background_command_requires_permission_before_spawn() {
    let root = TempDir::new().unwrap();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let response = aft.send(
        &serde_json::to_string(&json!({
            "id": "bg-permission",
            "method": "bash",
            "params": {
                "command": "rm /tmp/aft-background-permission-test.txt",
                "permissions_requested": true,
                "background": true,
            },
        }))
        .unwrap(),
    );

    assert_eq!(response["success"], false, "response: {response:?}");
    assert_eq!(response["code"], "permission_required");
    assert!(response.get("task_id").is_none(), "response: {response:?}");

    assert!(aft.shutdown().success());
}

/// Probe: figure out which command shapes produce ZERO asks. Any zero-ask
/// path is an effective bypass of the user's `bash: { "*": deny }` rule
/// because `bash.rs:110` only blocks the request when asks are non-empty.
/// This test drives a bunch of command shapes through the live bridge and
/// asserts they all produce at least one ask. Failures are real bypasses.
#[test]
fn no_command_shape_produces_zero_asks() {
    let root = TempDir::new().unwrap();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let cases = [
        // basic non-whitelisted commands
        "git status",
        "ls -la",
        "find . -name foo",
        "cat README.md",
        // bash builtins the agent might reach for
        "set -e",
        "shopt -s nullglob",
        "type git",
        "alias",
        // pure redirect (no command word)
        "> /tmp/x",
        ":",
        ": > /tmp/x",
        // variable assignment only
        "FOO=bar",
        // arithmetic / test-only forms — these parse as their own AST nodes
        // (arithmetic_command, test_command, declaration_command) and produce
        // ZERO `command` nodes, so the empty-command-nodes fail-closed branch
        // is the only thing standing between them and `bash: { "*": deny }`
        // bypass. Oracle audit (v0.19.5..HEAD MEDIUM #2): make sure each
        // shape is tracked explicitly so future grammar/scan refactors can't
        // silently regress them.
        "((i++))",
        "[[ -f foo ]]",
        "readonly FOO=bar",
        "declare -A map=()",
        // sub-shells and groups
        "(ls)",
        "{ ls; }",
        // backticks / command substitution at top level
        "$(ls)",
        "`ls`",
        // eval and bash -c smuggling
        "eval ls",
        "bash -c ls",
        // pipes
        "ls | head",
        // here-doc
        "cat <<EOF\nhi\nEOF",
        // unicode / odd whitespace
        "git\u{00a0}status",
    ];

    let mut bypasses = Vec::new();
    let mut zero_ask_failures = Vec::new();
    for (i, cmd) in cases.iter().enumerate() {
        let id = format!("probe-{i}");
        let response = bash(&mut aft, &id, cmd);
        let asks_len = response["asks"].as_array().map(|a| a.len()).unwrap_or(0);
        let success = response["success"].as_bool().unwrap_or(true);

        if success && asks_len == 0 {
            bypasses.push(format!("{cmd:?} -> {response}"));
        } else if asks_len == 0 && !success {
            zero_ask_failures.push(format!(
                "{cmd:?} (code={:?}, message={:?})",
                response["code"], response["message"]
            ));
        }
    }

    eprintln!("zero-ask but failed for unrelated reasons:");
    for line in &zero_ask_failures {
        eprintln!("  - {line}");
    }

    if !bypasses.is_empty() {
        let joined = bypasses.join("\n  ");
        panic!(
            "BYPASSES: the following commands produced zero asks AND ran successfully:\n  {joined}"
        );
    }

    assert!(aft.shutdown().success());
}

#[test]
fn malformed_bash_requires_full_permission_prompt() {
    let root = TempDir::new().unwrap();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let response = bash(&mut aft, "malformed", "echo 'unterminated");
    assert_eq!(response["success"], false, "response: {response:?}");
    assert_eq!(response["code"], "permission_required");
    let asks = response["asks"].as_array().unwrap();
    assert!(asks.iter().any(|ask| {
        ask["kind"] == "bash" && ask["patterns"].as_array().unwrap().iter().any(|p| p == "*")
    }));

    assert!(aft.shutdown().success());
}
