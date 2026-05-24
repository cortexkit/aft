use super::helpers::AftProcess;

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

    let response = aft.send(r#"{"id":"bash-1","method":"bash","params":{"command":"echo hello"}}"#);
    assert_eq!(response["id"], "bash-1");
    assert_eq!(response["success"], true);
    assert_eq!(response["status"], "running");

    let task_id = response["task_id"].as_str().unwrap();
    let started = std::time::Instant::now();
    let status = loop {
        let status = aft.send(
            &serde_json::json!({
                "id": "bash-1-status",
                "method": "bash_status",
                "params": { "task_id": task_id }
            })
            .to_string(),
        );
        if status["status"] == "completed" {
            break status;
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert_eq!(status["output_preview"], "hello\n");
    assert_eq!(status["exit_code"], 0);
    assert!(status["duration_ms"].is_u64());

    let status = aft.shutdown();
    assert!(status.success());
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
    assert_eq!(response["status"], "running");
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
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
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
