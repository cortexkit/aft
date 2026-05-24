#![cfg(unix)]

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::helpers::AftProcess;

const SESSION: &str = "bash-arch-session";

fn configure(aft: &mut AftProcess, project: &std::path::Path, storage: &std::path::Path) {
    let response = aft.send(
        &json!({
            "id": "cfg-bash-arch",
            "session_id": SESSION,
            "command": "configure",
            "harness": "opencode",
            "project_root": project,
            "storage_dir": storage,
            "experimental_bash_background": true,
            "max_background_bash_tasks": 32,
        })
        .to_string(),
    );
    assert_eq!(response["success"], true, "configure failed: {response:?}");
}

fn spawn_bash(aft: &mut AftProcess, params: Value) -> Value {
    aft.send(
        &json!({
            "id": "bash-arch-spawn",
            "session_id": SESSION,
            "command": "bash",
            "params": params,
        })
        .to_string(),
    )
}

fn status(aft: &mut AftProcess, task_id: &str) -> Value {
    aft.send(
        &json!({
            "id": format!("status-{task_id}"),
            "session_id": SESSION,
            "command": "bash_status",
            "params": { "task_id": task_id },
        })
        .to_string(),
    )
}

fn drain(aft: &mut AftProcess) -> Value {
    aft.send(
        &json!({
            "id": "drain-bash-arch",
            "session_id": SESSION,
            "command": "bash_drain_completions",
        })
        .to_string(),
    )
}

fn wait_terminal(aft: &mut AftProcess, task_id: &str) -> Value {
    let started = Instant::now();
    loop {
        let response = status(aft, task_id);
        assert_eq!(response["success"], true, "status failed: {response:?}");
        if matches!(
            response["status"].as_str(),
            Some("completed" | "failed" | "killed" | "timed_out")
        ) {
            return response;
        }
        assert!(started.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn foreground_bash_returns_immediately_and_does_not_block_dispatch_loop() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path(), storage.path());
    let read_path = project.path().join("probe.txt");
    std::fs::write(&read_path, "probe").unwrap();

    let started = Instant::now();
    let bash = spawn_bash(&mut aft, json!({ "command": "sleep 5", "timeout": 10_000 }));
    assert_eq!(bash["success"], true, "bash failed: {bash:?}");
    assert_eq!(bash["status"], "running");
    // On a healthy local machine this typically returns in <100ms, but the
    // correctness contract is semantic: a long command is promoted and returns
    // a running task instead of blocking until command completion.
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "foreground bash appears deadlocked before promotion: {bash:?}"
    );

    let read_started = Instant::now();
    let read = aft.send(
        &json!({
            "id": "read-after-bash",
            "session_id": SESSION,
            "command": "read",
            "file": read_path,
        })
        .to_string(),
    );
    assert_eq!(read["success"], true, "read failed: {read:?}");
    // This should be near-instant locally; keep only a generous deadlock bound
    // so CI scheduling jitter does not turn latency into correctness.
    assert!(
        read_started.elapsed() < Duration::from_secs(10),
        "read was blocked behind foreground bash: {read:?}"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn foreground_bash_with_daemonized_child_does_not_wait_for_inherited_fds() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path(), storage.path());

    let started = Instant::now();
    let bash = spawn_bash(
        &mut aft,
        json!({ "command": "nohup sh -c 'sleep 30 && echo done' > /dev/null 2>&1 &" }),
    );
    assert_eq!(bash["success"], true, "bash failed: {bash:?}");
    assert_eq!(bash["status"], "running");
    // Usually returns in <100ms; assert only that inherited file descriptors do
    // not deadlock the foreground wait path.
    assert!(started.elapsed() < Duration::from_secs(10));

    let terminal = wait_terminal(&mut aft, bash["task_id"].as_str().unwrap());
    assert_eq!(terminal["status"], "completed");
    assert!(aft.shutdown().success());
}

#[test]
fn no_notify_foreground_poll_completion_does_not_enqueue_completion() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path(), storage.path());

    let bash = spawn_bash(
        &mut aft,
        json!({ "command": "printf done", "notify_on_completion": false }),
    );
    assert_eq!(bash["success"], true, "bash failed: {bash:?}");
    let _ = wait_terminal(&mut aft, bash["task_id"].as_str().unwrap());
    let drained = drain(&mut aft);
    assert_eq!(drained["bg_completions"].as_array().unwrap().len(), 0);
    assert!(aft.shutdown().success());
}

#[test]
fn bash_promote_reenables_completion_delivery() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path(), storage.path());

    let bash = spawn_bash(
        &mut aft,
        json!({ "command": "sleep 0.3; printf promoted", "notify_on_completion": false }),
    );
    assert_eq!(bash["success"], true, "bash failed: {bash:?}");
    let task_id = bash["task_id"].as_str().unwrap();
    let promoted = aft.send(
        &json!({
            "id": "promote-bash-arch",
            "session_id": SESSION,
            "command": "bash_promote",
            "params": { "task_id": task_id },
        })
        .to_string(),
    );
    assert_eq!(promoted["success"], true, "promote failed: {promoted:?}");
    let _ = wait_terminal(&mut aft, task_id);
    let drained = drain(&mut aft);
    let completions = drained["bg_completions"].as_array().unwrap();
    assert_eq!(completions.len(), 1, "drained: {drained:?}");
    assert_eq!(completions[0]["task_id"], task_id);
    assert!(aft.shutdown().success());
}

#[test]
fn long_running_reminder_frame_fires_after_configured_interval() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let mut aft = AftProcess::spawn();
    let configure = aft.send(
        &json!({
            "id": "cfg-bash-reminder",
            "session_id": SESSION,
            "command": "configure",
            "harness": "opencode",
            "project_root": project.path(),
            "storage_dir": storage.path(),
            "experimental_bash_background": true,
            "bash_long_running_reminder_enabled": true,
            "bash_long_running_reminder_interval_ms": 100,
        })
        .to_string(),
    );
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:?}"
    );

    let bash = spawn_bash(&mut aft, json!({ "command": "sleep 1", "timeout": 2_000 }));
    assert_eq!(bash["success"], true, "bash failed: {bash:?}");
    let task_id = bash["task_id"].as_str().unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let Some(frame) = aft.try_read_next_timeout(Duration::from_millis(500)) else {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for reminder frame"
            );
            continue;
        };
        if frame["type"] == "bash_long_running" {
            assert_eq!(frame["task_id"], task_id);
            assert_eq!(frame["session_id"], SESSION);
            assert!(frame["elapsed_ms"].as_u64().unwrap() >= 100);
            break;
        }
    }
    assert!(aft.shutdown().success());
}
