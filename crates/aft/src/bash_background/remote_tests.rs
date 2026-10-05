use super::*;
use crate::exec_remote::wire_tests::{daemon, id, Script};
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
        connection_file: Some(connection),
        harness: "broca".into(),
        session: "session".into(),
        params: exec::FrozenParams {
            remote_exec: Some(exec::policy::RemoteExecPolicy {
                enabled: true,
                commands: vec!["printf".into()],
            }),
            ..Default::default()
        },
    }
}

fn start(registry: &BgTaskRegistry, dir: &Path, connection: PathBuf) -> String {
    registry
        .spawn_remote(
            launch(connection),
            SpawnPlan::Unsandboxed,
            "printf local-proof",
            resolve_posix_shell(),
            "session".into(),
            dir.into(),
            HashMap::from([("AFT_REMOTE_ENV_TEST".into(), "environment-proof".into())]),
            crate::bash_background::HardKill::After(Duration::from_secs(30)),
            dir.into(),
            10,
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
async fn exec_remote_bash_refusal_runs_local_with_disclosure() {
    for (script, reason) in [
        (Script::Refused, "future_refusal"),
        (Script::KnownRefused, "unreachable"),
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
        assert!(
            done.output_preview
                .contains(&format!("ran locally: remote executor refused ({reason})")),
            "{}",
            done.output_preview
        );
        let log = daemon.log.lock().unwrap();
        let request = log.iter().find(|(_, b)| b["method"] == "exec.run").unwrap();
        assert_eq!(
            request.1["params"]["env"]["AFT_REMOTE_ENV_TEST"],
            "environment-proof"
        );
        assert_eq!(request.1["params"]["cwd"], dir.path().display().to_string());
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
    assert!(done.output_preview.contains("ran remotely on ck-motor"));
    assert!(done
        .output_preview
        .contains("workspace_changes (not copied back): generated.txt"));
    assert!(!dir.path().join("generated.txt").exists());
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
        connection_file: Some(daemon.connection.clone()),
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
        connection_file: Some(connection),
        harness: "runner".into(),
        session: "session".into(),
        job_id: None,
        last_seq: None,
        stdout_len: 0,
        stderr_len: 0,
        unknown_len: 0,
        cancel_requested: false,
        terminal: None,
        fallback_digest: Some(digest),
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
    // A fresh configuration would choose ordinary unsandboxed inheritance;
    // the saved Host plan must still clear ambient HOME before its command.
    crate::context::AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(dir.into()),
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
    assert!(done
        .output_preview
        .contains("ran locally: remote executor refused (future_refusal)"));
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
            last_seq: Some(3)
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
        .restored_remote_fallback(&task, &remote, "future_refusal")
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
