use super::helpers::AftProcess;

use serde_json::json;
use tempfile::TempDir;

fn non_system_temp_dir(prefix: &str) -> TempDir {
    let base = std::env::current_dir()
        .expect("current dir")
        .join("target")
        .join("aft-non-system-temp-tests");
    std::fs::create_dir_all(&base).expect("create non-system temp test dir");
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(base)
        .expect("create non-system temp dir")
}

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
fn create_dir_symlink(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(src, dst)
}

#[cfg(windows)]
fn create_dir_symlink(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(src, dst)
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

fn has_external_directory_ask(response: &serde_json::Value) -> bool {
    response["asks"]
        .as_array()
        .is_some_and(|asks| asks.iter().any(|ask| ask["kind"] == "external_directory"))
}

fn has_external_directory_pattern_containing(response: &serde_json::Value, needle: &str) -> bool {
    response["asks"].as_array().is_some_and(|asks| {
        asks.iter().any(|ask| {
            ask["kind"] == "external_directory"
                && ask["patterns"].as_array().is_some_and(|patterns| {
                    patterns.iter().any(|p| {
                        p.as_str().is_some_and(|pattern| {
                            pattern.contains(needle) && pattern.ends_with("/*")
                        })
                    })
                })
        })
    })
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
fn system_temp_path_does_not_require_external_directory() {
    let root = TempDir::new().unwrap();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let temp_file = std::env::temp_dir().join("aft-bash-temp-exemption.txt");
    let command = format!("cat {}", temp_file.display());
    let response = bash(&mut aft, "cat-temp", &command);
    assert_eq!(response["success"], false, "response: {response:?}");
    assert_eq!(response["code"], "permission_required");
    assert!(
        !has_external_directory_ask(&response),
        "system temp paths must not emit external_directory asks: {response:?}"
    );
    assert!(
        response["asks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|ask| ask["kind"] == "bash"),
        "bash execution should remain permission-gated: {response:?}"
    );

    assert!(aft.shutdown().success());
}

#[cfg(unix)]
#[test]
fn etc_hosts_still_requires_external_directory() {
    let root = TempDir::new().unwrap();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let response = bash(&mut aft, "cat-etc-hosts", "cat /etc/hosts");
    assert_eq!(response["success"], false, "response: {response:?}");
    assert_eq!(response["code"], "permission_required");
    assert!(
        has_external_directory_pattern_containing(&response, "/etc"),
        "non-temp external paths must still ask: {response:?}"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn bash_permission_scan_collects_redirect_target() {
    let root = TempDir::new().unwrap();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let outside = non_system_temp_dir("aft-redirect-");
    let redirect_target = outside.path().join("aft-redirect-out");
    let response = bash(
        &mut aft,
        "redirect",
        &format!("echo hi > {}", redirect_target.display()),
    );
    assert_eq!(response["success"], false, "response: {response:?}");
    assert!(
        has_external_directory_pattern_containing(&response, "aft-redirect-"),
        "expected external_directory ask for redirect target: {response:?}"
    );

    // Issue #135: a DYNAMIC redirect target can't be resolved to a concrete
    // directory. We match native OpenCode and emit NO external_directory ask
    // (the old `*` wildcard rendered as "Access external directory ." and, if
    // "always"-approved, granted blanket external access). The command is still
    // gated by the bash approval below.
    let dynamic = bash(&mut aft, "dynamic-redirect", "echo hi > $OUTFILE");
    assert_eq!(dynamic["success"], false, "response: {dynamic:?}");
    let dyn_asks = dynamic["asks"].as_array().unwrap();
    assert!(
        !dyn_asks
            .iter()
            .any(|ask| ask["kind"] == "external_directory"),
        "dynamic redirect must not emit an external_directory ask: {dynamic:?}"
    );
    assert!(
        dyn_asks.iter().any(|ask| ask["kind"] == "bash"),
        "dynamic redirect must still require bash approval: {dynamic:?}"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn bash_permission_scan_collects_cd_redirect_target() {
    let root = TempDir::new().unwrap();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let outside = non_system_temp_dir("aft-cd-redirect-");
    let redirect_target = outside.path().join("aft-cd-redirect-out");
    let response = bash(
        &mut aft,
        "cd-redirect",
        &format!("cd /tmp > {}", redirect_target.display()),
    );
    assert_eq!(response["success"], false, "response: {response:?}");
    assert!(
        has_external_directory_pattern_containing(&response, "aft-cd-redirect-"),
        "expected external_directory ask for cd redirect target: {response:?}"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn dynamic_file_args_do_not_emit_external_directory_wildcard() {
    // Issue #135: a dynamic file arg (`rm "$DEST/file"`) is unresolvable. We
    // match native OpenCode (which skips dynamic path args) and emit NO
    // external_directory ask — the bash approval still gates the command.
    let root = TempDir::new().unwrap();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let response = bash(&mut aft, "dynamic-file-arg", r#"rm "$DEST/file""#);
    assert_eq!(response["success"], false, "response: {response:?}");
    assert_eq!(response["code"], "permission_required");
    let asks = response["asks"].as_array().unwrap();
    assert!(
        !asks.iter().any(|ask| ask["kind"] == "external_directory"),
        "dynamic file arg must not emit an external_directory ask: {response:?}"
    );
    assert!(
        asks.iter().any(|ask| ask["kind"] == "bash"),
        "dynamic file arg must still require bash approval: {response:?}"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn relative_redirect_permission_grant_matches_absolute_cwd_pattern() {
    let root = TempDir::new().unwrap();
    let outside = non_system_temp_dir("aft-relative-redirect-");
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let external_dir = std::fs::canonicalize(outside.path()).unwrap();
    let external_grant = format!("{}/*", external_dir.display());
    let response = aft.send(
        &serde_json::to_string(&json!({
            "id": "relative-redirect-grant",
            "method": "bash",
            "params": {
                "command": "echo hi > ./file.log",
                "workdir": outside.path(),
                "permissions_requested": true,
                "permissions_granted": [external_grant, "echo hi > ./file.log"],
            },
        }))
        .unwrap(),
    );

    assert_ne!(
        response["code"], "permission_required",
        "relative redirect should be canonicalized against workdir: {response:?}"
    );
    // Foreground bash spawns through bash_background::spawn and returns
    // immediately on slower CI runners — wait for the file with a deadline
    // rather than asserting synchronously.
    let target = outside.path().join("file.log");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline && !target.exists() {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(
        target.exists(),
        "expected redirect target to exist at {}",
        target.display()
    );

    assert!(aft.shutdown().success());
}

#[test]
fn bash_permission_scan_handles_source() {
    let root = TempDir::new().unwrap();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, &root);

    let outside = non_system_temp_dir("aft-source-");
    let source_target = outside.path().join("aft-source.sh");
    let response = bash(
        &mut aft,
        "source",
        &format!("source {}", source_target.display()),
    );
    assert_eq!(response["success"], false, "response: {response:?}");
    assert!(
        has_external_directory_pattern_containing(&response, "aft-source-"),
        "expected external_directory ask for source target: {response:?}"
    );

    let dot_target = outside.path().join("aft-dot-source.sh");
    let dot = bash(
        &mut aft,
        "dot-source",
        &format!(". {}", dot_target.display()),
    );
    assert_eq!(dot["success"], false, "response: {dot:?}");
    assert!(has_external_directory_pattern_containing(
        &dot,
        "aft-source-"
    ));

    assert!(aft.shutdown().success());
}

#[test]
fn symlink_path_resolving_outside_project_requires_permission() {
    let dir = non_system_temp_dir("aft-symlink-");
    let root = dir.path().join("project");
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    if let Err(error) = create_dir_symlink(&outside, &root.join("link")) {
        if cfg!(windows) {
            eprintln!(
                "skipping symlink_path_resolving_outside_project_requires_permission: Windows symlink privilege unavailable: {error}"
            );
            return;
        }
        panic!("create symlink: {error}");
    }
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

    let outside = non_system_temp_dir("aft-chain-");
    let response = bash(
        &mut aft,
        "chain",
        &format!("cd {} && rm foo", outside.path().display()),
    );
    assert_eq!(response["success"], false, "response: {response:?}");
    assert!(
        has_external_directory_pattern_containing(&response, "aft-chain-"),
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

/// Background-bash output files live under the AFT storage root, outside
/// every project. The scanner lets a session read ITS OWN task artifacts with
/// no external-directory ask, and only its own: the decision is the exact
/// registry check, never a `bash-tasks` prefix, and only for reads.
#[cfg(unix)]
mod session_owned_artifacts {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    const OWNER: &str = "artifact-owner-session";
    const OTHER: &str = "artifact-other-session";

    fn configure_with_storage(aft: &mut AftProcess, project: &Path, storage: &Path) {
        let response = aft.send(
            &serde_json::to_string(&json!({
                "id": "cfg-artifacts",
                "command": "configure",
                "harness": "opencode",
                "project_root": project,
                "storage_dir": storage,
                "config": super::super::helpers::user_config(json!({
                    "experimental": { "bash": { "background": true } }
                })),
            }))
            .unwrap(),
        );
        assert_eq!(response["success"], true, "configure failed: {response:?}");
    }

    /// Run a background task for `session` and return its canonical stdout path.
    fn completed_task_stdout(aft: &mut AftProcess, session: &str, text: &str) -> PathBuf {
        let spawned = aft.send(
            &serde_json::to_string(&json!({
                "id": format!("spawn-{session}"),
                "session_id": session,
                "command": "bash",
                "params": { "command": format!("echo {text}"), "background": true },
            }))
            .unwrap(),
        );
        assert_eq!(spawned["success"], true, "spawn failed: {spawned:?}");
        let task_id = spawned["task_id"].as_str().unwrap().to_string();

        let started = Instant::now();
        loop {
            let status = aft.send(
                &serde_json::to_string(&json!({
                    "id": format!("status-{task_id}"),
                    "session_id": session,
                    "command": "bash_status",
                    "params": { "task_id": task_id },
                }))
                .unwrap(),
            );
            assert_eq!(status["success"], true, "status failed: {status:?}");
            if status["status"] == "completed" {
                let path = status["output_path"]
                    .as_str()
                    .expect("stdout artifact path");
                return std::fs::canonicalize(path).expect("stdout artifact exists");
            }
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "task did not complete: {status:?}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn scan_as(aft: &mut AftProcess, session: &str, command: &str) -> serde_json::Value {
        let response = aft.send(
            &serde_json::to_string(&json!({
                "id": "scan",
                "session_id": session,
                "method": "bash",
                "params": { "command": command, "permissions_requested": true },
            }))
            .unwrap(),
        );
        // Every command here also needs a plain bash ask, so a
        // permission_required reply proves the scanner actually ran.
        assert_eq!(
            response["code"], "permission_required",
            "{command}: {response:?}"
        );
        response
    }

    fn owned(aft: &mut AftProcess, session: &str, path: &Path) -> bool {
        let response = aft.send(
            &serde_json::to_string(&json!({
                "id": "owned",
                "session_id": session,
                "command": "bash_artifact_owned",
                "params": { "path": path },
            }))
            .unwrap(),
        );
        assert_eq!(
            response["success"], true,
            "owned check failed: {response:?}"
        );
        response["owned"].as_bool().expect("owned flag")
    }

    struct Fixture {
        aft: AftProcess,
        project: TempDir,
        _storage: TempDir,
        own: PathBuf,
        foreign: PathBuf,
        lookalike: PathBuf,
    }

    fn fixture() -> Fixture {
        // Neither directory may sit under the system temp dir: temp paths
        // never ask, which would make every assertion below vacuous.
        let project = non_system_temp_dir("artifact-project-");
        let storage = non_system_temp_dir("artifact-storage-");
        let mut aft = AftProcess::spawn();
        configure_with_storage(&mut aft, project.path(), storage.path());
        let own = completed_task_stdout(&mut aft, OWNER, "owner-output");
        let foreign = completed_task_stdout(&mut aft, OTHER, "other-output");
        assert!(!own.starts_with(project.path()));
        let lookalike = own.parent().unwrap().join("unregistered-output");
        std::fs::write(&lookalike, "not a registered artifact\n").unwrap();
        Fixture {
            aft,
            project,
            _storage: storage,
            own,
            foreign,
            lookalike,
        }
    }

    #[test]
    fn own_artifact_reads_do_not_ask_for_external_directory() {
        let mut f = fixture();
        let own = f.own.display().to_string();
        for command in [
            format!("cat {own}"),
            format!("cat '{own}'"),
            format!("wc -l < {own}"),
            format!("cat {own} | tail -n 5"),
        ] {
            let response = scan_as(&mut f.aft, OWNER, &command);
            assert!(
                !has_external_directory_ask(&response),
                "{command} asked for its own artifact: {response:?}"
            );
        }
        assert!(owned(&mut f.aft, OWNER, &f.own));
        assert!(f.aft.shutdown().success());
    }

    #[test]
    fn foreign_and_lookalike_artifact_reads_still_ask() {
        let mut f = fixture();
        let link = f.project.path().join("task-dir-link");
        create_dir_symlink(f.own.parent().unwrap(), &link).unwrap();
        for (label, path) in [
            ("other session's artifact", f.foreign.clone()),
            ("unregistered file beside an artifact", f.lookalike.clone()),
            ("symlinked lookalike", link.join("unregistered-output")),
        ] {
            let command = format!("cat {}", path.display());
            let response = scan_as(&mut f.aft, OWNER, &command);
            assert!(
                has_external_directory_ask(&response),
                "{label} did not ask: {response:?}"
            );
            let redirected = format!("wc -l < {}", path.display());
            let response = scan_as(&mut f.aft, OWNER, &redirected);
            assert!(
                has_external_directory_ask(&response),
                "{label} via input redirect did not ask: {response:?}"
            );
            assert!(
                !owned(&mut f.aft, OWNER, &path),
                "{label} reported as owned"
            );
        }
        // The owner's artifact is not the other session's either.
        let response = scan_as(&mut f.aft, OTHER, &format!("cat {}", f.own.display()));
        assert!(has_external_directory_ask(&response), "{response:?}");
        assert!(!owned(&mut f.aft, OTHER, &f.own));
        assert!(f.aft.shutdown().success());
    }

    #[test]
    fn writes_deletes_and_extra_external_paths_around_own_artifact_still_ask() {
        let mut f = fixture();
        let own = f.own.display().to_string();
        let task_dir = f.own.parent().unwrap().display().to_string();
        let outside = f._storage.path().join("copy.txt").display().to_string();
        for command in [
            format!("cat {own} > {outside}"),
            format!("cat {own} > {task_dir}/copy.txt"),
            format!("cat {own} >> {own}"),
            format!("echo overwrite > {own}"),
            format!("rm {own}"),
            format!("cp {own} {task_dir}/copy.txt"),
            format!("cat {own} {}", f.lookalike.display()),
        ] {
            let response = scan_as(&mut f.aft, OWNER, &command);
            assert!(
                has_external_directory_ask(&response),
                "{command} did not ask: {response:?}"
            );
        }
        assert!(f.aft.shutdown().success());
    }
}
