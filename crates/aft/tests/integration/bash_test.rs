#[cfg(unix)]
use super::helpers::{link_cached_executable, warm_executable};
use super::helpers::{user_config, AftProcess};

#[cfg(unix)]
fn shell_quote_path(path: &std::path::Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

#[cfg(unix)]
fn write_executable_shim(path: &std::path::Path, body: &str) {
    link_cached_executable(path, body.as_bytes()).unwrap();
    warm_executable(path, &["--version"]);
}

#[cfg(unix)]
fn aft_binary() -> std::path::PathBuf {
    std::env::var_os("AFT_TEST_AFT_BINARY")
        .or_else(|| std::env::var_os("NEXTEST_BIN_EXE_aft"))
        .or_else(|| std::env::var_os("CARGO_BIN_EXE_aft"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_BIN_EXE_aft")))
}

#[cfg(unix)]
struct TestPty {
    master: std::fs::File,
    slave: std::fs::File,
}

#[cfg(unix)]
impl TestPty {
    fn open() -> std::io::Result<Self> {
        use std::os::fd::FromRawFd;

        let mut master = -1;
        let mut slave = -1;
        if unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        } == -1
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self {
            master: unsafe { std::fs::File::from_raw_fd(master) },
            slave: unsafe { std::fs::File::from_raw_fd(slave) },
        })
    }
}

/// Allow slow shell startup when polling for completion on a loaded CI runner;
/// waits that expect nothing to happen stay short.
#[cfg(unix)]
const POSITIVE_COMPLETION_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

#[cfg(unix)]
fn wait_for_terminal_status(aft: &mut AftProcess, task_id: &str) -> serde_json::Value {
    let started = std::time::Instant::now();
    loop {
        let status = aft.send(
            &serde_json::json!({
                "id": format!("status-{task_id}"),
                "method": "bash_status",
                "params": { "task_id": task_id }
            })
            .to_string(),
        );
        if matches!(
            status["status"].as_str(),
            Some("completed" | "failed" | "killed" | "timed_out")
        ) {
            return status;
        }
        assert!(
            started.elapsed() < POSITIVE_COMPLETION_WAIT,
            "timed out waiting for terminal bash status: {status:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[cfg(unix)]
#[test]
fn bash_inherits_login_shell_enriched_path() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let custom_bin = home.join(".custom/bin");
    let cargo_bin = home.join(".cargo/bin");
    let local_bin = home.join(".local/bin");
    let fake_shell = dir.path().join("bin/zsh");
    std::fs::create_dir_all(&custom_bin).unwrap();
    std::fs::create_dir_all(&cargo_bin).unwrap();
    std::fs::create_dir_all(&local_bin).unwrap();
    std::fs::create_dir_all(fake_shell.parent().unwrap()).unwrap();

    let tool_name = format!("aft-path-probe-{}", std::process::id());
    let tool_path = custom_bin.join(&tool_name);
    write_executable_shim(&tool_path, "#!/bin/sh\nexit 0\n");
    std::fs::write(
        home.join(".zshrc"),
        format!(
            "printf 'banner before\\n'; export PATH=\"$PATH:{}\"; printf 'banner after\\n'\\n",
            custom_bin.display()
        ),
    )
    .unwrap();

    write_executable_shim(
        &fake_shell,
        r#"#!/bin/sh
if [ "$1" != '-lic' ]; then
  exit 64
fi
if [ -f "$ZDOTDIR/.zshrc" ]; then
  . "$ZDOTDIR/.zshrc"
fi
eval "$2"
"#,
    );

    let daemon_path = format!(
        "/opt/homebrew/bin:/usr/local/bin:{}:{}:/usr/bin:/bin",
        cargo_bin.display(),
        local_bin.display()
    );
    let mut aft = AftProcess::spawn_with_env(&[
        // The harness defaults AFT_TEST_RAW_PATH=1 (PATH isolation for
        // formatter/checker tests); this test IS the PATH feature, so opt back
        // in to the real probe+enrichment pipeline.
        ("AFT_TEST_RAW_PATH", std::ffi::OsStr::new("0")),
        ("PATH", std::ffi::OsStr::new(&daemon_path)),
        ("HOME", home.as_os_str()),
        ("ZDOTDIR", home.as_os_str()),
        ("SHELL", fake_shell.as_os_str()),
    ]);

    let response = aft.send(
        &serde_json::json!({
            "id": "bash-login-path",
            "method": "bash",
            "params": { "command": format!("command -v {tool_name}") }
        })
        .to_string(),
    );
    assert_eq!(response["success"], true, "bash spawn failed: {response:?}");
    let task_id = response["task_id"].as_str().unwrap();
    let status = wait_for_terminal_status(&mut aft, task_id);

    assert_eq!(status["status"], "completed", "bash failed: {status:?}");
    assert_eq!(status["exit_code"], 0, "bash failed: {status:?}");
    assert_eq!(
        status["output_preview"].as_str().unwrap().trim(),
        tool_path.to_string_lossy(),
        "bash child did not inherit the rc-enriched PATH: {status:?}"
    );

    assert!(aft.shutdown().success());
}

#[cfg(unix)]
#[test]
fn tool_spawned_shells_carry_no_sdk_default_consumer_identity_material() {
    let fixture = tempfile::tempdir().unwrap();
    let project = fixture.path().join("project");
    let storage = fixture.path().join("storage");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&storage).unwrap();

    let mut aft = AftProcess::spawn_with_env(&[
        ("SUBC_MODULE_ID", std::ffi::OsStr::new("aft")),
        (
            "SUBC_LAUNCH_NONCE",
            std::ffi::OsStr::new("synthetic-module-launch-nonce"),
        ),
        (
            "SUBC_FUTURE_CREDENTIAL",
            std::ffi::OsStr::new("synthetic-future-secret"),
        ),
    ]);
    let configure = aft.send(
        &serde_json::json!({
            "id": "configure-subc-child-scrub",
            "command": "configure",
            "harness": "opencode",
            "project_root": project,
            "storage_dir": storage,
            "config": user_config(serde_json::json!({
                "bash": { "background": true },
                "sandbox": { "enabled": false }
            })),
        })
        .to_string(),
    );
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:?}"
    );

    for (id, background, pty) in [
        ("subc-scrub-foreground", false, false),
        ("subc-scrub-background", true, false),
        ("subc-scrub-pty", true, true),
    ] {
        let leaked_environment = project.join(format!("{id}.env"));
        let command = format!(
            "/usr/bin/env | /usr/bin/grep '^SUBC_' > {}",
            shell_quote_path(&leaked_environment)
        );
        let response = aft.send(
            &serde_json::json!({
                "id": id,
                "method": "bash",
                "params": {
                    "command": command,
                    "background": background,
                    "pty": pty,
                    "compressed": false,
                }
            })
            .to_string(),
        );
        assert_eq!(response["success"], true, "{id} spawn failed: {response:?}");
        let terminal = wait_for_terminal_status(&mut aft, response["task_id"].as_str().unwrap());
        assert_eq!(
            terminal["status"], "failed",
            "{id} found subc credentials instead of grep's no-match result: {terminal:?}"
        );
        assert_eq!(
            terminal["exit_code"], 1,
            "{id} did not produce grep's no-match exit: {terminal:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&leaked_environment).unwrap(),
            "",
            "{id} exposed the module's supervised-spawn credential"
        );
    }

    assert!(aft.shutdown().success());
}

#[cfg(unix)]
#[test]
fn bash_child_path_and_git_hook_environment_follow_the_resolved_gates() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    let storage = dir.path().join("storage");
    let upstream = dir.path().join("upstream");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&upstream).unwrap();
    let real_gh = upstream.join("gh");
    write_executable_shim(&real_gh, "#!/bin/sh\nprintf 'real-gh:%s\\n' \"$*\"\n");
    let requested_path = std::env::join_paths([
        upstream.clone(),
        std::path::PathBuf::from("/usr/bin"),
        std::path::PathBuf::from("/bin"),
    ])
    .unwrap()
    .to_string_lossy()
    .into_owned();
    let binary = aft_binary();
    let mut aft = AftProcess::spawn_with_env(&[("PATH", std::ffi::OsStr::new(&requested_path))]);

    let configure = aft.send(
        &serde_json::json!({
            "id": "configure-child-governance",
            "command": "configure",
            "harness": "opencode",
            "project_root": project,
            "storage_dir": storage,
            "config": user_config(serde_json::json!({
                "github": { "shim": true },
                "gh_shim": { "binary_path": binary },
                "git": { "co_author": "off" }
            })),
        })
        .to_string(),
    );
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:?}"
    );
    let response = aft.send(
        &serde_json::json!({
            "id": "bash-child-governance",
            "method": "bash",
            "params": {
                "command": "printf '%s\\n' \"${PATH%%:*}\"; command -v gh; gh version; test -z \"${GIT_CONFIG_COUNT+x}\""
            }
        })
        .to_string(),
    );
    assert_eq!(response["success"], true, "bash spawn failed: {response:?}");
    let status = wait_for_terminal_status(&mut aft, response["task_id"].as_str().unwrap());
    let shims = storage.join("shims");
    assert_eq!(status["status"], "completed", "bash failed: {status:?}");
    assert_eq!(
        status["output_preview"].as_str().unwrap(),
        format!(
            "{}\n{}\nreal-gh:version\n",
            shims.display(),
            shims.join("gh").display()
        )
    );

    let configure_disabled = aft.send(
        &serde_json::json!({
            "id": "configure-child-governance-off",
            "command": "configure",
            "harness": "opencode",
            "project_root": project,
            "storage_dir": storage,
            "config": user_config(serde_json::json!({
                "github": { "shim": false },
                "git": { "co_author": "off" }
            })),
        })
        .to_string(),
    );
    assert_eq!(configure_disabled["success"], true);
    let response = aft.send(
        &serde_json::json!({
            "id": "bash-child-governance-off",
            "method": "bash",
            "params": {
                "command": "printf '%s\\n' \"$PATH\"; command -v gh; test -z \"${GIT_CONFIG_COUNT+x}\"; test -z \"${AFT_GH_SHIMS_DIR+x}\""
            }
        })
        .to_string(),
    );
    assert_eq!(response["success"], true, "bash spawn failed: {response:?}");
    let status = wait_for_terminal_status(&mut aft, response["task_id"].as_str().unwrap());
    assert_eq!(status["status"], "completed", "bash failed: {status:?}");
    assert_eq!(
        status["output_preview"].as_str().unwrap(),
        format!("{requested_path}\n{}\n", real_gh.display())
    );
    assert!(!shims.join("gh").exists());
    assert!(aft.shutdown().success());
}

/// A head shell under AFT's own (non-worker) sandbox is the operator's agent:
/// with no ticket it keeps gh passthrough, even though the sandbox sets the
/// same no_new_privs or Seatbelt state as a worker jail and the head's profile
/// build creates the worker canary in its private storage.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn sandboxed_head_shell_without_a_ticket_keeps_gh_passthrough() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let project = base.join("project");
    let storage = base.join("storage");
    // Inside the project: a strict head shell can read and run it there.
    let upstream = project.join("upstream");
    std::fs::create_dir_all(&upstream).unwrap();
    let real_gh = upstream.join("gh");
    write_executable_shim(&real_gh, "#!/bin/sh\nprintf 'real-gh:%s\\n' \"$*\"\n");
    let requested_path = std::env::join_paths([
        upstream.clone(),
        std::path::PathBuf::from("/usr/bin"),
        std::path::PathBuf::from("/bin"),
    ])
    .unwrap()
    .to_string_lossy()
    .into_owned();
    let binary = aft_binary();
    let mut aft = AftProcess::spawn_with_env(&[("PATH", std::ffi::OsStr::new(&requested_path))]);
    let configure = aft.send(
        &serde_json::json!({
            "id": "configure-sandboxed-head",
            "command": "configure",
            "harness": "opencode",
            "project_root": project,
            "storage_dir": storage,
            "config": user_config(serde_json::json!({
                "github": { "shim": true },
                "gh_shim": { "binary_path": binary },
                "git": { "co_author": "off" },
                "sandbox": { "profile": { "head": "strict" } }
            })),
        })
        .to_string(),
    );
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:?}"
    );
    let response = aft.send(
        &serde_json::json!({
            "id": "sandboxed-head-gh",
            "method": "bash",
            "params": {
                "command": "test -f \"$HOME/.local/share/cortexkit/aft/gh-shim-worker-jail/canary\" && echo CANARY_PRESENT; env -u AFT_GH_SHIM_TICKET AFT_GH_SHIM_CANARY_DIAGNOSTICS=1 gh version; echo EXIT=$?"
            }
        })
        .to_string(),
    );
    assert_eq!(response["success"], true, "bash spawn failed: {response:?}");
    let status = wait_for_terminal_status(&mut aft, response["task_id"].as_str().unwrap());
    // The shim prints its canary probe (resolved path, stat errno, and on macOS
    // the raw sandbox_check return and errno) to stderr, which lands in the
    // output preview shown by these assertions.
    let output = status["output_preview"].as_str().unwrap_or_default();
    assert!(output.contains("CANARY_PRESENT"), "{status:?}");
    assert!(!output.contains("gh_shim_worker_native_gh"), "{status:?}");
    // The shim chose passthrough. On Linux the strict head profile cannot
    // execute a file inside the project (exec fails with EACCES), so there the
    // passthrough shows up as the shim's exec error instead of the fake gh.
    assert!(
        output.contains("real-gh:version\nEXIT=0")
            || output.contains("gh_shim_no_real_gh: unable to exec upstream gh"),
        "{status:?}"
    );
    assert!(aft.shutdown().success());
}

#[cfg(unix)]
fn process_exists(pid: i32) -> bool {
    let output = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    if !output.status.success() {
        return false;
    }
    !String::from_utf8_lossy(&output.stdout).contains('Z')
}

#[cfg(unix)]
fn wait_until_process_exits(pid: i32) -> bool {
    let started = std::time::Instant::now();
    while started.elapsed() < std::time::Duration::from_secs(2) {
        if !process_exists(pid) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    false
}

#[test]
fn bash_streams_progress_and_returns_final_response() {
    let mut aft = AftProcess::spawn();
    let frames = aft.send_until(
        r#"{"id":"bash-1","method":"bash","handles_progress":true,"params":{"command":"echo hello; sleep 1","foreground_orchestrate":true,"wait":true}}"#,
        |frame| frame["id"] == "bash-1",
    );
    let response = frames.last().unwrap();
    assert_eq!(response["success"], true);
    assert!(response["output"]
        .as_str()
        .unwrap()
        .replace("\r\n", "\n")
        .starts_with("hello\n"));
    assert_eq!(response["exit_code"], 0);
    assert!(
        frames[..frames.len() - 1].iter().any(|frame| {
            frame["type"] == "progress"
                && frame["request_id"] == "bash-1"
                && frame["kind"] == "stdout"
                && frame["chunk"]
                    .as_str()
                    .is_some_and(|chunk| chunk.replace("\r\n", "\n") == "hello\n")
        }),
        "missing stdout progress before terminal reply: {frames:?}"
    );
    assert!(aft.shutdown().success());
}

#[test]
fn bash_progress_requires_transport_opt_in() {
    let mut aft = AftProcess::spawn();
    let frames = aft.send_until(
        r#"{"id":"bash-no-progress","method":"bash","params":{"command":"echo hello; sleep 1","foreground_orchestrate":true,"wait":true}}"#,
        |frame| frame["id"] == "bash-no-progress",
    );
    assert_eq!(frames.last().unwrap()["success"], true);
    assert!(
        frames.iter().all(|frame| frame["type"] != "progress"),
        "unexpected progress: {frames:?}"
    );
    assert!(aft.shutdown().success());
}

#[test]
fn bash_progress_opt_in_is_cleared_after_reply_before_id_reuse() {
    let mut aft = AftProcess::spawn();
    let frames = aft.send_until(
        r#"{"id":"bash-reused","method":"bash","handles_progress":true,"params":{"command":"echo first; sleep 1","foreground_orchestrate":true,"wait":true}}"#,
        |frame| frame["id"] == "bash-reused",
    );
    assert_eq!(frames.last().unwrap()["success"], true);
    assert!(
        frames[..frames.len() - 1]
            .iter()
            .any(|frame| frame["type"] == "progress" && frame["request_id"] == "bash-reused"),
        "missing progress for the opted-in call: {frames:?}"
    );

    let frames = aft.send_until(
        r#"{"id":"bash-reused","method":"bash","params":{"command":"echo second; sleep 1","foreground_orchestrate":true,"wait":true}}"#,
        |frame| frame["id"] == "bash-reused",
    );
    assert_eq!(frames.last().unwrap()["success"], true);
    assert!(
        frames.iter().all(|frame| frame["type"] != "progress"),
        "a completed call must not opt a reused ID into progress: {frames:?}"
    );
    assert!(aft.shutdown().success());
}

#[test]
fn bash_progress_masks_environment_secrets() {
    let mut aft = AftProcess::spawn();
    let secret = "progress-secret-1052-very-sensitive-value";
    let frames = aft.send_until(&serde_json::json!({
        "id": "bash-mask-progress", "method": "bash", "handles_progress": true,
        "params": { "command": format!("echo {secret}; sleep 1"), "env": {"PROGRESS_SECRET_TOKEN": secret}, "foreground_orchestrate": true, "wait": true }
    }).to_string(), |frame| frame["id"] == "bash-mask-progress");
    let progress = frames
        .iter()
        .filter(|frame| frame["type"] == "progress")
        .collect::<Vec<_>>();
    assert!(!progress.is_empty(), "no progress: {frames:?}");
    for frame in progress {
        assert!(
            !frame["chunk"].as_str().unwrap().contains(secret),
            "secret in progress: {frame:?}"
        );
        assert!(
            frame["chunk"].as_str().unwrap().contains("REDACTED"),
            "expected redaction: {frame:?}"
        );
    }
    assert!(aft.shutdown().success());
}

#[test]
fn bash_rejects_blocked_env_vars() {
    let mut aft = AftProcess::spawn();

    let response = aft.send(
        &serde_json::json!({
            "id": "bash-blocked-env",
            "method": "bash",
            "params": {
                "command": "echo should-not-run",
                "env": { "LD_PRELOAD": "foo" }
            }
        })
        .to_string(),
    );

    assert_eq!(response["success"], false, "response: {response:?}");
    assert_eq!(response["code"], "blocked_env_var");
    assert!(response["message"].as_str().unwrap().contains("LD_PRELOAD"));

    let hijack_response = aft.send(
        &serde_json::json!({
            "id": "bash-preexec-env",
            "method": "bash",
            "params": {
                "command": "echo should-not-run",
                "env": { "NODE_OPTIONS": "--require injected" }
            }
        })
        .to_string(),
    );
    assert_eq!(
        hijack_response["success"], false,
        "response: {hijack_response:?}"
    );
    assert_eq!(hijack_response["code"], "blocked_env_var");
    assert!(hijack_response["message"]
        .as_str()
        .unwrap()
        .contains("NODE_OPTIONS"));

    assert!(aft.shutdown().success());
}

#[test]
fn bash_rejects_invalid_pty_dimensions() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let configure = aft.send(
        &serde_json::json!({
            "id": "cfg-bg",
            "command": "configure",
            "harness": "opencode",
            "project_root": dir.path(),
            "storage_dir": dir.path().join("storage"),
            "config": user_config(serde_json::json!({
                "experimental": { "bash": { "background": true } }
            })),
        })
        .to_string(),
    );
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:?}"
    );

    let cases = [
        (
            "pty-rows-too-large",
            serde_json::json!({
                "command": "echo nope",
                "background": true,
                "pty": true,
                "pty_rows": 61,
            }),
            "ptyRows must be an integer between 1 and 60",
        ),
        (
            "pty-cols-too-large",
            serde_json::json!({
                "command": "echo nope",
                "background": true,
                "pty": true,
                "pty_cols": 141,
            }),
            "ptyCols must be an integer between 1 and 140",
        ),
        (
            "pty-rows-float",
            serde_json::json!({
                "command": "echo nope",
                "background": true,
                "pty": true,
                "pty_rows": 1.5,
            }),
            "invalid params",
        ),
    ];

    for (id, params, message) in cases {
        let response = aft.send(
            &serde_json::json!({
                "id": id,
                "method": "bash",
                "params": params
            })
            .to_string(),
        );
        assert_eq!(response["success"], false, "case {id}: {response:?}");
        assert_eq!(
            response["code"], "invalid_request",
            "case {id}: {response:?}"
        );
        assert!(
            response["message"].as_str().unwrap().contains(message),
            "case {id}: expected message containing {message:?}, got {response:?}"
        );
    }

    assert!(aft.shutdown().success());
}

#[cfg(unix)]
#[test]
fn bash_piped_runner_exit_status_is_not_hidden() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let bin_dir = dir.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    write_executable_shim(
        &bin_dir.join("cargo"),
        "#!/bin/sh\nprintf 'fake cargo line\\n'\nexit 0\n",
    );
    write_executable_shim(
        &bin_dir.join("pytest"),
        "#!/bin/sh\nprintf 'fake pytest line\\n'\nexit 0\n",
    );
    let path_prefix = shell_quote_path(&bin_dir);
    let cases = [
        (
            "grep-v-empty",
            format!("PATH={path_prefix}:$PATH cargo test | grep -v '^'"),
        ),
        (
            "awk-end-exit",
            format!("PATH={path_prefix}:$PATH cargo test | awk 'END{{exit 1}}'"),
        ),
        (
            "pytest-grep-sentinel",
            format!("PATH={path_prefix}:$PATH pytest -q | grep SENTINEL || exit 1"),
        ),
    ];

    for (id, command) in cases {
        let response = aft.send(
            &serde_json::json!({
                "id": id,
                "method": "bash",
                "params": { "command": command }
            })
            .to_string(),
        );
        assert_eq!(
            response["success"], true,
            "spawn failed for {id}: {response:?}"
        );
        assert!(
            matches!(
                response["status"].as_str(),
                Some("running" | "completed" | "failed" | "killed" | "timed_out" | "fate_unknown")
            ),
            "unexpected spawn status for {id}: {response:?}"
        );
        let task_id = response["task_id"].as_str().unwrap();
        let status = wait_for_terminal_status(&mut aft, task_id);
        assert_eq!(
            status["status"], "failed",
            "{id} should preserve pipeline failure: {status:?}"
        );
        assert_eq!(
            status["exit_code"], 1,
            "{id} should report the shell pipeline exit code: {status:?}"
        );
    }

    assert!(aft.shutdown().success());
}

#[cfg(target_os = "linux")]
#[test]
fn non_pty_foreground_and_deferred_bash_children_get_isolated_sessions() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let mut aft = AftProcess::spawn();
    let configure = aft.send(
        &serde_json::json!({
            "id": "cfg-non-pty-session",
            "command": "configure",
            "harness": "opencode",
            "project_root": project.path(),
            "storage_dir": storage.path(),
            "config": user_config(serde_json::json!({
                "experimental": { "bash": { "background": true } }
            })),
        })
        .to_string(),
    );
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:?}"
    );

    let bridge_session = unsafe { libc::getsid(aft.pid() as libc::pid_t) };
    assert_ne!(bridge_session, -1, "could not read bridge session id");
    for (id, background) in [
        ("foreground-non-pty-session", false),
        ("deferred-non-pty-session", true),
    ] {
        let response = aft.send(
            &serde_json::json!({
                "id": id,
                "method": "bash",
                "params": {
                    "command": "ps -o sid= -o pgid= -p $$",
                    "background": background,
                    "pty": false,
                },
            })
            .to_string(),
        );
        assert_eq!(response["success"], true, "bash failed: {response:?}");
        let status = wait_for_terminal_status(&mut aft, response["task_id"].as_str().unwrap());
        assert_eq!(status["status"], "completed", "bash failed: {status:?}");
        let ids = status["output_preview"]
            .as_str()
            .unwrap()
            .split_whitespace()
            .map(|id| id.parse::<libc::pid_t>().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(ids.len(), 2, "unexpected process ids: {status:?}");
        assert_ne!(
            ids[0], bridge_session,
            "{id} inherited the bridge session instead of starting an isolated session"
        );
        assert_eq!(
            ids[0], ids[1],
            "{id} child session and process group must share their leader"
        );
    }

    assert!(aft.shutdown().success());
}

#[cfg(unix)]
#[test]
fn foreground_interactive_zsh_cannot_take_over_the_bridge_terminal() {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    if which::which("zsh").is_err() {
        eprintln!("skipping interactive zsh terminal test: zsh is unavailable");
        return;
    }
    let terminal = TestPty::open().expect("create terminal for Pi-shape probe");
    let master_fd = terminal.master.as_raw_fd();
    let slave_fd = terminal.slave.as_raw_fd();
    let mut probe = Command::new(std::env::current_exe().expect("current integration test binary"));
    probe
        .args([
            "--exact",
            "bash_test::interactive_zsh_terminal_probe_child",
            "--nocapture",
        ])
        .env("AFT_INTERACTIVE_ZSH_TERMINAL_PROBE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    unsafe {
        probe.pre_exec(move || {
            if libc::setsid() == -1
                || libc::ioctl(slave_fd, libc::TIOCSCTTY as libc::c_ulong, 0) == -1
                || libc::close(master_fd) == -1
                || libc::close(slave_fd) == -1
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let output = probe.output().expect("run Pi-shape terminal probe");
    assert!(
        output.status.success(),
        "interactive zsh terminal probe failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// Runs in a subprocess because it owns the pseudo-terminal used to simulate
/// Pi's interactive terminal; the parent test process cannot inspect that terminal.
#[cfg(unix)]
#[test]
fn interactive_zsh_terminal_probe_child() {
    use std::os::fd::AsRawFd;

    if std::env::var_os("AFT_INTERACTIVE_ZSH_TERMINAL_PROBE").is_none() {
        return;
    }
    let zsh = which::which("zsh").expect("parent skips this probe when zsh is unavailable");
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let mut aft = AftProcess::spawn();
    let configure = aft.send(
        &serde_json::json!({
            "id": "cfg-interactive-zsh-terminal",
            "command": "configure",
            "harness": "opencode",
            "project_root": project.path(),
            "storage_dir": storage.path(),
        })
        .to_string(),
    );
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:?}"
    );

    let response = aft.send(
        &serde_json::json!({
            "id": "interactive-zsh-without-pty",
            "method": "bash",
            "params": {
                "command": format!("{} -fic 'exit'", shell_quote_path(&zsh)),
                "background": false,
                "pty": false,
                "compressed": false,
            },
        })
        .to_string(),
    );
    assert_eq!(response["success"], true, "bash failed: {response:?}");
    let status = wait_for_terminal_status(&mut aft, response["task_id"].as_str().unwrap());
    assert_eq!(
        status["status"], "completed",
        "interactive zsh failed: {status:?}"
    );
    assert_eq!(status["exit_code"], 0, "interactive zsh failed: {status:?}");

    let tty = std::fs::File::open("/dev/tty").expect("open simulated Pi terminal");
    let foreground_group = unsafe { libc::tcgetpgrp(tty.as_raw_fd()) };
    assert_ne!(
        foreground_group, -1,
        "read terminal foreground process group"
    );
    assert_eq!(
        foreground_group,
        unsafe { libc::getpgrp() },
        "interactive zsh changed the simulated Pi terminal foreground process group"
    );

    assert!(aft.shutdown().success());
}

#[cfg(unix)]
#[test]
fn bash_timeout_terminates_shell_process_group_grandchild() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("sleep.pid");
    let command = format!("sleep 30 & echo $! > {}; wait", pid_file.display());

    let response = aft.send(
        &serde_json::json!({
            "id": "bash-timeout-pgroup",
            "method": "bash",
            "params": { "command": command, "timeout": 200 }
        })
        .to_string(),
    );

    assert_eq!(response["success"], true, "bash failed: {response:?}");
    assert!(
        matches!(
            response["status"].as_str(),
            Some("running" | "completed" | "failed" | "killed" | "timed_out" | "fate_unknown")
        ),
        "unexpected spawn status: {response:?}"
    );
    let task_id = response["task_id"].as_str().unwrap();
    let started = std::time::Instant::now();
    loop {
        let status = aft.send(
            &serde_json::json!({
                "id": "bash-timeout-pgroup-status",
                "method": "bash_status",
                "params": { "task_id": task_id }
            })
            .to_string(),
        );
        if status["status"] == "timed_out" {
            break;
        }
        assert!(
            started.elapsed() < POSITIVE_COMPLETION_WAIT,
            "task did not time out within {POSITIVE_COMPLETION_WAIT:?}; last status: {status:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        wait_until_process_exits(pid),
        "grandchild sleep process {pid} survived foreground timeout"
    );

    assert!(aft.shutdown().success());
}

#[cfg(unix)]
#[test]
fn bash_changed_files_foreground_and_background_completion_notice() {
    // Verify actual diffs with an ample test budget, not a Git startup benchmark.
    assert_changed_files_completion_with_git_budget("30000", false, false);
}

#[cfg(all(unix, debug_assertions))]
#[test]
fn bash_changed_files_git_budget_exhaustion_reports_paths_without_diffs() {
    assert_changed_files_completion_with_git_budget("0", true, false);
}

#[cfg(unix)]
#[test]
fn bash_memory_cap_stop_keeps_changed_files_completion_fence() {
    assert_changed_files_completion_with_git_budget("30000", false, true);
}

#[cfg(unix)]
fn assert_changed_files_completion_with_git_budget(
    budget_ms: &str,
    exhausted: bool,
    memory_stop: bool,
) {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.email", "test@example.invalid"],
        vec!["config", "user.name", "Test"],
    ] {
        assert!(std::process::Command::new("git")
            .arg("-C")
            .arg(project.path())
            .args(args)
            .status()
            .unwrap()
            .success());
    }
    std::fs::write(project.path().join("file.txt"), "old\n").unwrap();
    std::fs::write(project.path().join(".gitignore"), "config.local.json\n").unwrap();
    for args in [vec!["add", "."], vec!["commit", "-qm", "initial"]] {
        assert!(std::process::Command::new("git")
            .arg("-C")
            .arg(project.path())
            .args(args)
            .status()
            .unwrap()
            .success());
    }
    let mut aft = AftProcess::spawn_with_env(&[
        (
            "AFT_TEST_BASH_CHANGED_FILES_GIT_BUDGET_MS",
            std::ffi::OsStr::new(budget_ms),
        ),
        ("AFT_TEST_DISABLE_FILE_WATCHER", std::ffi::OsStr::new("0")),
        (
            "AFT_TEST_SYNC_FILE_WATCHER_START",
            std::ffi::OsStr::new("1"),
        ),
    ]);
    let mut bash_config = serde_json::json!({"changed_files": "diff", "rewrite": false});
    if memory_stop {
        bash_config["memory_limit_mib"] = serde_json::json!(64);
    }
    let configured = aft.send(&serde_json::json!({
        "id": "changed-files-config", "command": "configure", "harness": "opencode",
        "project_root": project.path(), "storage_dir": storage.path(),
        "config": user_config(serde_json::json!({"bash": bash_config, "sandbox": {"profile": {"head": "off"}}}))
    }).to_string());
    assert_eq!(configured["success"], true, "{configured}");
    // Establish native service readiness before testing command-exit delivery.
    // The separate saturation regression deliberately starts without warming up.
    let ready_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let probe = aft.send(&serde_json::json!({"id":"changed-files-ready", "method":"bash", "params":{"command":":", "notify_on_completion":false}}).to_string());
        assert_eq!(probe["success"], true, "{probe}");
        let probe = wait_for_terminal_status(&mut aft, probe["task_id"].as_str().unwrap());
        if probe["_bash_changed_files"]["incomplete"]
            .as_array()
            .is_some_and(Vec::is_empty)
        {
            break;
        }
        assert!(
            std::time::Instant::now() < ready_deadline,
            "watcher did not become ready: {probe}"
        );
    }
    for background in [false, true] {
        let name = if background {
            "background.txt"
        } else {
            "foreground.txt"
        };
        // Successful commands end with the write, without a watcher head start.
        // Capped commands must retain that change when the allocation is stopped.
        let mut command = format!("printf ignored > config.local.json; printf changed > file.txt; printf done; printf 'hello\\n' > {name}");
        if memory_stop {
            command.push_str("; python3 -c 'import time; allocation = bytearray(b\"x\" * (192 * 1024 * 1024)); time.sleep(20)'");
        }
        let mut params = serde_json::json!({
            "command": command, "background": background, "wait": !background,
            "compressed": false, "notify_on_completion": background
        });
        if memory_stop {
            params["timeout"] = serde_json::json!(10000);
        }
        let result = aft
            .send(&serde_json::json!({"id": name, "method": "bash", "params": params}).to_string());
        assert_eq!(result["success"], true, "{result}");
        if background {
            assert!(result.get("_bash_changed_files").is_none(), "{result}");
            wait_for_terminal_status(&mut aft, result["task_id"].as_str().unwrap());
            // Terminal status can be published before its completion notice is
            // queued, so wait for delivery rather than assuming both happen together.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let completion = loop {
                let completions = aft.send(
                    &serde_json::json!({"id":"drain-changes", "command":"bash_drain_completions"})
                        .to_string(),
                );
                if let Some(completion) = completions["bg_completions"]
                    .as_array()
                    .expect("completion array")
                    .iter()
                    .find(|item| item["task_id"] == result["task_id"])
                {
                    break completion.clone();
                }
                assert!(std::time::Instant::now() < deadline, "{completions}");
                std::thread::sleep(std::time::Duration::from_millis(10));
            };
            let files = completion["_bash_changed_files"]["files"]
                .as_array()
                .unwrap();
            if memory_stop {
                assert_eq!(completion["status"], "failed", "{completion}");
                assert!(
                    completion["status_reason"]
                        .as_str()
                        .unwrap_or_default()
                        .contains("64 MiB memory cap"),
                    "{completion}"
                );
            }
            assert!(
                files.iter().any(|file| file["path"] == "file.txt"),
                "{completion}"
            );
            assert!(
                !files.iter().any(|file| file["path"] == "config.local.json"),
                "ignored file leaked: {completion}"
            );
            assert!(
                completion["_bash_changed_files"]["text"]
                    .as_str()
                    .unwrap()
                    .contains(name),
                "{completion}"
            );
            assert_changed_file_image(&completion["_bash_changed_files"], name, exhausted);
            assert_eq!(
                completion["_bash_changed_files"]["incomplete"],
                serde_json::json!([]),
                "{completion}"
            );
            let ack = aft.send(&serde_json::json!({"id":"ack-changes", "command":"bash_ack_completions", "params":{"task_ids":[result["task_id"]]}}).to_string());
            assert_eq!(ack["success"], true);
            let drained = aft.send(&serde_json::json!({"id":"drain-changes-again", "command":"bash_drain_completions"}).to_string());
            assert!(
                drained["bg_completions"].as_array().unwrap().is_empty(),
                "{drained}"
            );
        } else {
            let result = wait_for_terminal_status(&mut aft, result["task_id"].as_str().unwrap());
            if memory_stop {
                assert_eq!(result["status"], "failed", "{result}");
                assert!(
                    result["status_reason"]
                        .as_str()
                        .unwrap_or_default()
                        .contains("64 MiB memory cap"),
                    "{result}"
                );
            }
            let files = result["_bash_changed_files"]["files"].as_array().unwrap();
            assert!(
                files.iter().any(|file| file["path"] == "file.txt"),
                "{result}"
            );
            assert!(
                !files.iter().any(|file| file["path"] == "config.local.json"),
                "ignored file leaked: {result}"
            );
            assert_eq!(
                result["_bash_changed_files"]["incomplete"],
                serde_json::json!([]),
                "{result}"
            );
            assert!(
                result["_bash_changed_files"]["text"]
                    .as_str()
                    .unwrap()
                    .contains("Files changed while the command ran"),
                "{result}"
            );
            assert_changed_file_image(&result["_bash_changed_files"], name, exhausted);
        }
    }
}

#[cfg(unix)]
fn assert_changed_file_image(report: &serde_json::Value, name: &str, exhausted: bool) {
    let files = report["files"].as_array().unwrap();
    if exhausted {
        assert!(report["text"].as_str().unwrap().contains(
            "No diffs: git before-images could not be captured within the time/output budget, so only paths are listed."
        ), "{report}");
        assert!(files.iter().any(|file| file["path"] == name), "{report}");
        assert!(
            files.iter().all(|file| file.get("diff").is_none()),
            "{report}"
        );
    } else {
        assert!(
            files.iter().any(|file| file["path"] == name
                && file["diff"]
                    .as_str()
                    .is_some_and(|diff| diff.contains("+hello"))),
            "{report}"
        );
    }
}

#[cfg(unix)]
#[test]
fn bash_changed_files_flush_timeout_reports_incomplete() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let mut aft = AftProcess::spawn_with_env(&[
        ("AFT_TEST_DISABLE_FILE_WATCHER", std::ffi::OsStr::new("0")),
        (
            "AFT_TEST_SYNC_FILE_WATCHER_START",
            std::ffi::OsStr::new("1"),
        ),
        (
            "AFT_TEST_BASH_CHANGED_FILES_FLUSH_TIMEOUT",
            std::ffi::OsStr::new("1"),
        ),
    ]);
    let configured = aft.send(&serde_json::json!({"id":"flush-timeout-config", "command":"configure", "harness":"opencode", "project_root":project.path(), "storage_dir":storage.path(), "config":user_config(serde_json::json!({"bash":{"changed_files":"list","rewrite":false},"sandbox":{"profile":{"head":"off"}}}))}).to_string());
    assert_eq!(configured["success"], true, "{configured}");
    let result = aft.send(&serde_json::json!({"id":"flush-timeout-command", "method":"bash", "params":{"command":"printf last > late.txt", "notify_on_completion":false}}).to_string());
    assert_eq!(result["success"], true, "{result}");
    let result = wait_for_terminal_status(&mut aft, result["task_id"].as_str().unwrap());
    assert_eq!(result["_bash_changed_files"]["incomplete"], serde_json::json!(["Incomplete: the file watcher had not caught up when the command ended, so some changes may be missing."]), "{result}");
}
