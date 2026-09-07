use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::helpers::{user_config, AftProcess, ReleaseOnDrop};

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
    // Process disappearance is the assertion; this deadline only catches a
    // wedged process-group kill without charging runner scheduling to it.
    let hang_deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < hang_deadline {
        if !process_exists(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn configure_background(aft: &mut AftProcess) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    // Keep background task storage per test. AftProcess::spawn() uses one
    // AFT_CACHE_DIR per test binary process, so parallel integration tests
    // would otherwise replay and mutate each other's live background tasks.
    let storage_dir = dir.path().join("aft-storage");
    std::fs::create_dir_all(&storage_dir).unwrap();
    let response = aft.send(
        &json!({
            "id": "cfg-bg",
            "command": "configure",
            "harness": "opencode",
            "project_root": dir.path(),
            "storage_dir": storage_dir,
            "config": user_config(serde_json::json!({
                "experimental": { "bash": { "background": true } }
            })),
        })
        .to_string(),
    );
    assert_eq!(response["success"], true, "configure failed: {response:?}");
    dir
}

fn configure_restricted_background(aft: &mut AftProcess) -> (tempfile::TempDir, tempfile::TempDir) {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let response = aft.send(
        &json!({
            "id": "cfg-restricted-bg",
            "command": "configure",
            "harness": "opencode",
            "project_root": project.path(),
            "storage_dir": storage.path(),
            "config": user_config(serde_json::json!({
                "restrict_to_project_root": true,
                "experimental": { "bash": { "background": true } }
            })),
        })
        .to_string(),
    );
    assert_eq!(response["success"], true, "configure failed: {response:?}");
    (project, storage)
}

fn spawn_bg(aft: &mut AftProcess, id: &str, command: &str) -> String {
    let response = aft.send(
        &json!({
            "id": id,
            "command": "bash",
            "params": { "command": command, "background": true }
        })
        .to_string(),
    );
    assert_eq!(response["success"], true, "spawn failed: {response:?}");
    assert_eq!(response["status"], "running");
    response["task_id"].as_str().unwrap().to_string()
}

fn spawn_bg_params(aft: &mut AftProcess, id: &str, params: Value) -> String {
    let response = aft.send(
        &json!({
            "id": id,
            "command": "bash",
            "params": params
        })
        .to_string(),
    );
    assert_eq!(response["success"], true, "spawn failed: {response:?}");
    assert_eq!(response["status"], "running");
    response["task_id"].as_str().unwrap().to_string()
}

fn status(aft: &mut AftProcess, task_id: &str) -> Value {
    aft.send(
        &json!({
            "id": format!("status-{task_id}"),
            "command": "bash_status",
            "params": { "task_id": task_id }
        })
        .to_string(),
    )
}

fn status_with_session(aft: &mut AftProcess, task_id: &str, session_id: &str) -> Value {
    aft.send(
        &json!({
            "id": format!("status-{session_id}-{task_id}"),
            "session_id": session_id,
            "command": "bash_status",
            "params": { "task_id": task_id }
        })
        .to_string(),
    )
}

fn is_terminal_status(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "killed" | "timed_out")
}

fn wait_for_status(aft: &mut AftProcess, task_id: &str, expected: &str) -> Value {
    let started = Instant::now();
    loop {
        let response = status(aft, task_id);
        assert_eq!(response["success"], true, "status failed: {response:?}");
        let observed = response["status"].as_str().unwrap_or_default();
        if observed == expected {
            return response;
        }
        if is_terminal_status(observed) {
            panic!(
                "got terminal status '{observed}', expected '{expected}'. last metadata: {response:?}"
            );
        }
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "timed out waiting for {expected}: {response:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for_bash_completed_frame(aft: &mut AftProcess, task_id: &str) -> Value {
    let started = Instant::now();
    loop {
        let frame = aft.read_next();
        if frame["type"] == "bash_completed" && frame["task_id"] == task_id {
            return frame;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "timed out waiting for bash_completed frame for {task_id}; last frame: {frame:?}"
        );
    }
}

fn quick_success_command() -> &'static str {
    if cfg!(windows) {
        "cmd /c exit /b 0"
    } else {
        "true"
    }
}

fn echo_text_command(text: &str) -> String {
    if cfg!(windows) {
        format!("cmd /c echo {text}")
    } else {
        format!("echo {text}")
    }
}

#[cfg(windows)]
fn cross_platform_echo_command() -> &'static str {
    "cmd /c echo hello"
}

#[cfg(not(windows))]
fn cross_platform_echo_command() -> &'static str {
    "echo hello"
}

#[cfg(unix)]
fn truncation_sized_output_command() -> &'static str {
    "i=1; while [ $i -le 2000 ]; do printf 'artifact-line-%04d\\n' \"$i\"; i=$((i+1)); done"
}

#[cfg(windows)]
fn truncation_sized_output_command() -> &'static str {
    "cmd /c \"for /L %i in (1,1,2000) do @echo artifact-line-%i\""
}

fn shell_quote_path(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

fn cmd_quote_path(path: &Path) -> String {
    format!("\"{}\"", path.display().to_string().replace('"', "\"\""))
}

fn cat_file_command(path: &Path) -> String {
    if cfg!(windows) {
        format!("cmd /c type {}", cmd_quote_path(path))
    } else {
        format!("cat {}", shell_quote_path(path))
    }
}

fn wait_for_file(path: &Path) {
    let started = Instant::now();
    while !path.exists() {
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn hold_until_release_command(marker: &Path, release: &Path) -> String {
    format!(
        "printf ready > {}; polls=0; while [ ! -f {} ] && [ \"$polls\" -lt 6000 ]; do sleep 0.05; polls=$((polls + 1)); done; if [ ! -f {} ]; then printf 'gate-timeout\\n'; fi",
        shell_quote_path(marker),
        shell_quote_path(release),
        shell_quote_path(release),
    )
}

fn cross_platform_hold_until_release_command(marker: &Path, release: &Path) -> String {
    if cfg!(windows) {
        format!(
            "Set-Content -NoNewline -Path {} -Value ready; $polls = 0; while ((-not (Test-Path {})) -and ($polls -lt 6000)) {{ Start-Sleep -Milliseconds 50; $polls++ }}; if (-not (Test-Path {})) {{ Write-Output 'gate-timeout' }}",
            cmd_quote_path(marker),
            cmd_quote_path(release),
            cmd_quote_path(release)
        )
    } else {
        hold_until_release_command(marker, release)
    }
}

#[cfg(unix)]
#[test]
fn pipeline_warning_reports_upstream_failure_in_status_and_completion_frame() {
    let mut aft = AftProcess::spawn();
    let _dir = configure_background(&mut aft);

    let task_id = spawn_bg(&mut aft, "pipeline-upstream-failure", "false | tail -1");
    let completed = wait_for_status(&mut aft, &task_id, "completed");
    let output = completed["output_preview"].as_str().unwrap_or_default();
    assert_eq!(completed["exit_code"], 0);
    let status_path =
        Path::new(completed["output_path"].as_str().unwrap()).with_file_name("pipeline-status");
    assert!(
        status_path.exists(),
        "expected pipeline status capture alongside stdout: {}",
        status_path.display()
    );
    assert_eq!(std::fs::read_to_string(&status_path).unwrap(), "1\n0\n");
    assert!(
        output.contains("note: `false` (segment 1 of 2) exited 1"),
        "expected upstream pipeline warning in status output: {completed:?}"
    );
    assert!(
        output.contains("the pipeline's exit code is `tail`'s."),
        "expected final-stage name in status output: {completed:?}"
    );

    let frame = wait_for_bash_completed_frame(&mut aft, &task_id);
    assert!(
        frame["output_preview"]
            .as_str()
            .unwrap_or_default()
            .contains("note: `false` (segment 1 of 2) exited 1"),
        "expected upstream pipeline warning in completion frame: {frame:?}"
    );
    assert!(aft.shutdown().success());
}

#[cfg(unix)]
#[test]
fn pipeline_warning_is_silent_for_healthy_final_failure_and_multi_statement_commands() {
    let mut aft = AftProcess::spawn();
    let _dir = configure_background(&mut aft);

    for (id, command, expected_exit) in [
        ("pipeline-healthy", "true | tail -1", 0),
        ("pipeline-final-failure", "true | false", 1),
        (
            "pipeline-multi-statement",
            "false | tail -1; true | tail -1",
            0,
        ),
    ] {
        let task_id = spawn_bg(&mut aft, id, command);
        let completed = wait_for_status(
            &mut aft,
            &task_id,
            if expected_exit == 0 {
                "completed"
            } else {
                "failed"
            },
        );
        assert_eq!(completed["exit_code"], expected_exit);
        if id == "pipeline-multi-statement" {
            let status_path = Path::new(completed["output_path"].as_str().unwrap())
                .with_file_name("pipeline-status");
            assert!(
                !status_path.exists(),
                "multi-statement command must not create a pipeline capture: {}",
                status_path.display()
            );
        }
        assert!(
            !completed["output_preview"]
                .as_str()
                .unwrap_or_default()
                .contains("the pipeline's exit code is"),
            "unexpected pipeline warning for {command:?}: {completed:?}"
        );
    }

    assert!(aft.shutdown().success());
}

#[test]
fn background_bash_spawns_and_completes_cross_platform() {
    let mut aft = AftProcess::spawn();
    let _dir = configure_background(&mut aft);

    let task_id = spawn_bg(
        &mut aft,
        "spawn-cross-platform",
        cross_platform_echo_command(),
    );
    let completed = wait_for_status(&mut aft, &task_id, "completed");

    assert_eq!(completed["exit_code"], 0);
    assert!(
        completed["output_preview"]
            .as_str()
            .unwrap_or_default()
            .contains("hello"),
        "expected output to contain hello: {completed:?}"
    );
    assert!(aft.shutdown().success());
}

#[test]
fn restricted_read_allows_only_current_session_bash_artifacts() {
    let mut aft = AftProcess::spawn();
    // The tempdir handle is only dereferenced by the unix-gated symlink block
    // below; keep the binding underscore-prefixed so Windows (-D warnings)
    // compiles while Drop still cleans the directory up.
    let (_dir, _storage) = configure_restricted_background(&mut aft);
    let owner_session = "artifact-owner-session";
    let spawn = aft.send(
        &json!({
            "id": "spawn-readable-artifact",
            "session_id": owner_session,
            "command": "bash",
            "params": {
                "command": truncation_sized_output_command(),
                "background": true,
                "compressed": true
            }
        })
        .to_string(),
    );
    assert_eq!(spawn["success"], true, "spawn failed: {spawn:?}");
    let task_id = spawn["task_id"].as_str().unwrap();

    let started = Instant::now();
    let completed = loop {
        let response = status_with_session(&mut aft, task_id, owner_session);
        assert_eq!(response["success"], true, "status failed: {response:?}");
        if response["status"] == "completed" {
            break response;
        }
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "timed out waiting for artifact task: {response:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    let output_path = Path::new(completed["output_path"].as_str().expect("stdout path"));
    let preview = completed["output_preview"].as_str().unwrap_or_default();
    assert!(completed["output_truncated"].as_bool().unwrap_or(false));
    assert!(
        preview.contains(&format!("read \"{}\"", output_path.display())),
        "truncation footer did not advertise the registered artifact: {preview:?}"
    );

    let write = aft.send(
        &json!({
            "id": "write-readable-artifact",
            "session_id": owner_session,
            "command": "write",
            "file": output_path,
            "content": "must stay blocked\n"
        })
        .to_string(),
    );
    assert_eq!(
        write["success"], false,
        "write escaped restriction: {write:?}"
    );
    assert_eq!(
        write["code"], "path_outside_root",
        "wrong write error: {write:?}"
    );

    let edit = aft.send(
        &json!({
            "id": "edit-readable-artifact",
            "session_id": owner_session,
            "command": "edit_match",
            "file": output_path,
            "match": "artifact-line-0001",
            "replacement": "blocked"
        })
        .to_string(),
    );
    assert_eq!(edit["success"], false, "edit escaped restriction: {edit:?}");
    assert_eq!(
        edit["code"], "path_outside_root",
        "wrong edit error: {edit:?}"
    );

    let cross_session = aft.send(
        &json!({
            "id": "cross-session-artifact-read",
            "session_id": "different-session",
            "command": "read",
            "file": output_path
        })
        .to_string(),
    );
    assert_eq!(
        cross_session["success"], false,
        "cross-session read leaked: {cross_session:?}"
    );
    assert_eq!(cross_session["code"], "path_outside_root");

    #[cfg(unix)]
    {
        let artifact_dir = output_path.parent().unwrap();
        let link = _dir.path().join("artifact-link");
        std::os::unix::fs::symlink(artifact_dir, &link).unwrap();
        let unregistered = artifact_dir.join("unregistered-output");
        std::fs::write(&unregistered, "not registered\n").unwrap();
        for target in [link.clone(), link.join("unregistered-output")] {
            let response = aft.send(
                &json!({
                    "id": "symlink-artifact-prefix-read",
                    "session_id": owner_session,
                    "command": "read",
                    "file": target
                })
                .to_string(),
            );
            assert_eq!(
                response["success"], false,
                "symlink widened artifact access: {response:?}"
            );
            assert_eq!(response["code"], "path_outside_root");
        }
    }

    let read = aft.send(
        &json!({
            "id": "read-owned-artifact",
            "session_id": owner_session,
            "command": "read",
            "file": output_path
        })
        .to_string(),
    );
    assert_eq!(
        read["success"], true,
        "owned artifact read failed: {read:?}"
    );
    assert_eq!(
        read["complete"], true,
        "artifact read was not complete: {read:?}"
    );
    let content = read["content"].as_str().unwrap_or_default();
    // The unix fixture zero-pads (%04d); cmd's for /L loop cannot, so the
    // first line differs per platform while the last line matches both.
    #[cfg(unix)]
    assert!(content.contains("artifact-line-0001"));
    #[cfg(windows)]
    assert!(content.contains("artifact-line-1\r\n") || content.contains("artifact-line-1\n"));
    assert!(content.contains("artifact-line-2000"));

    assert!(aft.shutdown().success());
}

#[cfg(unix)]
#[test]
fn pty_spawn_honors_requested_terminal_dimensions() {
    let mut aft = AftProcess::spawn();
    let _dir = configure_background(&mut aft);

    let task_id = spawn_bg_params(
        &mut aft,
        "pty-custom-dimensions",
        json!({
            "command": "stty size; exit",
            "background": true,
            "pty": true,
            "pty_rows": 50,
            "pty_cols": 120,
        }),
    );

    let completed = wait_for_status(&mut aft, &task_id, "completed");
    assert_eq!(completed["mode"], "pty");
    assert_eq!(completed["pty_rows"], 50);
    assert_eq!(completed["pty_cols"], 120);
    let output_path = completed["output_path"].as_str().unwrap();
    let output = std::fs::read_to_string(output_path).unwrap();
    assert!(
        output.contains("50 120"),
        "expected PTY output to report 50x120, got {output:?}"
    );

    assert!(aft.shutdown().success());
}

// POSIX-only harness: hold_until_release_command uses a `sh` while-loop +
// `printf`, which are not Windows PowerShell commands. Skip on Windows — the
// running->completed status transition is covered on Unix here, and the real
// Windows background spawn/status path is exercised by the Windows native E2E
// job (real OpenCode + bridge + hoisted bash through PowerShell).
#[cfg_attr(
    windows,
    ignore = "POSIX-only harness (`sh` loop); Unix + Windows native E2E cover this"
)]
#[test]
fn background_spawn_status_running_and_completion() {
    let mut aft = AftProcess::spawn();
    let dir = configure_background(&mut aft);
    let marker = dir.path().join("spawn-running.alive");
    let release = dir.path().join("spawn-running.release");
    // Declare after the TempDir: Rust drops locals in reverse declaration order,
    // so this guard writes the sentinel before the TempDir removes its directory.
    let _release_guard = ReleaseOnDrop::new(release.clone());
    let command = hold_until_release_command(&marker, &release);

    let task_id = spawn_bg(&mut aft, "spawn-running", &command);
    wait_for_file(&marker);
    let running = status(&mut aft, &task_id);
    assert_eq!(
        running["success"], true,
        "running status failed: {running:?}"
    );
    assert_eq!(running["status"], "running");

    drop(_release_guard);
    let completed = wait_for_status(&mut aft, &task_id, "completed");
    assert_eq!(completed["exit_code"], 0);
    assert!(completed["duration_ms"].is_u64());

    assert!(aft.shutdown().success());
}

#[test]
fn background_completion_push_frame_emits_on_terminal_transition() {
    let mut aft = AftProcess::spawn();
    let _dir = configure_background(&mut aft);

    let task_id = spawn_bg(&mut aft, "spawn-push-frame", "echo push-frame-done");
    let frame = wait_for_bash_completed_frame(&mut aft, &task_id);

    assert_eq!(frame["session_id"], "__default__");
    assert_eq!(frame["status"], "completed");
    assert_eq!(frame["exit_code"], 0);
    assert_eq!(frame["command"], "echo push-frame-done");

    assert!(aft.shutdown().success());
}

#[test]
fn background_completion_frame_remains_valid_json() {
    let mut aft = AftProcess::spawn();
    let _dir = configure_background(&mut aft);

    let task_id = spawn_bg(&mut aft, "spawn-valid-json", "echo json-frame-done");
    let frame = wait_for_bash_completed_frame(&mut aft, &task_id);

    assert_eq!(frame["type"], "bash_completed");
    assert_eq!(frame["status"], "completed");

    assert!(aft.shutdown().success());
}

// Unix-only: this test asserts `output.contains("hello\n")` after a
// PowerShell `Write-Output` round-trip. Windows uses CRLF and the
// shell's text-mode handling strips the trailing newline, so the
// assertion fails. Production behavior is verified by the Windows
// e2e harness (scenario 2c — bg via direct binary).
#[cfg(unix)]
#[test]
fn background_output_preview_updates_and_completes() {
    let mut aft = AftProcess::spawn();
    let _dir = configure_background(&mut aft);

    let task_id = spawn_bg(
        &mut aft,
        "spawn-output",
        "echo hello; sleep 0.5; echo world",
    );
    let started = Instant::now();
    loop {
        let response = status(&mut aft, &task_id);
        assert_eq!(response["success"], true, "status failed: {response:?}");
        if response["output_preview"]
            .as_str()
            .unwrap_or("")
            .contains("hello")
        {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(4));
        std::thread::sleep(Duration::from_millis(50));
    }

    let completed = wait_for_status(&mut aft, &task_id, "completed");
    let output = completed["output_preview"].as_str().unwrap();
    assert!(output.contains("hello\n"));
    assert!(output.contains("world\n"));

    assert!(aft.shutdown().success());
}

#[cfg(unix)]
#[test]
fn abort_inflight_leaves_explicit_background_and_pty_tasks_alive() {
    let mut aft = AftProcess::spawn();
    let _dir = configure_background(&mut aft);

    let background = spawn_bg(&mut aft, "abort-explicit-background", "sleep 5");
    let background_status = status(&mut aft, &background);
    let background_pid = background_status["child_pid"]
        .as_u64()
        .expect("background child pid") as i32;

    let pty = spawn_bg_params(
        &mut aft,
        "abort-explicit-pty",
        json!({
            "command": "sleep 5",
            "background": true,
            "pty": true,
        }),
    );
    let pty_status = status(&mut aft, &pty);
    let pty_pid = pty_status["child_pid"].as_u64().expect("pty child pid") as i32;

    let abort = aft.send(
        &json!({
            "id": "abort-no-foreground",
            "command": "bash_abort_inflight",
            "params": { "session_id": "spoofed-session" },
        })
        .to_string(),
    );
    assert_eq!(abort["success"], true, "abort failed: {abort:?}");
    assert_eq!(
        abort["killed"], 0,
        "abort touched detached tasks: {abort:?}"
    );
    assert_eq!(status(&mut aft, &background)["status"], "running");
    assert_eq!(status(&mut aft, &pty)["status"], "running");
    assert!(
        process_exists(background_pid),
        "explicit background task was killed"
    );
    assert!(process_exists(pty_pid), "explicit PTY task was killed");

    let _ = aft.send(
        &json!({
            "id": "cleanup-explicit-background",
            "command": "bash_kill",
            "params": { "task_id": background },
        })
        .to_string(),
    );
    let _ = aft.send(
        &json!({
            "id": "cleanup-explicit-pty",
            "command": "bash_kill",
            "params": { "task_id": pty },
        })
        .to_string(),
    );
    assert!(aft.shutdown().success());
}

#[test]
fn background_kill_running_task() {
    let mut aft = AftProcess::spawn();
    let _dir = configure_background(&mut aft);

    let task_id = spawn_bg(&mut aft, "spawn-kill", "sleep 5");
    let killed = aft.send(
        &json!({
            "id": "kill-bg",
            "command": "bash_kill",
            "params": { "task_id": task_id }
        })
        .to_string(),
    );
    assert_eq!(killed["success"], true, "kill failed: {killed:?}");
    assert_eq!(killed["status"], "killed");

    let after = status(&mut aft, &task_id);
    assert_eq!(after["status"], "killed");

    assert!(aft.shutdown().success());
}

#[test]
fn background_kill_long_running_task_stays_killed_not_failed() {
    let mut aft = AftProcess::spawn();
    let _dir = configure_background(&mut aft);

    let task_id = spawn_bg(&mut aft, "spawn-kill-race", "sleep 30");
    let killed = aft.send(
        &json!({
            "id": "kill-bg-race",
            "command": "bash_kill",
            "params": { "task_id": task_id }
        })
        .to_string(),
    );
    assert_eq!(killed["success"], true, "kill failed: {killed:?}");
    assert_eq!(killed["status"], "killed");

    let started = Instant::now();
    loop {
        let after = status(&mut aft, &task_id);
        assert_ne!(after["status"], "failed", "kill was overwritten: {after:?}");
        if after["status"] == "killed" {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(3));
        std::thread::sleep(Duration::from_millis(50));
    }

    assert!(aft.shutdown().success());
}

#[cfg(unix)]
#[test]
fn background_kill_terminates_shell_process_group_grandchild() {
    let mut aft = AftProcess::spawn();
    let dir = configure_background(&mut aft);
    let pid_file = dir.path().join("bg-sleep.pid");
    let command = format!("sleep 30 & echo $! > {}; wait", pid_file.display());

    let task_id = spawn_bg(&mut aft, "spawn-kill-pgroup", &command);
    // `> file` creates the file before the shell writes into it, so existence
    // is not proof the pid is readable yet: a read landing in that window
    // returns "" and the parse fails. Wait for parseable content instead.
    let started = Instant::now();
    let pid: i32 = loop {
        if let Some(pid) = std::fs::read_to_string(&pid_file)
            .ok()
            .and_then(|contents| contents.trim().parse::<i32>().ok())
        {
            break pid;
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "background shell never wrote a readable pid to {}",
            pid_file.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    };

    let killed = aft.send(
        &json!({
            "id": "kill-bg-pgroup",
            "command": "bash_kill",
            "params": { "task_id": task_id }
        })
        .to_string(),
    );
    assert_eq!(killed["success"], true, "kill failed: {killed:?}");
    assert_eq!(killed["status"], "killed");
    assert!(
        wait_until_process_exits(pid),
        "grandchild sleep process {pid} survived background kill"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn background_concurrent_task_cap_is_enforced() {
    let mut aft = AftProcess::spawn();
    let _dir = configure_background(&mut aft);

    let mut task_ids = Vec::new();
    for i in 0..8 {
        // Long sleep so the cap-filling tasks cannot self-complete and free a
        // slot during the spawn + 9th-send window on a slow runner (Windows CI
        // flaked here with `sleep 2`). They are killed in the cleanup loop
        // below, so the duration costs no wall-clock time.
        task_ids.push(spawn_bg(&mut aft, &format!("spawn-cap-{i}"), "sleep 30"));
    }
    let rejected = aft.send(
        &json!({
            "id": "spawn-cap-rejected",
            "command": "bash",
            "params": { "command": "sleep 1", "background": true }
        })
        .to_string(),
    );
    assert_eq!(
        rejected["success"], false,
        "9th task should fail: {rejected:?}"
    );
    assert_eq!(rejected["code"], "background_task_limit_exceeded");

    for task_id in task_ids {
        let _ = aft.send(
            &json!({
                "id": format!("kill-{task_id}"),
                "command": "bash_kill",
                "params": { "task_id": task_id }
            })
            .to_string(),
        );
    }

    assert!(aft.shutdown().success());
}

#[test]
fn background_output_spills_to_disk() {
    let mut aft = AftProcess::spawn();
    let dir = configure_background(&mut aft);

    let large = dir.path().join("large-output.txt");
    const LARGE_OUTPUT_BYTES: usize = 32_000;
    std::fs::write(&large, vec![b'x'; LARGE_OUTPUT_BYTES]).expect("write large output fixture");
    let task_id = spawn_bg(&mut aft, "spawn-spill", &cat_file_command(&large));
    let completed = wait_for_status(&mut aft, &task_id, "completed");
    assert_eq!(completed["success"], true, "status failed: {completed:?}");
    let output_path = completed["output_path"].as_str().expect("spill path");
    let metadata = std::fs::metadata(output_path).expect("spill file metadata");
    assert!(
        metadata.len() >= LARGE_OUTPUT_BYTES as u64,
        "spill was too small: {metadata:?}"
    );
    assert_eq!(completed["output_truncated"], true);

    assert!(aft.shutdown().success());
}

#[test]
fn background_feature_flag_disabled_rejects_spawn() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    // Explicitly disable background via a config tier. configure now ALWAYS
    // resolves (a tier-less configure applies the recommended-surface defaults,
    // which ENABLE background) — so to test the "background disabled rejects
    // spawn" path we must supply `bash: { background: false }` as a tier, the way
    // a real plugin sends its resolved config. This is also how the feature is
    // genuinely turned off, not an artifact of the prior skip-resolution leaving
    // Config::default().
    let configure = aft.send(
        &json!({
            "id": "cfg-disabled",
            "command": "configure",
            "harness": "opencode",
            "project_root": dir.path(),
            "config": [{
                "tier": "user",
                "source": "/tmp/aft-bg-disabled.jsonc",
                "doc": "{ \"bash\": { \"background\": false } }"
            }]
        })
        .to_string(),
    );
    assert_eq!(configure["success"], true);

    let response = aft.send(
        &json!({
            "id": "spawn-disabled",
            "command": "bash",
            "params": { "command": "sleep 1", "background": true }
        })
        .to_string(),
    );
    assert_eq!(response["success"], false);
    assert_eq!(response["code"], "feature_disabled");
    // Regression: error message must point at the CURRENT user-facing config
    // surface — top-level `bash: { background: true }` — not the deprecated
    // `experimental.bash.*` block (legacy-fallback only) nor the flat internal
    // key (`experimental_bash_background`) that v0.18 migrated away from.
    let message = response["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("bash: { background: true }"),
        "feature-disabled message should point at the top-level bash config, got: {message}"
    );
    assert!(
        !message.contains("experimental"),
        "feature-disabled message must not reference the deprecated experimental config, got: {message}"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn background_status_unknown_task_returns_task_not_found() {
    let mut aft = AftProcess::spawn();
    let _dir = configure_background(&mut aft);

    let response = aft.send(
        &json!({
            "id": "status-missing",
            "command": "bash_status",
            "params": { "task_id": "missing-task" }
        })
        .to_string(),
    );
    assert_eq!(response["success"], false);
    assert_eq!(response["code"], "task_not_found");
    assert_eq!(
        response["message"],
        "background task not found: missing-task. Task IDs only come from a bash tool result or completion notice. If you never received one, the command was not promoted — re-run the command instead of polling."
    );

    assert!(aft.shutdown().success());
}

#[test]
fn background_status_allows_cross_session_same_project_lookup() {
    let mut aft = AftProcess::spawn();
    let _dir = configure_background(&mut aft);

    let spawn = aft.send(
        &json!({
            "id": "spawn-owned-status",
            "session_id": "session-a",
            "command": "bash",
            "params": { "command": "sleep 2", "background": true }
        })
        .to_string(),
    );
    assert_eq!(spawn["success"], true, "spawn failed: {spawn:?}");
    let task_id = spawn["task_id"].as_str().unwrap().to_string();

    let cross_session = status_with_session(&mut aft, &task_id, "session-b");
    assert_eq!(
        cross_session["success"], true,
        "cross-session same-project status lookup failed: {cross_session:?}"
    );
    assert_eq!(cross_session["status"], "running");

    let owned = status_with_session(&mut aft, &task_id, "session-a");
    assert_eq!(owned["success"], true, "owner status failed: {owned:?}");

    assert!(aft.shutdown().success());
}

#[test]
fn background_kill_cross_session_same_project_finds_task_by_id() {
    let mut aft = AftProcess::spawn();
    let _dir = configure_background(&mut aft);

    let spawn = aft.send(
        &json!({
            "id": "spawn-owned-kill",
            "session_id": "session-a",
            "command": "bash",
            "params": { "command": "sleep 2", "background": true }
        })
        .to_string(),
    );
    assert_eq!(spawn["success"], true, "spawn failed: {spawn:?}");
    let task_id = spawn["task_id"].as_str().unwrap().to_string();

    let killed = aft.send(
        &json!({
            "id": "kill-cross-session",
            "session_id": "session-b",
            "command": "bash_kill",
            "params": { "task_id": task_id }
        })
        .to_string(),
    );
    assert_eq!(
        killed["success"], true,
        "cross-session kill failed: {killed:?}"
    );
    assert_eq!(killed["status"], "killed");

    let owned = status_with_session(&mut aft, &task_id, "session-a");
    assert_eq!(owned["success"], true, "owner status failed: {owned:?}");
    assert_eq!(owned["status"], "killed");

    assert!(aft.shutdown().success());
}

#[test]
fn background_completion_metadata_is_attached_to_next_response() {
    let mut aft = AftProcess::spawn();
    let _dir = configure_background(&mut aft);

    let done_command = echo_text_command("done");
    let task_id = spawn_bg(&mut aft, "spawn-completion", &done_command);
    let started = Instant::now();
    loop {
        let ping = aft.send(r#"{"id":"ping-bg","command":"ping"}"#);
        if let Some(completions) = ping["bg_completions"].as_array() {
            let completion = completions
                .iter()
                .find(|completion| completion["task_id"] == task_id)
                .expect("completion for task");
            assert_eq!(completion["status"], "completed");
            assert_eq!(completion["exit_code"], 0);
            assert_eq!(completion["command"], done_command);
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(12));
        std::thread::sleep(Duration::from_millis(100));
    }

    assert!(aft.shutdown().success());
}

#[test]
fn background_completion_delivery_is_scoped_by_session_id() {
    let mut aft = AftProcess::spawn();
    let dir = configure_background(&mut aft);
    let marker = dir.path().join("session-a-started");
    let release = dir.path().join("session-a-release");
    // Declare after the TempDir: Rust drops locals in reverse declaration order,
    // so this guard writes the sentinel before the TempDir removes its directory.
    let _release_guard = ReleaseOnDrop::new(release.clone());
    let command = cross_platform_hold_until_release_command(&marker, &release);

    let spawn = aft.send(
        &json!({
            "id": "spawn-session-a",
            "session_id": "session-a",
            "command": "bash",
            "params": { "command": command, "background": true }
        })
        .to_string(),
    );
    assert_eq!(spawn["success"], true, "spawn failed: {spawn:?}");
    let task_id = spawn["task_id"].as_str().unwrap().to_string();
    wait_for_file(&marker);
    drop(_release_guard);

    // The completion frame is emitted only after the authoritative completion
    // queue has admitted the task. Query the foreign session after that event,
    // which deterministically presents the interleaving that an unscoped drain
    // would mishandle.
    let terminal = wait_for_bash_completed_frame(&mut aft, &task_id);
    assert_eq!(terminal["session_id"], "session-a");
    let session_b = aft.send(
        &json!({
            "id": "ping-session-b",
            "session_id": "session-b",
            "command": "ping"
        })
        .to_string(),
    );
    assert_eq!(session_b["success"], true);
    assert!(
        session_b["bg_completions"]
            .as_array()
            .is_none_or(|items| items.is_empty()),
        "session B drained session A completion: {session_b:?}"
    );

    // Positive control: the same admitted completion is visible immediately
    // through its owning session, so the foreign-session absence is meaningful.
    let session_a = aft.send(
        &json!({
            "id": "ping-session-a",
            "session_id": "session-a",
            "command": "ping"
        })
        .to_string(),
    );
    assert_eq!(session_a["success"], true);
    assert!(session_a["bg_completions"]
        .as_array()
        .is_some_and(|completions| completions
            .iter()
            .any(|completion| completion["task_id"] == task_id)));

    assert!(aft.shutdown().success());
}

// Unix-only: this test passes a `workdir` JSON value built from a Rust
// `PathBuf`, which on Windows serializes with backslashes that the JSON
// parser rejects (os error 123, "invalid filename"). Production code
// receives the workdir from the plugin layer where paths are already
// JSON-escaped; the integration test layer doesn't replicate that
// escaping. Production behavior is verified by the Windows e2e harness.
#[cfg(unix)]
#[test]
fn background_spawn_honors_custom_workdir() {
    let mut aft = AftProcess::spawn();
    let dir = configure_background(&mut aft);
    let nested = dir.path().join("nested");
    std::fs::create_dir(&nested).unwrap();

    let task_id = spawn_bg_params(
        &mut aft,
        "spawn-bg-workdir",
        json!({ "command": "pwd", "background": true, "workdir": nested }),
    );
    let completed = wait_for_status(&mut aft, &task_id, "completed");
    let actual =
        std::fs::canonicalize(completed["output_preview"].as_str().unwrap().trim()).unwrap();
    let expected = std::fs::canonicalize(&nested).unwrap();
    assert_eq!(actual, expected);

    assert!(aft.shutdown().success());
}

#[test]
fn background_spawn_honors_env_overrides() {
    let mut aft = AftProcess::spawn();
    let _dir = configure_background(&mut aft);

    #[cfg(windows)]
    let command = "Write-Host -NoNewline $env:AFT_BG_ENV_TEST";
    #[cfg(not(windows))]
    let command = "printf '%s' \"$AFT_BG_ENV_TEST\"";

    let task_id = spawn_bg_params(
        &mut aft,
        "spawn-bg-env",
        json!({
            "command": command,
            "background": true,
            "env": { "AFT_BG_ENV_TEST": "from-bg-env" }
        }),
    );
    let completed = wait_for_status(&mut aft, &task_id, "completed");
    assert_eq!(completed["output_preview"].as_str().unwrap(), "from-bg-env");

    assert!(aft.shutdown().success());
}

#[test]
fn background_spawn_honors_timeout() {
    let mut aft = AftProcess::spawn();
    let _dir = configure_background(&mut aft);

    let task_id = spawn_bg_params(
        &mut aft,
        "spawn-bg-timeout",
        json!({ "command": "sleep 5", "background": true, "timeout": 200 }),
    );
    let started = Instant::now();
    let failed = wait_for_status(&mut aft, &task_id, "timed_out");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "timeout took too long: {failed:?}"
    );
    assert_eq!(failed["exit_code"], 124);

    assert!(aft.shutdown().success());
}

// ---------------------------------------------------------------------------
// Slug format regression — task IDs must be compact, agent-friendly slugs of
// the form `bash-{8-hex}` (4 OS-entropy bytes, hex-encoded). The earlier
// timestamp-XOR format produced predictable IDs; the earlier 16-char version
// used non-cryptographic mixing. Locked in by direct format assertion.
// ---------------------------------------------------------------------------

#[test]
fn background_task_ids_use_short_bash_slug_format() {
    let mut aft = AftProcess::spawn();
    let _dir = configure_background(&mut aft);

    let task_id = spawn_bg(&mut aft, "slug-format", "true");

    // Format: "bash-" + exactly 16 lowercase hex characters (8 OS-entropy bytes /
    // 64-bit — the width is load-bearing for subc delivery dedup, see random_slug).
    assert!(
        task_id.starts_with("bash-"),
        "task_id must start with `bash-` prefix; got `{task_id}`"
    );
    let suffix = &task_id["bash-".len()..];
    assert_eq!(
        suffix.len(),
        16,
        "task_id suffix must be exactly 16 hex chars; got `{suffix}` (len={})",
        suffix.len()
    );
    assert!(
        suffix
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()),
        "task_id suffix must be lowercase hex; got `{suffix}`"
    );

    // Wait for completion and check the completion event carries the same ID
    // — important so the in-turn delivery path isn't broken by the ID change.
    let completed = wait_for_status(&mut aft, &task_id, "completed");
    assert_eq!(completed["task_id"].as_str().unwrap(), task_id);

    assert!(aft.shutdown().success());
}

#[test]
fn background_task_ids_are_unique_across_rapid_spawns() {
    // Spawn 6 short-lived tasks back-to-back and assert all IDs are distinct.
    // Catches generator regressions where the time-based seed alone collapses
    // to the same slug for spawns within the same nanosecond — happens often
    // on macOS where realtime clock resolution is microseconds. The atomic
    // counter inside `random_slug()` is the load-bearing piece this guards.
    //
    // We spawn `true` (exits instantly) and wait for completion between
    // spawns so we don't trip the running-task cap (default 8).
    let mut aft = AftProcess::spawn();
    let _dir = configure_background(&mut aft);

    let mut ids = std::collections::HashSet::new();
    for i in 0..6 {
        let id = spawn_bg(&mut aft, &format!("unique-{i}"), quick_success_command());
        assert!(
            ids.insert(id.clone()),
            "duplicate task_id allocated: `{id}` (already in {ids:?})"
        );
        // Drain to completed before the next spawn so running_count stays low.
        let _ = wait_for_status(&mut aft, &id, "completed");
    }

    assert!(aft.shutdown().success());
}

#[test]
fn replay_does_not_return_acknowledged_completion_after_restart() {
    let mut aft = AftProcess::spawn();
    let dir = configure_background(&mut aft);
    let task_id = spawn_bg(&mut aft, "replay-acked", cross_platform_echo_command());
    let _completed = wait_for_bash_completed_frame(&mut aft, &task_id);
    let ack = aft.send(
        &json!({
            "id": "ack-replay-acked",
            "command": "bash_ack_completions",
            "params": { "task_ids": [task_id.clone()] }
        })
        .to_string(),
    );
    assert_eq!(ack["success"], true, "ack failed: {ack:?}");
    assert!(aft.shutdown().success());

    let mut restarted = AftProcess::spawn();
    let response = restarted.send(
        &json!({
            "id": "cfg-bg-restart",
            "command": "configure",
            "harness": "opencode",
            "project_root": dir.path(),
            "storage_dir": dir.path().join("aft-storage"),
            "config": user_config(serde_json::json!({
                "experimental": { "bash": { "background": true } }
            })),
        })
        .to_string(),
    );
    assert_eq!(response["success"], true, "configure failed: {response:?}");
    let drained = restarted.send(
        &json!({
            "id": "drain-replay-acked",
            "command": "bash_drain_completions",
        })
        .to_string(),
    );
    assert_eq!(drained["success"], true, "drain failed: {drained:?}");
    assert_eq!(
        drained["bg_completions"].as_array().unwrap().len(),
        0,
        "acknowledged completion replayed after restart: {drained:?}"
    );
    assert!(restarted.shutdown().success());
}

#[cfg(unix)]
#[test]
fn piped_terminal_preview_normalizes_crlf_and_overprints_without_changing_artifact() {
    let mut aft = AftProcess::spawn();
    let _dir = configure_background(&mut aft);

    let crlf_task = spawn_bg_params(
        &mut aft,
        "crlf-display-preview",
        json!({
            "command": r"printf 'Microsoft Windows [Version 10.0]\r\nDirectory of C:\\work\r\n'",
            "background": true,
            "compressed": false,
        }),
    );
    let crlf_terminal = wait_for_status(&mut aft, &crlf_task, "completed");
    assert_eq!(
        crlf_terminal["output_preview"],
        "Microsoft Windows [Version 10.0]\nDirectory of C:\\work\n"
    );
    let artifact = std::fs::read(
        crlf_terminal["output_path"]
            .as_str()
            .expect("completed task has a stdout artifact"),
    )
    .expect("read stdout artifact");
    assert_eq!(
        artifact,
        b"Microsoft Windows [Version 10.0]\r\nDirectory of C:\\work\r\n"
    );

    let overprint_task = spawn_bg_params(
        &mut aft,
        "carriage-return-overprint-preview",
        json!({
            "command": r"printf 'step-one\rstep-two\n'",
            "background": true,
            "compressed": false,
        }),
    );
    let overprint_terminal = wait_for_status(&mut aft, &overprint_task, "completed");
    assert_eq!(overprint_terminal["output_preview"], "step-two\n");

    assert!(aft.shutdown().success());
}
