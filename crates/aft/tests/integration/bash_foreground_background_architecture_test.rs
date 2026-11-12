#![cfg(unix)]

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use aft::bash_background::persistence::{
    read_exit_marker, resolve_task_layout, session_tasks_dir, ExitMarker,
};

use super::helpers::{user_config, wait_for_task_metadata, AftProcess, ReleaseOnDrop};

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
            "config": user_config(serde_json::json!({
                "bash": { "rewrite": false, "compress": false, "background": true }
            })),
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

/// Drain is non-destructive until ack. Observe that lane directly so readiness
/// polling does not consume the terminal result through bash_status first.
fn wait_terminal_with_drain_completion(
    aft: &mut AftProcess,
    storage: &std::path::Path,
    task_id: &str,
) -> Value {
    wait_for_task_metadata(storage, "opencode", task_id, "completed");
    let started = Instant::now();
    loop {
        let drained = drain(aft);
        assert_eq!(drained["success"], true, "drain failed: {drained:?}");
        let completions = drained["bg_completions"].as_array().unwrap();
        if completions
            .iter()
            .any(|completion| completion["task_id"].as_str() == Some(task_id))
        {
            return drained;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "timed out waiting for drain completion for {task_id}"
        );
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
    assert!(
        matches!(
            bash["status"].as_str(),
            Some("running" | "completed" | "failed" | "killed" | "timed_out" | "fate_unknown")
        ),
        "unexpected spawn status: {bash:?}"
    );
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
    let drained = wait_terminal_with_drain_completion(&mut aft, storage.path(), task_id);
    let completions = drained["bg_completions"].as_array().unwrap();
    assert_eq!(completions.len(), 1, "drained: {drained:?}");
    assert_eq!(completions[0]["task_id"], task_id);
    assert!(aft.shutdown().success());
}

#[test]
fn promoted_foreground_completion_while_daemon_is_down_replays_once() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let release = project.path().join("release-command");
    let release_guard = ReleaseOnDrop::new(release.clone());
    let mut first = AftProcess::spawn();
    configure(&mut first, project.path(), storage.path());
    let bash = spawn_bash(
        &mut first,
        json!({
            "command": format!(
                "{}; printf promoted",
                crate::test_helpers::test_gate::gate(&format!("'{}'", release.display()), "sleep 0.05")
            ),
            "notify_on_completion": false,
        }),
    );
    assert_eq!(bash["success"], true, "bash failed: {bash:?}");
    let task_id = bash["task_id"].as_str().unwrap();
    let metadata = wait_for_task_metadata(storage.path(), "opencode", task_id, "running");
    assert_eq!(metadata["completion_delivered"], true);
    let conn = rusqlite::Connection::open(storage.path().join("aft.db")).unwrap();
    assert!(
        aft::db::bash_tasks::get_bash_task(&conn, "opencode", SESSION, task_id)
            .unwrap()
            .unwrap()
            .completion_delivered
    );
    let paths = resolve_task_layout(&session_tasks_dir(storage.path(), SESSION), task_id)
        .unwrap()
        .paths;
    let promoted = first.send(
        &json!({
            "id": "promote-before-restart",
            "session_id": SESSION,
            "command": "bash_promote",
            "params": { "task_id": task_id },
        })
        .to_string(),
    );
    assert_eq!(promoted["success"], true, "promote failed: {promoted:?}");
    assert!(first.shutdown().success());
    assert!(read_exit_marker(&paths).unwrap().is_none());

    // The wrapper finishes without a daemon, before any recovery can observe
    // the command. Its exit marker, rather than a sleep, orders the restart.
    drop(release_guard);
    let deadline = Instant::now() + Duration::from_secs(30);
    while read_exit_marker(&paths).unwrap().is_none() {
        assert!(Instant::now() < deadline, "detached command did not finish");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(matches!(
        read_exit_marker(&paths).unwrap(),
        Some(ExitMarker::Code(0))
    ));

    let mut restarted = AftProcess::spawn();
    configure(&mut restarted, project.path(), storage.path());
    let drained = drain(&mut restarted);
    assert_eq!(drained["success"], true, "drain failed: {drained:?}");
    let completions = drained["bg_completions"].as_array().unwrap();
    assert_eq!(
        completions.len(),
        1,
        "promoted completion was lost: {drained:?}"
    );
    assert_eq!(completions[0]["task_id"], task_id);
    assert_eq!(completions[0]["status"], "completed");
    let acked = restarted.send(
        &json!({
            "id": "ack-promoted-completion",
            "session_id": SESSION,
            "command": "bash_ack_completions",
            "params": { "task_ids": [task_id] },
        })
        .to_string(),
    );
    assert_eq!(acked["success"], true, "ack failed: {acked:?}");
    assert!(drain(&mut restarted)["bg_completions"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(restarted.shutdown().success());

    let mut second_restart = AftProcess::spawn();
    configure(&mut second_restart, project.path(), storage.path());
    let drained = drain(&mut second_restart);
    assert_eq!(drained["success"], true, "drain failed: {drained:?}");
    assert!(
        drained["bg_completions"].as_array().unwrap().is_empty(),
        "acked promoted completion was replayed: {drained:?}"
    );
    assert!(second_restart.shutdown().success());
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
            "config": user_config(serde_json::json!({
                "bash": {
                    "background": true,
                    "long_running_reminder_enabled": true,
                    "long_running_reminder_interval_ms": 100
                }
            })),
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
