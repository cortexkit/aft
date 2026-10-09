//! The plugins attach `worker_session: true` next to `session_id` on every
//! request from a delegated worker (subagent) session. A worker cannot be
//! woken once its turn ends, so the engine must never promise it a completion
//! reminder. These tests send the flag the way the plugins do over the
//! standalone protocol and check the bash hand-off text each role gets. (The
//! role on a `tool_call` envelope is covered by the repeat-breaker tests: a
//! standalone `tool_call` bash returns the bare spawn reply, without a
//! hand-off text.)

use std::time::Duration;

use serde_json::{json, Value};

use crate::test_helpers::AftProcess;

const SESSION: &str = "worker-session-test";

fn background_bash_params(project: &std::path::Path) -> Value {
    json!({
        "command": "sleep 5",
        "workdir": project,
        "background": true,
        "notify_on_completion": true,
        "foreground_orchestrate": true,
    })
}

/// A raw `bash` request, as the Pi plugin and the OpenCode plugin in
/// standalone mode send it: the arguments nested under `params`, the role at
/// the top level beside `session_id`.
fn raw_bash_launch_text(aft: &mut AftProcess, project: &std::path::Path, worker: bool) -> String {
    let mut request = json!({
        "id": format!("raw-bash-worker-{worker}"),
        "command": "bash",
        "session_id": SESSION,
        "params": background_bash_params(project),
    });
    if worker {
        request["worker_session"] = json!(true);
    }
    let response = aft.send_with_timeout(&request.to_string(), Duration::from_secs(10));
    assert_eq!(response["success"], true, "raw bash: {response:?}");
    let task_id = response["task_id"].as_str().unwrap_or_default().to_string();
    kill(aft, &task_id);
    response["output"].as_str().unwrap_or_default().to_string()
}

fn kill(aft: &mut AftProcess, task_id: &str) {
    if task_id.is_empty() {
        return;
    }
    let _ = aft.send_with_timeout(
        &json!({
            "id": format!("kill-{task_id}"),
            "command": "bash_kill",
            "session_id": SESSION,
            "task_id": task_id,
        })
        .to_string(),
        Duration::from_secs(10),
    );
}

fn assert_worker_hand_off(label: &str, text: &str) {
    assert!(
        text.contains("Background task started") && text.contains("won't wake you"),
        "{label}: a worker must be told the task won't wake it: {text:?}"
    );
    assert!(
        !text.contains("completion reminder"),
        "{label}: a worker must not be promised a completion reminder: {text:?}"
    );
    // The task's own kill deadline is named, apart from any wait on it.
    assert!(
        text.contains("AFT kills this task at ")
            && text.contains("when it has run 30 minutes (its default background limit), but each wait you make on it moves that kill"),
        "{label}: a worker must be told the task's kill deadline: {text:?}"
    );
}

fn assert_primary_hand_off(label: &str, text: &str) {
    assert!(
        text.contains("A completion reminder will be delivered automatically"),
        "{label}: a primary keeps the completion-reminder text: {text:?}"
    );
    assert!(
        text.contains("AFT kills this task at ")
            && text.contains("when it has run 30 minutes (its default background limit) unless you pass a longer `timeout`"),
        "{label}: a primary must be told the task's kill deadline: {text:?}"
    );
}

#[test]
fn worker_role_reaches_the_bash_hand_off_text_over_standalone() {
    let project = tempfile::tempdir().expect("worker session project");
    let mut aft = AftProcess::spawn();
    aft.configure(project.path());

    assert_worker_hand_off(
        "raw bash",
        &raw_bash_launch_text(&mut aft, project.path(), true),
    );
    assert_primary_hand_off(
        "raw bash",
        &raw_bash_launch_text(&mut aft, project.path(), false),
    );
    assert!(aft.shutdown().success());
}

/// Worker wait limit used by the tests below: `bash.worker_wait_max_ms`
/// refuses values under a minute, so the engine process gets the test-only
/// override instead.
const WORKER_LIMIT_MS: &str = "1500";

fn spawn_with_worker_limit() -> AftProcess {
    AftProcess::spawn_with_env(&[(
        "AFT_TEST_WORKER_WAIT_MAX_MS",
        std::ffi::OsStr::new(WORKER_LIMIT_MS),
    )])
}

/// A foreground `bash` request as the plugins send it, with the role beside
/// `session_id` for a worker.
fn foreground_bash(
    aft: &mut AftProcess,
    id: &str,
    params: Value,
    worker: bool,
) -> (Value, Duration) {
    let mut request = json!({
        "id": id,
        "command": "bash",
        "session_id": SESSION,
        "params": params,
    });
    if worker {
        request["worker_session"] = json!(true);
    }
    let started = std::time::Instant::now();
    let response = aft.send_with_timeout(&request.to_string(), Duration::from_secs(20));
    (response, started.elapsed())
}

fn status_of(aft: &mut AftProcess, task_id: &str) -> Value {
    aft.send_with_timeout(
        &json!({
            "id": format!("status-{task_id}"),
            "command": "bash_status",
            "session_id": SESSION,
            "worker_session": true,
            "task_id": task_id,
        })
        .to_string(),
        Duration::from_secs(10),
    )
}

/// The reply a caller gets when its blocking call reaches the configured wait
/// limit: the call returns, the command keeps running in the background (it
/// is not killed), and the reply names the task, how long it ran and its
/// output so far.
fn assert_detached_at_limit(
    aft: &mut AftProcess,
    label: &str,
    response: &Value,
    elapsed: Duration,
) {
    assert_eq!(response["success"], true, "{label}: {response:?}");
    assert_eq!(response["status"], "running", "{label}: {response:?}");
    assert!(
        elapsed >= Duration::from_millis(1_400) && elapsed < Duration::from_secs(15),
        "{label}: the call must return at the 1.5s limit, took {elapsed:?}"
    );
    let task_id = response["task_id"].as_str().expect("task id").to_string();
    let text = response["output"].as_str().unwrap_or_default();
    assert!(
        text.contains("still running after 1.5s"),
        "{label}: {text:?}"
    );
    assert!(text.contains(&task_id), "{label}: {text:?}");
    assert!(text.contains("it was not killed"), "{label}: {text:?}");
    assert!(text.contains("It has run for"), "{label}: {text:?}");
    assert!(
        text.contains("Recent output:\nstarted"),
        "{label}: {text:?}"
    );
    assert!(text.contains("bash_kill"), "{label}: {text:?}");
    assert!(!text.contains("completion reminder"), "{label}: {text:?}");
    assert!(
        text.contains("AFT kills this task at ")
            && text.contains("when it has run 30 minutes (its default background limit)")
            && text.contains("remain."),
        "{label}: the reply names the task's own kill deadline: {text:?}"
    );
    let status = status_of(aft, &task_id);
    assert_eq!(
        status["status"], "running",
        "{label}: the command keeps running after the call returns: {status:?}"
    );
    kill(aft, &task_id);
}

/// A delegated worker's `wait: true` call without a timeout returns at the
/// worker wait limit instead of blocking until the command ends: a stuck test
/// run once held a worker this way for fifteen hours.
#[test]
fn worker_wait_detaches_at_the_worker_wait_limit_and_keeps_running() {
    let project = tempfile::tempdir().expect("worker wait project");
    let mut aft = spawn_with_worker_limit();
    aft.configure(project.path());
    let (response, elapsed) = foreground_bash(
        &mut aft,
        "worker-wait-cap",
        json!({
            "command": "printf 'started\\n'; sleep 30",
            "workdir": project.path(),
            "foreground_orchestrate": true,
            "wait": true,
            "compressed": false,
        }),
        true,
    );
    assert_detached_at_limit(&mut aft, "wait:true", &response, elapsed);
    assert!(aft.shutdown().success());
}

/// The same limit bounds a worker's plain foreground call that the plugin
/// marks `block_to_completion` (every worker foreground call when
/// `bash.subagent_background` is false), which is never auto-promoted.
#[test]
fn worker_blocking_foreground_detaches_at_the_worker_wait_limit() {
    let project = tempfile::tempdir().expect("worker block project");
    let mut aft = spawn_with_worker_limit();
    aft.configure(project.path());
    let (response, elapsed) = foreground_bash(
        &mut aft,
        "worker-block-cap",
        json!({
            "command": "printf 'started\\n'; sleep 30",
            "workdir": project.path(),
            "foreground_orchestrate": true,
            "block_to_completion": true,
            "compressed": false,
        }),
        true,
    );
    assert_detached_at_limit(&mut aft, "block_to_completion", &response, elapsed);
    assert!(aft.shutdown().success());
}

/// A worker's command that finishes before the limit returns its result.
#[test]
fn worker_wait_returns_the_result_before_the_limit() {
    let project = tempfile::tempdir().expect("worker fast project");
    let mut aft = spawn_with_worker_limit();
    aft.configure(project.path());
    let (response, _) = foreground_bash(
        &mut aft,
        "worker-wait-fast",
        json!({
            "command": "sleep 0.2; printf 'done\\n'",
            "workdir": project.path(),
            "foreground_orchestrate": true,
            "wait": true,
            "compressed": false,
        }),
        true,
    );
    assert_eq!(response["success"], true, "{response:?}");
    assert_eq!(response["status"], "completed", "{response:?}");
    assert!(response["output"]
        .as_str()
        .unwrap_or_default()
        .contains("done"));
    assert!(aft.shutdown().success());
}

/// Head blocking calls share the worker wait limit without killing the task.
#[test]
fn head_wait_detaches_at_the_configured_wait_limit_and_keeps_running() {
    let project = tempfile::tempdir().expect("primary wait project");
    let mut aft = spawn_with_worker_limit();
    aft.configure(project.path());
    let (response, elapsed) = foreground_bash(
        &mut aft,
        "primary-wait",
        json!({
            "command": "printf 'started\\n'; sleep 30",
            "workdir": project.path(),
            "foreground_orchestrate": true,
            "wait": true,
            "compressed": false,
        }),
        false,
    );
    assert_detached_at_limit(&mut aft, "head wait:true", &response, elapsed);
    assert!(aft.shutdown().success());
}

#[test]
fn head_blocking_foreground_detaches_at_the_configured_wait_limit() {
    let project = tempfile::tempdir().expect("head block project");
    let mut aft = spawn_with_worker_limit();
    aft.configure(project.path());
    let (response, elapsed) = foreground_bash(
        &mut aft,
        "head-block-cap",
        json!({
            "command": "printf 'started\\n'; sleep 30",
            "workdir": project.path(),
            "foreground_orchestrate": true,
            "block_to_completion": true,
            "compressed": false,
        }),
        false,
    );
    assert_detached_at_limit(&mut aft, "head block_to_completion", &response, elapsed);
    assert!(aft.shutdown().success());
}

#[test]
fn head_wait_returns_the_result_before_the_limit() {
    let project = tempfile::tempdir().expect("head fast project");
    let mut aft = spawn_with_worker_limit();
    aft.configure(project.path());
    let (response, _) = foreground_bash(
        &mut aft,
        "head-wait-fast",
        json!({
            "command": "sleep 0.2; printf 'head-done\\n'",
            "workdir": project.path(),
            "foreground_orchestrate": true,
            "wait": true,
            "compressed": false,
        }),
        false,
    );
    assert_eq!(response["status"], "completed", "{response:?}");
    assert_eq!(response["exit_code"], 0, "{response:?}");
    assert!(response["output"]
        .as_str()
        .unwrap_or_default()
        .contains("head-done"));
    assert!(aft.shutdown().success());
}
