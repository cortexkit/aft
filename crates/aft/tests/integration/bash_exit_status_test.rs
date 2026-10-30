use super::helpers::{user_config, AftProcess};
use serde_json::json;
use std::time::{Duration, Instant};

fn exit_status_matrix(form: &str) {
    let project = tempfile::tempdir().unwrap();
    let mut aft = AftProcess::spawn();
    let configured = aft.send(
        &json!({
            "id": "configure-exit-status",
            "command": "configure",
            "harness": "opencode",
            "project_root": project.path(),
            "config": user_config(json!({
                "experimental": { "bash": { "background": true } }
            })),
        })
        .to_string(),
    );
    assert_eq!(configured["success"], true, "{configured}");

    // Keep the PowerShell spellings on Unix too: their command-not-found output
    // distinguishes shell syntax differences from lost explicit exit statuses.
    let mut cases = vec![
        ("exit 3", 3, ""),
        (
            "Write-Output hi; exit 3",
            3,
            if cfg!(windows) { "hi" } else { "Write-Output" },
        ),
        ("exit 0", 0, ""),
        (
            "cmd.exe /D /C exit /B 5",
            if cfg!(windows) { 5 } else { 127 },
            if cfg!(windows) { "" } else { "cmd.exe" },
        ),
        (
            "throw \"boom\"",
            if cfg!(windows) { 1 } else { 127 },
            if cfg!(windows) { "boom" } else { "throw" },
        ),
    ];
    if cfg!(unix) {
        cases.push(("sh -c 'exit 5'", 5, ""));
    }
    let mut failures = Vec::new();
    for (command, expected_code, expected_output) in cases {
        let mut params = json!({
            "command": command,
            "background": form == "background",
            "pty": form == "pty",
        });
        if cfg!(windows) {
            params["shell"] = json!("powershell");
            params["permissions_granted"] = json!([command]);
        }
        let spawned = aft
            .send(&json!({"id": "exit-status", "command": "bash", "params": params}).to_string());
        assert_eq!(spawned["success"], true, "{spawned}");
        let task_id = spawned["task_id"].as_str().expect("task id");
        let deadline = Instant::now() + Duration::from_secs(30);
        let status = loop {
            let status = aft.send(
                &json!({
                    "id": "exit-status-result",
                    "command": "bash_status",
                    "params": { "task_id": task_id, "output_mode": "raw" },
                })
                .to_string(),
            );
            assert_eq!(status["success"], true, "{status}");
            if status["status"] != "running" && status["status"] != "starting" {
                break status;
            }
            assert!(Instant::now() < deadline, "{status}");
            std::thread::sleep(Duration::from_millis(50));
        };
        println!(
            "EXIT_STATUS_MATRIX os={} form={form} command={command:?} result={status}",
            std::env::consts::OS
        );
        let output = status[if form == "pty" {
            "pty_raw"
        } else {
            "output_preview"
        }]
        .as_str()
        .unwrap_or_default();
        let terminal_escapes =
            regex::Regex::new(r"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07]*(?:\x07|\x1b\\))").unwrap();
        let plain_output = terminal_escapes
            .replace_all(output, "")
            .replace("\r\n", "\n");
        let output_matches = if cfg!(windows) || expected_output.is_empty() {
            plain_output.trim() == expected_output
        } else {
            plain_output.contains(expected_output)
        };
        if status["status"]
            != if expected_code == 0 {
                "completed"
            } else {
                "failed"
            }
            || status["exit_code"] != expected_code
            || !output_matches
        {
            failures.push(
                json!({"command": command, "expected_code": expected_code, "result": status}),
            );
        }
    }
    assert!(aft.shutdown().success());
    assert!(
        failures.is_empty(),
        "{form} exit status failures: {failures:?}"
    );
}

#[test]
fn bash_foreground_preserves_explicit_exit_status() {
    exit_status_matrix("foreground");
}

#[test]
fn bash_background_preserves_explicit_exit_status() {
    exit_status_matrix("background");
}

#[test]
fn bash_pty_preserves_explicit_exit_status() {
    exit_status_matrix("pty");
}

#[cfg(windows)]
#[test]
fn bash_waiter_kill_terminates_shell_and_descendants() {
    use std::os::windows::io::RawHandle;
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    struct Children(Vec<(u32, RawHandle)>);
    impl Drop for Children {
        fn drop(&mut self) {
            for (pid, handle) in &self.0 {
                if unsafe { WaitForSingleObject(*handle, 0) } != WAIT_OBJECT_0 {
                    let _ = std::process::Command::new("taskkill.exe")
                        .args(["/PID", &pid.to_string(), "/T", "/F"])
                        .status();
                }
                unsafe {
                    CloseHandle(*handle);
                }
            }
        }
    }

    for form in ["foreground", "background", "pty"] {
        let project = tempfile::tempdir().unwrap();
        let mut aft = AftProcess::spawn();
        let configured = aft.send(&json!({
            "id":"configure-kill", "command":"configure", "harness":"opencode", "project_root":project.path(),
        }).to_string());
        assert_eq!(configured["success"], true, "{configured}");
        let pid_file = project.path().join("command-pids");
        let command = format!(
            "$child = Start-Process powershell.exe -ArgumentList '-NoProfile','-NonInteractive','-Command','Start-Sleep -Seconds 300' -PassThru -NoNewWindow; [IO.File]::WriteAllText('{}', ([string]$PID + ' ' + $child.Id)); Start-Sleep -Seconds 300",
            pid_file.display().to_string().replace('\'', "''")
        );
        let spawned = aft.send(
            &json!({
                "id":"spawn-kill", "command":"bash", "params":{
                    "command":command, "shell":"powershell", "permissions_granted":[command],
                    "background":form == "background", "pty":form == "pty",
                },
            })
            .to_string(),
        );
        assert_eq!(spawned["success"], true, "{spawned}");
        let deadline = Instant::now() + Duration::from_secs(30);
        let pids = loop {
            if let Ok(content) = std::fs::read_to_string(&pid_file) {
                if content.split_whitespace().count() == 2 {
                    break content;
                }
            }
            assert!(Instant::now() < deadline, "{form}: command did not start");
            std::thread::sleep(Duration::from_millis(50));
        };
        let mut children = Children(Vec::new());
        for pid in pids.split_whitespace().map(|s| s.parse::<u32>().unwrap()) {
            const SYNCHRONIZE_RIGHT: u32 = 0x0010_0000;
            let handle = unsafe {
                OpenProcess(
                    SYNCHRONIZE_RIGHT | PROCESS_QUERY_LIMITED_INFORMATION,
                    0,
                    pid,
                )
            };
            assert!(
                !handle.is_null(),
                "open child PID {pid}: {}",
                std::io::Error::last_os_error()
            );
            children.0.push((pid, handle));
        }
        let killed = aft.send(
            &json!({
                "id":"kill", "command":"bash_kill", "params":{"task_id":spawned["task_id"]},
            })
            .to_string(),
        );
        assert_eq!(killed["success"], true, "{killed}");
        for (pid, handle) in &children.0 {
            assert_eq!(
                unsafe { WaitForSingleObject(*handle, 5000) },
                WAIT_OBJECT_0,
                "{form}: descendant PID {pid} survived kill"
            );
        }
        assert!(aft.shutdown().success());
    }
}

/// Runs `command` in a PowerShell PTY task, types `input` once after the task
/// prints `ready`, and checks the exit code and output.
///
/// The wait has two separate budgets. PowerShell startup under a busy test
/// runner can take most of a single shared budget (it was seen taking 9-17 s
/// on a 4-CPU Windows runner with 16 tasks in parallel), which used to leave
/// almost no time for the read, echo and exit after a late `ready` and made
/// a slow start look like lost input. Failure messages name the phase and
/// the moments `ready` was seen and the write returned.
#[cfg(windows)]
fn pty_console_round_trip(command: &str, input: &str, expected_code: i32, expected_output: &str) {
    const STARTUP_BUDGET: Duration = Duration::from_secs(120);
    const ROUND_TRIP_BUDGET: Duration = Duration::from_secs(30);
    let project = tempfile::tempdir().unwrap();
    let mut aft = AftProcess::spawn();
    let configured = aft.send(&json!({
        "id":"configure-stdin", "command":"configure", "harness":"opencode", "project_root":project.path(),
    }).to_string());
    assert_eq!(configured["success"], true, "{configured}");
    let spawned = aft.send(&json!({
        "id":"spawn-stdin", "command":"bash", "params":{
            "command":command, "shell":"powershell", "permissions_granted":[command], "pty":true,
        },
    }).to_string());
    assert_eq!(spawned["success"], true, "{spawned}");
    let task_id = &spawned["task_id"];
    let started = Instant::now();
    let mut deadline = started + STARTUP_BUDGET;
    // Moments since spawn when `ready` was first seen and when the single
    // bash_write returned; `Some` on the latter also means the input was sent.
    let mut ready_seen = None;
    let mut write_returned = None;
    loop {
        let status = aft.send(&json!({
            "id":"stdin-status", "command":"bash_status", "params":{"task_id":task_id,"output_mode":"raw"},
        }).to_string());
        assert_eq!(status["success"], true, "{status}");
        let output = status["pty_raw"].as_str().unwrap_or_default();
        if write_returned.is_none() && output.contains("ready") {
            ready_seen = Some(started.elapsed());
            let written = aft.send(&json!({
                "id":"stdin-write", "command":"bash_write", "params":{"task_id":task_id,"input":input},
            }).to_string());
            assert_eq!(written["success"], true, "{written}");
            write_returned = Some(started.elapsed());
            deadline = Instant::now() + ROUND_TRIP_BUDGET;
        }
        let timing = format!("ready_seen={ready_seen:?} write_returned={write_returned:?}");
        if status["status"] != "running" {
            assert_eq!(status["exit_code"], expected_code, "{timing} {status}");
            assert!(output.contains(expected_output), "{timing} {status}");
            break;
        }
        let phase = if write_returned.is_some() {
            "no exit within the round-trip budget after the write"
        } else {
            "no `ready` within the startup budget"
        };
        assert!(Instant::now() < deadline, "{phase}: {timing} {status}");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(aft.shutdown().success());
}

#[cfg(windows)]
#[test]
fn bash_pty_waiter_inherits_console_input() {
    pty_console_round_trip(
        "Write-Output ready; $line = [Console]::ReadLine(); Write-Output ('read:' + $line); exit 3",
        "hello\r",
        3,
        "read:hello",
    );
}
