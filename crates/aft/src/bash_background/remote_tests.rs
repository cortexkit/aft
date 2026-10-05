use super::*;
use crate::exec_remote::wire_tests::{daemon, id, Script};

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
        (Script::KnownRefused, "server_unreachable"),
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
        cancel_requested: false,
        terminal: None,
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
