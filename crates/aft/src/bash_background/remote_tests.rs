use super::*;
use crate::bash_background::persistence::{read_task, write_task};
use crate::exec_remote::wire_tests::{daemon, id, Script};
fn reattach_budgets() -> &'static Mutex<HashMap<PathBuf, Duration>> {
    static BUDGETS: std::sync::OnceLock<Mutex<HashMap<PathBuf, Duration>>> =
        std::sync::OnceLock::new();
    BUDGETS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) fn take_reattach_budget(root: &Path) -> Option<Duration> {
    reattach_budgets().lock().unwrap().remove(root)
}

fn shorten_reattach_budget(root: &Path) {
    // Scope the hook to one worker, so parallel remote tests keep the real budget.
    // Empty attaches are 100 ms apart, so 1.5 s leaves room for several even on
    // a loaded runner. Tests bound the whole recovery at 30 s, which only
    // catches a budget that never expires.
    reattach_budgets()
        .lock()
        .unwrap()
        .insert(root.into(), Duration::from_millis(1_500));
}

fn crash_tasks() -> &'static Mutex<HashSet<String>> {
    static TASKS: std::sync::OnceLock<Mutex<HashSet<String>>> = std::sync::OnceLock::new();
    TASKS.get_or_init(|| Mutex::new(HashSet::new()))
}
pub(super) fn simulate_crash_before_local_pid(task: &str) -> bool {
    crash_tasks().lock().unwrap().remove(task)
}

fn post_spawn_failures() -> &'static Mutex<HashSet<String>> {
    static TASKS: std::sync::OnceLock<Mutex<HashSet<String>>> = std::sync::OnceLock::new();
    TASKS.get_or_init(|| Mutex::new(HashSet::new()))
}
pub(super) fn fail_running_metadata_after_spawn(task: &str) -> Result<(), String> {
    if post_spawn_failures().lock().unwrap().remove(task) {
        Err("injected running-metadata failure after spawn".into())
    } else {
        Ok(())
    }
}

struct MarkerGate {
    entered: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}
fn marker_gates() -> &'static Mutex<HashMap<String, MarkerGate>> {
    static GATES: std::sync::OnceLock<Mutex<HashMap<String, MarkerGate>>> =
        std::sync::OnceLock::new();
    GATES.get_or_init(|| Mutex::new(HashMap::new()))
}
pub(super) fn after_local_fallback_marker(task: &str) {
    if let Some(gate) = marker_gates().lock().unwrap().remove(task) {
        gate.entered.send(()).unwrap();
        gate.release.recv_timeout(Duration::from_secs(10)).unwrap();
    }
}

fn registry() -> BgTaskRegistry {
    BgTaskRegistry::new(Arc::new(Mutex::new(None)))
}

pub(crate) fn launch(connection: PathBuf) -> crate::bash_background::RemoteLaunch {
    crate::bash_background::RemoteLaunch {
        explicit_runon: false,
        connection_file: Some(connection),
        harness: "broca".into(),
        session: "session".into(),
        params: exec::FrozenParams {
            remote_exec: Some(exec::policy::RemoteExecPolicy {
                enabled: true,
                ..Default::default()
            }),
            ..Default::default()
        },
    }
}

fn start(registry: &BgTaskRegistry, dir: &Path, connection: PathBuf) -> String {
    ensure_git_fixture(dir);
    registry
        .spawn_remote(
            launch(connection),
            SpawnPlan::Unsandboxed,
            "printf local-proof",
            resolve_posix_shell(),
            "session".into(),
            dir.into(),
            HashMap::from([("BUILD_REMOTE_ENV_TEST".into(), "environment-proof".into())]),
            crate::bash_background::HardKill::After(Duration::from_secs(30)),
            dir.into(),
            crate::bash_background::TaskSlot::Background { max: 10 },
            true,
            false,
            Some(dir.into()),
        )
        .unwrap()
}

async fn terminal(registry: &BgTaskRegistry, task: &str) -> BgTaskSnapshot {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let snapshot = registry.observed_status(task, "session", 8192).unwrap();
            if snapshot.info.status.is_terminal() {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fake executor must terminate")
}

#[tokio::test]
async fn exec_remote_bash_background_slots_are_shared_with_local_tasks() {
    use crate::bash_background::{HardKill, TaskSlot};

    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let local = registry
        .spawn_with_shell(
            SpawnPlan::Unsandboxed,
            "sleep 30 # local-slot-holder",
            crate::bash_background::BashShell::Bash,
            resolve_posix_shell(),
            "session".into(),
            dir.path().into(),
            HashMap::new(),
            HardKill::After(Duration::from_secs(60)),
            dir.path().into(),
            1,
            false,
            false,
            Some(dir.path().into()),
        )
        .unwrap();
    let daemon = daemon(Script::Cancel, "exec-remote/v1").await;
    let remote_spawn = |slot| {
        let mut launch = launch(daemon.connection.clone());
        launch.explicit_runon = true;
        registry.spawn_remote(
            launch,
            SpawnPlan::Unsandboxed,
            "printf remote-slot-proof",
            resolve_posix_shell(),
            "session".into(),
            dir.path().into(),
            HashMap::new(),
            HardKill::After(Duration::from_secs(60)),
            dir.path().into(),
            slot,
            false,
            false,
            Some(dir.path().into()),
        )
    };
    // Remote background work uses the same project-local budget, whereas a
    // remote foreground call is admitted even when local work fills it.
    let refusal = remote_spawn(TaskSlot::Background { max: 1 }).unwrap_err();
    assert!(refusal.contains(&local), "{refusal}");
    assert!(refusal.contains("local-slot-holder"), "{refusal}");
    let foreground = remote_spawn(TaskSlot::Foreground).unwrap();
    registry.kill(&local, "session").unwrap();
    registry
        .check_background_slot(TaskSlot::Background { max: 1 }, "session")
        .expect("a waiting remote foreground command holds no slot");
    registry.promote(&foreground, "session").unwrap();
    let refusal = registry
        .check_background_slot(TaskSlot::Background { max: 1 }, "session")
        .unwrap_err();
    assert!(refusal.contains(&foreground), "{refusal}");
    assert!(refusal.contains("remote-slot-proof"), "{refusal}");
    // Wait for executor acceptance before cancellation, and leave the async
    // test runtime free to serve the fake executor's cancel response.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if registry
                .task(&foreground)
                .unwrap()
                .state
                .lock()
                .unwrap()
                .metadata
                .remote
                .as_ref()
                .unwrap()
                .job_id
                .is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fake executor must accept the foreground command");
    let killing = registry.clone();
    let kill_task = foreground.clone();
    tokio::task::spawn_blocking(move || killing.kill(&kill_task, "session").unwrap())
        .await
        .unwrap();
    let done = terminal(&registry, &foreground).await;
    assert_eq!(done.info.status, BgTaskStatus::Killed);
}

#[tokio::test]
async fn exec_remote_bash_refusal_runs_local_with_disclosure() {
    for (script, reason) in [
        (Script::Refused, "future_refusal"),
        (Script::KnownRefused, "unreachable"),
        (Script::WorkspaceSetupRefused, "workspace_setup_failed"),
    ] {
        let daemon = daemon(script, "exec-remote/v1").await;
        let dir = tempfile::tempdir().unwrap();
        let registry = registry();
        let task = start(&registry, dir.path(), daemon.connection.clone());
        let done = terminal(&registry, &task).await;
        assert_eq!(done.exit_code, Some(0));
        assert!(
            done.output_preview.contains("local-proof"),
            "{}",
            done.output_preview
        );
        assert_eq!(
            done.output_preview.lines().next(),
            Some(
                format!(
                    "ran locally on {}: remote refused: {reason}",
                    local_os_name()
                )
                .as_str()
            ),
            "{}",
            done.output_preview
        );
        let log = daemon.log.lock().unwrap();
        let request = log.iter().find(|(_, b)| b["method"] == "exec.run").unwrap();
        assert_eq!(
            request.1["params"]["env"]["BUILD_REMOTE_ENV_TEST"],
            "environment-proof"
        );
        assert_eq!(request.1["params"]["cwd"], dir.path().display().to_string());
    }
}

fn refusal_response(snapshot: BgTaskSnapshot) -> crate::protocol::Response {
    let now = std::time::Instant::now();
    match crate::commands::bash_orchestrate::decide_bash_step(
        snapshot, now, true, false, now, "runon",
    ) {
        crate::commands::bash_orchestrate::BashStep::Done(response) => response,
        _ => panic!("a terminal refusal must finish the foreground call"),
    }
}

fn assert_runon_refusal(response: &crate::protocol::Response, reason: &str) {
    assert!(!response.success, "{response:?}");
    assert_eq!(response.data["code"], "remote_unavailable", "{response:?}");
    assert_eq!(
        response.data["message"],
        format!("runon refused: remote refused: {reason}; command was not run; retry, or omit runon to run locally"),
        "{response:?}"
    );
}

#[test]
fn refusal_detail_is_sanitized_and_truncated_at_utf8_boundary() {
    let detail = format!("first\nsecond\t{}", "é".repeat(510));
    let rendered = rendered_refusal_reason("runner_draining", Some(&detail));
    assert_eq!(
        rendered,
        format!("runner_draining (first second {})", "é".repeat(505))
    );
    let rendered_detail = rendered
        .strip_prefix("runner_draining (")
        .unwrap()
        .strip_suffix(')')
        .unwrap();
    assert_eq!(rendered_detail.len(), 1023);
    assert!(!rendered_detail.chars().any(char::is_control));
}

#[test]
fn absent_refusal_detail_keeps_the_existing_message_exactly() {
    assert_eq!(
        runon_refusal_message("unreachable", None),
        "runon refused: remote refused: unreachable; command was not run; retry, or omit runon to run locally"
    );
}

#[tokio::test]
async fn runon_executor_refusal_never_spawns_locally_and_returns_error() {
    for (script, reason) in [
        (Script::KnownRefused, "unreachable"),
        (Script::WorkspaceSetupRefused, "workspace_setup_failed"),
        (Script::Refused, "future_refusal"),
    ] {
        let daemon = daemon(script, "exec-remote/v1").await;
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("must-not-run-locally");
        let ctx = remote_test_context(dir.path());
        let response = handle_with_policy(
            &ctx,
            Some(launch(daemon.connection.clone())),
            serde_json::json!({
                "command": format!("printf local-proof > '{}'", marker.display()),
                "runon": "linux", "compressed": false
            }),
        );
        assert!(
            response.success,
            "the asynchronous launch returns a task: {response:?}"
        );
        let task_id = response.data["task_id"].as_str().unwrap();
        let done = terminal(ctx.bash_background(), task_id).await;
        assert!(
            !marker.exists(),
            "explicit runon spawned locally after {reason}"
        );
        assert_eq!(done.child_pid, None);
        assert_eq!(done.info.status, BgTaskStatus::Failed);
        let task = ctx.bash_background().task(task_id).unwrap();
        assert!(!task.state.lock().unwrap().metadata.local_fallback_started);
        assert_runon_refusal(&refusal_response(done), reason);
        let status = crate::commands::bash_status::handle(
            &crate::protocol::RawRequest {
                id: "runon-status".into(),
                command: "bash_status".into(),
                session_id: Some("session".into()),
                lsp_hints: None,
                params: serde_json::json!({"task_id": task_id}),
            },
            &ctx,
        );
        assert_runon_refusal(&status, reason);
        assert_eq!(status.data["remote_refusal"]["code"], "remote_unavailable");
        assert_eq!(exec_runs(&daemon).len(), 1);
    }
}

#[tokio::test]
async fn runon_refusal_detail_is_rendered_and_sanitized() {
    let daemon = daemon(Script::RefusedWithDetail, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("must-not-run-locally");
    let ctx = restarted_context(dir.path());
    let response = handle_with_policy(
        &ctx,
        Some(launch(daemon.connection.clone())),
        serde_json::json!({
            "command": format!("printf local-proof > '{}'", marker.display()),
            "runon": "linux", "compressed": false
        }),
    );
    assert!(
        response.success,
        "the asynchronous launch returns a task: {response:?}"
    );
    let task_id = response.data["task_id"].as_str().unwrap();
    let done = terminal(ctx.bash_background(), task_id).await;
    assert!(!marker.exists(), "explicit runon must not start locally");
    let expected = "runon refused: remote refused: runner_draining (runner says: maintenance window); command was not run; retry, or omit runon to run locally";
    assert_eq!(refusal_response(done).data["message"], expected);
}

#[tokio::test]
async fn local_fallback_refusal_detail_is_rendered_and_sanitized() {
    let daemon = daemon(Script::RefusedWithDetail, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("local-fallback");
    let ctx = restarted_context(dir.path());
    let mut policy = launch(daemon.connection.clone());
    policy.params.remote_exec.as_mut().unwrap().legacy_commands =
        Some(serde_json::json!(["touch"]));
    let response = handle_with_policy(
        &ctx,
        Some(policy),
        serde_json::json!({
            "command": format!("touch '{}'", marker.display()),
            "compressed": false
        }),
    );
    assert!(response.success, "{response:?}");
    let done = terminal(
        ctx.bash_background(),
        response.data["task_id"].as_str().unwrap(),
    )
    .await;
    assert!(marker.exists(), "implicit routing still runs locally");
    assert!(
        done.output_preview.contains(&format!(
            "ran locally on {}: remote refused: runner_draining (runner says: maintenance window)",
            local_os_name()
        )),
        "{}",
        done.output_preview
    );
}

#[tokio::test]
async fn prefix_routing_executor_refusal_still_spawns_locally_with_advisory() {
    for (script, reason) in [
        (Script::KnownRefused, "unreachable"),
        (Script::WorkspaceSetupRefused, "workspace_setup_failed"),
        (Script::Refused, "future_refusal"),
    ] {
        let daemon = daemon(script, "exec-remote/v1").await;
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("local-fallback");
        let ctx = remote_test_context(dir.path());
        let mut policy = launch(daemon.connection.clone());
        policy.params.remote_exec.as_mut().unwrap().legacy_commands =
            Some(serde_json::json!(["touch"]));
        let response = handle_with_policy(
            &ctx,
            Some(policy),
            serde_json::json!({
                "command": format!("touch '{}'", marker.display()),
                "compressed": false
            }),
        );
        assert!(response.success, "{response:?}");
        let done = terminal(
            ctx.bash_background(),
            response.data["task_id"].as_str().unwrap(),
        )
        .await;
        assert!(
            marker.exists(),
            "prefix routing must run locally after {reason}"
        );
        assert_eq!(done.exit_code, Some(0));
        assert_eq!(
            exec_runs(&daemon).len(),
            1,
            "the prefix must actually route remotely"
        );
        let status_request = crate::protocol::RawRequest {
            id: "prefix-status".into(),
            command: "bash_status".into(),
            session_id: Some("session".into()),
            lsp_hints: None,
            params: serde_json::json!({"task_id": response.data["task_id"]}),
        };
        let status = crate::commands::bash_status::handle(&status_request, &ctx);
        assert!(status.success, "{status:?}");
        assert!(
            status.data["output_preview"]
                .as_str()
                .unwrap_or_default()
                .contains(&format!(
                    "ran locally on {}: remote refused: {reason}",
                    local_os_name()
                )),
            "tool status: {status:?}; execution note: {:?}",
            ctx.bash_background()
                .execution_note(response.data["task_id"].as_str().unwrap(), "session")
        );
        assert!(refusal_response(done).success);
        assert_eq!(exec_runs(&daemon).len(), 1);
    }
}

#[tokio::test]
async fn exec_remote_bash_unknown_never_reruns() {
    for script in [Script::Lost, Script::FutureOutcome, Script::Expired] {
        let daemon = daemon(script, "exec-remote/v1").await;
        let dir = tempfile::tempdir().unwrap();
        let registry = registry();
        let task = start(&registry, dir.path(), daemon.connection.clone());
        let done = terminal(&registry, &task).await;
        assert_eq!(done.info.status, BgTaskStatus::FateUnknown);
        assert!(
            !done.output_preview.contains("local-proof"),
            "unknown must not run locally"
        );
        assert!(
            done.output_preview.contains("unknown")
                || done.output_preview.contains("history_expired")
        );
        assert_eq!(
            daemon
                .log
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, b)| b["method"] == "exec.run")
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn exec_remote_bash_loss_reattaches_without_resubmission() {
    let daemon = daemon(Script::Restart, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let task = start(&registry, dir.path(), daemon.connection.clone());
    let done = terminal(&registry, &task).await;
    assert!(done.output_preview.contains("ABCD"));
    let log = daemon.log.lock().unwrap();
    assert_eq!(
        log.iter()
            .filter(|(_, b)| b["method"] == "exec.run")
            .count(),
        1
    );
    assert_eq!(
        log.iter()
            .find(|(_, b)| b["method"] == "exec.attach")
            .unwrap()
            .1["params"]["from_seq"],
        2
    );
}

#[tokio::test]
async fn exec_remote_bash_empty_attach_budget_reports_job_without_rerun() {
    let daemon = daemon(Script::MissingTerminal, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    shorten_reattach_budget(dir.path());
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    let done = tokio::time::timeout(Duration::from_secs(30), terminal(&registry, &task_id))
        .await
        .expect("empty attaches must exhaust the recovery budget");
    assert_eq!(done.info.status, BgTaskStatus::FateUnknown);
    assert_eq!(
        done.info.status_reason.as_deref(),
        Some(format!("remote outcome unknown: job {} could not be re-attached for 5 minutes; command not rerun", id()).as_str())
    );
    assert!(!done.output_preview.contains("local-proof"));
    let task = registry.task(&task_id).unwrap();
    let durable = crate::bash_background::persistence::read_task(&task.paths.json).unwrap();
    assert_eq!(durable.status, BgTaskStatus::FateUnknown);
    assert_eq!(durable.status_reason, done.info.status_reason);
    let log = daemon.log.lock().unwrap();
    assert!(
        log.iter()
            .filter(|(_, b)| b["method"] == "exec.attach")
            .count()
            >= 2
    );
    assert_eq!(
        log.iter()
            .filter(|(_, b)| b["method"] == "exec.run")
            .count(),
        1
    );
}

#[tokio::test]
async fn exec_remote_bash_gapped_attach_records_reset_budget_and_complete() {
    let daemon = daemon(
        Script::GappedAttach(Duration::from_millis(500)),
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    shorten_reattach_budget(dir.path());
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    let task = registry.task(&task_id).unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while task
            .state
            .lock()
            .unwrap()
            .metadata
            .remote
            .as_ref()
            .unwrap()
            .last_seq
            .is_none()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert_eq!(
        registry
            .observed_status(&task_id, "session", 8192)
            .unwrap()
            .info
            .status,
        BgTaskStatus::Running,
        "an open attach is not subject to an output-idle deadline"
    );
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Completed);
    assert_eq!(fs::read(&task.paths.stdout).unwrap(), b"ABC");
    let log = daemon.log.lock().unwrap();
    let from: Vec<_> = log
        .iter()
        .filter(|(_, b)| b["method"] == "exec.attach")
        .map(|(_, b)| b["params"]["from_seq"].as_u64().unwrap())
        .collect();
    assert_eq!(from, [0, 1, 2]);
    assert_eq!(
        log.iter()
            .filter(|(_, b)| b["method"] == "exec.run")
            .count(),
        1
    );
}

#[tokio::test]
async fn exec_remote_bash_persistent_terminal_gap_is_bounded() {
    let daemon = daemon(Script::PersistentTerminalGap, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Failed);
    assert_eq!(done.exit_code, Some(7));
    let task = registry.task(&task_id).unwrap();
    assert_eq!(fs::read(&task.paths.stdout).unwrap(), b"BC");
    let durable = read_task(&task.paths.json).unwrap();
    assert_eq!(durable.incomplete_output, [(0, 0)]);
    assert!(durable
        .execution_note
        .unwrap()
        .contains("output lost between seq 0 and 0: the executor never delivered it"));
    let log = daemon.log.lock().unwrap();
    let from: Vec<_> = log
        .iter()
        .filter(|(_, b)| b["method"] == "exec.attach")
        .map(|(_, b)| b["params"]["from_seq"].as_u64().unwrap())
        .collect();
    assert_eq!(from, [0, 0, 0]);
    assert_eq!(
        log.iter()
            .filter(|(_, b)| b["method"] == "exec.run")
            .count(),
        1
    );
}

#[tokio::test]
async fn exec_remote_bash_transient_terminal_gap_recovers_without_loss() {
    let daemon = daemon(Script::TransientTerminalGap, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Failed);
    assert_eq!(done.exit_code, Some(7));
    let task = registry.task(&task_id).unwrap();
    assert_eq!(fs::read(&task.paths.stdout).unwrap(), b"ABC");
    let durable = read_task(&task.paths.json).unwrap();
    assert!(durable.incomplete_output.is_empty());
    assert!(!durable
        .execution_note
        .unwrap_or_default()
        .contains("output lost"));
    let log = daemon.log.lock().unwrap();
    assert_eq!(
        log.iter()
            .filter(|(_, b)| b["method"] == "exec.attach")
            .count(),
        1
    );
    assert_eq!(
        log.iter()
            .filter(|(_, b)| b["method"] == "exec.run")
            .count(),
        1
    );
}

#[tokio::test]
async fn exec_remote_bash_reattach_budget_cancel_observes_real_terminal() {
    let daemon = daemon(
        Script::GappedCancel(Duration::from_millis(500)),
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    shorten_reattach_budget(dir.path());
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    let task = registry.task(&task_id).unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while task
            .state
            .lock()
            .unwrap()
            .metadata
            .remote
            .as_ref()
            .unwrap()
            .job_id
            .is_none()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(registry.record_remote_cancel(&task).unwrap());
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Killed);
    let state = task.state.lock().unwrap();
    let terminal = state
        .metadata
        .remote
        .as_ref()
        .unwrap()
        .terminal
        .as_ref()
        .unwrap();
    assert_eq!(terminal.outcome, Outcome::Signal { signal: 15 });
    assert_eq!(terminal.killed, Some(Killed::Cancel));
    drop(state);
    let log = daemon.log.lock().unwrap();
    let cancel = log
        .iter()
        .position(|(_, b)| b["method"] == "exec.cancel")
        .unwrap();
    let attach = log
        .iter()
        .position(|(_, b)| b["method"] == "exec.attach")
        .unwrap();
    assert!(
        cancel < attach,
        "cancel intent must be sent before attaching for its terminal"
    );
    assert_eq!(
        log.iter()
            .filter(|(_, b)| b["method"] == "exec.run")
            .count(),
        1
    );
}

#[tokio::test]
async fn exec_remote_bash_attach_call_and_reconnect_failures_exhaust_budget() {
    let daemon = daemon(Script::AttachDisconnected, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    shorten_reattach_budget(dir.path());
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    let done = tokio::time::timeout(Duration::from_secs(30), terminal(&registry, &task_id))
        .await
        .expect("failed attach calls and reconnects must exhaust the recovery budget");
    assert_eq!(done.info.status, BgTaskStatus::FateUnknown);
    assert!(done.info.status_reason.unwrap().contains(&format!(
        "job {} could not be re-attached for 5 minutes; command not rerun",
        id()
    )));
    assert!(!done.output_preview.contains("local-proof"));
    let log = daemon.log.lock().unwrap();
    assert!(log.iter().any(|(_, b)| b["method"] == "exec.attach"));
    assert_eq!(
        log.iter()
            .filter(|(_, b)| b["method"] == "exec.run")
            .count(),
        1
    );
}

#[tokio::test]
async fn exec_remote_bash_restart_connect_failures_exhaust_reattach_budget() {
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, paths) = persist_accepted_with_snapshot(
        dir.path(),
        dir.path().join("missing-connection.json"),
        "printf local-proof",
    );
    shorten_reattach_budget(dir.path());
    registry.resume_remote_task(&task_id).unwrap();
    let done = tokio::time::timeout(Duration::from_secs(30), terminal(&registry, &task_id))
        .await
        .expect("restarting an accepted job cannot retry connect forever");
    assert_eq!(done.info.status, BgTaskStatus::FateUnknown);
    assert!(done.info.status_reason.unwrap().contains(&format!(
        "job {} could not be re-attached for 5 minutes; command not rerun",
        id()
    )));
    assert_eq!(fs::read(&paths.stdout).unwrap(), b"");
}

#[tokio::test]
async fn exec_remote_bash_raw_utf8_pipeline_and_workspace_changes() {
    let daemon = daemon(Script::Utf8, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let task = start(&registry, dir.path(), daemon.connection.clone());
    let done = terminal(&registry, &task).await;
    let task = registry.task(&task).unwrap();
    assert_eq!(fs::read(&task.paths.stdout).unwrap(), "€".as_bytes());
    assert_eq!(fs::read(&task.paths.stderr).unwrap(), "😀".as_bytes());
    assert_eq!(
        fs::read_to_string(&task.paths.pipeline_status).unwrap(),
        "3 0\n"
    );
    assert_eq!(
        done.output_preview.lines().next(),
        Some("ran remotely on ck-motor")
    );
    // The changed file is named after the output, and it was not copied back.
    assert!(
        done.output_preview.ends_with(
            "These files changed on the server and were NOT copied back:\n  generated.txt\n\
             git state: not reported by the runner\n\
             untracked files: not reported by the runner\n\
             ignored writes: not reported by the runner"
        ),
        "{}",
        done.output_preview
    );
    assert!(!dir.path().join("generated.txt").exists());
}

#[tokio::test]
async fn exec_remote_bash_discloses_only_names_aft_stripped() {
    let daemon = daemon(Script::Utf8, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    ensure_git_fixture(dir.path());
    let registry = registry();
    let env = HashMap::from([
        ("BUILD_LABEL".into(), "ordinary-build".into()),
        ("SHELL_SECRET".into(), "secret-fixture-value".into()),
        ("AWS_REGION".into(), "not-off-host".into()),
        ("AFT_CONTROL_PATH".into(), "private-control-value".into()),
        ("NEXTEST_TEST_THREADS".into(), "5".into()),
        ("RUST_TEST_THREADS".into(), "6".into()),
    ]);
    let task_id = registry
        .spawn_remote(
            launch(daemon.connection.clone()),
            SpawnPlan::Unsandboxed,
            "printf unused",
            resolve_posix_shell(),
            "session".into(),
            dir.path().into(),
            env,
            crate::bash_background::HardKill::After(Duration::from_secs(30)),
            dir.path().into(),
            crate::bash_background::TaskSlot::Background { max: 10 },
            true,
            false,
            Some(dir.path().into()),
        )
        .unwrap();
    let done = terminal(&registry, &task_id).await;
    let log = daemon.log.lock().unwrap();
    let request = &log
        .iter()
        .find(|(_, body)| body["method"] == "exec.run")
        .unwrap()
        .1;
    assert!(request["params"]["env"].get("SHELL_SECRET").is_none());
    assert!(request["params"]["env"].get("AWS_REGION").is_none());
    assert!(request["params"]["env"].get("AFT_CONTROL_PATH").is_none());
    assert!(request["params"]["env"]
        .get("NEXTEST_TEST_THREADS")
        .is_none());
    assert!(request["params"]["env"].get("RUST_TEST_THREADS").is_none());
    assert_eq!(request["params"]["env"]["BUILD_LABEL"], "ordinary-build");
    assert!(
        done.output_preview.contains("AFT stripped env names:"),
        "{done:?}"
    );
    for name in [
        "SHELL_SECRET",
        "AWS_REGION",
        "AFT_CONTROL_PATH",
        "NEXTEST_TEST_THREADS",
        "RUST_TEST_THREADS",
    ] {
        assert!(done.output_preview.contains(name), "{done:?}");
    }
    for value in [
        "secret-fixture-value",
        "not-off-host",
        "private-control-value",
    ] {
        assert!(!done.output_preview.contains(value));
        assert!(!request.to_string().contains(value));
    }
    assert!(!done
        .output_preview
        .contains("the remote job does not receive"));
}

#[tokio::test]
async fn exec_remote_bash_executor_env_disclosure_survives_reattach_and_caps_names() {
    let StreamRecord::Accepted(base) =
        serde_json::from_str(include_str!("../exec_remote/fixtures/frames/accepted.json")).unwrap()
    else {
        panic!("accepted vector")
    };
    let names: Vec<String> = (0..13).map(|n| format!("VAR_{n:02}")).collect();
    for dropped in [base.env_not_forwarded.clone(), None, Some(names.clone())] {
        let daemon = daemon(Script::Utf8, "exec-remote/v1").await;
        let dir = tempfile::tempdir().unwrap();
        let (original, task_id, paths) = persist_accepted_with_snapshot(
            dir.path(),
            daemon.connection.clone(),
            "printf must-not-run",
        );
        let task = original.task(&task_id).unwrap();
        let mut sink = TaskSink::new(&original, task).unwrap();
        let mut accepted = base.clone();
        accepted.env_not_forwarded = dropped.clone();
        sink.accepted(&accepted).unwrap();
        drop(sink);
        drop(original);
        let restarted = registry();
        restarted.replay_session(dir.path(), "session").unwrap();
        let done = terminal(&restarted, &task_id).await;
        let line = "the remote job does not receive:";
        if dropped.as_ref().is_some_and(|names| !names.is_empty()) {
            assert!(
                done.output_preview
                    .contains(&format!("{line} {}; +3 more", names[..10].join(", "))),
                "{done:?}"
            );
            assert!(!done.output_preview.contains("VAR_10"));
        } else {
            assert!(!done.output_preview.contains(line), "{done:?}");
        }
        assert!(!done.output_preview.contains("all forwarded"));
        let json: serde_json::Value =
            serde_json::from_slice(&fs::read(&paths.json).unwrap()).unwrap();
        assert_eq!(
            json["remote"]["env_not_forwarded"],
            serde_json::to_value(&dropped).unwrap()
        );
        let replay = registry();
        replay.replay_session(dir.path(), "session").unwrap();
        let done = terminal(&replay, &task_id).await;
        assert_eq!(
            done.output_preview.contains(line),
            dropped.as_ref().is_some_and(|names| !names.is_empty())
        );
        assert!(!daemon
            .log
            .lock()
            .unwrap()
            .iter()
            .any(|(_, b)| b["method"] == "exec.run"));
    }
}

#[tokio::test]
async fn exec_remote_bash_kill_sends_cancel_and_reports_terminal() {
    let daemon = daemon(Script::Cancel, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let task = start(&registry, dir.path(), daemon.connection.clone());
    let accepted = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if registry
                .task(&task)
                .unwrap()
                .state
                .lock()
                .unwrap()
                .metadata
                .remote
                .as_ref()
                .unwrap()
                .job_id
                .is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    accepted.unwrap();
    let killing = registry.clone();
    let kill_task = task.clone();
    let done = tokio::task::spawn_blocking(move || killing.kill(&kill_task, "session").unwrap())
        .await
        .unwrap();
    assert_eq!(done.info.status, BgTaskStatus::Killed);
    assert!(done.info.status_reason.unwrap().contains("cancel"));
    assert!(daemon
        .log
        .lock()
        .unwrap()
        .iter()
        .any(|(_, b)| b["method"] == "exec.cancel"));
}

#[tokio::test]
async fn exec_remote_bash_continuous_output_cannot_starve_cancel() {
    let daemon = daemon(Script::Continuous, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    let task = registry.task(&task_id).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if task
                .state
                .lock()
                .unwrap()
                .metadata
                .remote
                .as_ref()
                .unwrap()
                .last_seq
                .is_some_and(|s| s >= 10)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(registry.record_remote_cancel(&task).unwrap());
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if daemon
                .log
                .lock()
                .unwrap()
                .iter()
                .any(|(_, body)| body["method"] == "exec.cancel")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("steady output must not defer exec.cancel until a quiet period");
    assert_eq!(
        terminal(&registry, &task_id).await.info.status,
        BgTaskStatus::Killed
    );
}

#[tokio::test]
async fn exec_remote_launch_is_fenced_by_a_refused_startup_receipt() {
    // The bash reply deadline refused this startup and told the caller the
    // command was not started; the remote launch must then create nothing:
    // no registered task, no task record, no remote request or local fallback.
    let daemon = daemon(Script::Refused, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let receipt = Arc::new(crate::bash_background::SpawnReceipt::new(
        std::time::Instant::now() + Duration::from_secs(600),
    ));
    assert_eq!(
        receipt.expire(),
        crate::bash_background::StartupOutcome::Refused
    );
    let result = crate::bash_background::with_spawn_receipt(receipt, || {
        registry.spawn_remote(
            launch(daemon.connection.clone()),
            SpawnPlan::Unsandboxed,
            "printf local-proof > remote-fence-probe",
            resolve_posix_shell(),
            "session".into(),
            dir.path().into(),
            HashMap::new(),
            crate::bash_background::HardKill::After(Duration::from_secs(30)),
            dir.path().into(),
            crate::bash_background::TaskSlot::Background { max: 10 },
            true,
            false,
            Some(dir.path().into()),
        )
    });
    let error = result.expect_err("a refused receipt must refuse the remote launch");
    assert!(error.contains("startup deadline expired"), "{error}");
    assert!(registry.list(0).is_empty());
    let (ids, _) = discover_task_ids(&session_tasks_dir(dir.path(), "session")).unwrap_or_default();
    assert!(
        ids.is_empty(),
        "refused remote launch left records: {ids:?}"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!dir.path().join("remote-fence-probe").exists());
    assert!(
        daemon.log.lock().unwrap().is_empty(),
        "no remote request may be sent"
    );
}

#[tokio::test]
async fn exec_remote_bash_deadline_is_not_cancel() {
    let daemon = daemon(Script::Deadline, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let task = start(&registry, dir.path(), daemon.connection.clone());
    let done = terminal(&registry, &task).await;
    assert_eq!(done.info.status, BgTaskStatus::TimedOut);
    assert!(done.info.status_reason.unwrap().contains("deadline"));
    assert!(!daemon
        .log
        .lock()
        .unwrap()
        .iter()
        .any(|(_, b)| b["method"] == "exec.cancel"));
}

#[tokio::test]
async fn exec_remote_bash_restart_uses_persisted_seq_without_duplicates_or_gaps() {
    let daemon = daemon(Script::Restart, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let original = registry();
    let db = Arc::new(Mutex::new(
        crate::db::open(&dir.path().join("aft.db")).unwrap(),
    ));
    original.set_harness(crate::harness::Harness::Runner);
    original.set_db_pool(db.clone());
    let layout = allocate_task_layout(dir.path(), "session").unwrap();
    let task_id = layout.paths.task_id.clone();
    let mut metadata = PersistedTask::starting(
        task_id.clone(),
        "session".into(),
        "printf local-proof".into(),
        dir.path().into(),
        Some(dir.path().into()),
        Some(30000),
        true,
        false,
    );
    metadata.status = BgTaskStatus::Running;
    metadata.harness = Some("runner".into());
    metadata.execution_note = Some("ran remotely on ck-motor".into());
    metadata.remote = Some(RemoteTask {
        explicit_runon: false,
        connection_file: Some(daemon.connection.clone()),
        harness: "broca".into(),
        session: "session".into(),
        job_id: None,
        last_seq: None,
        gap_recovery: None,
        undelivered_output: Vec::new(),
        stdout_len: 0,
        stderr_len: 0,
        unknown_len: 0,
        cancel_requested: false,
        terminal: None,
        fallback_digest: None,
        env_not_forwarded: None,
        queue_position: None,
        started: false,
    });
    let handles = TaskIoHandles::create(&layout, BgMode::Pipes, true).unwrap();
    write_task_at(&layout, &metadata).unwrap();
    original.dual_write_task(&layout.paths, &metadata);
    original
        .insert_rehydrated_task(metadata, layout.paths.clone(), false)
        .unwrap();
    let task = original.task(&task_id).unwrap();
    task.state.lock().unwrap().io_handles = Some(handles);
    let mut sink = TaskSink::new(&original, task.clone()).unwrap();
    let mut consumer = exec::StreamConsumer::new();
    consumer
        .consume(StreamRecord::Accepted(Accepted::new(id(), 1)), &mut sink)
        .unwrap();
    for (seq, b) in [(0, b'A'), (1, b'B')] {
        consumer
            .consume(
                StreamRecord::Output(Output::new(seq, OutputStream::Stdout, BytePayload(vec![b]))),
                &mut sink,
            )
            .unwrap();
    }
    // A crash can leave bytes beyond the durable cursor; recovery truncates
    // them before attaching rather than replaying into duplicated output.
    sink.stdout.write_all(b"uncommitted").unwrap();
    drop(sink);
    drop(consumer);
    original.inner.shutdown.store(true, Ordering::SeqCst);
    drop(task);
    drop(original);
    let restarted = registry();
    restarted.set_harness(crate::harness::Harness::Runner);
    restarted.set_db_pool(db);
    restarted.replay_session(dir.path(), "session").unwrap();
    let done = terminal(&restarted, &task_id).await;
    assert!(done.output_preview.contains("ABCD"));
    let task = restarted.task(&task_id).unwrap();
    assert_eq!(fs::read(&task.paths.stdout).unwrap(), b"ABCD");
    let log = daemon.log.lock().unwrap();
    assert_eq!(
        log.iter()
            .find(|(_, b)| b["method"] == "exec.attach")
            .unwrap()
            .1["params"]["from_seq"],
        2
    );
    assert!(!log.iter().any(|(_, b)| b["method"] == "exec.run"));
}

#[tokio::test]
async fn exec_remote_bash_retained_output_gap_survives_terminal_and_restart() {
    let daemon = daemon(Script::RetainedGap, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let (original, task_id, paths) = persist_accepted_with_snapshot(
        dir.path(),
        daemon.connection.clone(),
        "printf must-not-run",
    );
    let task = original.task(&task_id).unwrap();
    let mut sink = TaskSink::new(&original, task).unwrap();
    sink.output(0, OutputStream::Stdout, b"A").unwrap();
    drop(sink);
    drop(original);
    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    let done = terminal(&restarted, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Completed);
    assert_eq!(fs::read(&paths.stdout).unwrap(), b"AD");
    let warning = "output lost between seq 1 and 2: the executor no longer retained it";
    assert!(done.output_preview.contains(warning), "{done:?}");
    assert_eq!(
        crate::bash_background::persistence::read_task(&paths.json)
            .unwrap()
            .incomplete_output,
        vec![(1, 2)]
    );
    let replay = registry();
    replay.replay_session(dir.path(), "session").unwrap();
    let done = terminal(&replay, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Completed);
    assert!(done.output_preview.contains(warning), "{done:?}");
    assert!(!daemon
        .log
        .lock()
        .unwrap()
        .iter()
        .any(|(_, b)| b["method"] == "exec.run"));
}

#[tokio::test]
async fn exec_remote_bash_terminal_gap_budget_and_outcome_survive_restart() {
    let daemon = daemon(Script::TerminalGapOnce, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let (original, task_id, paths) = persist_accepted_with_snapshot(
        dir.path(),
        daemon.connection.clone(),
        "printf must-not-run",
    );
    let task = original.task(&task_id).unwrap();
    let mut sink = TaskSink::new(&original, task.clone()).unwrap();
    let mut consumer = exec::StreamConsumer::resume(
        task.state
            .lock()
            .unwrap()
            .metadata
            .remote
            .as_ref()
            .unwrap()
            .point()
            .unwrap(),
    );
    for attempt in 0..3 {
        for seq in [2, 1] {
            consumer
                .consume(
                    StreamRecord::Output(Output::new(
                        seq,
                        OutputStream::Stdout,
                        BytePayload(vec![b'A' + seq as u8]),
                    )),
                    &mut sink,
                )
                .unwrap();
        }
        let Err(exec::Error::RecoveryRequired {
            resume: Some(point),
            ..
        }) = consumer.consume(
            StreamRecord::Terminal(TerminalRecord::new(
                id(),
                Outcome::Exit { code: 7 },
                1,
                0,
                0,
            )),
            &mut sink,
        )
        else {
            panic!("recovery should still be possible before the bound");
        };
        assert_eq!(point.gap_recovery.as_ref().unwrap().reattaches, attempt);
        consumer = exec::StreamConsumer::resume(point);
    }
    let durable = read_task(&paths.json).unwrap();
    let remote = durable.remote.unwrap();
    assert_eq!(remote.gap_recovery.as_ref().unwrap().reattaches, 2);
    assert!(
        remote.terminal.is_none(),
        "output must be recovered before finalization"
    );
    drop(sink);
    drop(task);
    drop(original);
    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    let done = terminal(&restarted, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Failed);
    assert_eq!(done.exit_code, Some(7));
    assert_eq!(fs::read(&paths.stdout).unwrap(), b"BC");
    let durable = read_task(&paths.json).unwrap();
    assert_eq!(durable.incomplete_output, [(0, 0)]);
    assert!(durable
        .execution_note
        .unwrap()
        .contains("output lost between seq 0 and 0: the executor never delivered it"));
    assert!(durable.remote.unwrap().gap_recovery.is_none());
    let log = daemon.log.lock().unwrap();
    assert_eq!(
        log.iter()
            .filter(|(_, b)| b["method"] == "exec.attach")
            .count(),
        1
    );
    assert!(!log.iter().any(|(_, b)| b["method"] == "exec.run"));
}

#[tokio::test]
async fn exec_remote_bash_terminal_gap_keeps_outcome_when_attach_is_unavailable() {
    let daemon = daemon(Script::AttachDisconnected, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    shorten_reattach_budget(dir.path());
    let (original, task_id, paths) = persist_accepted_with_snapshot(
        dir.path(),
        daemon.connection.clone(),
        "printf must-not-run",
    );
    let task = original.task(&task_id).unwrap();
    let mut sink = TaskSink::new(&original, task.clone()).unwrap();
    let point = task
        .state
        .lock()
        .unwrap()
        .metadata
        .remote
        .as_ref()
        .unwrap()
        .point()
        .unwrap();
    let mut consumer = exec::StreamConsumer::resume(point);
    for seq in [2, 1] {
        consumer
            .consume(
                StreamRecord::Output(Output::new(
                    seq,
                    OutputStream::Stdout,
                    BytePayload(vec![b'A' + seq as u8]),
                )),
                &mut sink,
            )
            .unwrap();
    }
    assert!(matches!(
        consumer.consume(
            StreamRecord::Terminal(TerminalRecord::new(
                id(),
                Outcome::Exit { code: 7 },
                1,
                0,
                0,
            )),
            &mut sink
        ),
        Err(exec::Error::RecoveryRequired { .. })
    ));
    drop(sink);
    drop(task);
    drop(original);
    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    let done = terminal(&restarted, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Failed);
    assert_eq!(done.exit_code, Some(7));
    assert_eq!(fs::read(&paths.stdout).unwrap(), b"BC");
    let durable = read_task(&paths.json).unwrap();
    assert_eq!(durable.incomplete_output, [(0, 0)]);
    assert!(durable.remote.unwrap().terminal.is_some());
    assert!(!daemon
        .log
        .lock()
        .unwrap()
        .iter()
        .any(|(_, b)| b["method"] == "exec.run"));
}

fn persist_accepted_with_snapshot(
    dir: &Path,
    connection: PathBuf,
    command: &str,
) -> (BgTaskRegistry, String, TaskPaths) {
    let registry = registry();
    let layout = allocate_task_layout(dir, "session").unwrap();
    let task_id = layout.paths.task_id.clone();
    let shell = resolve_posix_shell();
    let environment = BTreeMap::from([(
        std::ffi::OsString::from("ORIGINAL_PLAN"),
        std::ffi::OsString::from("host-proof"),
    )]);
    let prepared = crate::sandbox_spawn::prepare_task_payload(
        &layout,
        command.as_bytes(),
        dir,
        dir,
        &crate::sandbox_spawn::AuthenticatedPrincipal::FirstParty,
        &shell,
        &environment,
    )
    .unwrap();
    let plan = SpawnPlan::Host {
        shell_path: shell.clone(),
        environment,
    }
    .with_prepared_task(prepared);
    let digest = crate::sandbox_spawn::save_local_launch(
        &plan,
        dir,
        dir,
        &crate::sandbox_spawn::AuthenticatedPrincipal::FirstParty,
        &shell,
        false,
    )
    .unwrap();
    let mut metadata = PersistedTask::starting(
        task_id.clone(),
        "session".into(),
        command.into(),
        dir.into(),
        Some(dir.into()),
        Some(30000),
        true,
        false,
    );
    metadata.status = BgTaskStatus::Running;
    metadata.execution_note = Some("ran remotely on ck-motor".into());
    metadata.remote = Some(RemoteTask {
        explicit_runon: false,
        connection_file: Some(connection),
        harness: "runner".into(),
        session: "session".into(),
        job_id: None,
        last_seq: None,
        gap_recovery: None,
        undelivered_output: Vec::new(),
        stdout_len: 0,
        stderr_len: 0,
        unknown_len: 0,
        cancel_requested: false,
        terminal: None,
        fallback_digest: Some(digest),
        env_not_forwarded: None,
        queue_position: None,
        started: false,
    });
    let handles = TaskIoHandles::create(&layout, BgMode::Pipes, true).unwrap();
    write_task_at(&layout, &metadata).unwrap();
    registry
        .insert_rehydrated_task(metadata, layout.paths.clone(), false)
        .unwrap();
    let task = registry.task(&task_id).unwrap();
    task.state.lock().unwrap().io_handles = Some(handles);
    let mut sink = TaskSink::new(&registry, task).unwrap();
    sink.accepted(&Accepted::new(id(), 1)).unwrap();
    (registry, task_id, layout.paths)
}

fn restarted_context(dir: &Path) -> crate::context::AppContext {
    restarted_context_with_runon(dir, true)
}

fn restarted_context_with_runon(dir: &Path, enabled: bool) -> crate::context::AppContext {
    // A fresh configuration would choose ordinary unsandboxed inheritance;
    // the saved Host plan must still clear ambient HOME before its command.
    crate::context::AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(dir.into()),
            bash: crate::config::BashConfig {
                runon_enabled: enabled,
                ..Default::default()
            },
            sandbox: crate::config::SandboxConfig {
                enabled: false,
                ..Default::default()
            },
            ..Default::default()
        },
    )
}

#[tokio::test]
async fn exec_remote_bash_restart_refusal_uses_original_launch_plan() {
    let daemon = daemon(Script::AttachRefused, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let command =
        "if [ -z \"$HOME\" ]; then printf '%s' \"$ORIGINAL_PLAN\"; else printf wrong-plan; fi";
    let (original, task_id, paths) =
        persist_accepted_with_snapshot(dir.path(), daemon.connection.clone(), command);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(
                paths
                    .control_dir
                    .join(crate::sandbox_spawn::REMOTE_LOCAL_LAUNCH)
            )
            .unwrap()
            .permissions()
            .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&paths.control_dir)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
    drop(original);
    let ctx = restarted_context(dir.path());
    let restarted = ctx.bash_background();
    restarted.replay_session(dir.path(), "session").unwrap();
    let done = terminal(restarted, &task_id).await;
    assert_eq!(fs::read(&paths.stdout).unwrap(), b"host-proof");
    assert!(done.output_preview.contains(&format!(
        "ran locally on {}: remote refused: future_refusal",
        local_os_name()
    )));
    assert!(
        crate::bash_background::persistence::read_task(&paths.json)
            .unwrap()
            .local_fallback_started
    );
    assert!(!daemon
        .log
        .lock()
        .unwrap()
        .iter()
        .any(|(_, b)| b["method"] == "exec.run"));
}

#[tokio::test]
async fn runon_restart_executor_refusal_never_restores_local_launch() {
    // Cover both an attach that learns a refusal and a refusal already saved
    // before the previous process could decide whether to launch locally.
    for saved_terminal in [false, true] {
        let daemon = daemon(Script::AttachRefused, "exec-remote/v1").await;
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("must-not-run-after-restart");
        let command = format!("printf local-proof > '{}'", marker.display());
        let (original, task_id, paths) =
            persist_accepted_with_snapshot(dir.path(), daemon.connection.clone(), &command);
        let mut metadata = read_task(&paths.json).unwrap();
        let remote = metadata.remote.as_mut().unwrap();
        remote.explicit_runon = true;
        if saved_terminal {
            remote.terminal = Some(TerminalRecord::new(
                id(),
                Outcome::RefusedBeforeStart {
                    reason: RefusalReason::Unknown("future_refusal".into()),
                },
                1,
                0,
                0,
            ));
        }
        write_task(&paths.json, &metadata).unwrap();
        drop(original);
        let ctx = restarted_context(dir.path());
        let restarted = ctx.bash_background();
        restarted.replay_session(dir.path(), "session").unwrap();
        let done = terminal(restarted, &task_id).await;
        assert!(
            !marker.exists(),
            "explicit runon restored a local launch after restart"
        );
        assert_eq!(done.child_pid, None);
        assert!(!read_task(&paths.json).unwrap().local_fallback_started);
        assert_runon_refusal(&refusal_response(done), "future_refusal");
        assert!(
            exec_runs(&daemon).is_empty(),
            "restart must not resubmit remotely"
        );
        // The terminal refusal and its structured error also survive another restart.
        drop(ctx);
        let ctx = restarted_context(dir.path());
        ctx.bash_background()
            .replay_session(dir.path(), "session")
            .unwrap();
        let done = terminal(ctx.bash_background(), &task_id).await;
        assert_runon_refusal(&refusal_response(done), "future_refusal");
        assert!(!marker.exists());
    }
}

#[tokio::test]
async fn exec_remote_bash_post_spawn_persistence_failure_never_claims_no_run() {
    for restarted in [true, false] {
        let daemon = daemon(
            if restarted {
                Script::AttachRefused
            } else {
                Script::Refused
            },
            "exec-remote/v1",
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let counter = dir.path().join("started-count");
        let command = format!("printf x >> '{}'", counter.display());
        let (registry, task_id, paths) =
            persist_accepted_with_snapshot(dir.path(), daemon.connection.clone(), &command);
        let task = registry.task(&task_id).unwrap();
        post_spawn_failures()
            .lock()
            .unwrap()
            .insert(task_id.clone());
        if restarted {
            registry.resume_remote_task(&task_id).unwrap();
        } else {
            let layout = resolve_task_layout(&paths.session_dir, &task_id).unwrap();
            let remote = task.state.lock().unwrap().metadata.remote.clone().unwrap();
            let (plan, shell_path, env, linux_scope) = crate::sandbox_spawn::restore_local_launch(
                &layout,
                remote.fallback_digest.as_deref().unwrap(),
            )
            .unwrap();
            let request = exec::build_request(
                dir.path(),
                dir.path(),
                dir.path(),
                &command,
                BTreeMap::new(),
                Some(30),
                &exec::PresetParams::default(),
            );
            registry
                .start_remote_worker(
                    task,
                    Some((
                        request,
                        LocalFallback {
                            plan,
                            shell_path,
                            env,
                            linux_scope,
                            capture_pipeline: false,
                        },
                    )),
                )
                .unwrap();
        }
        let done = terminal(&registry, &task_id).await;
        assert_eq!(
            done.info.status,
            BgTaskStatus::FateUnknown,
            "restarted={restarted}: {done:?}"
        );
        assert!(
            done.output_preview
                .contains("the command started locally; AFT lost track of it"),
            "{done:?}"
        );
        assert!(!done.output_preview.contains("did not run"), "{done:?}");
        tokio::time::timeout(Duration::from_secs(5), async {
            while !counter.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(fs::read(&counter).unwrap(), b"x");
        let replay = super::tests::registry();
        replay.replay_session(dir.path(), "session").unwrap();
        let replayed = terminal(&replay, &task_id).await;
        assert_eq!(replayed.info.status, BgTaskStatus::FateUnknown);
        assert_eq!(fs::read(&counter).unwrap(), b"x");
    }
}

#[tokio::test]
async fn exec_remote_bash_restart_snapshot_tampering_refuses_without_run() {
    for missing in [false, true] {
        let daemon = daemon(Script::AttachRefused, "exec-remote/v1").await;
        let dir = tempfile::tempdir().unwrap();
        let (original, task_id, paths) = persist_accepted_with_snapshot(
            dir.path(),
            daemon.connection.clone(),
            "printf must-not-run",
        );
        drop(original);
        let snapshot = paths
            .control_dir
            .join(crate::sandbox_spawn::REMOTE_LOCAL_LAUNCH);
        if missing {
            fs::remove_file(&snapshot).unwrap();
        } else {
            let mut value: serde_json::Value =
                serde_json::from_slice(&fs::read(&snapshot).unwrap()).unwrap();
            value["policy"] = serde_json::json!("Unsandboxed");
            fs::write(&snapshot, serde_json::to_vec(&value).unwrap()).unwrap();
        }
        let ctx = restarted_context(dir.path());
        let restarted = ctx.bash_background();
        restarted.replay_session(dir.path(), "session").unwrap();
        let done = terminal(restarted, &task_id).await;
        assert_eq!(done.info.status, BgTaskStatus::Failed);
        assert!(done.output_preview.contains(
            "local fallback could not be restored after restart, so the command did not run"
        ));
        assert!(fs::read(&paths.stdout).unwrap().is_empty());
    }
}

#[tokio::test]
async fn exec_remote_bash_double_restart_starts_local_fallback_once() {
    let daemon = daemon(Script::AttachRefused, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let counter = dir.path().join("local-runs");
    let command = format!("printf x >> '{}'; /bin/sleep 2", counter.display());
    let (original, task_id, paths) =
        persist_accepted_with_snapshot(dir.path(), daemon.connection.clone(), &command);
    let stale = crate::bash_background::persistence::read_task(&paths.json).unwrap();
    let db = Arc::new(Mutex::new(
        crate::db::open(&dir.path().join("aft.db")).unwrap(),
    ));
    crate::db::bash_tasks::upsert_bash_task(
        &db.lock().unwrap(),
        &stale.to_bash_task_row("runner", &paths).unwrap(),
    )
    .unwrap();
    drop(original);
    crash_tasks().lock().unwrap().insert(task_id.clone());
    let first = registry();
    first.replay_session(dir.path(), "session").unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !counter.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    first.inner.shutdown.store(true, Ordering::SeqCst);
    drop(first);
    let second = registry();
    second.set_harness(crate::harness::Harness::Runner);
    second.set_db_pool(db.clone());
    second.replay_session(dir.path(), "session").unwrap();
    let _ = terminal(&second, &task_id).await;
    assert_eq!(
        fs::read(&counter).unwrap(),
        b"x",
        "a replay must not start another local fallback"
    );
    second.inner.shutdown.store(true, Ordering::SeqCst);
    drop(second);
    let third = registry();
    third.set_harness(crate::harness::Harness::Runner);
    third.set_db_pool(db);
    third.replay_session(dir.path(), "session").unwrap();
    let _ = terminal(&third, &task_id).await;
    assert_eq!(fs::read(&counter).unwrap(), b"x");
}

#[tokio::test]
async fn exec_remote_bash_no_policy_and_disabled_are_byte_identical_local_outputs() {
    let mut outputs = Vec::new();
    for policy in [None, Some(false)] {
        let dir = tempfile::tempdir().unwrap();
        let ctx = restarted_context(dir.path());
        let request = crate::protocol::RawRequest {
            id: "parity".into(),
            command: "bash".into(),
            session_id: Some("session".into()),
            lsp_hints: None,
            params: serde_json::json!({"command":"printf local-proof","compressed":false}),
        };
        let mut frozen = launch(dir.path().join("absent-connection"));
        frozen.params.remote_exec.as_mut().unwrap().enabled = false;
        let response = crate::bash_background::with_remote_policy(policy.map(|_| frozen), || {
            crate::commands::bash::handle(&request, &ctx)
        });
        assert!(response.success, "{response:?}");
        let task = response.data["task_id"].as_str().unwrap();
        let done = terminal(ctx.bash_background(), task).await;
        let task = ctx.bash_background().task(task).unwrap();
        assert!(task.state.lock().unwrap().metadata.remote.is_none());
        outputs.push((
            fs::read(&task.paths.stdout).unwrap(),
            fs::read(&task.paths.stderr).unwrap(),
            crate::commands::bash_orchestrate::format_foreground_result(&done),
        ));
    }
    assert_eq!(outputs[0], outputs[1]);
    assert_eq!(outputs[0].0, b"local-proof");
    assert_eq!(outputs[0].2, "local-proof");
}

#[test]
fn exec_remote_bash_unknown_output_bytes_are_durable_without_misattribution() {
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, paths) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf unused");
    let task = registry.task(&task_id).unwrap();
    let mut sink = TaskSink::new(&registry, task.clone()).unwrap();
    sink.unknown_output(0, b"future").unwrap();
    drop(sink);
    let mut sink = TaskSink::new(&registry, task).unwrap();
    sink.output(1, OutputStream::Stdout, b"ordinary").unwrap();
    assert_eq!(
        fs::read(paths.io_dir.join("remote-unknown-output")).unwrap(),
        b"future"
    );
    assert_eq!(fs::read(&paths.stdout).unwrap(), b"ordinary");
    assert!(fs::read(&paths.stderr).unwrap().is_empty());
}

#[test]
fn exec_remote_bash_only_not_sent_errors_prove_local_fallback_safe() {
    assert!(proves_no_start(&exec::Error::Transport(
        subc_client_rs::CallError::NotSent(Box::new(io::Error::other("not sent")))
    )));
    assert!(!proves_no_start(&exec::Error::Transport(
        subc_client_rs::CallError::OutcomeUnknown(Box::new(io::Error::other(
            "sent; transport lost"
        )))
    )));
    assert!(!proves_no_start(&exec::Error::RecoveryRequired {
        resume: Some(ResumePoint {
            job_id: id(),
            last_seq: Some(3),
            gap_recovery: None,
        }),
        reason: "lost".into()
    }));
}

#[tokio::test]
async fn exec_remote_bash_refusal_queue_wait_is_not_local_run_time() {
    let daemon = daemon(Script::AttachRefused, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let (original, task_id, paths) = persist_accepted_with_snapshot(
        dir.path(),
        daemon.connection.clone(),
        "/bin/sleep 0.8; printf budget-proof",
    );
    let layout = resolve_task_layout(&paths.session_dir, &task_id).unwrap();
    let mut metadata = read_task_at(&layout).unwrap();
    metadata.started_at = unix_millis().saturating_sub(60000);
    metadata.timeout_ms = Some(15000);
    write_task_at(&layout, &metadata).unwrap();
    drop(original);
    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let task = restarted.task(&task_id).unwrap();
            let state = task.state.lock().unwrap();
            if state.metadata.local_fallback_started {
                assert!(
                    task.elapsed_for_metadata(&state.metadata) < Duration::from_secs(15),
                    "the minute in the remote queue cannot consume the local run budget"
                );
                break;
            }
            drop(state);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        terminal(&restarted, &task_id).await.info.status,
        BgTaskStatus::Completed
    );
    assert_eq!(fs::read(&paths.stdout).unwrap(), b"budget-proof");
}

#[tokio::test]
async fn exec_remote_bash_durable_cancel_before_refusal_suppresses_fallback() {
    let daemon = daemon(Script::AttachRefused, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let (original, task_id, paths) = persist_accepted_with_snapshot(
        dir.path(),
        daemon.connection.clone(),
        "printf must-not-run",
    );
    let task = original.task(&task_id).unwrap();
    assert!(original.record_remote_cancel(&task).unwrap());
    assert!(
        crate::bash_background::persistence::read_task(&paths.json)
            .unwrap()
            .remote
            .unwrap()
            .cancel_requested
    );
    drop(task);
    drop(original);
    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    let done = terminal(&restarted, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Failed);
    assert!(done
        .output_preview
        .contains("local fallback skipped because cancellation was requested"));
    assert!(fs::read(&paths.stdout).unwrap().is_empty());
    let task = restarted.task(&task_id).unwrap();
    let state = task.state.lock().unwrap();
    assert!(matches!(
        state
            .metadata
            .remote
            .as_ref()
            .unwrap()
            .terminal
            .as_ref()
            .unwrap()
            .outcome,
        Outcome::RefusedBeforeStart { .. }
    ));
    assert!(!state.metadata.local_fallback_started);

    // The launch decision reads the durable record, even if another in-memory
    // view has not observed that cancellation yet.
    let dir = tempfile::tempdir().unwrap();
    let (stale, task_id, paths) = persist_accepted_with_snapshot(
        dir.path(),
        dir.path().join("unused"),
        "printf must-not-run",
    );
    let task = stale.task(&task_id).unwrap();
    assert!(stale.record_remote_cancel(&task).unwrap());
    let remote = task.state.lock().unwrap().metadata.remote.clone().unwrap();
    task.state
        .lock()
        .unwrap()
        .metadata
        .remote
        .as_mut()
        .unwrap()
        .cancel_requested = false;
    stale
        .restored_remote_fallback(&task, &remote, "future_refusal", None)
        .unwrap();
    assert!(fs::read(&paths.stdout).unwrap().is_empty());
    assert!(!task.state.lock().unwrap().metadata.local_fallback_started);
}

#[tokio::test]
async fn exec_remote_bash_fallback_marker_before_cancel_uses_local_kill() {
    let daemon = daemon(Script::AttachRefused, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let (original, task_id, paths) =
        persist_accepted_with_snapshot(dir.path(), daemon.connection.clone(), "/bin/sleep 30");
    drop(original);
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    marker_gates().lock().unwrap().insert(
        task_id.clone(),
        MarkerGate {
            entered: entered_tx,
            release: release_rx,
        },
    );
    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    tokio::task::spawn_blocking(move || entered_rx.recv_timeout(Duration::from_secs(10)).unwrap())
        .await
        .unwrap();
    let metadata = crate::bash_background::persistence::read_task(&paths.json).unwrap();
    assert!(metadata.local_fallback_started);
    assert!(metadata.remote.is_none());
    let killing = restarted.clone();
    let id = task_id.clone();
    let (invoked_tx, invoked_rx) = std::sync::mpsc::channel();
    let kill = tokio::task::spawn_blocking(move || {
        invoked_tx.send(()).unwrap();
        killing.kill(&id, "session").unwrap()
    });
    tokio::task::spawn_blocking(move || invoked_rx.recv_timeout(Duration::from_secs(10)).unwrap())
        .await
        .unwrap();
    release_tx.send(()).unwrap();
    assert_eq!(kill.await.unwrap().info.status, BgTaskStatus::Killed);
    assert!(!daemon
        .log
        .lock()
        .unwrap()
        .iter()
        .any(|(_, b)| b["method"] == "exec.cancel"));
}

/// Run `params` through the bash command handler with `policy` installed as
/// the session's remote policy, as the subc tool-call path does.
fn handle_with_policy(
    ctx: &crate::context::AppContext,
    policy: Option<crate::bash_background::RemoteLaunch>,
    params: serde_json::Value,
) -> crate::protocol::Response {
    let request = crate::protocol::RawRequest {
        id: "runon".into(),
        command: "bash".into(),
        session_id: Some("session".into()),
        lsp_hints: None,
        params,
    };
    crate::bash_background::with_remote_policy(policy, || {
        crate::commands::bash::handle(&request, ctx)
    })
}

fn exec_runs(daemon: &crate::exec_remote::wire_tests::Daemon) -> Vec<serde_json::Value> {
    daemon
        .log
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, body)| body["method"] == "exec.run")
        .map(|(_, body)| body["params"].clone())
        .collect()
}

#[tokio::test]
async fn runon_sends_the_whole_compound_line_remote_exactly_as_written() {
    let daemon = daemon(Script::Plain, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = remote_test_context(dir.path());
    // Pipes, an environment prefix, a list and a command the old prefix
    // matcher would never have sent anywhere: all of it goes as one line.
    let line = "BUILD_FLAVOR=ci git status | tr a-z A-Z && printf '%s' \"$HOME\" ; ls | wc -l";
    let response = handle_with_policy(
        &ctx,
        Some(launch(daemon.connection.clone())),
        serde_json::json!({"command": line, "runon": "linux", "compressed": false}),
    );
    assert!(response.success, "{response:?}");
    let task_id = response.data["task_id"].as_str().unwrap().to_string();
    let done = terminal(ctx.bash_background(), &task_id).await;
    let runs = exec_runs(&daemon);
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0]["command"], line);
    assert_eq!(
        Path::new(runs[0]["cwd"].as_str().unwrap()),
        dir.path(),
        "the remote run uses the call's working directory"
    );
    let rendered = crate::commands::bash_orchestrate::format_foreground_result(&done);
    assert_eq!(
        rendered.lines().next(),
        Some("ran remotely on ck-motor"),
        "{rendered}"
    );
    // A report without a changed-file list says so, after the output.
    assert!(
        rendered.ends_with(
            "changed files: not reported by the runner\n\
             git state: not reported by the runner\n\
             untracked files: not reported by the runner\n\
             ignored writes: not reported by the runner"
        ),
        "{rendered}"
    );
}

#[tokio::test]
async fn old_shape_prefix_routing_survives_the_runon_kill_switch_and_runon_works_when_enabled() {
    for enabled in [false, true] {
        let daemon =
            crate::exec_remote::wire_tests::daemon_with_clients(Script::Plain, "exec-remote/v1", 2)
                .await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = remote_test_context_with_runon(dir.path(), enabled);
        let mut old = launch(daemon.connection.clone());
        old.params.remote_exec.as_mut().unwrap().legacy_commands =
            Some(serde_json::json!(["cd", "cargo test"]));
        let plain = handle_with_policy(
            &ctx,
            Some(old.clone()),
            serde_json::json!({"command":"cargo test", "compressed":false}),
        );
        assert!(plain.success, "{plain:?}");
        let done = terminal(
            ctx.bash_background(),
            plain.data["task_id"].as_str().unwrap(),
        )
        .await;
        assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");
        assert_eq!(
            exec_runs(&daemon).len(),
            1,
            "plain cargo must keep using the deployed route"
        );
        let whole = handle_with_policy(
            &ctx,
            Some(old),
            serde_json::json!({"command":"FOO=1 cargo test | tail -1", "runon":"linux"}),
        );
        if enabled {
            assert!(whole.success, "{whole:?}");
            let done = terminal(
                ctx.bash_background(),
                whole.data["task_id"].as_str().unwrap(),
            )
            .await;
            assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");
            assert_eq!(exec_runs(&daemon).len(), 2);
        } else {
            assert!(!whole.success);
            assert!(whole.data["message"]
                .as_str()
                .unwrap()
                .contains("bash.runon_enabled"));
            assert_eq!(exec_runs(&daemon).len(), 1);
        }
    }
}

#[tokio::test]
async fn runon_is_refused_when_the_daemon_has_no_exec_remote_provider() {
    let daemon = daemon(Script::Plain, "exec.remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("must-not-run-locally");
    let ctx = restarted_context(dir.path());
    let response = handle_with_policy(
        &ctx,
        Some(launch(daemon.connection.clone())),
        serde_json::json!({
            "command": format!("echo SHOULD_NOT_RUN > '{}'", marker.display()),
            "runon": "linux"
        }),
    );
    assert!(response.success, "{response:?}");
    let task = response.data["task_id"].as_str().unwrap();
    let snapshot = terminal(ctx.bash_background(), task).await;
    assert_eq!(snapshot.info.status, BgTaskStatus::Failed);
    let rendered = crate::commands::bash_orchestrate::format_foreground_result(&snapshot);
    assert!(rendered.contains("runon refused"), "{rendered}");
    assert!(rendered.contains("exec-remote/v1"), "{rendered}");
    assert!(!rendered.contains("ran locally"), "{rendered}");
    assert!(exec_runs(&daemon).is_empty());
    assert!(
        !marker.exists(),
        "missing providers must not cause local execution"
    );
}

#[tokio::test]
async fn without_runon_new_shape_plans_keep_commands_local() {
    let daemon = daemon(Script::Plain, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    // New-shape plans have no prefix list and require an explicit remote demand.
    let mut policy = launch(daemon.connection.clone());
    policy.params.remote_exec.as_mut().unwrap().default_demand = Some("linux".into());
    let response = handle_with_policy(
        &ctx,
        Some(policy),
        serde_json::json!({"command": "printf local-proof", "compressed": false}),
    );
    assert!(response.success, "{response:?}");
    let task_id = response.data["task_id"].as_str().unwrap().to_string();
    let done = terminal(ctx.bash_background(), &task_id).await;
    assert!(exec_runs(&daemon).is_empty());
    let task = ctx.bash_background().task(&task_id).unwrap();
    assert!(task.state.lock().unwrap().metadata.remote.is_none());
    assert_eq!(
        crate::commands::bash_orchestrate::format_foreground_result(&done),
        "local-proof"
    );
}

#[tokio::test]
async fn runon_is_refused_by_name_whenever_it_cannot_run_remotely() {
    let daemon = daemon(Script::Plain, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("ran-locally");
    let command = format!("printf x > '{}'", marker.display());
    let enabled = || Some(launch(daemon.connection.clone()));
    let mut disabled = launch(daemon.connection.clone());
    disabled.params.remote_exec.as_mut().unwrap().enabled = false;
    let ctx = restarted_context(dir.path());
    let mut project_off = crate::config::Config {
        project_root: Some(dir.path().into()),
        ..Default::default()
    };
    project_off.sandbox.enabled = false;
    project_off.remote_exec.project_off = true;
    project_off.bash.runon_enabled = true;
    let project_off_ctx = crate::context::AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        project_off,
    );
    for (ctx, policy, extra, expected) in [
        (
            &project_off_ctx,
            enabled(),
            serde_json::json!({}),
            "remote runs are off for this project",
        ),
        (
            &ctx,
            None,
            serde_json::json!({}),
            "this session has no remote runner",
        ),
        (
            &ctx,
            Some(disabled.clone()),
            serde_json::json!({}),
            "this session has no remote runner",
        ),
        (
            &ctx,
            enabled(),
            serde_json::json!({"pty": true}),
            "runon cannot be combined with pty:true",
        ),
        (
            &ctx,
            enabled(),
            serde_json::json!({"shell": "powershell"}),
            "runon cannot run PowerShell",
        ),
        (
            &ctx,
            enabled(),
            serde_json::json!({"runon": "windows"}),
            "unknown runner demand \"windows\"",
        ),
    ] {
        let mut params = serde_json::json!({"command": command, "runon": "linux"});
        for (key, value) in extra.as_object().unwrap() {
            params[key] = value.clone();
        }
        let response = handle_with_policy(ctx, policy, params);
        assert!(!response.success, "{expected}: {response:?}");
        let message = response.data["message"].as_str().unwrap_or_default();
        assert!(message.contains(expected), "{expected}: {response:?}");
    }
    // Nothing ran anywhere: not on the server, and not quietly on this host.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(exec_runs(&daemon).is_empty());
    assert!(!marker.exists());
}

#[cfg(unix)]
fn hermetic_git(cwd: &Path, args: &[&str]) {
    let mut command = std::process::Command::new("git");
    let output = crate::test_env::apply_hermetic_git_env(command.current_dir(cwd))
        .args(args)
        .output()
        .expect("run git fixture command");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
fn git_fixture(root: &Path) {
    std::fs::create_dir_all(root).unwrap();
    hermetic_git(root, &["init", "--quiet"]);
    hermetic_git(root, &["config", "user.name", "AFT Test"]);
    hermetic_git(root, &["config", "user.email", "aft-test@example.invalid"]);
    std::fs::write(root.join("tracked.txt"), "fixture\n").unwrap();
    hermetic_git(root, &["add", "tracked.txt"]);
    hermetic_git(root, &["commit", "--quiet", "-m", "fixture"]);
}

#[cfg(unix)]
fn ensure_git_fixture(root: &Path) {
    if !root.join(".git").exists() {
        git_fixture(root);
    }
}

#[cfg(unix)]
fn remote_test_context(root: &Path) -> crate::context::AppContext {
    ensure_git_fixture(root);
    restarted_context(root)
}

#[cfg(unix)]
fn remote_test_context_with_runon(root: &Path, enabled: bool) -> crate::context::AppContext {
    ensure_git_fixture(root);
    restarted_context_with_runon(root, enabled)
}

#[cfg(unix)]
fn add_git_worktree(repository: &Path, path: &Path, branch: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let path = path.to_str().unwrap();
    hermetic_git(
        repository,
        &["worktree", "add", "--quiet", "-b", branch, path, "HEAD"],
    );
}

#[cfg(unix)]
fn assert_remote_request_paths(
    request: RunRequest,
    workspace_key: &Path,
    repository_root: &Path,
    cwd: &Path,
) {
    let wire = serde_json::to_value(request).unwrap();
    assert_eq!(wire["workspace_key"], workspace_key.to_str().unwrap());
    assert_eq!(wire["repository_root"], repository_root.to_str().unwrap());
    assert_eq!(wire["cwd"], cwd.to_str().unwrap());
}

#[cfg(unix)]
#[test]
fn linked_worktree_remote_requests_use_the_linked_worktree_root() {
    let temp = tempfile::tempdir().unwrap();
    let main = temp.path().join("main");
    git_fixture(&main);
    let main = main.canonicalize().unwrap();
    let linked = temp.path().join("linked");
    add_git_worktree(&main, &linked, "linked-sibling");
    let linked = linked.canonicalize().unwrap();
    let subdir = linked.join("subdir");
    std::fs::create_dir(&subdir).unwrap();

    for cwd in [&linked, &subdir] {
        let request = build_remote_request(
            &main,
            cwd,
            "true",
            BTreeMap::new(),
            None,
            &exec::PresetParams::default(),
        )
        .expect("linked worktree cwd should be accepted");
        assert_remote_request_paths(request, &linked, &main, cwd);
    }
}

#[cfg(unix)]
#[test]
fn remote_request_for_main_worktree_keeps_the_main_root() {
    let temp = tempfile::tempdir().unwrap();
    let main = temp.path().join("main");
    git_fixture(&main);
    let main = main.canonicalize().unwrap();
    let cwd = main.join("subdir");
    std::fs::create_dir(&cwd).unwrap();

    let request = build_remote_request(
        &main,
        &cwd,
        "true",
        BTreeMap::new(),
        None,
        &exec::PresetParams::default(),
    )
    .unwrap();
    assert_remote_request_paths(request, &main, &main, &cwd);
}

#[cfg(unix)]
#[test]
fn nested_linked_worktree_remote_request_uses_the_nested_root() {
    let temp = tempfile::tempdir().unwrap();
    let main = temp.path().join("main");
    git_fixture(&main);
    let main = main.canonicalize().unwrap();
    let nested = main.join(".cortexkit/worktrees/nested");
    add_git_worktree(&main, &nested, "linked-nested");
    let nested = nested.canonicalize().unwrap();
    let cwd = nested.join("subdir");
    std::fs::create_dir(&cwd).unwrap();

    let request = build_remote_request(
        &main,
        &cwd,
        "true",
        BTreeMap::new(),
        None,
        &exec::PresetParams::default(),
    )
    .unwrap();
    assert_remote_request_paths(request, &nested, &main, &cwd);
}

#[cfg(unix)]
#[test]
fn remote_request_refuses_unrelated_repositories_and_non_git_directories() {
    let temp = tempfile::tempdir().unwrap();
    let main = temp.path().join("main");
    let unrelated = temp.path().join("unrelated");
    git_fixture(&main);
    git_fixture(&unrelated);
    let main = main.canonicalize().unwrap();
    let unrelated = unrelated.canonicalize().unwrap();
    let outside = temp.path().join("plain-outside");
    std::fs::create_dir(&outside).unwrap();

    for cwd in [&unrelated, &outside] {
        let error = build_remote_request(
            &main,
            cwd,
            "true",
            BTreeMap::new(),
            None,
            &exec::PresetParams::default(),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains(
                "workdir is not inside this session's repository or one of its linked worktrees"
            ),
            "unexpected refusal reason: {error}"
        );
    }
}

#[test]
fn published_report_vectors_render_changes_say_nothing_when_empty_and_name_what_is_absent() {
    use crate::exec_remote::wire_tests::report_vector;
    // Reported and changed: every kind of change is named, none copied back.
    let all = "These files changed on the server and were NOT copied back:\n  result.txt\n\
         Git state changed on the server and was NOT copied back:\n  \
         HEAD: 111111111111 -> 222222222222\n  \
         ref: refs/heads/main -> refs/heads/build\n  \
         index tree changed (staged changes differ)\n  \
         stash count: 0 -> 1 (+1)\n\
         These untracked files were created on the server and were NOT copied back:\n  \
         generated/new.txt\n  notes.txt\n\
         25 writes under ignored paths on the server were NOT copied back, for example:\n  \
         scratch/debug.log\n  cache/result.bin";
    assert_eq!(
        render_terminal_report(&report_vector("all")).as_deref(),
        Some(all)
    );
    // Reported and unchanged: a reporting runner sent every field, all
    // empty, so there is nothing to say.
    let unchanged = report_vector("unchanged");
    assert!(unchanged.git_state_changed.is_some());
    assert_eq!(render_terminal_report(&unchanged), None);
    // Absent (an older runner): each missing report is named on its own line.
    assert_eq!(
        render_terminal_report(&report_vector("older-runner")).as_deref(),
        Some(
            "These files changed on the server and were NOT copied back:\n  result.txt\n\
             git state: not reported by the runner\n\
             untracked files: not reported by the runner\n\
             ignored writes: not reported by the runner"
        )
    );
    // A detached HEAD has no ref on either side, so no ref line; an index
    // tree that became available counts as changed.
    let detached = render_terminal_report(&report_vector("detached-head")).unwrap();
    assert!(!detached.contains("ref:"), "{detached}");
    assert!(detached.contains("HEAD: 111111111111 -> 222222222222\n  index tree changed"));
    // A capped untracked list says the runner listed only some of them.
    let truncated = render_terminal_report(&report_vector("truncated-untracked")).unwrap();
    assert!(
        truncated
            .contains("  notes.txt\n  (the runner listed only some of them; more were created)\n"),
        "{truncated}"
    );
}

#[tokio::test]
async fn a_reporting_runner_report_is_printed_after_the_output() {
    let daemon = daemon(Script::Reported, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = remote_test_context(dir.path());
    let response = handle_with_policy(
        &ctx,
        Some(launch(daemon.connection.clone())),
        serde_json::json!({"command": "bash build.sh", "runon": "linux", "compressed": false}),
    );
    assert!(response.success, "{response:?}");
    let task_id = response.data["task_id"].as_str().unwrap().to_string();
    let done = terminal(ctx.bash_background(), &task_id).await;
    let rendered = crate::commands::bash_orchestrate::format_foreground_result(&done);
    assert_eq!(
        rendered.lines().next(),
        Some("ran remotely on ck-motor"),
        "{rendered}"
    );
    assert!(
        rendered.ends_with(
            &render_terminal_report(&crate::exec_remote::wire_tests::report_vector("all")).unwrap()
        ),
        "{rendered}"
    );
    assert!(
        !rendered.contains("not reported by the runner"),
        "{rendered}"
    );
}

/// The bash call the OpenCode and Pi plugins send for a model's `runon` call.
fn orchestrated_request(params: serde_json::Value) -> crate::protocol::RawRequest {
    crate::protocol::RawRequest {
        id: "runon".into(),
        command: "bash".into(),
        session_id: Some("session".into()),
        lsp_hints: None,
        params,
    }
}

/// Spawn `params` through the bash handler and drive its foreground wait the
/// way the standalone request loop does, until the call answers. Returns the
/// reply and how long after the call arrived it came.
async fn orchestrated_reply(
    ctx: &crate::context::AppContext,
    daemon: &crate::exec_remote::wire_tests::Daemon,
    params: serde_json::Value,
) -> (crate::protocol::Response, Duration) {
    let request = orchestrated_request(params);
    let received = std::time::Instant::now();
    let spawned =
        crate::bash_background::with_remote_policy(Some(launch(daemon.connection.clone())), || {
            crate::commands::bash::handle(&request, ctx)
        });
    assert!(spawned.success, "{spawned:?}");
    match crate::commands::bash_orchestrate::build_bash_outcome(&request, ctx, spawned) {
        crate::response_finalize::DispatchOutcome::Immediate(response) => {
            (response, received.elapsed())
        }
        crate::response_finalize::DispatchOutcome::Deferred(mut pending) => {
            let response = tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    if let Some(response) = (pending.poll)(ctx) {
                        return response;
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            })
            .await
            .expect("a blocking call must always answer");
            (response, received.elapsed())
        }
    }
}

/// Every cancellation AFT sent the runner: `exec.cancel`, or a Cancel frame
/// (sent when a held-open run stream is dropped).
fn runner_cancels(daemon: &crate::exec_remote::wire_tests::Daemon) -> usize {
    daemon
        .log
        .lock()
        .unwrap()
        .iter()
        .filter(|(header, body)| {
            body["method"] == "exec.cancel" || header.ty == subc_protocol::FrameType::Cancel
        })
        .count()
}

/// The plugins' transport deadline for a blocking call with this `timeout`
/// (`orchestratedTransportTimeoutMs`: the timeout plus a 10-second margin).
fn plugin_transport_deadline(timeout_ms: u64) -> Duration {
    Duration::from_millis(timeout_ms + 10_000)
}

#[tokio::test]
async fn blocking_runon_queued_past_its_timeout_answers_with_task_job_and_queue_position() {
    // The runner accepts the job at queue position 11 and holds it queued well
    // past the call's `timeout` before running it.
    let daemon = daemon(
        Script::Staged {
            position: 11,
            queue: Duration::from_secs(4),
            run: Duration::ZERO,
        },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    let timeout_ms = 1_500;
    let (reply, waited) = orchestrated_reply(
        &ctx,
        &daemon,
        serde_json::json!({
            "command": "uname -s; date -u",
            "runon": "linux",
            "wait": true,
            "timeout": timeout_ms,
            "foreground_orchestrate": true,
            "compressed": false,
        }),
    )
    .await;
    assert!(reply.success, "{reply:?}");
    assert!(
        waited >= Duration::from_millis(timeout_ms),
        "answered after {waited:?}, before its wait was over"
    );
    assert!(
        waited < plugin_transport_deadline(timeout_ms),
        "answered after {waited:?}, when the plugin had already given up"
    );
    let task_id = reply.data["task_id"].as_str().unwrap().to_string();
    assert_eq!(reply.data["status"], "running", "{reply:?}");
    assert_eq!(reply.data["remote_phase"], "queued", "{reply:?}");
    assert_eq!(reply.data["queue_position"], 11, "{reply:?}");
    assert_eq!(reply.data["remote_job_id"], id().to_string(), "{reply:?}");
    let output = reply.data["output"].as_str().unwrap();
    for expected in [
        task_id.as_str(),
        &id().to_string(),
        "queued at position 11",
        "It was not killed",
    ] {
        assert!(output.contains(expected), "missing {expected:?}: {output}");
    }
    assert_eq!(
        runner_cancels(&daemon),
        0,
        "handing the call back must not cancel the queued job"
    );
    // The job runs once it leaves the queue: AFT did not count its timeout
    // from submission, and nothing cancelled it.
    let done = terminal(ctx.bash_background(), &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");
    assert!(
        crate::commands::bash_orchestrate::format_foreground_result(&done).contains("started"),
        "{done:?}"
    );
    assert_eq!(runner_cancels(&daemon), 0);
    assert_eq!(
        exec_runs(&daemon).len(),
        1,
        "the job must not be resubmitted"
    );
}

#[tokio::test]
async fn blocking_runon_still_running_at_its_wait_limit_answers_with_running_phase() {
    let daemon = daemon(
        Script::Staged {
            position: 1,
            queue: Duration::ZERO,
            run: Duration::from_secs(4),
        },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    let timeout_ms = 1_500;
    let (reply, waited) = orchestrated_reply(
        &ctx,
        &daemon,
        serde_json::json!({
            "command": "cargo test",
            "runon": "linux",
            "wait": true,
            "timeout": timeout_ms,
            "foreground_orchestrate": true,
            "compressed": false,
        }),
    )
    .await;
    assert!(reply.success, "{reply:?}");
    assert!(waited < plugin_transport_deadline(timeout_ms), "{waited:?}");
    assert_eq!(reply.data["remote_phase"], "running", "{reply:?}");
    assert_eq!(reply.data["queue_position"], serde_json::Value::Null);
    assert!(
        reply.data["output"]
            .as_str()
            .unwrap()
            .contains("running on ck-motor"),
        "{reply:?}"
    );
    assert_eq!(runner_cancels(&daemon), 0);
    let task_id = reply.data["task_id"].as_str().unwrap();
    let done = terminal(ctx.bash_background(), task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");
}

#[tokio::test]
async fn blocking_runon_killed_at_its_runner_deadline_reports_exit_124() {
    let daemon = daemon(Script::Deadline, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    let (reply, _) = orchestrated_reply(
        &ctx,
        &daemon,
        serde_json::json!({
            "command": "sleep 600",
            "runon": "linux",
            "wait": true,
            "timeout": 5_000,
            "foreground_orchestrate": true,
            "compressed": false,
        }),
    )
    .await;
    assert!(reply.success, "{reply:?}");
    assert_eq!(reply.data["timed_out"], true, "{reply:?}");
    assert_eq!(reply.data["exit_code"], 124, "{reply:?}");
    let output = reply.data["output"].as_str().unwrap();
    assert!(output.contains("[command timed out]"), "{output}");
    assert!(output.contains("[exit code: 124]"), "{output}");
    // The runner killed the job itself; AFT has nothing to cancel.
    assert_eq!(runner_cancels(&daemon), 0);
}

#[tokio::test]
async fn non_blocking_runon_is_still_promoted_at_the_foreground_wait_window() {
    let daemon = daemon(
        Script::Staged {
            position: 3,
            queue: Duration::from_secs(6),
            run: Duration::ZERO,
        },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    ctx.update_config(|config| config.foreground_wait_window_ms = 2_500);
    let (reply, waited) = orchestrated_reply(
        &ctx,
        &daemon,
        serde_json::json!({
            "command": "uname -s",
            "runon": "linux",
            "timeout": 500,
            "foreground_orchestrate": true,
            "compressed": false,
        }),
    )
    .await;
    assert!(reply.success, "{reply:?}");
    // A non-blocking call ends at the foreground wait window, as before; the
    // blocking-call hand-back (at its 500 ms `timeout`) does not apply to it.
    assert!(waited >= Duration::from_millis(2_500), "{waited:?}");
    assert_eq!(reply.data["status"], "running", "{reply:?}");
    assert!(reply.data.get("remote_phase").is_none(), "{reply:?}");
    assert!(
        reply.data["output"]
            .as_str()
            .unwrap()
            .starts_with("Foreground bash didn't finish within"),
        "{reply:?}"
    );
    assert_eq!(runner_cancels(&daemon), 0);
}

#[test]
fn remote_phase_comes_from_acceptance_and_output_only() {
    let mut remote = RemoteTask {
        connection_file: None,
        explicit_runon: true,
        harness: "broca".into(),
        session: "session".into(),
        job_id: None,
        last_seq: None,
        stdout_len: 0,
        stderr_len: 0,
        unknown_len: 0,
        cancel_requested: false,
        terminal: None,
        fallback_digest: None,
        env_not_forwarded: None,
        queue_position: None,
        started: false,
    };
    assert_eq!(RemotePhase::of(&remote), RemotePhase::WaitingOnRunner);
    remote.job_id = Some(id());
    assert_eq!(
        RemotePhase::of(&remote),
        RemotePhase::WaitingOnRunner,
        "a job id restored without its queue position does not claim a position"
    );
    remote.queue_position = Some(4);
    assert_eq!(
        RemotePhase::of(&remote),
        RemotePhase::Queued { position: 4 }
    );
    remote.started = true;
    assert_eq!(RemotePhase::of(&remote), RemotePhase::Running);
}
