use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::helpers::AftProcess;

fn configure_background(aft: &mut AftProcess) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let response = aft.send(
        &json!({
            "id": "cfg-drain-bg",
            "command": "configure",
            "harness": "opencode",
            "project_root": dir.path(),
            "experimental_bash_background": true,
        })
        .to_string(),
    );
    assert_eq!(response["success"], true, "configure failed: {response:?}");
    dir
}

fn drain(aft: &mut AftProcess) -> Value {
    aft.send(
        &json!({
            "id": "drain-bg",
            "command": "bash_drain_completions"
        })
        .to_string(),
    )
}

fn ack(aft: &mut AftProcess, task_id: &str) -> Value {
    aft.send(
        &json!({
            "id": "ack-bg",
            "command": "bash_ack_completions",
            "params": { "task_ids": [task_id] }
        })
        .to_string(),
    )
}

#[test]
fn drain_completions_returns_empty_success_when_none_pending() {
    let mut aft = AftProcess::spawn();
    let _dir = configure_background(&mut aft);

    let response = drain(&mut aft);

    assert_eq!(response["success"], true);
    assert_eq!(response["bg_completions"].as_array().unwrap().len(), 0);
    assert!(aft.shutdown().success());
}

#[test]
fn drain_completions_peeks_until_ack_consumes_background_completions() {
    let mut aft = AftProcess::spawn();
    let _dir = configure_background(&mut aft);

    let spawn = aft.send(
        &json!({
            "id": "spawn-drain-bg",
            "command": "bash",
            "params": { "command": "echo drained", "background": true }
        })
        .to_string(),
    );
    assert_eq!(spawn["success"], true, "spawn failed: {spawn:?}");
    let task_id = spawn["task_id"].as_str().unwrap().to_string();

    let started = Instant::now();
    let first = loop {
        let response = drain(&mut aft);
        assert_eq!(response["success"], true, "drain failed: {response:?}");
        if let Some(completion) = response["bg_completions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|completion| completion["task_id"] == task_id)
        {
            break completion.clone();
        }
        assert!(started.elapsed() < Duration::from_secs(4));
        std::thread::sleep(Duration::from_millis(100));
    };

    assert_eq!(first["status"], "completed");
    assert_eq!(first["exit_code"], 0);
    assert_eq!(first["command"], "echo drained");

    let second = drain(&mut aft);
    assert_eq!(second["success"], true);
    assert_eq!(second["bg_completions"].as_array().unwrap().len(), 1);

    let acked = ack(&mut aft, &task_id);
    assert_eq!(acked["success"], true);
    assert!(acked["acked_task_ids"]
        .as_array()
        .unwrap()
        .iter()
        .any(|acked| acked.as_str() == Some(task_id.as_str())));

    let third = drain(&mut aft);
    assert_eq!(third["success"], true);
    assert_eq!(third["bg_completions"].as_array().unwrap().len(), 0);
    assert!(aft.shutdown().success());
}
