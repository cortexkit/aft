use super::fixture::*;
use super::seams::*;
use super::*;
use crate::bash_background::persistence::{read_task, write_task};
use crate::exec_remote::wire_fixture::{daemon, id, Script};

#[path = "bookkeeping_probe.rs"]
mod bookkeeping_probe;

#[test]
fn runon_read_only_note_ignores_stdout_and_local_tasks() {
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, _) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf remote");
    let task = registry.task(&task_id).unwrap();
    let mut sink = TaskSink::new(&registry, task.clone()).unwrap();
    let error = b"src/x.rs: Operation not permitted\n";
    sink.output(1, OutputStream::Stdout, error).unwrap();
    let mut state = task.state.lock().unwrap();
    let mut metadata = state.metadata.clone();
    metadata.mark_terminal(BgTaskStatus::Failed, Some(1), None);
    metadata.remote.as_mut().unwrap().terminal = Some(
        TerminalRecord::new(id(), Outcome::Exit { code: 1 }, 1, 0, 0).with_source_read_only(true),
    );
    let mut text = "original output".to_owned();
    read_only_status::append_source_read_only_note(&mut text, &metadata, &state.buffer);
    assert_eq!(text, "original output", "stdout alone is not error output");
    metadata.remote = None;
    drop(state);
    sink.output(2, OutputStream::Stderr, error).unwrap();
    state = task.state.lock().unwrap();
    read_only_status::append_source_read_only_note(&mut text, &metadata, &state.buffer);
    assert_eq!(
        text, "original output",
        "local jobs have no remote lock proof"
    );
}

#[test]
fn exec_remote_bash_oom_optional_measurements_and_unknown_cause() {
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, _) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "true");
    let task = registry.task(&task_id).unwrap();
    for (cause, stated, expected) in [
        (
            "job_limit",
            None,
            "killed: it went over its memory limit; rerun with a larger limit",
        ),
        (
            "job_limit",
            Some(1536),
            "killed: it went over its memory limit of 1536 MiB; rerun with a larger limit",
        ),
        (
            "host_oom",
            None,
            "killed: the build server ran out of memory, not this job's limit; rerun as is",
        ),
        (
            "future_cause",
            None,
            "killed for memory; the build server reported an unrecognized OOM cause",
        ),
    ] {
        let mut record = TerminalRecord::new(id(), Outcome::Signal { signal: 9 }, 1, 0, 0)
            .with_oom_kill(OomKill::new(cause));
        if let Some(stated) = stated {
            record = record.with_memory_stated_mib(stated);
        }
        let mut metadata = task.state.lock().unwrap().metadata.clone();
        metadata.remote.as_mut().unwrap().terminal = Some(record);
        let oom = remote_oom_kill(&metadata).unwrap();
        assert_eq!(oom.message(), expected);
        let structured = serde_json::to_value(&oom).unwrap();
        assert_eq!(structured["cause"], cause);
        assert!(structured.get("memory_peak_working_mib").is_none());
    }
}

#[test]
fn remote_admission_progress_lines_are_exact_and_detail_is_plain_text() {
    use exec::types::{Progress, ProgressPhase};
    for (phase, elapsed, without, with) in [
        (ProgressPhase::Upload, 14_000,
         "The remote job is uploading its files (14s so far).",
         "The remote job is uploading its files (14s so far): <room> & **capacity**."),
        (ProgressPhase::PlacementWait, 130_000,
         "The remote job is waiting for a free build host (2m 10s so far).",
         "The remote job is waiting for a free build host (2m 10s so far): <room> & **capacity**."),
        (ProgressPhase::Connect, 3_000,
         "The remote job is connecting to its build host (3s so far).",
         "The remote job is connecting to its build host (3s so far): <room> & **capacity**."),
    ] {
        for detail in [None, Some(String::new()), Some(" \u{2003}  ".into())] {
            assert_eq!(admission_progress_line(&Progress::new(phase.clone(), elapsed, detail).unwrap()).as_deref(), Some(without));
        }
        assert_eq!(admission_progress_line(&Progress::new(phase, elapsed, Some("<room> & **capacity**".into())).unwrap()).as_deref(), Some(with));
    }
    assert_eq!(
        admission_progress_line(
            &Progress::new(
                ProgressPhase::Unknown("future_phase".into()),
                40_000,
                Some("unrecognized".into())
            )
            .unwrap()
        ),
        None
    );
}

#[test]
fn remote_admission_progress_detail_is_normalized_redacted_and_character_bounded() {
    use exec::types::{Progress, ProgressPhase};
    let token = format!("ghp_{}", "a".repeat(36));
    assert_eq!(
        admission_progress_line(
            &Progress::new(
                ProgressPhase::Upload,
                14_000,
                Some(format!(" \u{2003}{token}  "))
            )
            .unwrap()
        )
        .as_deref(),
        Some("The remote job is uploading its files (14s so far): <REDACTED_SECRET>.")
    );
    // Exercise the display boundary directly: the wire decoder independently
    // rejects multiline or oversized detail before constructing Progress.
    let detail = format!(
        "begin\n\r\0\u{001b} {token}\u{2028}\u{2029}end\t{}",
        "界".repeat(121)
    );
    let rendered = admission_progress_detail(&detail);
    assert_eq!(
        rendered,
        format!("begin <REDACTED_SECRET> end {}…", "界".repeat(91))
    );
    assert_eq!(rendered.chars().count(), 120);
    assert!(!rendered.contains("ghp_"));
    assert!(!rendered
        .chars()
        .any(|character| character.is_control() || matches!(character, '\u{2028}' | '\u{2029}')));
    assert!(!rendered.contains("  "));

    let boundary_token = admission_progress_detail(&format!("{} {token}", "x".repeat(110)));
    assert!(
        !boundary_token.contains("ghp_"),
        "credentials must be redacted before the character cut"
    );
    assert_eq!(boundary_token.chars().count(), 120);
    assert!(boundary_token.ends_with('…'));
    assert_eq!(
        admission_progress_detail(&"界".repeat(120)),
        "界".repeat(120)
    );
    assert_eq!(
        admission_progress_detail(&"界".repeat(121)),
        format!("{}…", "界".repeat(119))
    );
    assert_eq!(admission_progress_detail(" \r\n\t\0\u{2028}\u{2029}"), "");
}

#[tokio::test]
async fn remote_admission_progress_is_latest_only_ephemeral_and_changes_no_timers_or_cursor() {
    use exec::types::{Progress, ProgressPhase};
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, paths) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused-connection"), "true");
    let task = registry.task(&task_id).unwrap();
    {
        let mut state = task.state.lock().unwrap();
        let remote = state.metadata.remote.as_mut().unwrap();
        remote.accepted = false;
        remote.explicit_runon = true;
        remote.last_seq = Some(7);
        remote.retry = Some(DrainingRetry {
            request_digest: "unchanged".into(),
            attempts: 2,
            first_refusal_at_ms: Some(1),
            next_retry_at_ms: Some(2),
            waited_ms: Some(3),
            resubmitted_at_ms: Some(4),
        });
        registry.persist_task(&paths, &state.metadata).unwrap();
    }
    let mut sink = TaskSink::new(&registry, task.clone()).unwrap();
    let metadata = task.state.lock().unwrap().metadata.clone();
    let saved = fs::read(&paths.json).unwrap();
    let started = task.started;
    let launch_ready = task.remote_launch_ready.load(Ordering::Acquire);
    let last_checkpoint = sink.last_progress_persist;
    let mut recovery = ReattachBudget::new(dir.path(), &metadata);
    recovery.deadline();
    recovery.attempts = 3;
    let deadlines = (
        recovery.job_end,
        recovery.deadline,
        recovery.grace,
        recovery.attempts,
    );
    let mut consumer = exec::StreamConsumer::keyed_run(Some(7));
    let unknown = StreamRecord::Progress(
        Progress::new(ProgressPhase::Unknown("future_phase".into()), 40_000, None).unwrap(),
    );
    let previous_phase_note = task.snapshot(0).remote_phase_note;
    consumer.consume(unknown.clone(), &mut sink).unwrap();
    assert!(task.state.lock().unwrap().remote_admission_note.is_none());
    assert_eq!(task.snapshot(0).remote_phase_note, previous_phase_note);
    for elapsed in 0..10_000 {
        let bytes = serde_json::to_vec(&StreamRecord::Progress(
            Progress::new(ProgressPhase::Upload, elapsed, Some("latest".into())).unwrap(),
        ))
        .unwrap();
        consumer.consume_bytes(&bytes, &mut sink).unwrap();
        recovery.observe(&StreamProgress::AdmissionProgress);
        assert!(consumer.resume_point().is_none());
        let state = task.state.lock().unwrap();
        assert!(state.remote_admission_note.as_ref().unwrap().len() < 100);
    }
    consumer.consume(unknown, &mut sink).unwrap();
    let latest = "The remote job is uploading its files (10s so far): latest.";
    assert_eq!(task.snapshot(0).remote_phase_note.as_deref(), Some(latest));
    assert_eq!(
        registry
            .runon_launch_state(&task_id, "session")
            .unwrap()
            .admission_note
            .as_deref(),
        Some(latest)
    );
    assert_eq!(
        serde_json::to_value(&task.state.lock().unwrap().metadata).unwrap(),
        serde_json::to_value(&metadata).unwrap()
    );
    assert_eq!(fs::read(&paths.json).unwrap(), saved);
    assert_eq!(fs::metadata(&paths.stdout).unwrap().len(), 0);
    assert_eq!(task.started, started);
    assert_eq!(sink.last_progress_persist, last_checkpoint);
    assert_eq!(sink.output_progress.last_seq, Some(7));
    assert_eq!(
        (
            recovery.job_end,
            recovery.deadline,
            recovery.grace,
            recovery.attempts
        ),
        deadlines
    );
    assert_eq!(
        task.remote_launch_ready.load(Ordering::Acquire),
        launch_ready
    );

    consumer
        .consume(StreamRecord::Accepted(Accepted::new(id(), 1)), &mut sink)
        .unwrap();
    let cursor = consumer.resume_point();
    let accepted = serde_json::to_value(&task.state.lock().unwrap().metadata).unwrap();
    consumer
        .consume(
            StreamRecord::Progress(Progress::new(ProgressPhase::Connect, 3_000, None).unwrap()),
            &mut sink,
        )
        .unwrap();
    assert_eq!(consumer.resume_point(), cursor);
    assert_eq!(
        serde_json::to_value(&task.state.lock().unwrap().metadata).unwrap(),
        accepted
    );
    assert!(task.state.lock().unwrap().remote_admission_note.is_none());
    assert!(registry
        .runon_launch_state(&task_id, "session")
        .unwrap()
        .admission_note
        .is_none());
    assert!(!task
        .snapshot(0)
        .remote_phase_note
        .unwrap()
        .contains("so far"));
    // A sink reused by an attach also refuses late display updates.
    sink.admission_progress(&Progress::new(ProgressPhase::Connect, 3_000, None).unwrap());
    assert!(task.state.lock().unwrap().remote_admission_note.is_none());
}

fn shorten_reattach_budget(root: &Path) {
    // Scope the hook to one worker, so parallel remote tests keep the real budget.
    // Tests advance this logical budget after observing the relevant recovery
    // attempts; scheduler latency must not decide how many attempts fit.
    reattach_budgets()
        .lock()
        .unwrap()
        .insert(test_clock_key(root), Duration::from_millis(1_500));
}

// Remote workers run on separate runtimes. Advance their recovery budget only
// after a consumed record has reset it, without charging scheduler latency.

#[test]
fn runon_restart_reattach_log_capture_matches_symlinked_project_root() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    fs::create_dir(&project).unwrap();
    let alias = dir.path().join("project-alias");
    std::os::unix::fs::symlink(&project, &alias).unwrap();
    let (registry, task_id, _) = persist_accepted_with_snapshot(
        &alias,
        dir.path().join("unused-connection"),
        "printf capture-proof",
    );
    let task = registry.task(&task_id).unwrap();
    assert_ne!(
        task.state.lock().unwrap().metadata.project_root.as_deref(),
        Some(project.canonicalize().unwrap().as_path()),
    );
    let capture = Arc::new(Mutex::new(Vec::new()));
    remote_launch_log_captures()
        .lock()
        .unwrap()
        .insert(project.canonicalize().unwrap(), capture.clone());
    capture_remote_launch_logs(&task, || {
        crate::slog_info!("runon: aliased-root capture proof")
    });
    let lines = capture.lock().unwrap();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].contains("runon: aliased-root capture proof"),
        "{lines:?}"
    );
}

/// Fail unless `replay` left the task registered for `session`. Replay
/// registers every task it restores before it returns, so a missing task means
/// the record was skipped (deferred, refused or quarantined) rather than slow.
fn assert_replayed(registry: &BgTaskRegistry, task: &str, storage: &Path, replay: &str) {
    if registry.observed_status(task, "session", 0).is_none() {
        panic!(
            "{replay} did not register task {task} for session `session` (held under session {:?}); {}",
            registry.task_for_test(task).map(|held| held.session_id.clone()),
            storage_report(storage)
        );
    }
}

// Poll observable task state rather than inferring progress from elapsed time.
// The timeout is a hang guard; only a successful probe releases the caller.

#[tokio::test]
async fn exec_remote_bash_background_slots_are_shared_with_local_tasks() {
    use crate::bash_background::{HardKill, TaskSlot};

    let dir = tempfile::tempdir().unwrap();
    ensure_git_fixture(dir.path());
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
            None,
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
            None,
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
    // The fake executor runs on this thread's Tokio runtime. Keep it serving
    // the foreground worker's handshake while the local process group exits.
    let (kill_reached, release_kill) = install_kill_signal_gate_for_test(&local);
    let killing = registry.clone();
    let local_kill = tokio::task::spawn_blocking(move || killing.kill(&local, "session").unwrap());
    tokio::task::spawn_blocking(move || {
        kill_reached
            .recv_timeout(Duration::from_secs(30))
            .expect("local kill reached signal checkpoint");
    })
    .await
    .unwrap();
    wait_for(
        "fake executor must accept the foreground command while local kill is held",
        || {
            registry
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
                .map(|_| ())
        },
    )
    .await;
    release_kill.send(()).unwrap();
    local_kill.await.unwrap();
    registry
        .check_background_slot(TaskSlot::Background { max: 1 }, "session")
        .expect("a waiting remote foreground command holds no slot");
    registry.promote(&foreground, "session").unwrap();
    let refusal = registry
        .check_background_slot(TaskSlot::Background { max: 1 }, "session")
        .expect_err(&format!(
            "promoted task holds no slot: {:?}",
            registry.task(&foreground).map(|task| task.snapshot(0))
        ));
    assert!(refusal.contains(&foreground), "{refusal}");
    assert!(refusal.contains("remote-slot-proof"), "{refusal}");
    // Keep the runtime free to serve the fake executor's cancel response too.
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

#[tokio::test]
async fn memory_limit_delayed_fallback_keeps_submitted_cap_after_reload() {
    assert_delayed_fallback_cap(None).await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn memory_limit_attach_eperm_delayed_fallback_after_reload_enforces_watcher() {
    assert_delayed_fallback_cap(Some(libc::EPERM)).await;
}

async fn assert_delayed_fallback_cap(attach_error: Option<i32>) {
    #[cfg(target_os = "linux")]
    if attach_error.is_some()
        && !crate::bash_background::memory_limit::sampling_available_for_test()
    {
        super::super::memory_limit_tests::report_memory_runtime_skip(
            "memory_limit_attach_eperm_delayed_fallback_after_reload_enforces_watcher",
            "RSS sampling is unavailable for the forced fallback",
        );
        return;
    }
    let daemon = daemon(Script::GatedAcceptanceThenRefused, "exec-remote/v1").await;
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    ensure_git_fixture(project.path());
    let ctx = super::super::memory_limit_tests::memory_request_context(
        project.path(),
        storage.path(),
        Some(64),
    );
    let submitted = {
        let _pin = ctx.pin_config();
        super::super::memory_limit_tests::spawn_memory_command(
            &ctx,
            "memory-fallback-a",
            if attach_error.is_some() {
                super::super::memory_limit_tests::MEMORY_PRESSURE_COMMAND
            } else {
                "exit 0"
            },
            false,
            Some(launch(daemon.connection.clone())),
        )
    };
    assert!(submitted.success, "{submitted:?}");
    let task_id = submitted.data["task_id"].as_str().unwrap().to_owned();
    #[cfg(target_os = "linux")]
    let attach_fault = attach_error.map(|errno| {
        crate::bash_background::memory_limit::AttachFaultGuard::for_task(
            task_id.clone(),
            storage.path().join("denied-cgroup"),
            errno,
        )
    });
    let (saved_layout, saved_digest) = {
        let task = ctx.bash_background().task(&task_id).unwrap();
        let digest = task
            .state
            .lock()
            .unwrap()
            .metadata
            .remote
            .as_ref()
            .unwrap()
            .fallback_digest
            .clone()
            .unwrap();
        (
            resolve_task_layout(&task.paths.session_dir, &task_id).unwrap(),
            digest,
        )
    };

    let mut reloaded = ctx.config().as_ref().clone();
    reloaded.bash.memory_limit_mib = None;
    ctx.set_config(reloaded);
    let newer = {
        let _pin = ctx.pin_config();
        super::super::memory_limit_tests::spawn_memory_request(&ctx, "memory-fallback-b", None)
    };
    assert!(newer.success, "{newer:?}");
    daemon.acceptance_gate.notify_one();
    let done = terminal(ctx.bash_background(), &task_id).await;
    if attach_error.is_some() {
        assert_eq!(done.info.status, BgTaskStatus::Failed, "{done:?}");
        assert!(
            done.info
                .status_reason
                .as_deref()
                .unwrap_or_default()
                .contains("64 MiB memory cap"),
            "{done:?}"
        );
        assert!(
            done.output_preview
                .contains("Linux process-group RSS watcher; kernel memory controller unavailable"),
            "{done:?}"
        );
    } else {
        assert_eq!(done.exit_code, Some(0), "{done:?}");
    }
    #[cfg(target_os = "linux")]
    if let Some(fault) = &attach_fault {
        assert_eq!(fault.attempts(), 1);
        assert!(!storage.path().join("denied-cgroup").exists());
    }
    let task = ctx.bash_background().task(&task_id).unwrap();
    let state = task.state.lock().unwrap();
    assert!(state.metadata.local_fallback_started);
    assert_eq!(
        state
            .metadata
            .memory_policy
            .as_ref()
            .map(|policy| policy.mib),
        Some(64),
        "fallback lost the cap from its submitted request"
    );
    drop(state);
    let (_, _, _, _, saved_cap, _) =
        crate::sandbox_spawn::restore_local_launch(&saved_layout, &saved_digest).unwrap();
    assert_eq!(
        saved_cap,
        Some(64),
        "private fallback snapshot lost its admitted cap"
    );
}

#[tokio::test]
async fn memory_limit_replayed_fallback_keeps_saved_cap() {
    assert_replayed_fallback_cap(None).await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn memory_limit_attach_eperm_replayed_fallback_enforces_watcher() {
    assert_replayed_fallback_cap(Some(libc::EPERM)).await;
}

async fn assert_replayed_fallback_cap(attach_error: Option<i32>) {
    #[cfg(target_os = "linux")]
    if attach_error.is_some()
        && !crate::bash_background::memory_limit::sampling_available_for_test()
    {
        super::super::memory_limit_tests::report_memory_runtime_skip(
            "memory_limit_attach_eperm_replayed_fallback_enforces_watcher",
            "RSS sampling is unavailable for the forced fallback",
        );
        return;
    }
    let daemon = daemon(Script::AttachRefused, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let (original, task_id, paths) = persist_accepted_with_snapshot_cap(
        dir.path(),
        daemon.connection.clone(),
        if attach_error.is_some() {
            super::super::memory_limit_tests::MEMORY_PRESSURE_COMMAND
        } else {
            "exit 0"
        },
        Some(64),
    );
    drop(original);
    #[cfg(target_os = "linux")]
    let attach_fault = attach_error.map(|errno| {
        crate::bash_background::memory_limit::AttachFaultGuard::for_task(
            task_id.clone(),
            dir.path().join("denied-cgroup"),
            errno,
        )
    });
    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    let done = terminal(&restarted, &task_id).await;
    if attach_error.is_some() {
        assert_eq!(done.info.status, BgTaskStatus::Failed, "{done:?}");
        assert!(
            done.info
                .status_reason
                .as_deref()
                .unwrap_or_default()
                .contains("64 MiB memory cap"),
            "{done:?}"
        );
        assert!(
            done.output_preview
                .contains("Linux process-group RSS watcher; kernel memory controller unavailable"),
            "{done:?}"
        );
    } else {
        assert_eq!(done.exit_code, Some(0), "{done:?}");
    }
    #[cfg(target_os = "linux")]
    if let Some(fault) = &attach_fault {
        assert_eq!(fault.attempts(), 1);
        assert!(!dir.path().join("denied-cgroup").exists());
    }
    let metadata = read_task(&paths.json).unwrap();
    assert!(metadata.local_fallback_started);
    assert_eq!(
        metadata.memory_policy.as_ref().map(|policy| policy.mib),
        Some(64)
    );
}

#[tokio::test]
async fn bash_changed_files_new_runon_completion_keeps_local_window() {
    assert_new_remote_changed_files(false).await;
}

#[tokio::test]
async fn bash_changed_files_runon_local_fallback_completion_keeps_local_window() {
    assert_new_remote_changed_files(true).await;
}

async fn assert_new_remote_changed_files(fallback: bool) {
    let daemon = daemon(
        if fallback {
            Script::KnownRefused
        } else {
            Script::GatedStages { position: 1 }
        },
        "exec-remote/v1",
    )
    .await;
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    ensure_git_fixture(project.path());
    let ctx = crate::context::AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(project.path().into()),
            storage_dir: Some(storage.path().into()),
            bash: crate::config::BashConfig {
                changed_files: crate::config::BashChangedFiles::List,
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let capture = crate::bash_changed_files::Capture::start(&ctx).unwrap();
    let mut launch = launch(daemon.connection.clone());
    launch.explicit_runon = !fallback;
    // Only the local fallback shell creates local.txt. The explicit remote
    // command writes disposable output outside the project; its local change
    // report comes from the observer event injected below.
    let write = if fallback {
        "printf local-change > local.txt".to_owned()
    } else {
        format!(
            "printf remote-change > '{}'",
            storage.path().join("remote-output.txt").display()
        )
    };
    let command = format!(
        "{write}; {}",
        crate::test_gate::gate_exists("\"$CHANGED_FILES_RELEASE\"", "sleep 0.01")
    );
    let task_id = {
        let _capture = crate::bash_changed_files::scope(Some(capture.clone()));
        ctx.bash_background()
            .spawn_remote(
                launch,
                SpawnPlan::Unsandboxed,
                &command,
                resolve_posix_shell(),
                "session".into(),
                project.path().into(),
                HashMap::from([(
                    "CHANGED_FILES_RELEASE".into(),
                    storage.path().join("release").display().to_string(),
                )]),
                crate::bash_background::HardKill::from_timeout_ms(Some(20_000)),
                None,
                storage.path().into(),
                crate::bash_background::TaskSlot::Background { max: 10 },
                true,
                false,
                Some(project.path().into()),
            )
            .unwrap()
    };
    if fallback {
        wait_for("local fallback must write the project file", || {
            project.path().join("local.txt").exists().then_some(())
        })
        .await;
    } else {
        std::fs::write(project.path().join("local.txt"), "local observer change\n").unwrap();
    }
    // Inject a file-create event into the project's changed-files tracker while
    // the remote stream or fallback shell is held open, independently of OS delivery timing.
    ctx.watcher_counters().bash_changes.get().unwrap().event(
        &notify::Event::new(notify::EventKind::Create(notify::event::CreateKind::File))
            .add_path(project.path().canonicalize().unwrap().join("local.txt")),
    );
    std::fs::write(storage.path().join("release"), "").unwrap();
    daemon.output_gate.notify_one();
    daemon.completion_gate.notify_one();
    let done = terminal(ctx.bash_background(), &task_id).await;
    assert_eq!(done.exit_code, Some(0), "{done:?}");
    let completion = wait_for("completion must retain its local change report", || {
        ctx.bash_background()
            .drain_completions_for_session(Some("session"))
            .into_iter()
            .find(|completion| completion.task_id == task_id)
    })
    .await;
    let report = completion
        .changed_files
        .as_ref()
        .expect("capture lost at remote registration");
    assert!(
        report.files.iter().any(|file| file.path == "local.txt"),
        "{report:?}"
    );
    assert!(serde_json::to_value(&completion).unwrap()["_bash_changed_files"].is_object());

    let task = ctx.bash_background().task(&task_id).unwrap();
    let metadata = task.state.lock().unwrap().metadata.clone();
    let replay = registry();
    let _capture = crate::bash_changed_files::scope(Some(capture));
    replay
        .insert_rehydrated_task(metadata, task.paths.clone(), true)
        .unwrap();
    assert!(replay.task(&task_id).unwrap().changed_files.is_none());
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
async fn exec_remote_bash_unknown_reply_names_job_and_safe_rerun() {
    let daemon = daemon(Script::Lost, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let task = start(&registry, dir.path(), daemon.connection.clone());
    let done = terminal(&registry, &task).await;
    let expected = format!("remote outcome unknown (job {}); the remote job could not affect this machine or the network, so rerunning is safe; job {} may still be running on ck-motor and holding this workspace for up to its run timeout of 30s, and AFT can no longer fetch its result: a rerun in this workspace waits behind it and can be refused with queue_wait_exceeded, so wait for it to end or cancel it with bash_kill, then rerun", id(), id());
    assert_eq!(done.info.status_reason.as_deref(), Some(expected.as_str()));
    assert!(
        done.output_preview.contains(&expected),
        "{}",
        done.output_preview
    );
    assert!(!done.output_preview.contains("never rerun"));
}

#[test]
fn exec_remote_bash_unknown_reply_without_job_omits_status_hint() {
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, _) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf unused");
    let task = registry.task(&task_id).unwrap();
    {
        let mut state = task.state.lock().unwrap();
        state.metadata.remote.as_mut().unwrap().job_id = None;
        state.metadata.execution_note = Some("ran remotely on ck-motor".into());
    }
    registry.remote_terminal(
        &task,
        Verdict::OutcomeUnknown,
        Some("remote route interrupted before acceptance".into()),
    );
    let done = registry.observed_status(&task_id, "session", 8192).unwrap();
    let expected = "remote outcome unknown; the remote job could not affect this machine or the network, so rerunning is safe";
    assert_eq!(done.info.status_reason.as_deref(), Some(expected));
    assert!(
        done.output_preview.contains(expected),
        "{}",
        done.output_preview
    );
    assert!(!done.output_preview.contains("exec.status"));
    assert!(!done.output_preview.contains("(job"));
    assert_eq!(
        done.output_preview.lines().next(),
        Some("ran remotely on ck-motor")
    );
    assert!(!done.output_preview.contains("never rerun"));
    assert!(done
        .output_preview
        .contains("remote route interrupted before acceptance"));
    let completion = registry.completion_snapshot_for_task(&task).unwrap();
    assert_eq!(
        completion.output_preview.lines().next(),
        Some("ran remotely on ck-motor")
    );
}

#[test]
fn remote_execution_header_without_a_job_id_keeps_its_existing_text() {
    let mut remote = RemoteTask {
        copy_warning: false,
        submission: None,
        accepted_platform: None,
        explicit_runon: false,
        connection_file: None,
        harness: "runner".into(),
        session: "session".into(),
        requested_platform: None,
        requested_vcpus: Some(4),
        requested_network: false,
        granted_network: false,
        accepted: true,
        started: None,
        job_id: None,
        last_seq: None,
        gap_recovery: None,
        undelivered_output: Vec::new(),
        stdout_len: 0,
        stderr_len: 0,
        unknown_len: 0,
        output_limit_dropped_bytes: 0,
        cancel_requested: false,
        cancel_pending: false,
        owner_pause_cancel: None,
        terminal: None,
        pre_start_refusal: None,
        fallback_digest: None,
        env_not_forwarded: None,
        queue_position: None,
        queue_wait_ms: None,
        output_received: false,
        retry: None,
        output_incomplete: None,
        workspace_holder: None,
        return_paths: Vec::new(),
        returned_files: None,
        return_first_seq: None,
        return_staging: Default::default(),
        return_max_bytes: exec::return_max_bytes(),
        return_cap_retried: false,
        expired_gap_unverified: Vec::new(),
    };
    assert_eq!(
        remote_execution_note(&remote),
        "ran remotely on ck-motor (4 vCPUs requested)"
    );
    remote.requested_vcpus = None;
    assert_eq!(remote_execution_note(&remote), "ran remotely on ck-motor");
}

#[test]
fn runon_network_lost_acceptance_does_not_claim_rerun_safe() {
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, _) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf unused");
    let task = registry.task(&task_id).unwrap();
    {
        let mut state = task.state.lock().unwrap();
        let remote = state.metadata.remote.as_mut().unwrap();
        remote.job_id = None;
        remote.requested_network = true;
        remote.accepted = false;
    }
    registry.remote_terminal(
        &task,
        Verdict::OutcomeUnknown,
        Some("remote acceptance lost".into()),
    );
    let done = registry.observed_status(&task_id, "session", 8192).unwrap();
    assert!(
        done.output_preview.contains("check before rerunning"),
        "{done:?}"
    );
    assert!(
        !done.output_preview.contains("rerunning is safe"),
        "{done:?}"
    );
}

#[test]
fn exec_remote_bash_local_unknown_reply_is_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, _) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf unused");
    let task = registry.task(&task_id).unwrap();
    task.state.lock().unwrap().metadata.remote = None;
    let expected = "the command started locally; AFT lost track of it: injected running-metadata failure after spawn; never rerun";
    registry.remote_terminal(&task, Verdict::OutcomeUnknown, Some(expected.into()));
    let done = registry.observed_status(&task_id, "session", 8192).unwrap();
    assert_eq!(done.info.status_reason.as_deref(), Some(expected));
    assert!(done.output_preview.contains(expected));
    assert!(!done.output_preview.contains("rerunning is safe"));
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
    let clock = PausedReattachClock::new(dir.path());
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    daemon.wait_for_requests("exec.attach", 2).await;
    clock.advance(Duration::from_millis(1_500));
    let done = tokio::time::timeout(Duration::from_secs(30), terminal(&registry, &task_id))
        .await
        .expect("empty attaches must exhaust the recovery budget");
    assert_eq!(done.info.status, BgTaskStatus::FateUnknown);
    assert_eq!(
        done.info.status_reason.as_deref(),
        Some(format!("remote outcome unknown (job {}); the remote job could not affect this machine or the network, so rerunning is safe; job {} may still be running on ck-motor and holding this workspace for up to its run timeout of 30s, and AFT can no longer fetch its result: a rerun in this workspace waits behind it and can be refused with queue_wait_exceeded, so wait for it to end or cancel it with bash_kill, then rerun", id(), id()).as_str())
    );
    assert!(done.output_preview.contains(&format!(
        "job {} could not be re-attached before its own time limit passed",
        id()
    )));
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
    let daemon = daemon(Script::GappedAttach, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    shorten_reattach_budget(dir.path());
    let clock = PausedReattachClock::new(dir.path());
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    let task = registry.task(&task_id).unwrap();
    for seq in 0..3 {
        daemon
            .wait_for_requests("exec.attach", seq as usize + 1)
            .await;
        wait_for(
            "attach output must be consumed before releasing its end",
            // Output is visible before the coalesced replay cursor becomes durable.
            || (fs::metadata(&task.paths.stdout).unwrap().len() == seq + 1).then_some(()),
        )
        .await;
        // Accepted and the initial stream end each reset once, then each
        // attach contributes an output record and (except the last) an end.
        clock.after_resets(3 + seq * 2).await;
        clock.advance(Duration::from_millis(1_501));
        assert_eq!(
            registry
                .observed_status(&task_id, "session", 8192)
                .unwrap()
                .info
                .status,
            BgTaskStatus::Running,
            "an open attach is not subject to an output-idle deadline"
        );
        daemon.completion_gate.notify_one();
    }
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
    let daemon = daemon(Script::GappedCancel, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    shorten_reattach_budget(dir.path());
    let _clock = PausedReattachClock::new(dir.path());
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    let task = registry.task(&task_id).unwrap();
    wait_for(
        "acceptance must be consumed before recording cancellation",
        || {
            task.state
                .lock()
                .unwrap()
                .metadata
                .remote
                .as_ref()
                .unwrap()
                .job_id
                .map(|_| ())
        },
    )
    .await;
    assert!(registry.record_remote_cancel(&task).unwrap());
    daemon.wait_for_requests("exec.cancel", 1).await;
    daemon.wait_for_requests("exec.attach", 1).await;
    daemon.completion_gate.notify_one();
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
    assert_eq!(
        log.iter()
            .filter(|(_, b)| b["method"] == "exec.cancel")
            .count(),
        1,
        "terminal recovery must not resend an acknowledged job cancel"
    );
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
    let clock = PausedReattachClock::new(dir.path());
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    daemon.wait_for_requests("exec.attach", 1).await;
    clock.advance(Duration::from_millis(1_500));
    let done = tokio::time::timeout(Duration::from_secs(30), terminal(&registry, &task_id))
        .await
        .expect("failed attach calls and reconnects must exhaust the recovery budget");
    assert_eq!(done.info.status, BgTaskStatus::FateUnknown);
    assert!(done
        .info
        .status_reason
        .unwrap()
        .contains("rerunning is safe"));
    assert!(done.output_preview.contains(&format!(
        "job {} could not be re-attached before its own time limit passed (",
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
    let clock = PausedReattachClock::new(dir.path());
    registry.resume_remote_task(&task_id).unwrap();
    clock.after_waits(1).await;
    clock.advance(Duration::from_millis(1_500));
    let done = tokio::time::timeout(Duration::from_secs(30), terminal(&registry, &task_id))
        .await
        .expect("restarting an accepted job cannot retry connect forever");
    assert_eq!(done.info.status, BgTaskStatus::FateUnknown);
    assert!(done
        .info
        .status_reason
        .unwrap()
        .contains("rerunning is safe"));
    assert!(done.output_preview.contains(&format!(
        "job {} could not be re-attached before its own time limit passed (",
        id()
    )));
    assert_eq!(fs::read(&paths.stdout).unwrap(), b"");
}

/// ck-motor restarted mid-job and its provider stayed away longer than the
/// five minutes AFT used to allow, while the job kept running on the build
/// server. AFT must keep re-attaching (never resubmitting) and deliver the
/// job's real result once the runner is back.
#[tokio::test]
async fn exec_remote_bash_route_outage_past_five_minutes_still_delivers_result() {
    let daemon = crate::exec_remote::wire_fixture::outage_daemon().await;
    let dir = tempfile::tempdir().unwrap();
    let clock = PausedReattachClock::new(dir.path());
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    daemon.after_refused_connects(3).await;
    // Past the old five-minute cap, short of this job's own limits (a 5m queue
    // wait limit plus a 30s run timeout, plus 5m of grace).
    clock.advance(Duration::from_secs(6 * 60));
    let refused = *daemon.refused_connects.borrow();
    let still_trying = wait_for("AFT must keep reconnecting during the outage", || {
        let snapshot = registry.observed_status(&task_id, "session", 8192).unwrap();
        if snapshot.info.status.is_terminal() {
            return Some(Err(snapshot));
        }
        (*daemon.refused_connects.borrow() >= refused + 3).then_some(Ok(()))
    })
    .await;
    if let Err(snapshot) = still_trying {
        panic!(
            "AFT gave up six minutes into the outage: {:?}: {}",
            snapshot.info.status, snapshot.output_preview
        );
    }
    daemon.restore();
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");
    assert_eq!(done.exit_code, Some(0));
    let task = registry.task(&task_id).unwrap();
    assert_eq!(fs::read(&task.paths.stdout).unwrap(), b"AB");
    assert_eq!(daemon.methods("exec.run").len(), 1, "never resubmitted");
    let attaches = daemon.methods("exec.attach");
    assert_eq!(attaches.last().unwrap()["params"]["from_seq"], 2);
    drop(clock);
}

#[tokio::test]
async fn exec_remote_bash_keyed_disconnect_replays_job_finished_while_detached() {
    use crate::exec_remote::wire_fixture::{outage_daemon_with_replay, OutageReplay};
    let daemon = outage_daemon_with_replay(OutageReplay::Running).await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    daemon.after_refused_connects(1).await;
    daemon.hold_next_connect();
    daemon.after_held_connect().await;
    daemon.finish_job();
    daemon.restore();
    daemon.release_held_connect();
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");
    assert_eq!(done.exit_code, Some(0));
    let task = registry.task(&task_id).unwrap();
    assert_eq!(fs::read(&task.paths.stdout).unwrap(), b"AB");
    assert_eq!(daemon.methods("exec.run").len(), 1);
    let attaches = daemon.methods("exec.attach");
    assert_eq!(attaches.len(), 1);
    assert_eq!(attaches[0]["params"]["from_seq"], 2);
}

async fn keyed_disconnect_exhausts_allowance(
    replay: crate::exec_remote::wire_fixture::OutageReplay,
) {
    let daemon = crate::exec_remote::wire_fixture::outage_daemon_with_replay(replay).await;
    let dir = tempfile::tempdir().unwrap();
    shorten_reattach_budget(dir.path());
    let clock = PausedReattachClock::new(dir.path());
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    daemon.after_refused_connects(1).await;
    daemon.restore();
    wait_for("failed attaches must retry within the allowance", || {
        (daemon.methods("exec.attach").len() >= 2).then_some(())
    })
    .await;
    clock.advance(Duration::from_millis(1_500));
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::FateUnknown, "{done:?}");
    assert_eq!(done.info.status_reason.as_deref(), Some(format!("remote outcome unknown (job {}); the remote job could not affect this machine or the network, so rerunning is safe; job {} may still be running on ck-motor and holding this workspace for up to its run timeout of 30s, and AFT can no longer fetch its result: a rerun in this workspace waits behind it and can be refused with queue_wait_exceeded, so wait for it to end or cancel it with bash_kill, then rerun", id(), id()).as_str()));
    assert!(done.output_preview.contains(&format!(
        "job {} could not be re-attached before its own time limit passed",
        id()
    )));
    remote_worker_exited(&task_id).await;
    let attempts = daemon.methods("exec.attach").len();
    assert!(attempts >= 2);
    assert_eq!(
        daemon.methods("exec.run").len(),
        1,
        "never resubmit a known job"
    );
    let task = registry.task(&task_id).unwrap();
    assert_eq!(fs::read(&task.paths.stdout).unwrap(), b"A");
    assert_eq!(
        read_task(&task.paths.json).unwrap().status,
        BgTaskStatus::FateUnknown
    );
}

#[tokio::test]
async fn exec_remote_bash_keyed_disconnect_job_not_found_keeps_unknown_text() {
    keyed_disconnect_exhausts_allowance(crate::exec_remote::wire_fixture::OutageReplay::NotFound)
        .await;
}

#[tokio::test]
async fn exec_remote_bash_keyed_disconnect_repeated_closes_stop_at_allowance() {
    keyed_disconnect_exhausts_allowance(crate::exec_remote::wire_fixture::OutageReplay::Close)
        .await;
}

#[tokio::test]
async fn exec_remote_bash_keyed_disconnect_unknown_attach_terminal_is_not_resubmitted() {
    use crate::exec_remote::wire_fixture::{outage_daemon_with_replay, OutageReplay};
    let daemon = outage_daemon_with_replay(OutageReplay::Unknown).await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    daemon.after_refused_connects(1).await;
    daemon.restore();
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::FateUnknown, "{done:?}");
    assert!(done
        .info
        .status_reason
        .unwrap()
        .starts_with(&format!("remote outcome unknown (job {})", id())));
    assert_eq!(daemon.methods("exec.run").len(), 1);
    assert_eq!(daemon.methods("exec.attach").len(), 1);
}

/// The common agent call: `runon` with no `timeout`. It gets the 30-minute
/// background default as its run timeout, which AFT records and sends to the
/// runner, so a restart outage far past five minutes must still end with the
/// job's result.
#[tokio::test]
async fn exec_remote_bash_untimed_runon_survives_an_outage_past_five_minutes() {
    let daemon = crate::exec_remote::wire_fixture::outage_daemon().await;
    let dir = tempfile::tempdir().unwrap();
    let clock = PausedReattachClock::new(dir.path());
    let registry = registry();
    // `bash` without `timeout` builds its hard kill exactly like this.
    let task_id = start_with_hard_kill(
        &registry,
        dir.path(),
        daemon.connection.clone(),
        crate::bash_background::HardKill::from_timeout_ms(None),
    );
    let task = registry.task(&task_id).unwrap();
    // Recorded no lower than the 30-minute default that was sent: inserting
    // the record can raise a default kill to one worker wait from now.
    let recorded = task.state.lock().unwrap().metadata.timeout_ms;
    assert!(
        recorded.is_some_and(|ms| ms >= 30 * 60 * 1000),
        "an untimed runon records at least the background default as its run timeout: {recorded:?}"
    );
    daemon.after_refused_connects(3).await;
    assert_eq!(
        daemon.methods("exec.run")[0]["params"]["timeout"],
        30 * 60,
        "the runner enforces the same limit AFT records"
    );
    // Far past the old five-minute cap, short of 5m queue + 30m run + 5m grace.
    clock.advance(Duration::from_secs(25 * 60));
    let refused = *daemon.refused_connects.borrow();
    let still_trying = wait_for("AFT must keep reconnecting during the outage", || {
        let snapshot = registry.observed_status(&task_id, "session", 8192).unwrap();
        if snapshot.info.status.is_terminal() {
            return Some(Err(snapshot));
        }
        (*daemon.refused_connects.borrow() >= refused + 3).then_some(Ok(()))
    })
    .await;
    if let Err(snapshot) = still_trying {
        panic!(
            "AFT gave up 25 minutes into the outage of an untimed runon: {:?}: {}",
            snapshot.info.status, snapshot.output_preview
        );
    }
    daemon.restore();
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");
    assert_eq!(fs::read(&task.paths.stdout).unwrap(), b"AB");
    assert_eq!(daemon.methods("exec.run").len(), 1, "never resubmitted");
    drop(clock);
}

/// The same outage outlasting the job's own limits ends the wait, and the
/// result says why, names the job and gives advice the agent can follow.
#[tokio::test]
async fn exec_remote_bash_route_outage_gives_up_after_the_jobs_own_limits() {
    let daemon = crate::exec_remote::wire_fixture::outage_daemon().await;
    let dir = tempfile::tempdir().unwrap();
    let clock = PausedReattachClock::new(dir.path());
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    daemon.after_refused_connects(3).await;
    clock.advance(Duration::from_secs(12 * 60));
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::FateUnknown, "{done:?}");
    assert!(
        done.output_preview.contains(&format!(
            "job {} could not be re-attached before its own time limit passed (a queue wait allowance of 5m and its run timeout of 30s have passed since AFT submitted it, plus 5m of grace); command not rerun",
            id()
        )),
        "{}",
        done.output_preview
    );
    let reason = done.info.status_reason.unwrap();
    assert!(
        reason.contains(&format!("job {} may still be running on ck-motor", id())),
        "{reason}"
    );
    assert!(
        reason.contains("wait for it to end or cancel it with bash_kill, then rerun"),
        "{reason}"
    );
    assert!(!done.output_preview.contains("exec.status"));
    assert!(!done.output_preview.contains("local-proof"));
    assert_eq!(daemon.methods("exec.run").len(), 1, "never resubmitted");
}

#[test]
fn reattach_backoff_doubles_from_a_tenth_of_a_second_up_to_thirty_seconds() {
    let schedule: Vec<_> = (0..12).map(reattach_backoff).collect();
    assert_eq!(
        schedule,
        [100, 200, 400, 800, 1_600, 3_200, 6_400, 12_800, 25_600, 30_000, 30_000, 30_000]
            .map(Duration::from_millis)
    );
    assert_eq!(reattach_backoff(u32::MAX), Duration::from_secs(30));
}

#[tokio::test]
async fn large_queued_runon_recovers_after_the_small_queue_deadline() {
    let daemon = crate::exec_remote::wire_fixture::queued_outage_daemon().await;
    let dir = tempfile::tempdir().unwrap();
    let clock = PausedReattachClock::new(dir.path());
    let registry = registry();
    let mut remote = launch(daemon.connection.clone());
    remote.explicit_runon = true;
    remote.requested_vcpus = Some(16);
    let task_id = start_with_launch(
        &registry,
        dir.path(),
        remote,
        crate::bash_background::HardKill::After(DEFAULT_BG_TIMEOUT),
    );
    daemon.after_refused_connects(3).await;
    assert_eq!(
        registry.remote_progress(&task_id, "session").unwrap().phase,
        RemotePhase::Queued { position: 1 }
    );
    let run = &daemon.methods("exec.run")[0];
    assert_eq!(run["params"]["weight_hint"], 16);
    assert_eq!(run["params"]["timeout"], 1800);
    // An explicit timeout now carries the same queue wait limit.
    assert_eq!(
        run["params"]
            .get("queue_wait_limit_s")
            .and_then(serde_json::Value::as_u64),
        Some(1800),
        "an explicit timeout also bounds admission"
    );

    // Pass the old 5m queue + 30m run + 5m grace deadline without real waiting.
    clock.advance(Duration::from_secs(41 * 60));
    let refused = *daemon.refused_connects.borrow();
    let still_trying = wait_for("the queued job must still be recoverable", || {
        let snapshot = registry.observed_status(&task_id, "session", 8192).unwrap();
        if snapshot.info.status.is_terminal() {
            return Some(Err(snapshot));
        }
        (*daemon.refused_connects.borrow() >= refused + 3).then_some(Ok(()))
    })
    .await;
    assert!(
        still_trying.is_ok(),
        "queued job abandoned: {still_trying:?}"
    );

    let metadata = registry
        .task(&task_id)
        .unwrap()
        .state
        .lock()
        .unwrap()
        .metadata
        .clone();
    let submission = KeyedSubmission {
        key: "queued-submission".into(),
        created_at_ms: metadata.started_at,
        request_digest: "digest".into(),
        attempts: 1,
        recovery_started_at_ms: None,
        fresh_key_used: false,
        resuming: false,
        reattached: false,
        refusal: None,
    };
    assert_eq!(
        submission.expires_at(&metadata),
        metadata.started_at + 60 * 60 * 1000,
        "a lost Accepted must remain recoverable for the same queue and runtime allowance"
    );
    daemon.restore();
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");
    assert_eq!(
        fs::read(&registry.task(&task_id).unwrap().paths.stdout).unwrap(),
        b"AB"
    );
    assert_eq!(
        daemon.methods("exec.run").len(),
        1,
        "recovery must not submit another job"
    );
    assert!(!daemon.methods("exec.attach").is_empty());
}

#[tokio::test]
async fn large_queued_runon_recovery_ends_after_queue_runtime_and_grace() {
    let daemon = crate::exec_remote::wire_fixture::queued_outage_daemon().await;
    let dir = tempfile::tempdir().unwrap();
    let clock = PausedReattachClock::new(dir.path());
    let registry = registry();
    let mut remote = launch(daemon.connection.clone());
    remote.explicit_runon = true;
    remote.requested_vcpus = Some(16);
    let task_id = start_with_launch(
        &registry,
        dir.path(),
        remote,
        crate::bash_background::HardKill::After(DEFAULT_BG_TIMEOUT),
    );
    daemon.after_refused_connects(3).await;
    clock.advance(Duration::from_secs(66 * 60));
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::FateUnknown, "{done:?}");
    assert!(done.output_preview.contains(&format!(
        "job {} could not be re-attached before its own time limit passed (a queue wait allowance of 30m and its run timeout of 30m have passed since AFT submitted it, plus 5m of grace); command not rerun", id()
    )), "{}", done.output_preview);
    assert_eq!(daemon.methods("exec.run").len(), 1);
}

#[tokio::test]
async fn short_queued_runon_keeps_the_five_minute_queue_allowance() {
    let daemon = crate::exec_remote::wire_fixture::queued_outage_daemon().await;
    let dir = tempfile::tempdir().unwrap();
    let clock = PausedReattachClock::new(dir.path());
    let registry = registry();
    let mut remote = launch(daemon.connection.clone());
    remote.explicit_runon = true;
    remote.requested_vcpus = Some(8);
    let task_id = start_with_launch(
        &registry,
        dir.path(),
        remote,
        crate::bash_background::HardKill::After(Duration::from_secs(30)),
    );
    daemon.after_refused_connects(3).await;
    let metadata = registry
        .task(&task_id)
        .unwrap()
        .state
        .lock()
        .unwrap()
        .metadata
        .clone();
    let (remaining, _, _) = ReattachBudget::until_job_end(&metadata);
    assert!(
        (Duration::from_secs(325)..=Duration::from_secs(330)).contains(&remaining),
        "{remaining:?}"
    );
    clock.advance(Duration::from_secs(11 * 60));
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::FateUnknown, "{done:?}");
    assert!(done.output_preview.contains(&format!(
        "job {} could not be re-attached before its own time limit passed (a queue wait allowance of 5m and its run timeout of 30s have passed since AFT submitted it, plus 5m of grace); command not rerun", id()
    )), "{}", done.output_preview);
    assert_eq!(daemon.methods("exec.run").len(), 1);
}

#[test]
fn workspace_owner_holders_reads_ck_motor_queue_report() {
    let holder: Uuid = "0192a64a-1234-7000-8000-0000000000aa".parse().unwrap();
    let other: Uuid = "0192a64a-1234-7000-8000-0000000000bb".parse().unwrap();
    let detail = crate::exec_remote::wire_fixture::queue_owner_detail(holder);
    assert_eq!(workspace_owner_holders(&detail), [holder, other]);
    // Another blocker is not a workspace owner, even with running holders.
    let other_phase = detail.replacen("waiting_on=workspace owner", "waiting_on=permits", 1);
    assert!(workspace_owner_holders(&other_phase).is_empty());
    // A checkout held because another workspace's job mounts it as a sibling,
    // or a sibling user held back behind an earlier queued job of the same
    // repository, is never this session's own earlier job, even when the
    // report lists the same job ids. These are ck-motor's phase names.
    for phase in ["sibling user of this checkout", "queued workspace job"] {
        let sibling = detail.replacen(
            "waiting_on=workspace owner;",
            &format!(
                "waiting_on={phase}; sibling_users=[job={holder} workspace=/work/other permits=4];"
            ),
            1,
        );
        assert!(workspace_owner_holders(&sibling).is_empty(), "{phase}");
    }
    assert!(workspace_owner_holders("maintenance").is_empty());
}

async fn queue_wait_refusal(holder: Uuid, earlier_job: Uuid) -> (BgTaskSnapshot, String, String) {
    let daemon = daemon(Script::QueueWaitOwner { holder }, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    ensure_git_fixture(dir.path());
    let (registry, earlier, _) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf earlier");
    registry
        .task(&earlier)
        .unwrap()
        .state
        .lock()
        .unwrap()
        .metadata
        .remote
        .as_mut()
        .unwrap()
        .job_id = Some(earlier_job);
    let task_id = start_runon_retry(&registry, dir.path(), daemon.connection.clone());
    let done = terminal(&registry, &task_id).await;
    let task = registry.task(&task_id).unwrap();
    // A restarted reader renders the refusal from the saved record alone.
    let durable = read_task(&task.paths.json).unwrap();
    let restarted = remote_refusal(&durable).unwrap().message;
    (done, restarted, earlier)
}

#[tokio::test]
async fn runon_queue_wait_refusal_names_this_sessions_earlier_job_in_the_workspace() {
    let holder: Uuid = "0192a64a-1234-7000-8000-0000000000aa".parse().unwrap();
    let (done, restarted, earlier) = queue_wait_refusal(holder, holder).await;
    let expected = format!(
        "runon refused: your earlier job {holder} in this workspace is still running (task {earlier}); this command waited behind it until the runner's queue wait limit passed and was not run; wait for task {earlier} to end (bash_status shows it), then retry; remote job 0192a64a-1234-7000-8000-000000000001"
    );
    assert_eq!(done.info.status_reason.as_deref(), Some(expected.as_str()));
    assert_eq!(restarted, expected);
}

#[tokio::test]
async fn runon_queue_wait_refusal_behind_another_owner_keeps_the_runner_report() {
    let holder: Uuid = "0192a64a-1234-7000-8000-0000000000aa".parse().unwrap();
    let unrelated: Uuid = "0192a64a-1234-7000-8000-0000000000cc".parse().unwrap();
    let (done, restarted, _) = queue_wait_refusal(holder, unrelated).await;
    let reason = done.info.status_reason.unwrap();
    assert!(
        reason.starts_with(
            "runon refused: remote refused: queue_wait_exceeded (waiting_on=workspace owner;"
        ),
        "{reason}"
    );
    assert!(!reason.contains("your earlier job"), "{reason}");
    assert_eq!(restarted, reason);
}

#[tokio::test]
async fn bash_background_remote_restart_without_secrets_keeps_status_watch_and_completion_preview()
{
    let daemon = daemon(Script::Utf8, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    ensure_git_fixture(dir.path());
    let original = registry();
    let id = original
        .spawn_remote(
            launch(daemon.connection.clone()),
            SpawnPlan::Host {
                shell_path: resolve_posix_shell(),
                environment: Default::default(),
            },
            "printf output",
            resolve_posix_shell(),
            "session".into(),
            dir.path().into(),
            HashMap::new(),
            crate::bash_background::HardKill::After(Duration::from_secs(30)),
            None,
            dir.path().into(),
            crate::bash_background::TaskSlot::Background { max: 10 },
            true,
            false,
            Some(dir.path().into()),
        )
        .unwrap();
    terminal(&original, &id).await;
    let task = original.task(&id).unwrap();
    let mut metadata = read_task(&task.paths.json).unwrap();
    assert_eq!(metadata.completion_has_env_secrets, Some(false));
    assert_eq!(metadata.completion_has_non_ticket_env_secrets, Some(false));
    assert_eq!(metadata.completion_has_secret_placeholders, Some(false));
    // Rebuild the completion from the task's captured output bytes, not a saved notice.
    metadata.completion_preview = None;
    metadata.status = BgTaskStatus::Running;
    metadata.finished_at = None;
    metadata.duration_ms = None;
    let restarted = registry();
    restarted
        .insert_rehydrated_task(metadata, task.paths.clone(), true)
        .unwrap();
    let reloaded = restarted.task(&id).unwrap();
    {
        let mut state = reloaded.lock_state().unwrap();
        assert_eq!(state.metadata.status, BgTaskStatus::Running);
        // Simulate the runner reporting completion after task recovery.
        state
            .metadata
            .mark_terminal(BgTaskStatus::Completed, Some(0), None);
    }
    let status = restarted
        .status(&id, "session", None, None, RUNNING_OUTPUT_PREVIEW_BYTES)
        .unwrap();
    assert!(status.output_preview.contains('€'));
    assert!(status.output_preview.contains('😀'));
    let (matched, context) = restarted.render_watch_preview(&id, "€\n".as_bytes(), 0..3);
    assert_eq!(matched, "€");
    assert!(!context.contains("withheld"));
    let completion = restarted.completion_snapshot_for_task(&reloaded).unwrap();
    assert!(completion.output_preview.contains('€'));
    assert!(completion.output_preview.contains('😀'));
    assert!(!completion.output_preview.contains("withheld"));
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
        "3\n0\n"
    );
    assert_eq!(
        done.output_preview.lines().next(),
        Some("ran remotely on ck-motor (job 0192a64a-1234-7000-8000-000000000001)")
    );
    // The changed file is named after the output, and it was not copied back.
    // The runner never fills the Git, untracked-file, or ignored-write
    // reports, so their absence stays silent.
    assert!(
        done.output_preview.ends_with(
            "These files changed on the server and were NOT copied back:\n  generated.txt"
        ),
        "{}",
        done.output_preview
    );
    assert!(!dir.path().join("generated.txt").exists());
}

#[tokio::test]
async fn exec_remote_bash_from_a_jailed_worker_is_submitted_unchanged() {
    // A jailed worker's shell plan is a native-launcher plan. runon still
    // snapshots and submits from AFT itself, outside the shell sandbox; the
    // launcher plan only backs a local fallback, which this run never needs.
    let daemon = daemon(Script::Utf8, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    ensure_git_fixture(dir.path());
    let temp = dir.path().join("sandbox-temp");
    fs::create_dir_all(&temp).unwrap();
    let launcher = temp.join("unexpected-local-launch");
    fs::write(&launcher, b"#!/bin/sh\nexit 99\n").unwrap();
    fs::set_permissions(
        &launcher,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    let profile = crate::sandbox_profile::SandboxProfile::build(
        vec![dir.path().into()],
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        temp,
    )
    .unwrap();
    let registry = registry();
    let task = crate::sandbox_spawn::with_worker_jail(true, || {
        registry
            .spawn_remote(
                launch(daemon.connection.clone()),
                SpawnPlan::launcher_for_test(profile, launcher),
                "printf local-proof",
                resolve_posix_shell(),
                "session".into(),
                dir.path().into(),
                HashMap::new(),
                crate::bash_background::HardKill::After(Duration::from_secs(30)),
                None,
                dir.path().into(),
                crate::bash_background::TaskSlot::Background { max: 10 },
                true,
                false,
                Some(dir.path().into()),
            )
            .unwrap()
    });
    let done = terminal(&registry, &task).await;
    let task = registry.task(&task).unwrap();
    assert_eq!(fs::read(&task.paths.stdout).unwrap(), "€".as_bytes());
    assert_eq!(
        done.output_preview.lines().next(),
        Some("ran remotely on ck-motor (job 0192a64a-1234-7000-8000-000000000001)")
    );
    assert!(!done.output_preview.contains("local-proof"));
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
            None,
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
        assert_replayed(&restarted, &task_id, dir.path(), "the first replay");
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
        assert_replayed(&replay, &task_id, dir.path(), "the second replay");
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
    wait_for(
        "acceptance must be consumed before killing the task",
        || {
            registry
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
                .map(|_| ())
        },
    )
    .await;
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
async fn remote_cancel_before_dispatch_reports_never_started() {
    let dir = tempfile::tempdir().unwrap();
    ensure_git_fixture(dir.path());
    let daemon = daemon(Script::GatedRoute, "exec-remote/v1").await;
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    wait_for("route setup must hold the job before dispatch", || {
        daemon
            .log
            .lock()
            .unwrap()
            .iter()
            .any(|(_, body)| body["op"] == "route.open")
            .then_some(())
    })
    .await;
    let killing = registry.clone();
    let killed_id = task_id.clone();
    let operation =
        tokio::task::spawn_blocking(move || killing.kill(&killed_id, "session").unwrap());
    wait_for("kill intent must precede remote dispatch", || {
        registry
            .task(&task_id)
            .unwrap()
            .state
            .lock()
            .unwrap()
            .metadata
            .remote
            .as_ref()
            .unwrap()
            .cancel_requested
            .then_some(())
    })
    .await;
    daemon.acceptance_gate.notify_one();
    let done = operation.await.unwrap();
    assert_eq!(done.info.status, BgTaskStatus::Killed);
    assert_eq!(done.remote_job_id, None);
    assert!(
        done.output_preview
            .starts_with("cancelled before remote dispatch to ck-motor; command never started"),
        "{done:?}"
    );
    assert!(!done.output_preview.contains("ran remotely"), "{done:?}");
    assert!(exec_runs(&daemon).is_empty());
    assert!(
        cancel_requests(&daemon).is_empty(),
        "nothing was submitted to cancel"
    );
}

fn cancel_terminal(outcome: Outcome, killed: Option<Killed>, ran: Ran) -> TerminalRecord {
    let mut terminal = TerminalRecord::new(id(), outcome, 1, 0, 0);
    terminal.killed = killed;
    terminal.ran = Some(ran);
    terminal
}

#[tokio::test]
async fn remote_cancel_placement_without_job_id_expires_with_truthful_warning() {
    let daemon = crate::exec_remote::wire_fixture::daemon_with_capabilities(
        Script::PlacementWait { job_id: false },
        &["exec-remote/v1", exec::types::PROGRESS_CAPABILITY],
        1,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    shorten_reattach_budget(dir.path());
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    wait_for("placement progress before cancellation", || {
        registry
            .task(&task_id)
            .unwrap()
            .state
            .lock()
            .unwrap()
            .remote_admission_note
            .clone()
    })
    .await;
    let (pending, _) = timed_kill(&registry, &task_id, Some(Duration::from_millis(250))).await;
    let pending = pending.unwrap_err();
    assert!(pending.contains("remote cancellation pending"), "{pending}");
    assert!(pending.contains(&task_id), "{pending}");
    assert!(pending.contains("the job may still run"), "{pending}");
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::FateUnknown);
    assert_eq!(done.info.task_id, task_id);
    assert_eq!(done.remote_job_id, None);
    assert!(
        done.output_preview
            .contains("no job id was received, so the job may still run"),
        "{done:?}"
    );
    assert!(
        done.output_preview
            .contains("runner confirmation time limit passed"),
        "{done:?}"
    );
    assert!(cancel_requests(&daemon).is_empty());
}

#[tokio::test]
async fn remote_cancel_placement_progress_id_cancels_before_acceptance() {
    let daemon = crate::exec_remote::wire_fixture::daemon_with_capabilities(
        Script::PlacementWait { job_id: true },
        &["exec-remote/v1", exec::types::PROGRESS_CAPABILITY],
        1,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    wait_for(
        "progress must supply the job id before cancellation",
        || {
            registry
                .status(&task_id, "session", None, None, 0)
                .unwrap()
                .remote_job_id
        },
    )
    .await;
    let task = registry.task(&task_id).unwrap();
    let saved = read_task(&task.paths.json).unwrap();
    let remote = saved.remote.unwrap();
    assert_eq!(remote.job_id, Some(id()));
    assert!(!remote.accepted);
    assert_eq!(remote.last_seq, None);
    let killing = registry.clone();
    let killed_id = task_id.clone();
    let done = tokio::task::spawn_blocking(move || killing.kill(&killed_id, "session").unwrap())
        .await
        .unwrap();
    assert_eq!(done.info.status, BgTaskStatus::Killed);
    let expected_job_id = id().to_string();
    assert_eq!(
        done.remote_job_id.as_deref(),
        Some(expected_job_id.as_str())
    );
    assert!(
        done.output_preview
            .contains("command never started on ck-motor"),
        "{done:?}"
    );
    assert!(!done.output_preview.contains("ran remotely"), "{done:?}");
    let requests = cancel_requests(&daemon);
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["params"]["job_id"], id().to_string());
    let saved = read_task(&task.paths.json).unwrap();
    let remote = saved.remote.unwrap();
    assert!(!remote.accepted, "the fake server never released Accepted");
    assert_eq!(remote.terminal.unwrap().ran, Some(Ran::None));
}

#[tokio::test]
async fn remote_cancel_restart_with_progress_id_cancels_before_acceptance() {
    let daemon = daemon(Script::PlacementWait { job_id: true }, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let (original, task_id, paths) = persist_accepted_with_snapshot(
        dir.path(),
        daemon.connection.clone(),
        "printf must-not-run",
    );
    let mut saved = read_task(&paths.json).unwrap();
    let remote = saved.remote.as_mut().unwrap();
    remote.accepted = false;
    remote.queue_position = None;
    remote.cancel_requested = true;
    write_task(&paths.json, &saved).unwrap();
    original.dual_write_task(&paths, &saved);
    drop(original);
    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    let done = terminal(&restarted, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Killed);
    assert!(
        done.output_preview
            .contains("command never started on ck-motor"),
        "{done:?}"
    );
    let requests = cancel_requests(&daemon);
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["params"]["job_id"], id().to_string());
    assert!(!daemon
        .log
        .lock()
        .unwrap()
        .iter()
        .any(|(_, body)| body["method"] == "exec.run"));
}

#[tokio::test]
async fn remote_cancel_restart_without_job_id_does_not_claim_cancellation() {
    let daemon = daemon(Script::PlacementWait { job_id: false }, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let (original, task_id, paths) = persist_accepted_with_snapshot(
        dir.path(),
        daemon.connection.clone(),
        "printf must-not-run",
    );
    let mut saved = read_task(&paths.json).unwrap();
    let remote = saved.remote.as_mut().unwrap();
    remote.accepted = false;
    remote.job_id = None;
    remote.queue_position = None;
    remote.cancel_requested = true;
    saved.execution_note = None;
    write_task(&paths.json, &saved).unwrap();
    original.dual_write_task(&paths, &saved);
    drop(original);
    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    let done = terminal(&restarted, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::FateUnknown);
    assert_eq!(done.info.task_id, task_id);
    assert!(
        done.output_preview
            .contains("no job id was received, so the job may still run"),
        "{done:?}"
    );
    assert!(!daemon
        .log
        .lock()
        .unwrap()
        .iter()
        .any(|(_, body)| body["method"] == "exec.run"));
}

async fn assert_cancel_terminal(
    record: fn() -> TerminalRecord,
    stderr: &'static str,
    expected: BgTaskStatus,
) {
    let daemon = daemon(
        Script::CancelTerminal {
            terminal: record,
            stderr,
        },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let task = start(&registry, dir.path(), daemon.connection.clone());
    wait_for("runner acceptance before cancel", || {
        registry
            .status(&task, "session", None, None, 0)
            .unwrap()
            .remote_job_id
    })
    .await;
    let killing = registry.clone();
    let kill_task = task.clone();
    tokio::task::spawn_blocking(move || killing.kill(&kill_task, "session").unwrap())
        .await
        .unwrap();
    let done = terminal(&registry, &task).await;
    assert_eq!(done.info.status, expected, "{done:?}");
    assert_eq!(done.exit_code, None);
    if expected == BgTaskStatus::Killed {
        assert_eq!(done.info.status_reason.as_deref(), Some("remote cancel"));
        assert!(!done.output_preview.contains("may still be running"));
        if record().ran == Some(Ran::None) {
            assert!(
                done.output_preview
                    .contains("command never started on ck-motor"),
                "{done:?}"
            );
        }
    } else {
        assert!(done
            .info
            .status_reason
            .as_deref()
            .unwrap()
            .contains("may still be running"));
        assert!(done.output_preview.contains("may still be running"));
    }
    if !stderr.is_empty() {
        assert!(done.output_preview.contains(stderr), "{done:?}");
    }
    let saved = read_task(&registry.task(&task).unwrap().paths.json).unwrap();
    assert_eq!(saved.remote.unwrap().terminal.as_ref(), Some(&record()));
}

#[tokio::test]
async fn exec_remote_cancel_before_start_is_killed_without_unknown_warning() {
    assert_cancel_terminal(
        || cancel_terminal(Outcome::Cancelled, None, Ran::None),
        "",
        BgTaskStatus::Killed,
    )
    .await;
}

#[tokio::test]
async fn exec_remote_cancelled_exit_is_killed_without_unknown_warning() {
    for code in [143, 137] {
        assert_cancel_terminal(
            if code == 143 {
                || {
                    cancel_terminal(
                        Outcome::Exit { code: 143 },
                        Some(Killed::Cancel),
                        Ran::Remote,
                    )
                }
            } else {
                || {
                    cancel_terminal(
                        Outcome::Exit { code: 137 },
                        Some(Killed::Cancel),
                        Ran::Remote,
                    )
                }
            },
            "",
            BgTaskStatus::Killed,
        )
        .await;
    }
}

#[tokio::test]
async fn exec_remote_cancelled_signal_is_killed_without_unknown_warning() {
    assert_cancel_terminal(
        || {
            cancel_terminal(
                Outcome::Signal { signal: 15 },
                Some(Killed::Cancel),
                Ran::Remote,
            )
        },
        "",
        BgTaskStatus::Killed,
    )
    .await;
    assert_cancel_terminal(
        || {
            cancel_terminal(
                Outcome::Signal { signal: 9 },
                Some(Killed::Cancel),
                Ran::Remote,
            )
        },
        "",
        BgTaskStatus::Killed,
    )
    .await;
}

#[tokio::test]
async fn exec_remote_cancel_unknown_exit_keeps_warning_despite_stderr() {
    for stderr in [
        "runner monitoring: outcome_unknown error=cancel recorded after unit exited",
        "runner monitoring: outcome_unknown error=cancelled unit already exited",
        "runner monitoring: outcome_unknown error=restart_execution_not_reattachable",
        "",
    ] {
        assert_cancel_terminal(cancel_unknown_terminal, stderr, BgTaskStatus::FateUnknown).await;
    }
}

#[tokio::test]
async fn exec_remote_cancel_execution_ended_true_is_killed_without_unknown_warning() {
    assert_cancel_terminal(
        || cancel_unknown_terminal().with_execution_ended(),
        "",
        BgTaskStatus::Killed,
    )
    .await;
}

#[tokio::test]
async fn exec_remote_cancel_execution_ended_false_keeps_unknown_warning() {
    assert_cancel_terminal(
        || {
            let mut value = serde_json::to_value(cancel_unknown_terminal()).unwrap();
            value["execution_ended"] = false.into();
            serde_json::from_value(value).unwrap()
        },
        "",
        BgTaskStatus::FateUnknown,
    )
    .await;
}

const TERMINAL_PENDING: &str =
    "remote cancellation pending for job 0192a64a-1234-7000-8000-000000000001; the job may still run until the build server confirms it stopped; inspect bash_status (never rerun)";

/// Start a remote task on a runner that confirms a cancel but sends the
/// terminal only once the test releases `completion_gate`, and wait until the
/// runner has accepted it.
async fn start_held_terminal_task(
    registry: &BgTaskRegistry,
    dir: &Path,
) -> (crate::exec_remote::wire_fixture::Daemon, String) {
    let daemon = daemon(Script::GappedCancel, "exec-remote/v1").await;
    let task_id = start(registry, dir, daemon.connection.clone());
    let task = registry.task(&task_id).unwrap();
    wait_for(
        "acceptance must be consumed before killing the task",
        || {
            task.state
                .lock()
                .unwrap()
                .metadata
                .remote
                .as_ref()
                .unwrap()
                .job_id
                .map(|_| ())
        },
    )
    .await;
    (daemon, task_id)
}

/// Kill `task_id` on a blocking thread, inside a request budget of `budget`
/// when one is given, and time the call.
async fn timed_kill(
    registry: &BgTaskRegistry,
    task_id: &str,
    budget: Option<Duration>,
) -> (Result<BgTaskSnapshot, String>, Duration) {
    let (killing, task_id) = (registry.clone(), task_id.to_string());
    tokio::task::spawn_blocking(move || {
        let started = Instant::now();
        let result =
            crate::bounded_io::with_budget(budget.map(crate::bounded_io::Budget::after), || {
                killing.kill(&task_id, "session")
            });
        (result, started.elapsed())
    })
    .await
    .unwrap()
}

/// The cancellation is recorded even when the kill stopped waiting: once the
/// runner sends the terminal, the task still ends as killed.
async fn assert_held_terminal_still_lands(
    registry: &BgTaskRegistry,
    daemon: &crate::exec_remote::wire_fixture::Daemon,
    task_id: &str,
) {
    daemon.wait_for_requests("exec.cancel", 1).await;
    daemon.wait_for_requests("exec.attach", 1).await;
    daemon.completion_gate.notify_one();
    let done = terminal(registry, task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Killed);
}

/// A runner that confirms the cancel but holds back the terminal must not keep
/// a `bash_kill` past its request's budget: the kill stops waiting a reply
/// margin before the budget ends and answers "terminal pending", instead of
/// waiting until the caller's own deadline passes and the reply is lost.
#[tokio::test]
async fn exec_remote_bash_kill_with_held_terminal_replies_pending_within_request_budget() {
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let (daemon, task_id) = start_held_terminal_task(&registry, dir.path()).await;
    let budget = REMOTE_KILL_REPLY_MARGIN + Duration::from_secs(1);
    let (result, elapsed) = timed_kill(&registry, &task_id, Some(budget)).await;
    assert_eq!(result.unwrap_err(), TERMINAL_PENDING);
    assert!(
        elapsed < budget,
        "the kill must reply before its request budget runs out: took {elapsed:?} of a {budget:?} budget"
    );
    assert_held_terminal_still_lands(&registry, &daemon, &task_id).await;
}

/// Without a request budget, or inside an ordinary 25-second tool call budget,
/// the wait for a held-back terminal still stops at a small fixed cap, so a
/// kill never holds its executor worker (and its project's other mutating
/// calls) for long. The bound is absolute, not derived from the cap, so
/// raising the cap toward the old 30-second wait goes red here; it leaves room
/// above the cap for a task-record write stalled on a busy test disk.
#[tokio::test]
async fn exec_remote_bash_kill_with_held_terminal_stops_waiting_at_the_cap() {
    for budget in [None, Some(Duration::from_secs(25))] {
        let dir = tempfile::tempdir().unwrap();
        let registry = registry();
        let (daemon, task_id) = start_held_terminal_task(&registry, dir.path()).await;
        let (result, elapsed) = timed_kill(&registry, &task_id, budget).await;
        assert_eq!(result.unwrap_err(), TERMINAL_PENDING);
        assert!(
            elapsed < Duration::from_secs(12),
            "the kill must stop waiting at the cap (budget {budget:?}): took {elapsed:?}"
        );
        assert_held_terminal_still_lands(&registry, &daemon, &task_id).await;
    }
}

/// Inside an ordinary tool call's budget, a terminal that arrives promptly is
/// still waited for, and the kill returns the killed snapshot.
#[tokio::test]
async fn exec_remote_bash_kill_inside_tool_call_budget_returns_prompt_terminal() {
    let daemon = daemon(Script::Cancel, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    let task = registry.task(&task_id).unwrap();
    wait_for(
        "acceptance must be consumed before killing the task",
        || {
            task.state
                .lock()
                .unwrap()
                .metadata
                .remote
                .as_ref()
                .unwrap()
                .job_id
                .map(|_| ())
        },
    )
    .await;
    let (result, _) = timed_kill(&registry, &task_id, Some(Duration::from_secs(25))).await;
    let done = result.unwrap();
    assert_eq!(done.info.status, BgTaskStatus::Killed);
    assert!(done.info.status_reason.unwrap().contains("cancel"));
}

#[path = "remote_tests/output_limit.rs"]
mod output_limit;

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
            None,
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
    // The refused receipt returned before registering a worker, so there is
    // no asynchronous launch left to wait for.
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
    restart_uses_persisted_seq_without_duplicates_or_gaps(false).await;
}

#[tokio::test]
async fn runon_restart_reattach_drain_child() {
    super::fixture::runon_restart_reattach_drain_child().await;
}

#[tokio::test]
async fn runon_restart_reattach_relaxed_kill_records_stop_before_recovery() {
    let daemon = daemon(Script::Cancel, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let (original, task_id, paths) = persist_accepted_with_snapshot(
        dir.path(),
        daemon.connection.clone(),
        "printf must-not-run",
    );
    let mut metadata = read_task(&paths.json).unwrap();
    metadata.remote.as_mut().unwrap().terminal = Some(TerminalRecord::new(
        id(),
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::Unreachable,
        },
        1,
        0,
        0,
    ));
    write_task(&paths.json, &metadata).unwrap();
    original.detach();
    let restarted = registry();
    let kill_id = task_id.clone();
    let root = dir.path().to_path_buf();
    let killer = restarted.clone();
    let stopped =
        tokio::task::spawn_blocking(move || killer.kill_relaxed(&kill_id, &root, &root).unwrap())
            .await
            .unwrap();
    assert!(stopped.info.status.is_terminal(), "{stopped:?}");
    remote_worker_exited(&task_id).await;
    // A saved refusal can permit a local fallback. Its worker must already
    // see a durable stop when it starts, so it can settle without launching.
    assert_eq!(remote_workers(&task_id), (1, 1));
    assert_eq!(
        cancellation_at_worker_start().lock().unwrap().get(&task_id),
        Some(&true)
    );
    let saved = read_task(&paths.json).unwrap();
    assert!(!saved.local_fallback_started);
    assert!(saved.remote.unwrap().cancel_requested);
}

#[tokio::test]
async fn exec_remote_bash_keyed_restart_attaches_by_id_without_duplicates_or_gaps() {
    restart_uses_persisted_seq_without_duplicates_or_gaps(true).await;
}

async fn restart_uses_persisted_seq_without_duplicates_or_gaps(keyed: bool) {
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
        copy_warning: false,
        submission: keyed.then(|| KeyedSubmission {
            key: format!("aft:{task_id}"),
            created_at_ms: unix_millis(),
            request_digest: "unused-for-attach".into(),
            attempts: SUBMISSION_RECOVERY_ATTEMPTS,
            recovery_started_at_ms: None,
            fresh_key_used: false,
            resuming: false,
            reattached: false,
            refusal: None,
        }),
        accepted_platform: None,
        explicit_runon: false,
        connection_file: Some(daemon.connection.clone()),
        harness: "broca".into(),
        session: "session".into(),
        requested_platform: None,
        requested_vcpus: None,
        requested_network: false,
        granted_network: false,
        accepted: true,
        started: None,
        job_id: None,
        last_seq: None,
        gap_recovery: None,
        undelivered_output: Vec::new(),
        stdout_len: 0,
        stderr_len: 0,
        unknown_len: 0,
        output_limit_dropped_bytes: 0,
        cancel_requested: false,
        cancel_pending: false,
        owner_pause_cancel: None,
        terminal: None,
        pre_start_refusal: None,
        fallback_digest: None,
        env_not_forwarded: None,
        queue_position: None,
        queue_wait_ms: None,
        output_received: false,
        retry: None,
        output_incomplete: None,
        workspace_holder: None,
        return_paths: Vec::new(),
        returned_files: None,
        return_first_seq: None,
        return_staging: Default::default(),
        return_max_bytes: exec::return_max_bytes(),
        return_cap_retried: false,
        expired_gap_unverified: Vec::new(),
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
        // Checkpoint B explicitly; A's first-output running transition is
        // durable, while ordinary output now checkpoints at most once a second.
        sink.last_progress_persist = Instant::now() - PROGRESS_CHECKPOINT_INTERVAL;
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
async fn exec_remote_bash_restart_after_terminal_proof_restores_exit_and_pipeline_files() {
    let dir = tempfile::tempdir().unwrap();
    let (original, task_id, paths) = persist_accepted_with_snapshot(
        dir.path(),
        dir.path().join("unavailable-connection"),
        "false | true",
    );
    let task = original.task(&task_id).unwrap();
    let mut sink = TaskSink::new(&original, task.clone()).unwrap();
    sink.output(0, OutputStream::Stderr, b"381 tests run: 381 passed\n")
        .unwrap();
    let mut record = TerminalRecord::new(id(), Outcome::Exit { code: 0 }, 1, 0, 0);
    {
        let mut state = task.state.lock().unwrap();
        state.metadata.pipeline_segments = vec!["false".into(), "true".into()];
        original.persist_task(&paths, &state.metadata).unwrap();
    }
    record.pipestatus = Some(vec![1, 0]);
    sink.terminal(&record, &exec::grade(&record)).unwrap();
    // The runner finished and its proof is durable, but the daemon has not
    // published terminal status or written the local marker files yet.
    assert_eq!(
        read_task(&paths.json).unwrap().status,
        BgTaskStatus::Running
    );
    assert!(fs::read(&paths.exit).unwrap().is_empty());
    assert!(fs::read(&paths.pipeline_status).unwrap().is_empty());
    let readonly = registry();
    let (disk_status, _) = readonly
        .terminal_status_from_disk(&task_id, "session", Some(dir.path()), dir.path())
        .unwrap();
    assert_eq!(disk_status.info.status, BgTaskStatus::Completed);
    assert_eq!(disk_status.exit_code, Some(0));
    assert!(readonly.list(0).is_empty());
    assert_eq!(
        read_task(&paths.json).unwrap().status,
        BgTaskStatus::Running
    );
    drop(sink);
    drop(task);
    drop(original);

    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    let done = terminal(&restarted, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Completed);
    assert_eq!(done.exit_code, Some(0));
    assert_eq!(read_exit_marker(&paths).unwrap(), Some(ExitMarker::Code(0)));
    assert_eq!(
        fs::read_to_string(&paths.pipeline_status).unwrap(),
        "1\n0\n"
    );
    assert!(done.output_preview.contains("381 tests run: 381 passed"));
    assert!(done.output_preview.contains("segment 1 of 2"));
    let disk = read_task(&paths.json).unwrap();
    assert_eq!(disk.exit_code, Some(0));
    assert_eq!(
        disk.schema_version,
        crate::bash_background::persistence::SCHEMA_VERSION
    );
    drop(restarted);

    // Already-completed records from older daemons also need marker repair.
    fs::write(&paths.exit, "").unwrap();
    fs::write(&paths.pipeline_status, "").unwrap();
    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    assert_eq!(read_exit_marker(&paths).unwrap(), Some(ExitMarker::Code(0)));
    assert_eq!(
        fs::read_to_string(&paths.pipeline_status).unwrap(),
        "1\n0\n"
    );
}

#[tokio::test]
async fn exec_remote_bash_restart_without_terminal_proof_names_unknown_and_removes_empty_markers() {
    let daemon = daemon(Script::MissingTerminal, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let (original, task_id, paths) =
        persist_accepted_with_snapshot(dir.path(), daemon.connection.clone(), "false | true");
    let task = original.task(&task_id).unwrap();
    let mut sink = TaskSink::new(&original, task.clone()).unwrap();
    sink.output(0, OutputStream::Stderr, b"381 tests run: 381 passed\n")
        .unwrap();
    drop(sink);
    drop(task);
    drop(original);
    shorten_reattach_budget(dir.path());

    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    let done = terminal(&restarted, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::FateUnknown);
    assert_eq!(done.exit_code, None);
    assert!(done
        .info
        .status_reason
        .as_deref()
        .unwrap()
        .contains("outcome"));
    assert!(done.output_preview.contains("381 tests run: 381 passed"));
    let disk = read_task(&paths.json).unwrap();
    assert!(disk
        .execution_note
        .as_deref()
        .unwrap()
        .contains("re-attached"));
    assert!(done.output_preview.contains("re-attached"));
    assert!(done.exit_marker_unavailable.is_some());
    assert!(
        !paths.exit.exists(),
        "an unknown outcome must not leave an empty exit marker"
    );
    assert!(
        !paths.pipeline_status.exists(),
        "missing runner proof must not leave an empty pipeline marker"
    );
    assert!(disk
        .pipeline_status_unavailable
        .as_deref()
        .unwrap()
        .contains("terminal"));
    let log = daemon.log.lock().unwrap();
    assert!(log.iter().any(|(_, body)| body["method"] == "exec.attach"));
    assert!(!log.iter().any(|(_, body)| body["method"] == "exec.run"));
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
    let clock = PausedReattachClock::new(dir.path());
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
    daemon.wait_for_requests("exec.attach", 1).await;
    clock.advance(Duration::from_millis(1_500));
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

#[test]
fn exec_remote_bash_unchanged_metadata_saves_and_progress_skip_syncs_then_recover() {
    use crate::bash_background::persistence::work_counts;
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, paths) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf remote");
    let task = registry.task(&task_id).unwrap();
    let mut sink = TaskSink::new(&registry, task.clone()).unwrap();
    let started = Started::new(0, 123, 1);
    sink.started(&started).unwrap();
    let saved = fs::read(&paths.json).unwrap();
    work_counts::reset();
    const RECORDS: u64 = 300;
    for _ in 0..RECORDS {
        // Repeated persistence requests must not sync the same durable state.
        sink.started(&started).unwrap();
    }
    let unchanged = work_counts::get();
    assert_eq!(
        (
            unchanged.metadata_writes,
            unchanged.renames,
            unchanged.syncs
        ),
        (0, 0, 0)
    );
    assert_eq!(fs::read(&paths.json).unwrap(), saved);

    // Freeze the checkpoint deadline so this measures only in-memory progress.
    sink.last_progress_persist = Instant::now() + Duration::from_secs(60);
    work_counts::reset();
    for seq in 1..=RECORDS {
        sink.output(seq, OutputStream::Stdout, b"progress\n")
            .unwrap();
    }
    let progress = work_counts::get();
    eprintln!(
        "remote metadata: {RECORDS} unchanged saves: writes={} renames={} syncs={}; {RECORDS} progress records: writes={} renames={} syncs={}",
        unchanged.metadata_writes, unchanged.renames, unchanged.syncs,
        progress.metadata_writes, progress.renames, progress.syncs,
    );
    assert_eq!(
        (progress.metadata_writes, progress.renames, progress.syncs),
        (0, 0, 0)
    );
    assert_eq!(fs::read(&paths.json).unwrap(), saved);
    drop(sink);
    drop(task);
    drop(registry);

    let restarted = self::registry();
    let disk = read_task(&paths.json).unwrap();
    let expected = serde_json::to_value(&disk).unwrap();
    restarted
        .insert_rehydrated_task(disk, paths.clone(), false)
        .unwrap();
    let task = restarted.task(&task_id).unwrap();
    let sink = TaskSink::new(&restarted, task.clone()).unwrap();
    assert_eq!(
        serde_json::to_value(&task.state.lock().unwrap().metadata).unwrap(),
        expected
    );
    assert_eq!(sink.output_progress.last_seq, Some(0));
    assert_eq!(sink.output_progress.stdout_len, 0);
    assert_eq!(fs::metadata(&paths.stdout).unwrap().len(), 0);
    assert_eq!(fs::read(&paths.json).unwrap(), saved);
}

#[test]
fn exec_remote_bash_plain_output_has_no_per_record_durable_io() {
    use crate::bash_background::persistence::work_counts;
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, paths) = persist_accepted_with_snapshot(
        dir.path(),
        dir.path().join("unavailable-connection"),
        "printf remote",
    );
    let task = registry.task(&task_id).unwrap();
    let mut sink = TaskSink::new(&registry, task.clone()).unwrap();
    sink.started(&Started::new(0, 123, 1)).unwrap();
    // Keep this operation-count measurement independent of host speed. The
    // separate checkpoint test advances the persistence deadline explicitly.
    sink.last_progress_persist = Instant::now() + Duration::from_secs(60);
    work_counts::reset();
    let started = Instant::now();
    const RECORDS: u64 = 1000;
    for seq in 1..=RECORDS {
        sink.output(seq, OutputStream::Stdout, b"stdout\n").unwrap();
        sink.output(seq + RECORDS, OutputStream::Stderr, b"stderr\n")
            .unwrap();
        sink.unknown_output(seq + 2 * RECORDS, b"unknown\n")
            .unwrap();
    }
    let counts = work_counts::get();
    eprintln!(
        "relay persistence: {} records, {} sync_all, {} renames, {:?}",
        RECORDS * 3,
        counts.syncs,
        counts.renames,
        started.elapsed()
    );
    assert_eq!(counts.syncs, 0, "plain output must not sync files");
    assert_eq!(counts.renames, 0, "plain output must not replace metadata");
    assert_eq!(
        fs::read(&paths.stdout).unwrap(),
        b"stdout\n".repeat(RECORDS as usize)
    );
    assert_eq!(
        fs::read(&paths.stderr).unwrap(),
        b"stderr\n".repeat(RECORDS as usize)
    );
    assert_eq!(sink.output_progress.last_seq, Some(3 * RECORDS));
    assert_eq!(
        task.state
            .lock()
            .unwrap()
            .metadata
            .remote
            .as_ref()
            .unwrap()
            .last_seq,
        Some(0)
    );
}

#[test]
fn exec_remote_bash_state_transitions_are_durable() {
    use crate::bash_background::persistence::work_counts;
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, paths) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf remote");
    let task = registry.task(&task_id).unwrap();
    let mut sink = TaskSink::new(&registry, task).unwrap();
    work_counts::reset();
    sink.accepted(&Accepted::new(id(), 7)).unwrap();
    let remote = read_task(&paths.json).unwrap().remote.unwrap();
    assert_eq!(remote.job_id, Some(id()));
    assert_eq!(
        RemotePhase::of(&remote),
        RemotePhase::Queued { position: 7 }
    );
    // One record write: the new record file and its directory are synced.
    assert_eq!(
        work_counts::get().syncs,
        2,
        "the job id and the execution note are saved in one record write"
    );
    assert_eq!(work_counts::get().renames, 1);

    work_counts::reset();
    // An older runner's first output is a running transition, not just progress.
    sink.output(0, OutputStream::Stdout, b"old runner").unwrap();
    assert_eq!(
        RemotePhase::of(&read_task(&paths.json).unwrap().remote.unwrap()),
        RemotePhase::Running
    );
    assert_eq!(work_counts::get().syncs, 3);
    assert_eq!(work_counts::get().renames, 1);

    work_counts::reset();
    sink.started(&Started::new(1, 123, 1)).unwrap();
    let remote = read_task(&paths.json).unwrap().remote.unwrap();
    assert_eq!(remote.started.unwrap().seq, 1);
    assert_eq!(remote.last_seq, Some(1));
    assert_eq!(work_counts::get().syncs, 2);
    assert_eq!(work_counts::get().renames, 1);
}

#[test]
fn exec_remote_bash_progress_checkpoints_are_coalesced() {
    use crate::bash_background::persistence::work_counts;
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, paths) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf remote");
    let task = registry.task(&task_id).unwrap();
    let mut sink = TaskSink::new(&registry, task).unwrap();
    sink.started(&Started::new(0, 123, 1)).unwrap();
    sink.output(1, OutputStream::Stdout, b"A").unwrap();
    assert_eq!(
        read_task(&paths.json).unwrap().remote.unwrap().last_seq,
        Some(0)
    );
    sink.last_progress_persist = Instant::now() - PROGRESS_CHECKPOINT_INTERVAL;
    work_counts::reset();
    sink.output(2, OutputStream::Stderr, b"B").unwrap();
    let remote = read_task(&paths.json).unwrap().remote.unwrap();
    assert_eq!(
        (remote.last_seq, remote.stdout_len, remote.stderr_len),
        (Some(2), 1, 1)
    );
    // Two dirty output files and the new metadata file; a progress-only
    // checkpoint leaves the directory unsynced.
    assert_eq!(work_counts::get().syncs, 3);
    assert_eq!(work_counts::get().renames, 1);
    work_counts::reset();
    sink.output(3, OutputStream::Stdout, b"C").unwrap();
    assert_eq!(work_counts::get().syncs, 0);
    assert_eq!(work_counts::get().renames, 0);
    assert_eq!(
        read_task(&paths.json).unwrap().remote.unwrap().last_seq,
        Some(2)
    );
}

#[test]
fn exec_remote_bash_progress_checkpoints_over_a_20s_fake_run() {
    use crate::bash_background::persistence::work_counts;
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, paths) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf remote");
    let task = registry.task(&task_id).unwrap();
    let mut sink = TaskSink::new(&registry, task).unwrap();
    let start = Instant::now();
    let elapsed_ms = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let clock_ticks = Arc::clone(&elapsed_ms);
    sink.set_progress_clock_for_test(Arc::new(move || {
        start + Duration::from_millis(clock_ticks.load(std::sync::atomic::Ordering::Relaxed))
    }));
    sink.started(&Started::new(0, 123, 1)).unwrap();
    work_counts::reset();
    // A fake runner streams 100 records a second, alternating stdout and
    // stderr, for 20 simulated seconds. The injected clock advances without
    // sleeping or depending on how long checkpoint writes take.
    const TICK: Duration = Duration::from_millis(10);
    for seq in 1..=2000_u64 {
        elapsed_ms.fetch_add(
            TICK.as_millis() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        let stream = if seq % 2 == 0 {
            OutputStream::Stdout
        } else {
            OutputStream::Stderr
        };
        sink.output(seq, stream, b"line\n").unwrap();
    }
    let counts = work_counts::get();
    eprintln!(
        "20s fake run: {} checkpoints, {} fsyncs; per minute: {} checkpoints, {} fsyncs",
        counts.renames,
        counts.syncs,
        counts.renames * 3,
        counts.syncs * 3
    );
    assert_eq!(counts.renames, 4, "one checkpoint per five seconds");
    // Each checkpoint syncs the two dirty output files and the new metadata
    // file; a progress-only checkpoint does not sync the directory.
    assert_eq!(counts.syncs, 12);
    let remote = read_task(&paths.json).unwrap().remote.unwrap();
    let saved = remote.last_seq.unwrap();
    assert!(saved >= 1500, "the last checkpoint is recent: {saved}");
    assert_eq!(
        (remote.stdout_len, remote.stderr_len),
        (saved / 2 * 5, saved.div_ceil(2) * 5),
        "the saved lengths match the saved sequence number"
    );
}

#[test]
fn exec_remote_bash_stale_checkpoint_snapshot_loses_to_newer_locked_write() {
    use crate::bash_background::persistence::work_counts;
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, paths) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf remote");
    let task = registry.task(&task_id).unwrap();
    let mut sink = TaskSink::new(&registry, task.clone()).unwrap();
    sink.started(&Started::new(0, 123, 1)).unwrap();
    sink.output(1, OutputStream::Stdout, b"A").unwrap();
    // A cancel lands after the checkpoint took its snapshot (without the
    // cancel) and before the snapshot reaches the file.
    let (cancel_registry, cancel_task) = (registry.clone(), task.clone());
    sink.before_checkpoint_write = Some(Box::new(move || {
        assert!(cancel_registry.record_remote_cancel(&cancel_task).unwrap());
    }));
    sink.last_progress_persist = Instant::now() - PROGRESS_CHECKPOINT_INTERVAL;
    work_counts::reset();
    sink.output(2, OutputStream::Stderr, b"B").unwrap();
    let remote = read_task(&paths.json).unwrap().remote.unwrap();
    assert!(
        remote.cancel_requested,
        "the older checkpoint snapshot must not replace the cancel record"
    );
    assert_eq!(
        (remote.last_seq, remote.stdout_len, remote.stderr_len),
        (Some(2), 1, 1),
        "the cancel record already carries the checkpointed cursor"
    );
    assert_eq!(work_counts::get().renames, 1, "only the cancel was written");
}

#[test]
fn exec_remote_bash_restart_after_progress_checkpoint_replays_from_saved_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, paths) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf remote");
    let task = registry.task(&task_id).unwrap();
    let mut sink = TaskSink::new(&registry, task).unwrap();
    sink.started(&Started::new(0, 123, 1)).unwrap();
    sink.output(1, OutputStream::Stdout, b"A").unwrap();
    sink.output(2, OutputStream::Stderr, b"a").unwrap();
    sink.last_progress_persist = Instant::now() - PROGRESS_CHECKPOINT_INTERVAL;
    // A progress-only checkpoint saves the cursor after this record.
    sink.output(3, OutputStream::Stdout, b"B").unwrap();
    // Received after the checkpoint and never saved in metadata.
    sink.output(4, OutputStream::Stdout, b"C").unwrap();
    sink.output(5, OutputStream::Stderr, b"b").unwrap();
    let disk = read_task(&paths.json).unwrap();
    let point = disk.remote.as_ref().unwrap().point().unwrap();
    drop(sink);
    drop(registry);

    let restarted = self::registry();
    restarted
        .insert_rehydrated_task(disk, paths.clone(), false)
        .unwrap();
    let task = restarted.task(&task_id).unwrap();
    let mut sink = TaskSink::new(&restarted, task).unwrap();
    let mut consumer = exec::StreamConsumer::resume(point);
    let from_seq = consumer
        .resume_point()
        .unwrap()
        .attach_request()
        .unwrap()
        .from_seq;
    // The runner replays every record after the saved sequence number.
    for (seq, stream, bytes) in [
        (4, OutputStream::Stdout, b"C"),
        (5, OutputStream::Stderr, b"b"),
        (6, OutputStream::Stdout, b"D"),
    ] {
        consumer
            .consume(
                StreamRecord::Output(Output::new(seq, stream, BytePayload(bytes.to_vec()))),
                &mut sink,
            )
            .unwrap();
    }
    assert_eq!(fs::read(&paths.stdout).unwrap(), b"ABCD");
    assert_eq!(fs::read(&paths.stderr).unwrap(), b"ab");
    assert_eq!(from_seq, 4, "attach resumes after the progress checkpoint");
}

#[test]
fn exec_remote_bash_cancel_persists_only_durable_output_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, paths) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf remote");
    let task = registry.task(&task_id).unwrap();
    let mut sink = TaskSink::new(&registry, task.clone()).unwrap();
    sink.started(&Started::new(0, 123, 1)).unwrap();
    sink.last_progress_persist = Instant::now() + Duration::from_secs(60);
    sink.output(1, OutputStream::Stdout, b"tail").unwrap();
    registry.record_remote_cancel(&task).unwrap();
    let disk = read_task(&paths.json).unwrap();
    let remote = disk.remote.as_ref().unwrap();
    assert!(remote.cancel_requested);
    assert_eq!(
        (remote.last_seq, remote.stdout_len),
        (Some(0), 0),
        "cancel must not publish the unsynced output cursor"
    );
    let point = remote.point().unwrap();
    drop(sink);
    drop(registry);
    // Simulate a host crash losing the stdout bytes written since the checkpoint.
    fs::write(&paths.stdout, b"").unwrap();
    let restarted = self::registry();
    restarted
        .insert_rehydrated_task(disk, paths.clone(), false)
        .unwrap();
    let task = restarted.task(&task_id).unwrap();
    let mut sink = TaskSink::new(&restarted, task).unwrap();
    let mut consumer = exec::StreamConsumer::resume(point);
    consumer
        .consume(
            StreamRecord::Output(Output::new(
                1,
                OutputStream::Stdout,
                BytePayload(b"tail".to_vec()),
            )),
            &mut sink,
        )
        .unwrap();
    assert_eq!(fs::read(&paths.stdout).unwrap(), b"tail");
}

#[test]
fn exec_remote_bash_checkpoint_syncs_dirty_output_before_metadata() {
    use crate::bash_background::persistence::{with_task_io_fault, work_counts, TaskIoFault};
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, paths) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf remote");
    let task = registry.task(&task_id).unwrap();
    let mut sink = TaskSink::new(&registry, task).unwrap();
    sink.started(&Started::new(0, 123, 1)).unwrap();
    sink.last_progress_persist = Instant::now() + Duration::from_secs(60);
    for seq in 1..=10 {
        sink.output(seq, OutputStream::Stdout, b"A").unwrap();
        sink.output(seq + 10, OutputStream::Stderr, b"B").unwrap();
        sink.unknown_output(seq + 20, b"C").unwrap();
    }
    sink.last_progress_persist = Instant::now() - PROGRESS_CHECKPOINT_INTERVAL;
    work_counts::reset();
    sink.unknown_output(31, b"").unwrap();
    assert_eq!(
        work_counts::get().syncs,
        4,
        "three dirty files plus the metadata file"
    );
    assert_eq!(work_counts::get().renames, 1);
    let remote = read_task(&paths.json).unwrap().remote.unwrap();
    assert_eq!(
        (remote.stdout_len, remote.stderr_len, remote.unknown_len),
        (10, 10, 10)
    );
    assert_eq!(remote.last_seq, Some(31));

    sink.last_progress_persist = Instant::now() - PROGRESS_CHECKPOINT_INTERVAL;
    work_counts::reset();
    sink.unknown_output(32, b"").unwrap();
    assert_eq!(
        work_counts::get().syncs,
        1,
        "clean files must not sync again"
    );

    sink.output(33, OutputStream::Stdout, b"tail").unwrap();
    sink.last_progress_persist = Instant::now() - PROGRESS_CHECKPOINT_INTERVAL;
    work_counts::reset();
    let result = with_task_io_fault(TaskIoFault::RunningEnospc, || sink.unknown_output(34, b""));
    assert!(result.is_err());
    assert_eq!(
        work_counts::get().syncs,
        1,
        "dirty stdout must sync before the metadata write fails"
    );
    assert_eq!(
        read_task(&paths.json).unwrap().remote.unwrap().last_seq,
        Some(32)
    );
    work_counts::reset();
    sink.unknown_output(34, b"").unwrap();
    assert_eq!(
        work_counts::get().syncs,
        1,
        "a metadata retry must not resync clean output"
    );
}

#[test]
fn exec_remote_bash_terminal_syncs_dirty_output_before_metadata() {
    use crate::bash_background::persistence::{with_task_io_fault, work_counts, TaskIoFault};
    for fail_metadata in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (registry, task_id, paths) =
            persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf remote");
        let task = registry.task(&task_id).unwrap();
        let mut sink = TaskSink::new(&registry, task).unwrap();
        sink.started(&Started::new(0, 123, 1)).unwrap();
        sink.last_progress_persist = Instant::now() + Duration::from_secs(60);
        sink.output(1, OutputStream::Stdout, b"A").unwrap();
        sink.output(2, OutputStream::Stderr, b"B").unwrap();
        sink.unknown_output(3, b"C").unwrap();
        let record = TerminalRecord::new(id(), Outcome::Exit { code: 0 }, 3, 0, 0);
        let verdict = Verdict::Exited { code: 0 };
        work_counts::reset();
        if fail_metadata {
            let result = with_task_io_fault(TaskIoFault::RunningEnospc, || {
                sink.terminal(&record, &verdict)
            });
            assert!(result.is_err());
            assert_eq!(
                work_counts::get().syncs,
                3,
                "all dirty output must sync before the terminal proof write"
            );
            assert!(read_task(&paths.json)
                .unwrap()
                .remote
                .unwrap()
                .terminal
                .is_none());
            work_counts::reset();
        }
        sink.terminal(&record, &verdict).unwrap();
        assert_eq!(work_counts::get().syncs, if fail_metadata { 2 } else { 5 });
        assert_eq!(work_counts::get().renames, 1);
        let remote = read_task(&paths.json).unwrap().remote.unwrap();
        assert!(remote.terminal.is_some());
        assert_eq!(
            (
                remote.last_seq,
                remote.stdout_len,
                remote.stderr_len,
                remote.unknown_len
            ),
            (Some(3), 1, 1, 1)
        );
    }
}

#[test]
fn exec_remote_bash_terminal_proof_is_durable_before_completion() {
    use crate::bash_background::persistence::{with_task_io_fault, work_counts, TaskIoFault};
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, paths) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf remote");
    let task = registry.task(&task_id).unwrap();
    let mut sink = TaskSink::new(&registry, task.clone()).unwrap();
    let mut consumer = exec::StreamConsumer::new();
    consumer
        .consume(StreamRecord::Accepted(Accepted::new(id(), 1)), &mut sink)
        .unwrap();
    consumer
        .consume(StreamRecord::Started(Started::new(0, 123, 1)), &mut sink)
        .unwrap();
    sink.output(1, OutputStream::Stdout, b"tail").unwrap();
    let record = TerminalRecord::new(id(), Outcome::Exit { code: 0 }, 1, 0, 0);
    let result = with_task_io_fault(TaskIoFault::RunningEnospc, || {
        consumer.consume(StreamRecord::Terminal(record.clone()), &mut sink)
    });
    assert!(
        result.is_err(),
        "a failed durable write must not complete the stream"
    );
    assert!(consumer.finish().is_err());
    assert!(task
        .state
        .lock()
        .unwrap()
        .metadata
        .remote
        .as_ref()
        .unwrap()
        .terminal
        .is_none());
    assert!(read_task(&paths.json)
        .unwrap()
        .remote
        .unwrap()
        .terminal
        .is_none());
    work_counts::reset();
    consumer
        .consume(StreamRecord::Terminal(record), &mut sink)
        .unwrap();
    assert!(consumer.finish().is_ok());
    assert_eq!(
        work_counts::get().syncs,
        2,
        "proof file and parent directory must both sync"
    );
    assert_eq!(work_counts::get().renames, 1);
    let persisted = read_task(&paths.json).unwrap();
    let remote = persisted.remote.unwrap();
    assert!(remote.terminal.is_some());
    assert_eq!((remote.last_seq, remote.stdout_len), (Some(1), 4));
    // Simulate a crash before remote_terminal publishes the registry status.
    assert_eq!(persisted.status, BgTaskStatus::Running);
    let readonly = self::registry();
    assert_eq!(
        readonly
            .terminal_status_from_disk(&task_id, "session", Some(dir.path()), dir.path())
            .unwrap()
            .0
            .info
            .status,
        BgTaskStatus::Completed
    );
}

#[test]
fn exec_remote_bash_restart_short_checkpoint_never_zero_fills() {
    for (terminal_proof, unknown_missing) in
        [(false, false), (true, false), (false, true), (true, true)]
    {
        let dir = tempfile::tempdir().unwrap();
        let (original, task_id, paths) =
            persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf remote");
        let task = original.task(&task_id).unwrap();
        let mut sink = TaskSink::new(&original, task).unwrap();
        sink.started(&Started::new(0, 123, 1)).unwrap();
        sink.output(1, OutputStream::Stdout, b"stdout").unwrap();
        sink.output(2, OutputStream::Stderr, b"stderr").unwrap();
        sink.unknown_output(3, b"unknown").unwrap();
        if terminal_proof {
            sink.terminal(
                &TerminalRecord::new(id(), Outcome::Exit { code: 0 }, 3, 0, 0),
                &Verdict::Exited { code: 0 },
            )
            .unwrap();
        } else {
            sink.last_progress_persist = Instant::now() - PROGRESS_CHECKPOINT_INTERVAL;
            sink.unknown_output(4, b"").unwrap();
        }
        drop(sink);
        drop(original);
        let unknown_path = paths.io_dir.join("remote-unknown-output");
        fs::write(&paths.stdout, b"out").unwrap();
        fs::write(&paths.stderr, b"err").unwrap();
        if unknown_missing {
            fs::remove_file(&unknown_path).unwrap();
        } else {
            fs::write(&unknown_path, b"unk").unwrap();
        }
        let disk = read_task(&paths.json).unwrap();
        let seq = disk.remote.as_ref().unwrap().last_seq;
        let restarted = registry();
        restarted
            .insert_rehydrated_task(disk, paths.clone(), false)
            .unwrap();
        let task = restarted.task(&task_id).unwrap();
        let mut sink = TaskSink::new(&restarted, task).unwrap();
        assert_eq!(
            fs::read(&paths.stdout).unwrap(),
            b"out",
            "short stdout must not be padded"
        );
        assert_eq!(
            fs::read(&paths.stderr).unwrap(),
            b"err",
            "short stderr must not be padded"
        );
        let disk = read_task(&paths.json).unwrap();
        let incomplete = disk
            .remote
            .as_ref()
            .unwrap()
            .output_incomplete
            .as_ref()
            .expect("short checkpoint loss is stored structurally");
        assert!(
            incomplete.reason == RemoteOutputIncompleteReason::CheckpointShort,
            "{incomplete:?}"
        );
        for stream in [
            RemoteOutputStream::Stdout,
            RemoteOutputStream::Stderr,
            RemoteOutputStream::Unknown,
        ] {
            assert!(
                incomplete.streams.iter().any(|loss| loss.stream == stream),
                "missing {stream:?}: {incomplete:?}"
            );
        }
        assert!(!disk
            .execution_note
            .as_deref()
            .unwrap_or_default()
            .contains("Output incomplete"));
        let remote = disk.remote.unwrap();
        assert_eq!(
            (remote.stdout_len, remote.stderr_len, remote.unknown_len),
            (3, 3, if unknown_missing { 0 } else { 3 })
        );
        assert_eq!(
            remote.last_seq, seq,
            "byte losses have no safe sequence mapping"
        );
        assert_eq!(remote.terminal.is_some(), terminal_proof);
        if !terminal_proof {
            sink.output(seq.unwrap() + 1, OutputStream::Stdout, b"next")
                .unwrap();
            sink.unknown_output(seq.unwrap() + 2, b"new").unwrap();
            assert_eq!(fs::read(&paths.stdout).unwrap(), b"outnext");
            assert_eq!(
                fs::read(&unknown_path).unwrap(),
                if unknown_missing {
                    b"new".as_slice()
                } else {
                    b"unknew".as_slice()
                }
            );
        }
        drop(sink);
        drop(restarted);
        let disk = read_task(&paths.json).unwrap();
        let again = registry();
        again
            .insert_rehydrated_task(disk, paths.clone(), false)
            .unwrap();
        let task = again.task(&task_id).unwrap();
        let _sink = TaskSink::new(&again, task).unwrap();
        let disk = read_task(&paths.json).unwrap();
        assert!(disk.remote.as_ref().unwrap().output_incomplete.is_some());
        assert!(!disk
            .execution_note
            .as_deref()
            .unwrap_or_default()
            .contains("Output incomplete"));
        assert!(!fs::read(&paths.stdout).unwrap().contains(&0));
        assert!(!fs::read(&paths.stderr).unwrap().contains(&0));
    }
}

fn recover_short_checkpoint(
    terminal_proof: bool,
) -> (
    tempfile::TempDir,
    BgTaskRegistry,
    String,
    TaskPaths,
    Arc<BgTask>,
    Option<TerminalRecord>,
) {
    let dir = tempfile::tempdir().unwrap();
    let (original, task_id, paths) = persist_accepted_with_snapshot(
        dir.path(),
        dir.path().join("unused-connection"),
        "printf remote",
    );
    let task = original.task(&task_id).unwrap();
    let mut sink = TaskSink::new(&original, task).unwrap();
    sink.started(&Started::new(0, 123, 1)).unwrap();
    sink.output(1, OutputStream::Stdout, b"stdout").unwrap();
    sink.output(2, OutputStream::Stderr, b"stderr").unwrap();
    sink.unknown_output(3, b"unknown").unwrap();
    let record = TerminalRecord::new(id(), Outcome::Exit { code: 0 }, 3, 0, 0);
    if terminal_proof {
        sink.terminal(&record, &exec::grade(&record)).unwrap();
    } else {
        sink.last_progress_persist = Instant::now() - PROGRESS_CHECKPOINT_INTERVAL;
        sink.unknown_output(4, b"").unwrap();
    }
    drop(sink);
    drop(original);

    fs::write(&paths.stdout, b"out").unwrap();
    fs::write(&paths.stderr, b"err").unwrap();
    fs::write(paths.io_dir.join("remote-unknown-output"), b"unk").unwrap();
    let disk = read_task(&paths.json).unwrap();
    let restarted = registry();
    restarted
        .insert_rehydrated_task(disk, paths.clone(), false)
        .unwrap();
    let task = restarted.task(&task_id).unwrap();
    drop(TaskSink::new(&restarted, task.clone()).unwrap());

    (
        dir,
        restarted,
        task_id,
        paths,
        task,
        terminal_proof.then_some(record),
    )
}

fn assert_checkpoint_warning_is_visible(registry: &BgTaskRegistry, task_id: &str) {
    let task = registry.task(task_id).unwrap();
    let metadata = task.state.lock().unwrap().metadata.clone();
    let incomplete = metadata
        .remote
        .as_ref()
        .unwrap()
        .output_incomplete
        .as_ref()
        .expect("short checkpoint loss remains in remote metadata");
    let warning = incomplete.warning_line();
    let note = registry.execution_note(task_id, "session").unwrap();
    assert!(note.starts_with(&warning), "{note}");
    assert!(!metadata
        .execution_note
        .as_deref()
        .unwrap_or_default()
        .contains("Output incomplete"));
    let snapshot = registry.observed_status(task_id, "session", 8192).unwrap();
    assert_eq!(snapshot.info.status, BgTaskStatus::Completed);
    assert!(snapshot.output_preview.contains(&warning), "{snapshot:?}");
}

fn assert_checkpoint_warning_is_in_completion(registry: &BgTaskRegistry, task_id: &str) {
    let completion = registry
        .drain_completions()
        .into_iter()
        .find(|completion| completion.task_id == task_id)
        .expect("completed remote task queues a completion notice");
    assert!(
        completion
            .output_preview
            .contains("Output incomplete (remote_output_checkpoint_short)"),
        "{completion:?}"
    );
}

#[test]
fn exec_remote_bash_short_checkpoint_warning_survives_normal_completion() {
    let (_dir, registry, task_id, _paths, task, _) = recover_short_checkpoint(false);
    let mut sink = TaskSink::new(&registry, task.clone()).unwrap();
    let record = TerminalRecord::new(id(), Outcome::Exit { code: 0 }, 3, 0, 0);
    sink.terminal(&record, &exec::grade(&record)).unwrap();
    drop(sink);

    registry.remote_terminal(&task, exec::grade(&record), None);

    assert_checkpoint_warning_is_visible(&registry, &task_id);
    assert_checkpoint_warning_is_in_completion(&registry, &task_id);
}

#[test]
fn exec_remote_bash_short_checkpoint_warning_survives_saved_terminal_proof() {
    let (_dir, registry, task_id, _paths, task, record) = recover_short_checkpoint(true);
    registry.remote_terminal(&task, exec::grade(&record.unwrap()), None);

    assert_checkpoint_warning_is_visible(&registry, &task_id);
    assert_checkpoint_warning_is_in_completion(&registry, &task_id);
}

#[test]
fn exec_remote_bash_short_checkpoint_warning_survives_second_restart() {
    let (dir, task_registry, task_id, _paths, task, _) = recover_short_checkpoint(false);
    let mut sink = TaskSink::new(&task_registry, task.clone()).unwrap();
    let record = TerminalRecord::new(id(), Outcome::Exit { code: 0 }, 3, 0, 0);
    sink.terminal(&record, &exec::grade(&record)).unwrap();
    drop(sink);
    task_registry.remote_terminal(&task, exec::grade(&record), None);
    drop(task);
    drop(task_registry);

    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();

    assert_checkpoint_warning_is_visible(&restarted, &task_id);
}

#[test]
fn exec_remote_bash_old_record_without_output_incomplete_loads() {
    let dir = tempfile::tempdir().unwrap();
    let (_registry, _task_id, paths) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf remote");
    let bytes = fs::read(&paths.json).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(value["remote"].get("output_incomplete").is_none());

    let loaded = read_task(&paths.json).unwrap();

    assert!(loaded.remote.unwrap().output_incomplete.is_none());
}

#[test]
fn exec_remote_bash_restart_replays_uncheckpointed_output() {
    let dir = tempfile::tempdir().unwrap();
    let (original, task_id, paths) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf remote");
    let task = original.task(&task_id).unwrap();
    let mut sink = TaskSink::new(&original, task).unwrap();
    sink.started(&Started::new(0, 123, 1)).unwrap();
    sink.output(1, OutputStream::Stdout, b"uncheckpointed")
        .unwrap();
    sink.output(2, OutputStream::Stderr, b"stderr tail")
        .unwrap();
    sink.unknown_output(3, b"unknown tail").unwrap();
    drop(sink);
    drop(original);
    let disk = read_task(&paths.json).unwrap();
    assert_eq!(disk.remote.as_ref().unwrap().last_seq, Some(0));
    let restarted = registry();
    restarted
        .insert_rehydrated_task(disk, paths.clone(), false)
        .unwrap();
    let task = restarted.task(&task_id).unwrap();
    let mut sink = TaskSink::new(&restarted, task.clone()).unwrap();
    assert!(fs::read(&paths.stdout).unwrap().is_empty());
    assert!(fs::read(&paths.stderr).unwrap().is_empty());
    assert!(fs::read(paths.io_dir.join("remote-unknown-output"))
        .unwrap()
        .is_empty());
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
    for (seq, stream, bytes) in [
        (1, OutputStream::Stdout, b"replayed".to_vec()),
        (2, OutputStream::Stderr, b"stderr".to_vec()),
    ] {
        consumer
            .consume(
                StreamRecord::Output(Output::new(seq, stream, BytePayload(bytes))),
                &mut sink,
            )
            .unwrap();
    }
    consumer
        .consume(
            StreamRecord::Terminal(TerminalRecord::new(
                id(),
                Outcome::Exit { code: 0 },
                1,
                0,
                0,
            )),
            &mut sink,
        )
        .unwrap();
    assert!(consumer.finish().is_ok());
    assert_eq!(fs::read(&paths.stdout).unwrap(), b"replayed");
    assert_eq!(fs::read(&paths.stderr).unwrap(), b"stderr");
    assert_eq!(
        read_task(&paths.json).unwrap().remote.unwrap().last_seq,
        Some(2)
    );
}

#[test]
fn shells_v1_remote_acceptance_persists_only_a_reported_placement_host() {
    use exec::types::Placement;
    for (placement, expected) in [
        (
            Some(Placement::new("first_party").with_host("ovh-1")),
            Some("ovh-1"),
        ),
        (Some(Placement::new("overflow")), None),
        (None, None),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (registry, task_id, paths) = persist_accepted_with_snapshot(
            dir.path(),
            dir.path().join("connection.json"),
            "printf host",
        );
        let task = registry.task(&task_id).unwrap();
        task.state.lock().unwrap().metadata.shell_remote_host = Some("previous-host".into());
        let mut sink = TaskSink::new(&registry, task).unwrap();
        let mut accepted = serde_json::to_value(Accepted::new(id(), 1)).unwrap();
        if let Some(placement) = placement {
            accepted["placement"] = serde_json::to_value(placement).unwrap();
        }
        let accepted: Accepted = serde_json::from_value(accepted).unwrap();
        sink.accepted(&accepted).unwrap();
        let layout = resolve_task_layout(&paths.session_dir, &task_id).unwrap();
        let saved = read_task_at(&layout).unwrap();
        assert_eq!(saved.shell_remote_host.as_deref(), expected);
        assert_eq!(
            registry.shell_snapshots()[0]
                .metadata
                .shell_remote_host
                .as_deref(),
            expected
        );
        assert!(serde_json::to_value(&saved)
            .unwrap()
            .get("provider")
            .is_none());
    }
}

#[tokio::test]
async fn fate_unknown_bash_kill_cancels_recorded_job_and_reports_runner_answers() {
    for (script, expected) in [
        (Script::Cancel, "runner confirmed cancellation"),
        (
            Script::CancelTerminal {
                terminal: || {
                    let mut terminal = cancel_unknown_terminal().with_execution_ended();
                    terminal.killed = None;
                    terminal
                },
                stderr: "",
            },
            "runner reports job already ended",
        ),
        (Script::CancelRefused, "outcome is still unknown"),
        (
            Script::CancelTerminal {
                terminal: cancel_unknown_terminal,
                stderr: "runner monitoring: cancelled; unit already exited",
            },
            "outcome is still unknown",
        ),
    ] {
        let daemon = daemon(script, "exec-remote/v1").await;
        let dir = tempfile::tempdir().unwrap();
        let (registry, task, paths) = finished_remote_for_cancel(
            dir.path(),
            daemon.connection.clone(),
            cancel_unknown_terminal(),
        );
        let before = fs::read(&paths.json).unwrap();
        let killing = registry.clone();
        let reply = tokio::task::spawn_blocking(move || killing.kill(&task, "session").unwrap())
            .await
            .unwrap();
        let requests = cancel_requests(&daemon);
        assert_eq!(
            requests.len(),
            1,
            "fate_unknown must send exactly one exec.cancel"
        );
        assert_eq!(requests[0]["params"]["job_id"], id().to_string());
        let data = serde_json::to_value(&reply).unwrap();
        assert!(
            data["remote_cancel"].as_str().unwrap().contains(expected),
            "{data}"
        );
        assert_eq!(reply.info.status, BgTaskStatus::FateUnknown);
        assert_eq!(
            fs::read(&paths.json).unwrap(),
            before,
            "cancel must not invent terminal proof"
        );
    }
}

#[tokio::test]
async fn known_outcome_bash_kill_sends_no_remote_cancel() {
    let daemon = daemon(Script::Plain, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let (registry, task, _) = finished_remote_for_cancel(
        dir.path(),
        daemon.connection.clone(),
        TerminalRecord::new(id(), Outcome::Exit { code: 0 }, 1, 0, 0),
    );
    let reply = registry.kill(&task, "session").unwrap();
    assert_eq!(reply.info.status, BgTaskStatus::Completed);
    assert!(cancel_requests(&daemon).is_empty());
    registry.kill_scope(&owner_scope("unknown-outcome"));
    assert!(cancel_requests(&daemon).is_empty());
}

#[tokio::test]
async fn fate_unknown_bash_kill_outside_runner_limits_sends_no_cancel() {
    let daemon = daemon(Script::Plain, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let (registry, task, _) = finished_remote_for_cancel(
        dir.path(),
        daemon.connection.clone(),
        cancel_unknown_terminal(),
    );
    registry
        .task(&task)
        .unwrap()
        .state
        .lock()
        .unwrap()
        .metadata
        .started_at = 0;
    let reply = registry.kill(&task, "session").unwrap();
    assert_eq!(reply.info.status, BgTaskStatus::FateUnknown);
    registry.kill_scope(&owner_scope("unknown-outcome"));
    assert!(cancel_requests(&daemon).is_empty());
}

#[tokio::test]
async fn fate_unknown_scope_end_cancels_remote_without_rewriting_status() {
    let daemon = daemon(Script::Cancel, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let (registry, task, paths) = finished_remote_for_cancel(
        dir.path(),
        daemon.connection.clone(),
        cancel_unknown_terminal(),
    );
    let before = fs::read(&paths.json).unwrap();
    let stopping = registry.clone();
    tokio::task::spawn_blocking(move || stopping.kill_scope(&owner_scope("unknown-outcome")))
        .await
        .unwrap();
    let requests = cancel_requests(&daemon);
    assert_eq!(requests.len(), 1, "scope end must cancel fate_unknown jobs");
    assert_eq!(requests[0]["params"]["job_id"], id().to_string());
    assert_eq!(
        registry
            .task(&task)
            .unwrap()
            .state
            .lock()
            .unwrap()
            .metadata
            .status,
        BgTaskStatus::FateUnknown
    );
    assert_eq!(fs::read(&paths.json).unwrap(), before);
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
                    .join(crate::sandbox_spawn::local_launch::REMOTE_LOCAL_LAUNCH)
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
            let (
                plan,
                shell_path,
                env,
                linux_scope,
                memory_limit_mib,
                linux_scope_max_runtime_secs,
            ) = crate::sandbox_spawn::restore_local_launch(
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
                        request.map_err(RemoteRequestError::Build),
                        LocalFallback {
                            plan,
                            shell_path,
                            env,
                            linux_scope,
                            linux_scope_max_runtime_secs,
                            memory_limit_mib,
                            capture_pipeline: false,
                        },
                    )),
                    None,
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
        // `>>` creates the file before printf writes to it, so existence alone
        // can be observed while the file is still empty; wait for the byte.
        wait_for("fallback must write its execution marker", || {
            fs::read(&counter).ok().filter(|bytes| !bytes.is_empty())
        })
        .await;
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
            .join(crate::sandbox_spawn::local_launch::REMOTE_LOCAL_LAUNCH);
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
    let release = dir.path().join("release");
    let _release_on_failure = ReleaseOnDrop::new(release.clone());
    let command = format!(
        "printf x >> '{}'; {}",
        counter.display(),
        crate::test_gate::gate(&format!("'{}'", release.display()), "/bin/sleep 0.01")
    );
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
    // Wait for the byte, not the file: `>>` creates it before printf writes.
    wait_for("fallback must write its execution marker", || {
        fs::read(&counter).ok().filter(|bytes| !bytes.is_empty())
    })
    .await;
    first.inner.shutdown.store(true, Ordering::SeqCst);
    drop(first);
    let second = registry();
    second.set_harness(crate::harness::Harness::Runner);
    second.set_db_pool(db.clone());
    second.replay_session(dir.path(), "session").unwrap();
    fs::write(&release, b"release").unwrap();
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

#[test]
fn exec_remote_bash_unknown_output_bytes_are_durable_without_misattribution() {
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, paths) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf unused");
    let task = registry.task(&task_id).unwrap();
    let mut sink = TaskSink::new(&registry, task.clone()).unwrap();
    sink.last_progress_persist = Instant::now() - PROGRESS_CHECKPOINT_INTERVAL;
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
            last_output_seq: None,
        }),
        reason: "lost".into()
    }));
}

#[tokio::test]
async fn exec_remote_bash_refusal_queue_wait_is_not_local_run_time() {
    let daemon = daemon(Script::AttachRefused, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let release = dir.path().join("release");
    let _release_on_failure = ReleaseOnDrop::new(release.clone());
    let command = format!(
        "{}; printf budget-proof",
        crate::test_gate::gate(&format!("'{}'", release.display()), "/bin/sleep 0.01")
    );
    let (original, task_id, paths) =
        persist_accepted_with_snapshot(dir.path(), daemon.connection.clone(), &command);
    let layout = resolve_task_layout(&paths.session_dir, &task_id).unwrap();
    let mut metadata = read_task_at(&layout).unwrap();
    metadata.started_at = unix_millis().saturating_sub(60000);
    metadata.timeout_ms = Some(15000);
    write_task_at(&layout, &metadata).unwrap();
    drop(original);
    let restarted = registry();
    let replay_started_at = unix_millis();
    restarted.replay_session(dir.path(), "session").unwrap();
    let local_started_at = wait_for("fallback must publish its local start", || {
        let task = restarted.task(&task_id).unwrap();
        let state = task.state.lock().unwrap();
        state
            .metadata
            .local_fallback_started
            .then_some(state.metadata.started_at)
    })
    .await;
    assert!(
        local_started_at >= replay_started_at,
        "the minute in the remote queue cannot consume the local run budget"
    );
    fs::write(&release, b"release").unwrap();
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

#[tokio::test]
async fn exec_remote_started_sets_running_phase_without_aft_timeout() {
    let daemon = daemon(Script::Started, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    let running = wait_for("Started must publish the running phase", || {
        let snapshot = registry.observed_status(&task_id, "session", 8192).unwrap();
        (snapshot.phase == Some("running")).then_some(snapshot)
    })
    .await;
    assert_eq!(running.info.status, BgTaskStatus::Running);
    assert_eq!(serde_json::to_value(&running).unwrap()["phase"], "running");
    assert!(running.hard_kill.is_none());
    let task = registry.task(&task_id).unwrap();
    let persisted = read_task(&task.paths.json).unwrap();
    let started = persisted.remote.as_ref().unwrap().started.as_ref().unwrap();
    assert_eq!(
        (started.seq, started.queue_wait_ms, started.started_at_ms),
        (0, 123, 1)
    );
    assert_eq!(persisted.remote.as_ref().unwrap().last_seq, Some(0));
    assert!(
        persisted.started_at > 1,
        "runner time must not reset the local start or timeout"
    );
    assert!(running.info.status_reason.is_none());
    let killing = registry.clone();
    tokio::task::spawn_blocking(move || killing.kill_remote_task(&task).unwrap())
        .await
        .unwrap();
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Killed);
    assert!(done.phase.is_none());
    assert_eq!(exec_runs(&daemon).len(), 1);
}

#[test]
fn runon_memory_changes_the_durable_retry_and_reattach_request_digest() {
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, _) = persist_accepted_with_snapshot(
        dir.path(),
        dir.path().join("unused-connection"),
        "cargo test",
    );
    let task = registry.task(&task_id).unwrap();
    let request = RunRequest::new("/workspace", "/repository", "/workspace", "cargo test");
    let unrequested = serde_json::to_vec(&request).unwrap();
    let request = request.with_memory_mib(16_384);
    let sized = serde_json::to_vec(&request).unwrap();
    let larger = serde_json::to_vec(&request.clone().with_memory_mib(32_768)).unwrap();
    let digest = save_retry_request(&task, &request).unwrap();
    assert_eq!(digest, retry_request_digest(&task, &sized));
    assert_ne!(digest, retry_request_digest(&task, &unrequested));
    assert_ne!(digest, retry_request_digest(&task, &larger));
    assert_eq!(restore_retry_request(&task, &digest).unwrap(), request);
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
            30 * 60,
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
        30 * 60,
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
        30 * 60,
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

    let error = build_remote_request(
        &main,
        &unrelated,
        "true",
        BTreeMap::new(),
        30 * 60,
        &exec::PresetParams::default(),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains(&format!(
            "workdir {} is in the checkout {}, which belongs to a different repository than this session's checkout {}",
            unrelated.display(),
            unrelated.display(),
            main.display()
        )),
        "unexpected refusal reason: {error}"
    );

    // The runner may place TMPDIR inside a checkout. Use the system's other
    // Unix temporary root so this directory is in no checkout at all.
    let outside = tempfile::Builder::new()
        .prefix("aft-runon-plain-outside-")
        .tempdir_in("/var/tmp")
        .unwrap();
    assert!(
        worktree_root_containing(outside.path(), exec::DirectoryKind::WorkingDirectory)
            .unwrap()
            .is_none()
    );
    assert_eq!(return_paths_root(&main, outside.path()), main);
    let error = build_remote_request(
        &main,
        outside.path(),
        "true",
        BTreeMap::new(),
        30 * 60,
        &exec::PresetParams::default(),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains(&format!(
            "workdir {} is not inside a git checkout; this session's checkout is {}",
            outside.path().canonicalize().unwrap().display(),
            main.display()
        )),
        "unexpected refusal reason: {error}"
    );
}

#[test]
fn published_report_vectors_render_changes_say_nothing_when_empty_and_name_what_is_absent() {
    use crate::exec_remote::wire_fixture::report_vector;
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
        render_terminal_report(&report_vector("all"), &[]).as_deref(),
        Some(all)
    );
    // Reported and unchanged: a reporting runner sent every field, all
    // empty, so there is nothing to say.
    let unchanged = report_vector("unchanged");
    assert!(unchanged.git_state_changed.is_some());
    assert_eq!(render_terminal_report(&unchanged, &[]), None);
    // Absent (an older runner): only the missing changed-file list is named.
    // The Git, untracked-file, and ignored-write reports stay silent when
    // absent because the runner never fills them.
    assert_eq!(
        render_terminal_report(&report_vector("older-runner"), &[]).as_deref(),
        Some("These files changed on the server and were NOT copied back:\n  result.txt")
    );
    // A detached HEAD has no ref on either side, so no ref line; an index
    // tree that became available counts as changed.
    let detached = render_terminal_report(&report_vector("detached-head"), &[]).unwrap();
    assert!(!detached.contains("ref:"), "{detached}");
    assert!(detached.contains("HEAD: 111111111111 -> 222222222222\n  index tree changed"));
    // A capped untracked list says the runner listed only some of them.
    let truncated = render_terminal_report(&report_vector("truncated-untracked"), &[]).unwrap();
    assert!(
        truncated
            .contains("  notes.txt\n  (the runner listed only some of them; more were created)\n"),
        "{truncated}"
    );
}

const CHANGED_FILES_PREFIX: &str = "\n  shown-20.txt\n  shown-19.txt\n  shown-18.txt\n  \
    shown-17.txt\n  shown-16.txt\n  shown-15.txt\n  shown-14.txt\n  shown-13.txt\n  \
    shown-12.txt\n  shown-11.txt\n  shown-10.txt\n  shown-09.txt\n  shown-08.txt\n  \
    shown-07.txt\n  shown-06.txt\n  shown-05.txt\n  shown-04.txt\n  shown-03.txt\n  \
    shown-02.txt\n  shown-01.txt";

fn changed_files_record(remainder: &[&str]) -> TerminalRecord {
    let mut terminal = TerminalRecord::new(id(), Outcome::Exit { code: 0 }, 1, 0, 0);
    let mut changes: Vec<String> = (1..=20)
        .rev()
        .map(|index| format!("shown-{index:02}.txt"))
        .collect();
    changes.extend(remainder.iter().map(|path| (*path).to_string()));
    terminal.workspace_changes = Some(changes);
    terminal
}

#[test]
fn changed_files_report_twenty_paths_keeps_the_original_format_and_order() {
    assert_eq!(
        render_terminal_report(&changed_files_record(&[]), &[]).unwrap(),
        format!(
            "These files changed on the server and were NOT copied back:{CHANGED_FILES_PREFIX}"
        )
    );
}

#[test]
fn changed_files_report_twenty_one_paths_names_the_total_and_one_more() {
    assert_eq!(
        render_terminal_report(&changed_files_record(&["src/nested/extra.txt"]), &[]).unwrap(),
        format!(
            "These files changed on the server and were NOT copied back (21 files):\
             {CHANGED_FILES_PREFIX}\n  and 1 more: src/ (1)"
        )
    );
}

#[test]
fn changed_files_report_shows_five_largest_folders_and_counts_the_rest_elsewhere() {
    let terminal = changed_files_record(&[
        "z/a", "z/b", "z/c", "y/a", "y/b", "x/a", "w/a", "v/a", "u/a", "t/a",
    ]);
    assert_eq!(
        render_terminal_report(&terminal, &[]).unwrap(),
        format!(
            "These files changed on the server and were NOT copied back (30 files):\
             {CHANGED_FILES_PREFIX}\n  and 10 more: z/ (3), y/ (2), t/ (1), u/ (1), v/ (1), 2 elsewhere"
        )
    );
}

#[test]
fn changed_files_report_folder_count_ties_sort_by_name() {
    let terminal =
        changed_files_record(&["zeta/one", "beta/one", "alpha/one", "beta/two", "alpha/two"]);
    assert_eq!(
        render_terminal_report(&terminal, &[]).unwrap(),
        format!(
            "These files changed on the server and were NOT copied back (25 files):\
             {CHANGED_FILES_PREFIX}\n  and 5 more: alpha/ (2), beta/ (2), zeta/ (1)"
        )
    );
}

#[test]
fn changed_files_report_top_level_files_count_as_elsewhere() {
    let terminal = changed_files_record(&["root.txt", "src/item", "root2.txt"]);
    assert_eq!(
        render_terminal_report(&terminal, &[]).unwrap(),
        format!(
            "These files changed on the server and were NOT copied back (23 files):\
             {CHANGED_FILES_PREFIX}\n  and 3 more: src/ (1), 2 elsewhere"
        )
    );
    let terminal = changed_files_record(&["root.txt"]);
    assert_eq!(
        render_terminal_report(&terminal, &[]).unwrap(),
        format!(
            "These files changed on the server and were NOT copied back (21 files):\
             {CHANGED_FILES_PREFIX}\n  and 1 more: 1 elsewhere"
        )
    );
}

#[test]
fn changed_files_report_escapes_listed_paths_and_folder_names() {
    let mut terminal = changed_files_record(&["odd\nfolder/sub/file.txt"]);
    terminal.workspace_changes.as_mut().unwrap()[0] = "listed\nfile.txt".into();
    let expected_prefix = CHANGED_FILES_PREFIX.replacen("shown-20.txt", "listed\\nfile.txt", 1);
    assert_eq!(
        render_terminal_report(&terminal, &[]).unwrap(),
        format!(
            "These files changed on the server and were NOT copied back (21 files):\
             {expected_prefix}\n  and 1 more: odd\\nfolder/ (1)"
        )
    );
}

#[test]
fn changed_files_report_uses_comma_separators_for_large_counts() {
    let mut terminal = changed_files_record(&["one.txt", "two.txt"]);
    let changes = terminal.workspace_changes.as_mut().unwrap();
    for (folder, count) in [("node_modules", 1198), (".npm", 60), ("dist", 4)] {
        changes.extend((0..count).map(|index| format!("{folder}/{index}.txt")));
    }
    assert_eq!(
        render_terminal_report(&terminal, &[]).unwrap(),
        format!(
            "These files changed on the server and were NOT copied back (1,284 files):\
             {CHANGED_FILES_PREFIX}\n  and 1,264 more: node_modules/ (1,198), .npm/ (60), dist/ (4), 2 elsewhere"
        )
    );
}

#[test]
fn changed_files_report_excludes_copied_back_paths_before_applying_the_cap() {
    let mut terminal = changed_files_record(&["returned/extra"]);
    terminal
        .workspace_changes
        .as_mut()
        .unwrap()
        .insert(0, "returned/first".into());
    let copied_back = ["returned/first", "returned/extra"];
    assert_eq!(
        render_terminal_report(&terminal, &copied_back).unwrap(),
        format!(
            "These files changed on the server and were NOT copied back:{CHANGED_FILES_PREFIX}"
        )
    );
    terminal
        .workspace_changes
        .as_mut()
        .unwrap()
        .push("src/one".into());
    assert_eq!(
        render_terminal_report(&terminal, &copied_back).unwrap(),
        format!(
            "These files changed on the server and were NOT copied back (21 files):\
             {CHANGED_FILES_PREFIX}\n  and 1 more: src/ (1)"
        )
    );
}

#[test]
fn unreported_git_untracked_and_ignored_reports_stay_silent() {
    // The build-server runner never fills the Git, untracked-file, or
    // ignored-write fields, so their absence carries no information and
    // renders nothing. Only a missing changed-file list is still named.
    let mut changed = TerminalRecord::new(id(), Outcome::Exit { code: 0 }, 1, 0, 0);
    changed.workspace_changes = Some(vec!["result.txt".into()]);
    let changed = render_terminal_report(&changed, &[]).unwrap();
    assert_eq!(
        changed,
        "These files changed on the server and were NOT copied back:\n  result.txt"
    );
    assert!(!changed.contains("not reported by the runner"), "{changed}");
    let mut missing = TerminalRecord::new(id(), Outcome::Exit { code: 0 }, 1, 0, 0);
    missing.workspace_changes = None;
    let missing = render_terminal_report(&missing, &[]).unwrap();
    assert_eq!(missing, "changed files: not reported by the runner");
    assert!(!missing.contains("git state"), "{missing}");
    assert!(!missing.contains("untracked files"), "{missing}");
    assert!(!missing.contains("ignored writes"), "{missing}");
}

#[test]
fn remote_phase_comes_from_runner_evidence() {
    let mut remote = RemoteTask {
        copy_warning: false,
        submission: None,
        accepted_platform: None,
        connection_file: None,
        explicit_runon: true,
        harness: "broca".into(),
        session: "session".into(),
        requested_platform: None,
        requested_vcpus: None,
        requested_network: false,
        granted_network: false,
        accepted: false,
        started: None,
        job_id: None,
        last_seq: None,
        gap_recovery: None,
        undelivered_output: Vec::new(),
        stdout_len: 0,
        stderr_len: 0,
        unknown_len: 0,
        output_limit_dropped_bytes: 0,
        cancel_requested: false,
        cancel_pending: false,
        owner_pause_cancel: None,
        terminal: None,
        pre_start_refusal: None,
        fallback_digest: None,
        env_not_forwarded: None,
        queue_position: None,
        queue_wait_ms: None,
        output_received: false,
        retry: None,
        output_incomplete: None,
        workspace_holder: None,
        return_paths: Vec::new(),
        returned_files: None,
        return_first_seq: None,
        return_staging: Default::default(),
        return_max_bytes: exec::return_max_bytes(),
        return_cap_retried: false,
        expired_gap_unverified: Vec::new(),
    };
    assert_eq!(RemotePhase::of(&remote), RemotePhase::Submitting);
    remote.job_id = Some(id());
    assert_eq!(
        RemotePhase::of(&remote),
        RemotePhase::WaitingOnRunner,
        "a saved job id identifies a submitted job, not an input-capture phase"
    );
    remote.queue_wait_ms = Some(42_000);
    assert_eq!(
        RemotePhase::of(&remote),
        RemotePhase::QueueWait { elapsed_ms: 42_000 }
    );
    remote.queue_wait_ms = None;
    remote.accepted = true;
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
    remote.started = Some(Started::new(0, 123, 1));
    assert_eq!(RemotePhase::of(&remote), RemotePhase::Running);
    remote.started = None;
    remote.output_received = true;
    assert_eq!(RemotePhase::of(&remote), RemotePhase::Running);
}

#[test]
fn remote_job_id_lookup_rejects_foreign_identity_and_local_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, paths) = persist_accepted_with_snapshot(
        dir.path(),
        dir.path().join("missing-connection"),
        "cargo test",
    );
    let owner = crate::bash_background::TaskIdentity {
        principal: Some("owner".into()),
        harness: "runner".into(),
        session: "session".into(),
    };
    let task = registry.task(&task_id).unwrap();
    let mut metadata = read_task(&paths.json).unwrap();
    metadata.launching_identity = Some(owner.clone());
    write_task(&paths.json, &metadata).unwrap();
    task.state.lock().unwrap().metadata = metadata.clone();
    let job = id().to_string();
    {
        let _identity = crate::bash_background::task_identity_scope(Some(owner.clone()));
        assert_eq!(
            registry.resolve_task_reference(&job, "session", dir.path()),
            task_id
        );
        let cold = self::registry();
        assert_eq!(
            cold.resolve_task_reference(&job, "session", dir.path()),
            task_id
        );
    }
    {
        let mut foreign = owner;
        foreign.principal = Some("foreign".into());
        let _identity = crate::bash_background::task_identity_scope(Some(foreign));
        assert_eq!(
            registry.resolve_task_reference(&job, "session", dir.path()),
            job
        );
        assert_eq!(
            self::registry().resolve_task_reference(&job, "session", dir.path()),
            job
        );
    }
    metadata.remote = None;
    metadata.shell_remote_job_id = Some(job.clone());
    write_task(&paths.json, &metadata).unwrap();
    task.state.lock().unwrap().metadata = metadata;
    assert_eq!(
        registry.resolve_task_reference(&job, "session", dir.path()),
        job
    );
    assert_eq!(
        self::registry().resolve_task_reference(&job, "session", dir.path()),
        job
    );
    assert_eq!(
        registry.resolve_task_reference(&task_id, "session", dir.path()),
        task_id
    );
}

#[test]
fn remote_phase_note_says_submitting_then_queued_until_started() {
    let dir = tempfile::tempdir().unwrap();
    let mut metadata = PersistedTask::starting(
        "bash-0000000000000001".into(),
        "session".into(),
        "cargo test".into(),
        dir.path().into(),
        None,
        Some(600_500),
        true,
        false,
    );
    metadata.status = BgTaskStatus::Running;
    metadata.started_at = 1_000_000;
    metadata.remote = Some(RemoteTask {
        copy_warning: false,
        submission: None,
        accepted_platform: None,
        connection_file: None,
        explicit_runon: true,
        harness: "runner".into(),
        session: "session".into(),
        requested_platform: None,
        requested_vcpus: None,
        requested_network: false,
        granted_network: false,
        accepted: false,
        started: None,
        job_id: None,
        last_seq: None,
        gap_recovery: None,
        undelivered_output: Vec::new(),
        stdout_len: 0,
        stderr_len: 0,
        unknown_len: 0,
        output_limit_dropped_bytes: 0,
        cancel_requested: false,
        cancel_pending: false,
        owner_pause_cancel: None,
        terminal: None,
        pre_start_refusal: None,
        fallback_digest: None,
        env_not_forwarded: None,
        queue_position: None,
        queue_wait_ms: None,
        output_received: false,
        retry: None,
        output_incomplete: None,
        workspace_holder: None,
        return_paths: Vec::new(),
        returned_files: None,
        return_first_seq: None,
        return_staging: Default::default(),
        return_max_bytes: exec::return_max_bytes(),
        return_cap_retried: false,
        expired_gap_unverified: Vec::new(),
    });
    let now = 1_042_900;
    // Before acceptance the runner has not confirmed it copied the inputs.
    assert_eq!(
        remote_phase_note(&metadata, now).as_deref(),
        Some("submitting to the runner (capturing inputs; not accepted yet)")
    );
    let remote = metadata.remote.as_mut().unwrap();
    remote.accepted = true;
    remote.job_id = Some(id());
    remote.queue_position = Some(7);
    assert_eq!(
        remote_phase_note(&metadata, now).as_deref(),
        Some("queued on the runner (42 s, position 7)")
    );
    metadata.remote.as_mut().unwrap().started = Some(Started::new(3, 40_000, 1));
    // The task's timeout_ms is 600_500 (set above); the runner takes whole
    // seconds, so AFT asks it for 601 and the note says the same.
    assert_eq!(
        remote_phase_note(&metadata, now).as_deref(),
        Some("the runner stops it 601 s after it started")
    );
    // A `runon` task the runner refused may fall back to a local process
    // (here, a local child_pid is set); it then has no remote phase note.
    let mut fallback = metadata.clone();
    fallback.child_pid = Some(4242);
    assert_eq!(remote_phase_note(&fallback, now), None);
    metadata.mark_terminal(BgTaskStatus::Completed, Some(0), None);
    assert_eq!(remote_phase_note(&metadata, now), None);
}

#[test]
fn remote_phase_note_queue_wait_uses_runner_elapsed_not_task_age() {
    let dir = tempfile::tempdir().unwrap();
    let (_registry, _task_id, paths) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "cargo test");
    let mut metadata = read_task(&paths.json).unwrap();
    let now = unix_millis();
    metadata.started_at = now;
    let remote = metadata.remote.as_mut().unwrap();
    remote.accepted = false;
    remote.queue_position = None;
    remote.queue_wait_ms = Some(42_000);
    assert_eq!(
        remote_phase_note(&metadata, now).as_deref(),
        Some("queued on the runner (42 s)")
    );
}

/// A configured name the user forwarded through `bash.runon_env` travels to
/// the job, and so does a per-call name. Both live in the request map, which is
/// layered over the ambient environment rather than filtered like it.
#[tokio::test]
async fn runon_keeps_a_forwarded_configured_name_and_a_per_call_name() {
    let daemon = daemon(Script::GatedAcceptance, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    ensure_git_fixture(dir.path());
    let ctx = Arc::new(restarted_context(dir.path()));
    let mut policy = launch(daemon.connection.clone());
    policy.explicit_runon = true;
    // `TZ` is configured and forwarded; `OTHER` is configured but not
    // forwarded, so it is not in the request map at all.
    policy.configured_env_names = vec!["TZ".to_string(), "OTHER".to_string()];
    ctx.bash_background()
        .spawn_remote(
            policy,
            SpawnPlan::Unsandboxed,
            "uname -s",
            resolve_posix_shell(),
            "session".into(),
            dir.path().into(),
            HashMap::from([
                ("TZ".to_string(), "UTC".to_string()),
                ("CALL_ONLY".to_string(), "call-value".to_string()),
            ]),
            crate::bash_background::HardKill::After(Duration::from_secs(30)),
            None,
            dir.path().into(),
            crate::bash_background::TaskSlot::Background { max: 10 },
            true,
            false,
            Some(dir.path().into()),
        )
        .unwrap();
    daemon.wait_for_requests("exec.run", 1).await;
    let runs = exec_runs(&daemon);
    assert_eq!(runs.len(), 1, "{runs:?}");
    let env = &runs[0]["env"];
    assert_eq!(
        env["TZ"], "UTC",
        "a forwarded configured name must survive into the request: {env}"
    );
    assert_eq!(
        env["CALL_ONLY"], "call-value",
        "a per-call name must survive into the request: {env}"
    );
}

#[test]
fn cancelled_call_notice_names_a_remote_task_and_its_job_once() {
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, paths) = persist_accepted_with_snapshot(
        dir.path(),
        dir.path().join("missing-connection.json"),
        "cargo test",
    );
    registry.note_cancelled_call(&task_id, "session");
    assert!(read_task(&paths.json).unwrap().cancelled_call_unannounced);
    assert!(registry
        .take_cancelled_call_notices("other-session")
        .is_empty());
    assert_eq!(
        registry.take_cancelled_call_notices("session"),
        vec![format!(
            "task {task_id} (job {}) from your cancelled call is still running; check it with bash_status or stop it with bash_kill",
            id()
        )]
    );
    assert!(registry.take_cancelled_call_notices("session").is_empty());
    assert!(!read_task(&paths.json).unwrap().cancelled_call_unannounced);
    registry.detach();
}

#[tokio::test]
async fn runon_draining_twice_then_accepts_and_discloses_wait() {
    let daemon = daemon(
        Script::Draining {
            refusals: 2,
            retry_after_ms: Some(150),
        },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut clock = PausedDrainingClock::new(dir.path());
    ensure_git_fixture(dir.path());
    let registry = registry();
    let mut policy = launch(daemon.connection.clone());
    policy.explicit_runon = true;
    let task_id = registry
        .spawn_remote(
            policy,
            SpawnPlan::Unsandboxed,
            "printf local-proof",
            resolve_posix_shell(),
            "session".into(),
            dir.path().into(),
            HashMap::new(),
            crate::bash_background::HardKill::After(Duration::from_secs(30)),
            None,
            dir.path().into(),
            crate::bash_background::TaskSlot::Background { max: 10 },
            true,
            false,
            Some(dir.path().into()),
        )
        .unwrap();
    let first = clock.now();
    clock.expect_sleep(first, first + 1000).await;
    let waiting = registry.observed_status(&task_id, "session", 8192).unwrap();
    assert_eq!(waiting.info.status, BgTaskStatus::Starting, "{waiting:?}");
    assert_eq!(
        registry.runon_launch_pending(&task_id, "session"),
        Some(true)
    );
    assert!(
        waiting
            .output_preview
            .contains("draining for maintenance; retrying (attempt 2"),
        "{waiting:?}"
    );
    clock.advance(5000);
    clock.expect_sleep(first + 5000, first + 6000).await;
    assert_eq!(exec_runs(&daemon).len(), 2);
    clock.advance(5000);
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.exit_code, Some(0), "{done:?}");
    assert!(!done.output_preview.contains("local-proof"));
    let requests = exec_runs(&daemon);
    assert_eq!(requests.len(), 3);
    assert!(requests.windows(2).all(|pair| pair[0] == pair[1]));
    // The record keeps when the accepted resubmission was sent, so re-attach
    // limits count from it after a restart too.
    let saved = read_task(&registry.task(&task_id).unwrap().paths.json).unwrap();
    let resubmitted_at = saved
        .remote
        .as_ref()
        .and_then(|remote| remote.retry.as_ref())
        .and_then(|retry| retry.resubmitted_at_ms);
    assert!(
        resubmitted_at.is_some_and(|at| at >= saved.started_at),
        "the accepted resubmission must record its time: {resubmitted_at:?}"
    );
}

/// A draining retry the runner accepts late starts a new queue wait and run
/// there. AFT's re-attach limit must count from that resubmission, both in
/// the worker that sent it and after a daemon restart reads the record back.
#[tokio::test]
async fn reattach_limit_counts_from_a_late_draining_resubmission() {
    const MINUTE_MS: u64 = 60_000;
    let dir = tempfile::tempdir().unwrap();
    let now = unix_millis();
    let mut metadata = PersistedTask::starting(
        "bash-0000000000000002".into(),
        "session".into(),
        "cargo test".into(),
        dir.path().into(),
        None,
        // An untimed runon records the 30-minute background default.
        Some(30 * MINUTE_MS),
        true,
        false,
    );
    // Submitted first nine minutes ago; the runner drained until now.
    metadata.started_at = now - 9 * MINUTE_MS;
    metadata.remote = Some(RemoteTask {
        copy_warning: false,
        submission: None,
        accepted_platform: None,
        connection_file: None,
        explicit_runon: true,
        harness: "runner".into(),
        session: "session".into(),
        requested_platform: None,
        requested_vcpus: None,
        requested_network: false,
        granted_network: false,
        accepted: false,
        started: None,
        job_id: None,
        last_seq: None,
        gap_recovery: None,
        undelivered_output: Vec::new(),
        stdout_len: 0,
        stderr_len: 0,
        unknown_len: 0,
        output_limit_dropped_bytes: 0,
        cancel_requested: false,
        cancel_pending: false,
        owner_pause_cancel: None,
        terminal: None,
        pre_start_refusal: None,
        fallback_digest: None,
        env_not_forwarded: None,
        queue_position: None,
        queue_wait_ms: None,
        output_received: false,
        retry: Some(DrainingRetry {
            request_digest: "digest".into(),
            attempts: 1,
            first_refusal_at_ms: Some(metadata.started_at),
            next_retry_at_ms: None,
            waited_ms: None,
            resubmitted_at_ms: None,
        }),
        output_incomplete: None,
        workspace_holder: None,
        return_paths: Vec::new(),
        returned_files: None,
        return_first_seq: None,
        return_staging: Default::default(),
        return_max_bytes: exec::return_max_bytes(),
        return_cap_retried: false,
        expired_gap_unverified: Vec::new(),
    });
    let minutes_left = |budget: &ReattachBudget| {
        budget
            .job_end
            .saturating_duration_since(tokio::time::Instant::now())
            .as_secs_f64()
            / 60.0
    };
    // A conservative 30-minute queue allowance plus 30 of run, counted from submission.
    let full_window = 60.0;

    let mut budget = ReattachBudget::new(dir.path(), &metadata);
    let before = minutes_left(&budget);
    assert!(
        (full_window - 9.5..=full_window - 9.0).contains(&before),
        "before the resubmission the limit counts from the first submission: {before:.2} minutes left"
    );

    metadata
        .remote
        .as_mut()
        .unwrap()
        .retry
        .as_mut()
        .unwrap()
        .resubmitted_at_ms = Some(now);
    budget.resubmitted(&metadata);
    let live = minutes_left(&budget);
    assert!(
        live >= full_window - 0.5,
        "the worker that resubmitted must get the full window from the resubmission: {live:.2} minutes left"
    );

    let record = dir.path().join("task.json");
    write_task(&record, &metadata).unwrap();
    let reloaded = read_task(&record).unwrap();
    let restarted = minutes_left(&ReattachBudget::new(dir.path(), &reloaded));
    assert!(
        restarted >= full_window - 0.5,
        "a restarted daemon must count from the recorded resubmission: {restarted:.2} minutes left"
    );

    // A record written before the resubmission time existed still loads and
    // counts from the task's start.
    let mut older = serde_json::to_value(&reloaded).unwrap();
    let removed = older["remote"]["retry"]
        .as_object_mut()
        .unwrap()
        .remove("resubmitted_at_ms");
    assert!(removed.is_some());
    let older: PersistedTask = serde_json::from_value(older).unwrap();
    let fallback = minutes_left(&ReattachBudget::new(dir.path(), &older));
    assert!(
        (full_window - 9.5..=full_window - 9.0).contains(&fallback),
        "an older record counts from the task's start: {fallback:.2} minutes left"
    );
}

// Remote workers have their own runtimes, so pausing the test's Tokio runtime
// cannot control their durable timestamps or sleeps. Share one clock per root
// across the original and replayed worker, without affecting parallel tests.

async fn assert_zero_hint_waits_for_floor(signer: bool) {
    let daemon = daemon(retry_backoff_script(signer, 0), "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let mut clock = PausedDrainingClock::new(dir.path());
    let registry = registry();
    let task = start_runon_retry(&registry, dir.path(), daemon.connection.clone());
    let first = clock.now();
    tokio::select! {
        _ = clock.expect_sleep(first, first + if signer { 5000 } else { 1000 }) => {},
        done = terminal(&registry, &task) => panic!("zero hint resubmitted without waiting: {done:?}"),
    }
    clock.advance(4999);
    if !signer {
        clock.expect_sleep(first + 4999, first + 5000).await;
    }
    assert_eq!(
        exec_runs(&daemon).len(),
        1,
        "must wait at least five seconds"
    );
    clock.advance(1);
    let done = terminal(&registry, &task).await;
    assert_eq!(done.exit_code, Some(0), "{done:?}");
    assert_eq!(exec_runs(&daemon).len(), 2);
}

#[tokio::test]
async fn runon_retry_backoff_draining_zero_hint_waits_five_seconds() {
    assert_zero_hint_waits_for_floor(false).await;
}

#[tokio::test]
async fn runon_retry_backoff_signer_zero_hint_waits_five_seconds() {
    assert_zero_hint_waits_for_floor(true).await;
}

#[tokio::test]
async fn runon_retry_backoff_large_hints_are_not_shortened() {
    for signer in [false, true] {
        let daemon = daemon(retry_backoff_script(signer, 30_000), "exec-remote/v1").await;
        let dir = tempfile::tempdir().unwrap();
        let mut clock = PausedDrainingClock::new(dir.path());
        let registry = registry();
        let mut remote = launch(daemon.connection.clone());
        remote.explicit_runon = true;
        // An explicit timeout also caps hinted retries, so allow the 30s hint to finish.
        let task = start_with_launch(
            &registry,
            dir.path(),
            remote,
            crate::bash_background::HardKill::After(Duration::from_secs(60)),
        );
        let first = clock.now();
        clock
            .expect_sleep(first, first + if signer { 30_000 } else { 1000 })
            .await;
        clock.advance(29_999);
        if !signer {
            clock.expect_sleep(first + 29_999, first + 30_000).await;
        }
        assert_eq!(exec_runs(&daemon).len(), 1, "must honour the runner's hint");
        clock.advance(1);
        assert_eq!(terminal(&registry, &task).await.exit_code, Some(0));
        assert_eq!(exec_runs(&daemon).len(), 2);
    }
}

#[tokio::test]
async fn runon_retry_backoff_simultaneous_refusals_resubmit_at_different_times() {
    for signer in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut clock = PausedDrainingClock::new(dir.path());
        let registry = registry();
        let first = clock.now();
        let mut tasks = Vec::new();
        for jitter in [0, 500, 1000] {
            clock.set_retry_jitter_ms(jitter);
            let daemon = daemon(retry_backoff_script(signer, 5000), "exec-remote/v1").await;
            let task = start_runon_retry(&registry, dir.path(), daemon.connection.clone());
            clock
                .expect_sleep(first, first + if signer { 5000 + jitter } else { 1000 })
                .await;
            if !signer {
                let saved = read_task(&registry.task(&task).unwrap().paths.json).unwrap();
                assert_eq!(
                    saved.remote.unwrap().retry.unwrap().next_retry_at_ms,
                    Some(first + 5000 + jitter)
                );
            }
            tasks.push((daemon, task));
        }
        // All three refusals used the same logical time; only their jitter differs.
        clock.advance(5000);
        for (index, (daemon, task)) in tasks.iter().enumerate() {
            if index > 0 {
                assert_eq!(
                    exec_runs(daemon).len(),
                    1,
                    "later retries must still be waiting"
                );
                clock.advance(500);
            }
            assert_eq!(terminal(&registry, task).await.exit_code, Some(0));
            assert_eq!(exec_runs(daemon).len(), 2);
        }
    }
}

#[tokio::test]
async fn runon_draining_budget_caps_retry_after() {
    let daemon = daemon(
        Script::Draining {
            refusals: u32::MAX,
            retry_after_ms: Some(60_000),
        },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    shorten_draining_budget(dir.path(), 250);
    let mut clock = PausedDrainingClock::new(dir.path());
    let registry = registry();
    let task = start_runon_retry(&registry, dir.path(), daemon.connection.clone());
    let first = clock.now();
    clock.expect_sleep(first, first + 250).await;
    clock.advance(250);
    let done = terminal(&registry, &task).await;
    assert_eq!(done.info.status, BgTaskStatus::Failed);
    let saved = read_task(&registry.task(&task).unwrap().paths.json).unwrap();
    assert_eq!(saved.remote.unwrap().retry.unwrap().waited_ms, Some(250));
    assert_eq!(exec_runs(&daemon).len(), 1);
    let refusal = done.remote_refusal.unwrap();
    assert!(
        refusal
            .message
            .contains("runner_draining (maintenance) after retrying for "),
        "{refusal:?}"
    );
    assert!(refusal.message.contains("ms; command was not run"));
    assert_eq!(retry_duration(DRAINING_RETRY_BUDGET_MS), "10m");
}

#[tokio::test]
async fn runon_draining_retry_after_is_honoured() {
    let daemon = daemon(
        Script::Draining {
            refusals: 1,
            retry_after_ms: Some(300),
        },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut clock = PausedDrainingClock::new(dir.path());
    let registry = registry();
    let task = start_runon_retry(&registry, dir.path(), daemon.connection.clone());
    let first = clock.now();
    clock.expect_sleep(first, first + 1000).await;
    clock.advance(100);
    assert_eq!(exec_runs(&daemon).len(), 1, "must not resubmit early");
    clock.advance(4900);
    let done = terminal(&registry, &task).await;
    assert_eq!(done.exit_code, Some(0));
    assert_eq!(exec_runs(&daemon).len(), 2);
}

#[tokio::test]
async fn runon_draining_kill_stops_retries() {
    let daemon = daemon(
        Script::Draining {
            refusals: u32::MAX,
            retry_after_ms: Some(500),
        },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    shorten_draining_budget(dir.path(), 60_000);
    let mut clock = PausedDrainingClock::new(dir.path());
    let registry = registry();
    let task_id = start_runon_retry(&registry, dir.path(), daemon.connection.clone());
    let first = clock.now();
    clock.expect_sleep(first, first + 1000).await;
    let killing = registry.clone();
    let killed_id = task_id.clone();
    // With the retry clock frozen, a terminal kill proves cancellation woke
    // the worker; the wall-clock timeout only guards against a deadlock.
    let killed = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || killing.kill(&killed_id, "session")),
    )
    .await
    .expect("kill must interrupt retry sleep without advancing the clock")
    .unwrap()
    .unwrap();
    assert_eq!(killed.info.status, BgTaskStatus::Killed);
    assert_eq!(clock.now(), first);
    clock.advance(6000);
    assert_eq!(
        exec_runs(&daemon).len(),
        1,
        "kill must veto any new exec.run"
    );
}

#[tokio::test]
async fn runon_draining_restart_preserves_remaining_budget_and_request() {
    let daemon = crate::exec_remote::wire_fixture::daemon_with_clients(
        Script::Draining {
            refusals: u32::MAX,
            retry_after_ms: Some(10_000),
        },
        "exec-remote/v1",
        2,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    shorten_draining_budget(dir.path(), 600);
    let mut clock = PausedDrainingClock::new(dir.path());
    stopped_draining_roots()
        .lock()
        .unwrap()
        .insert(dir.path().into());
    let original = registry();
    let task_id = start_runon_retry(&original, dir.path(), daemon.connection.clone());
    clock.expect_event(DrainingClockEvent::Stopped).await;
    let task = original.task(&task_id).unwrap();
    let saved = read_task(&task.paths.json).unwrap();
    let retry = saved.remote.as_ref().unwrap().retry.as_ref().unwrap();
    assert_eq!(retry.attempts, 1);
    let first = retry.first_refusal_at_ms.unwrap();
    assert_eq!(first, clock.now());
    assert_eq!(retry.next_retry_at_ms, Some(first + 10_000));
    assert!(!serde_json::to_string(&saved)
        .unwrap()
        .contains("private-value-proof"));
    let request =
        serde_json::to_value(restore_retry_request(&task, &retry.request_digest).unwrap()).unwrap();
    assert_eq!(request, exec_runs(&daemon)[0]);
    assert_eq!(
        request["env"]["REMOTE_REQUEST_PRIVATE_VALUE"],
        "private-value-proof"
    );
    clock.advance(400);
    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    // The saved refusal used 400 ms of its 600 ms budget. Replaying must
    // schedule only the remaining 200 ms, not another full 600 ms.
    clock.expect_sleep(first + 400, first + 600).await;
    let replayed_task = restarted.task(&task_id).unwrap();
    assert_eq!(
        serde_json::to_value(restore_retry_request(&replayed_task, &retry.request_digest).unwrap())
            .unwrap(),
        request
    );
    clock.advance(200);
    let done = terminal(&restarted, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Failed);
    assert_eq!(exec_runs(&daemon).len(), 1);
    let restored = read_task(&task.paths.json).unwrap();
    let retry = restored.remote.unwrap().retry.unwrap();
    assert_eq!(retry.first_refusal_at_ms, Some(first));
    assert_eq!(retry.attempts, 1);
    assert_eq!(retry.waited_ms, Some(600));
    assert_eq!(
        retry.request_digest,
        saved.remote.unwrap().retry.unwrap().request_digest
    );
}

#[tokio::test]
async fn runon_draining_restart_resubmits_and_accepts() {
    let daemon = crate::exec_remote::wire_fixture::daemon_with_clients(
        Script::Draining {
            refusals: 1,
            retry_after_ms: Some(200),
        },
        "exec-remote/v1",
        2,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut clock = PausedDrainingClock::new(dir.path());
    clock.set_retry_jitter_ms(1000);
    stopped_draining_roots()
        .lock()
        .unwrap()
        .insert(dir.path().into());
    let original = registry();
    let task = start_runon_retry(&original, dir.path(), daemon.connection.clone());
    clock.expect_event(DrainingClockEvent::Stopped).await;
    let first = clock.now();
    let saved = read_task(&original.task(&task).unwrap().paths.json).unwrap();
    assert_eq!(
        saved.remote.unwrap().retry.unwrap().next_retry_at_ms,
        Some(first + 6000)
    );
    // Replaying must keep the sampled deadline, not choose jitter again.
    clock.set_retry_jitter_ms(0);
    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    clock.expect_sleep(first, first + 1000).await;
    clock.advance(5999);
    clock.expect_sleep(first + 5999, first + 6000).await;
    assert_eq!(
        exec_runs(&daemon).len(),
        1,
        "persisted jitter must survive restart"
    );
    clock.advance(1);
    let done = terminal(&restarted, &task).await;
    assert_eq!(done.exit_code, Some(0), "{done:?}");
    let requests = exec_runs(&daemon);
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0], requests[1]);
}

#[test]
fn draining_default_backoff_is_bounded() {
    for (attempt, expected) in [
        (1, 5000),
        (2, 10000),
        (3, 20000),
        (4, 40000),
        (5, 60000),
        (100, 60000),
    ] {
        assert_eq!(draining_delay_ms(attempt, None, |_| 0), expected);
        assert_eq!(
            draining_delay_ms(attempt, None, |max| max),
            expected + expected / 5
        );
    }
    assert_eq!(draining_delay_ms(100, Some(17), |_| 0), 5000);
    assert_eq!(draining_delay_ms(1, Some(0), |max| max), 6000);
    assert_eq!(draining_delay_ms(1, Some(30_000), |max| max), 36_000);
    assert_eq!(draining_delay_ms(1, Some(u64::MAX), |max| max), u64::MAX);
}

#[tokio::test]
async fn runon_draining_restart_obeys_persisted_cancel() {
    let daemon = daemon(
        Script::Draining {
            refusals: u32::MAX,
            retry_after_ms: Some(10_000),
        },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut clock = PausedDrainingClock::new(dir.path());
    stopped_draining_roots()
        .lock()
        .unwrap()
        .insert(dir.path().into());
    let original = registry();
    let task_id = start_runon_retry(&original, dir.path(), daemon.connection.clone());
    clock.expect_event(DrainingClockEvent::Stopped).await;
    let task = original.task(&task_id).unwrap();
    original.record_remote_cancel(&task).unwrap();
    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    let done = terminal(&restarted, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Killed, "{done:?}");
    assert_eq!(exec_runs(&daemon).len(), 1);
}

#[tokio::test]
async fn runon_draining_restart_does_not_resubmit_ambiguous_dispatch() {
    let daemon = crate::exec_remote::wire_fixture::daemon_with_clients(
        Script::Draining {
            refusals: 1,
            retry_after_ms: Some(10_000),
        },
        "exec-remote/v1",
        2,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut clock = PausedDrainingClock::new(dir.path());
    stopped_draining_roots()
        .lock()
        .unwrap()
        .insert(dir.path().into());
    let original = registry();
    let task_id = start_runon_retry(&original, dir.path(), daemon.connection.clone());
    clock.expect_event(DrainingClockEvent::Stopped).await;
    let task = original.task(&task_id).unwrap();
    let mut saved = read_task(&task.paths.json).unwrap();
    let remote = saved.remote.as_mut().unwrap();
    remote.terminal = None;
    remote.job_id = None;
    remote.retry.as_mut().unwrap().attempts = 2;
    write_task(&task.paths.json, &saved).unwrap();
    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    let done = terminal(&restarted, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::FateUnknown, "{done:?}");
    assert_eq!(
        exec_runs(&daemon).len(),
        1,
        "missing acceptance must never authorize resubmission"
    );
}

/// Run the startup pass that sends owed cancels under `storage` to its end,
/// off the test runtime that serves the fake runner.
async fn run_owed_cancel_pass(storage: &Path) -> super::OwedCancelReport {
    let handle = BgTaskRegistry::start_owed_cancel_pass(storage).expect("first pass for this root");
    tokio::time::timeout(
        Duration::from_secs(60),
        tokio::task::spawn_blocking(move || handle.join().unwrap()),
    )
    .await
    .expect("the owed cancel pass must finish")
    .unwrap()
}

/// A paused worker's checkout may never bind again after a restart, so its
/// task's owed runner cancel must not wait for a replay. The startup pass
/// finds it in the saved records and sends it, with no project bound and no
/// task restored.
#[tokio::test]
async fn an_owed_runner_cancel_is_sent_at_startup_without_a_bind() {
    let dir = tempfile::tempdir().unwrap();
    let (daemon, task_id, job_id, json) = saved_task_owing_a_cancel(dir.path(), 2).await;
    // A new module starts; no project binds.
    let restarted = registry();
    let report = run_owed_cancel_pass(dir.path()).await;
    assert_eq!(report.sent, 1, "{report:?}");
    assert_owed_cancel_sent_once(&daemon, job_id, &json);
    assert!(
        restarted.task(&task_id).is_none(),
        "the pass restores no task"
    );
    assert_eq!(
        remote_workers(&task_id),
        (1, 1),
        "the pass starts no worker"
    );
}

/// The runner's connection is not available when the startup pass first
/// tries to send an owed cancel. The pass waits and tries again, and the
/// cancel is sent once the runner can be reached.
#[tokio::test]
async fn the_owed_cancel_pass_retries_until_the_runner_is_reachable() {
    let dir = tempfile::tempdir().unwrap();
    let (daemon, task_id, job_id, json) = saved_task_owing_a_cancel(dir.path(), 2).await;
    let hidden = daemon.connection.with_extension("hidden");
    fs::rename(&daemon.connection, &hidden).unwrap();
    let pass = {
        let handle = BgTaskRegistry::start_owed_cancel_pass(dir.path()).unwrap();
        tokio::task::spawn_blocking(move || handle.join().unwrap())
    };
    wait_for("the first attempt must fail", || {
        (failed_owed_cancel_attempts()
            .lock()
            .unwrap()
            .get(&task_id)
            .copied()
            .unwrap_or_default()
            >= 1)
            .then_some(())
    })
    .await;
    fs::rename(&hidden, &daemon.connection).unwrap();
    let report = tokio::time::timeout(Duration::from_secs(60), pass)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.sent, 1, "{report:?}");
    assert_owed_cancel_sent_once(&daemon, job_id, &json);
}

/// The pass reads only the owed-cancel index, not the task history: with 200
/// other finished records saved beside the one owing a cancel, it examines
/// one marker.
#[tokio::test]
async fn the_owed_cancel_pass_reads_only_owed_markers() {
    let dir = tempfile::tempdir().unwrap();
    let (daemon, _task_id, job_id, json) = saved_task_owing_a_cancel(dir.path(), 2).await;
    let finished = read_task(&json).unwrap();
    for n in 0..200u64 {
        let mut record = finished.clone();
        record.task_id = format!("bash-{:016x}", 0xabc000 + n);
        record.remote.as_mut().unwrap().cancel_pending = false;
        let layout = crate::bash_background::persistence::create_task_layout(
            dir.path(),
            &record.session_id,
            &record.task_id,
        )
        .unwrap();
        write_task_at(&layout, &record).unwrap();
    }
    let report = run_owed_cancel_pass(dir.path()).await;
    assert_eq!(report.examined, 1, "{report:?}");
    assert_eq!(report.sent, 1, "{report:?}");
    assert_owed_cancel_sent_once(&daemon, job_id, &json);
}

/// A marker left behind after its record stopped owing the cancel (a crash
/// between the record write and the marker's removal) is removed by the
/// pass, which sends nothing.
#[tokio::test]
async fn a_stale_owed_cancel_marker_is_removed_without_sending() {
    let dir = tempfile::tempdir().unwrap();
    let (daemon, task_id, _job_id, json) = saved_task_owing_a_cancel(dir.path(), 2).await;
    let mut saved = read_task(&json).unwrap();
    saved.remote.as_mut().unwrap().cancel_pending = false;
    write_task(&json, &saved).unwrap();
    let report = run_owed_cancel_pass(dir.path()).await;
    assert_eq!(
        (report.examined, report.stale_markers, report.sent),
        (1, 1, 0),
        "{report:?}"
    );
    assert!(exec_cancels(&daemon).is_empty());
    assert!(!owed_marker(dir.path(), &task_id).exists());
}

/// While the startup pass saves a paused task's owed-cancel outcome, the
/// task's project (which replayed it meanwhile) takes the session's
/// cancelled-call notices, clearing the record's notice flag. The pass must
/// keep that change: the session gets the owner-cancellation notice once,
/// and not again after another replay.
#[tokio::test]
async fn the_owed_cancel_pass_keeps_a_cancelled_call_notice_taken_during_its_save() {
    let dir = tempfile::tempdir().unwrap();
    let (daemon, task_id, job_id, json) = saved_task_owing_a_cancel(dir.path(), 3).await;
    assert!(read_task(&json).unwrap().cancelled_call_unannounced);
    let mut claimed = HeldDrainingGate::arm(dir.path(), DrainingGate::OwedCancelClaimed);
    let (save_reached, release_save) = {
        let (reached_tx, reached) = tokio::sync::oneshot::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        owed_cancel_save_gates()
            .lock()
            .unwrap()
            .insert(task_id.clone(), (reached_tx, release_rx));
        (reached, release)
    };
    let pass = {
        let handle = BgTaskRegistry::start_owed_cancel_pass(dir.path()).unwrap();
        tokio::task::spawn_blocking(move || handle.join().unwrap())
    };
    claimed.reached().await;
    // The project replays the task while the pass holds its claim.
    let project = registry();
    project.replay_session(dir.path(), "session").unwrap();
    claimed.release();
    tokio::time::timeout(Duration::from_secs(30), save_reached)
        .await
        .expect("the pass must reach its save")
        .unwrap();
    // The session takes its notices between the pass's read and its write.
    let taking = {
        let project = project.clone();
        tokio::task::spawn_blocking(move || project.take_cancelled_call_notices("session"))
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    release_save.send(()).unwrap();
    let notices = taking.await.unwrap();
    assert_eq!(
        notices
            .iter()
            .filter(|notice| notice.contains(&task_id))
            .count(),
        1,
        "{notices:?}"
    );
    tokio::time::timeout(Duration::from_secs(60), pass)
        .await
        .unwrap()
        .unwrap();
    assert_owed_cancel_sent_once(&daemon, job_id, &json);
    let saved = read_task(&json).unwrap();
    assert!(!saved.cancelled_call_unannounced, "{saved:?}");
    project.detach();
    // Another replay must not announce the cancelled call again.
    let again = registry();
    again.replay_session(dir.path(), "session").unwrap();
    let repeated = again.take_cancelled_call_notices("session");
    assert!(
        !repeated.iter().any(|notice| notice.contains(&task_id)),
        "the notice was delivered again: {repeated:?}"
    );
}

/// The startup pass has claimed a saved task's owed cancel when its project
/// binds and replays the task. The replay must not start a worker that sends
/// the cancel a second time; it takes the pass's outcome instead.
#[tokio::test]
async fn a_replay_during_the_owed_cancel_pass_sends_no_second_cancel() {
    let dir = tempfile::tempdir().unwrap();
    let (daemon, task_id, job_id, json) = saved_task_owing_a_cancel(dir.path(), 3).await;
    let mut gate = HeldDrainingGate::arm(dir.path(), DrainingGate::OwedCancelClaimed);
    let pass = {
        let handle = BgTaskRegistry::start_owed_cancel_pass(dir.path()).unwrap();
        tokio::task::spawn_blocking(move || handle.join().unwrap())
    };
    gate.reached().await;
    let project = registry();
    project.replay_session(dir.path(), "session").unwrap();
    assert_eq!(
        remote_workers(&task_id),
        (1, 1),
        "the replay started a worker"
    );
    gate.release();
    let report = tokio::time::timeout(Duration::from_secs(60), pass)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.sent, 1, "{report:?}");
    assert_owed_cancel_sent_once(&daemon, job_id, &json);
    let task = project.task(&task_id).unwrap();
    let remote = task.state.lock().unwrap().metadata.remote.clone().unwrap();
    assert!(!remote.cancel_pending, "{remote:?}");
    assert_eq!(remote.owner_pause_cancel.as_deref(), Some("sent"));
    assert_eq!(remote_workers(&task_id), (1, 1));
}

/// The startup pass and a project's replay of the same saved task run at the
/// same moment, ten times. Whichever claims the owed cancel first sends it;
/// the runner must receive it exactly once each time.
#[tokio::test]
async fn the_owed_cancel_pass_and_a_replay_racing_send_one_cancel() {
    for round in 0..10 {
        let dir = tempfile::tempdir().unwrap();
        let (daemon, _task_id, job_id, json) = saved_task_owing_a_cancel(dir.path(), 3).await;
        let start = Arc::new(std::sync::Barrier::new(2));
        let pass = {
            let (start, storage) = (start.clone(), dir.path().to_path_buf());
            tokio::task::spawn_blocking(move || {
                start.wait();
                BgTaskRegistry::start_owed_cancel_pass(&storage)
                    .unwrap()
                    .join()
                    .unwrap()
            })
        };
        let project = registry();
        let replay = {
            let (project, storage) = (project.clone(), dir.path().to_path_buf());
            tokio::task::spawn_blocking(move || {
                start.wait();
                project.replay_session(&storage, "session").unwrap();
            })
        };
        tokio::time::timeout(Duration::from_secs(60), pass)
            .await
            .unwrap()
            .unwrap();
        replay.await.unwrap();
        wait_for(
            &format!("round {round}: the owed cancel must be saved"),
            || {
                let remote = read_task(&json).ok()?.remote?;
                (!remote.cancel_pending && remote.owner_pause_cancel.as_deref() == Some("sent"))
                    .then_some(())
            },
        )
        .await;
        // Give a second sender the time it would need to reach the runner.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            exec_cancels(&daemon).len(),
            1,
            "round {round}: {:?}",
            exec_cancels(&daemon)
        );
        assert_owed_cancel_sent_once(&daemon, job_id, &json);
        project.detach();
    }
}

/// The owner pauses a scope whose `runon` job the runner has accepted, and
/// the module stops before its worker sends the cancellation. The pause
/// already saved the task as stopped, so the saved record must also say the
/// runner is still owed that cancellation: replaying it after the restart
/// sends it, and the runner job does not keep running unnoticed.
#[tokio::test]
async fn a_paused_runon_jobs_unsent_cancel_is_sent_after_a_restart() {
    let daemon =
        crate::exec_remote::wire_fixture::daemon_with_clients(Script::Cancel, "exec-remote/v1", 2)
            .await;
    let dir = tempfile::tempdir().unwrap();
    let scope = owner_scope("unsent-cancel");
    let original = registry();
    let task_id =
        start_scoped_runon_retry(&original, dir.path(), daemon.connection.clone(), &scope);
    let job_id = wait_for("the runner must accept the job", || {
        let task = original.task(&task_id)?;
        let state = task.state.lock().unwrap();
        state.metadata.remote.as_ref()?.job_id
    })
    .await;
    // The worker stops right before it would send the cancellation.
    stopped_draining_roots()
        .lock()
        .unwrap()
        .insert(dir.path().into());
    let members = original.begin_scope_pause(&scope).unwrap();
    original.pause_scope(&scope, true, &members).unwrap();
    remote_worker_exited(&task_id).await;
    assert!(exec_cancels(&daemon).is_empty(), "the cancel was sent");
    let json = original.task(&task_id).unwrap().paths.json.clone();
    let saved = read_task(&json).unwrap();
    assert_eq!(saved.status, BgTaskStatus::Killed, "{saved:?}");
    original.detach();

    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    daemon.wait_for_requests("exec.cancel", 1).await;
    let cancels = exec_cancels(&daemon);
    assert_eq!(cancels.len(), 1, "{cancels:?}");
    assert!(
        cancels[0].to_string().contains(&job_id.to_string()),
        "{cancels:?}"
    );
    let settled = wait_for("the sent cancellation must be saved", || {
        let saved = read_task(&json).ok()?;
        let remote = saved.remote.clone()?;
        (!remote.cancel_pending && remote.owner_pause_cancel.is_some()).then_some(saved)
    })
    .await;
    assert_eq!(settled.status, BgTaskStatus::Killed, "{settled:?}");
    assert_eq!(
        settled.status_reason.as_deref(),
        Some("paused_by_owner"),
        "{settled:?}"
    );
    assert_eq!(
        settled.remote.unwrap().owner_pause_cancel.as_deref(),
        Some("sent")
    );
    assert_eq!(exec_runs(&daemon).len(), 1, "never resubmitted");
}

/// What the threads of a stress round that has not finished are doing.
struct RoundSnapshot {
    /// Which of the shared task's main locks are held, and what each
    /// relevant thread is blocked in, by syscall and (for a lock wait) by
    /// the lock's name.
    report: String,
    /// A round thread is in a syscall other than a lock wait (on the build
    /// servers, typically an `fsync` of a task record that is slow under
    /// load, made while holding the task's state lock), or a background-task
    /// thread is in a disk syscall: the round may still be making progress.
    /// Always true where threads cannot be inspected.
    progressing: bool,
}

/// Disk syscalls (read, write, open, openat, fsync, fdatasync, rename,
/// renameat2, unlink, unlinkat, newfstatat, getdents64, ftruncate) by number.
#[cfg(target_arch = "x86_64")]
const DISK_SYSCALLS: &[&str] = &[
    "0", "1", "2", "257", "74", "75", "82", "316", "87", "263", "262", "217", "77",
];
#[cfg(target_arch = "aarch64")]
const DISK_SYSCALLS: &[&str] = &[
    "63", "64", "56", "82", "83", "38", "276", "35", "79", "61", "46",
];
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
const DISK_SYSCALLS: &[&str] = &[];

/// Snapshot the round's threads. On Linux, a thread blocked on a std mutex
/// or condvar waits in `futex` (syscall 202 on x86_64, 98 on aarch64) with
/// the address of that mutex or condvar as its first argument; the snapshot
/// compares it with the named locks below and labels each waiter it matches.
fn round_snapshot(
    task: &BgTask,
    recovery: &BgTaskRegistry,
    project: &BgTaskRegistry,
) -> RoundSnapshot {
    fn addr<T>(lock: &T) -> usize {
        lock as *const T as usize
    }
    let mut named: Vec<(String, usize)> = vec![
        ("claims".into(), addr(saved_remote_task_claims())),
        ("task.state".into(), addr(&task.state)),
        ("task.notice_registry".into(), addr(&task.notice_registry)),
        (
            "task.routed".into(),
            addr(&task.terminal_notification_routed),
        ),
        ("task.terminal_render".into(), addr(&task.terminal_render)),
        ("task.terminal_at".into(), addr(&task.terminal_at)),
        (
            "task.completion_secrets".into(),
            addr(&task.completion_secrets),
        ),
        ("task.kill_settled".into(), addr(&task.kill_settled)),
        (
            "task.json_writes".into(),
            addr(&task.db_write_order.json_writes),
        ),
        ("task.db_pending".into(), addr(&task.db_write_order.pending)),
        ("task.db_written".into(), addr(&task.db_write_order.written)),
        ("db_write_orders".into(), addr(db_write_orders())),
    ];
    for (name, registry) in [("recovery", recovery), ("project", project)] {
        let inner = &registry.inner;
        named.extend([
            (format!("{name}.tasks"), addr(&inner.tasks)),
            (
                format!("{name}.watchdog_tasks"),
                addr(&inner.watchdog_tasks),
            ),
            (format!("{name}.cleanup_queue"), addr(&inner.cleanup_queue)),
            (format!("{name}.completions"), addr(&inner.completions)),
            (
                format!("{name}.watch_registry"),
                addr(&inner.watch_registry),
            ),
            (
                format!("{name}.session_harnesses"),
                addr(&inner.session_harnesses),
            ),
            (
                format!("{name}.live_delivery_sessions"),
                addr(&inner.live_delivery_sessions),
            ),
            (
                format!("{name}.cancelled_call_tasks"),
                addr(&inner.cancelled_call_tasks),
            ),
            (
                format!("{name}.held_remote_workers"),
                addr(&inner.held_remote_workers),
            ),
        ]);
    }
    let held_now = [
        ("claims", held(saved_remote_task_claims())),
        ("task.state", held(&task.state)),
        ("task.notice_registry", held(&task.notice_registry)),
        ("task.routed", held(&task.terminal_notification_routed)),
        ("task.terminal_render", held(&task.terminal_render)),
    ]
    .into_iter()
    .filter(|(_, held)| *held)
    .map(|(name, _)| name)
    .collect::<Vec<_>>();
    let Ok(entries) = fs::read_dir("/proc/self/task") else {
        return RoundSnapshot {
            report: format!("held: {held_now:?}; threads cannot be inspected here"),
            progressing: true,
        };
    };
    let mut threads = Vec::new();
    let mut progressing = false;
    for entry in entries.flatten() {
        let dir = entry.path();
        let comm = fs::read_to_string(dir.join("comm")).unwrap_or_default();
        let comm = comm.trim();
        let syscall = fs::read_to_string(dir.join("syscall")).unwrap_or_default();
        let mut fields = syscall.split_whitespace();
        let number = fields.next().unwrap_or("").to_string();
        let lock_wait = number == "202" || number == "98";
        let target = fields
            .next()
            .and_then(|arg| usize::from_str_radix(arg.trim_start_matches("0x"), 16).ok());
        let lock = target
            .filter(|_| lock_wait)
            .and_then(|target| {
                named
                    .iter()
                    .find(|(_, start)| target >= *start && target < *start + 64)
            })
            .map(|(name, _)| name.as_str());
        // A round thread outside a futex wait may still be making progress;
        // so may a background-task thread (watchdog, scratch cleanup) in a
        // disk syscall, possibly while holding a lock a round thread waits for.
        if (comm.starts_with("stress-") && !lock_wait)
            || (comm.starts_with("aft-bash") && DISK_SYSCALLS.contains(&number.as_str()))
        {
            progressing = true;
        }
        if lock.is_some() || comm.starts_with("stress-") {
            let wchan = fs::read_to_string(dir.join("wchan")).unwrap_or_default();
            threads.push(format!(
                "{comm}: syscall {number} ({}){}",
                wchan.trim(),
                lock.map(|lock| format!(" waiting on {lock}"))
                    .unwrap_or_default()
            ));
        }
    }
    RoundSnapshot {
        report: format!("held: {held_now:?}; threads: {threads:?}"),
        progressing,
    }
}

/// The owner's stop of a saved task that `bash.kill_scope` recovered, and
/// the task's project registering the same task, run at the same moment on
/// two threads, 200 times. Each round must finish (a lock-order deadlock
/// between the two paths would hang one of them) and must queue exactly one
/// completion for the project's session, whichever path gets there first.
/// Both paths write the task's record while holding its state lock, and on
/// a loaded build server one such disk sync can take several seconds; a
/// round is therefore failed as deadlocked once it has run five seconds with
/// its threads only waiting on locks, and otherwise after thirty.
#[tokio::test]
async fn concurrent_stop_and_registration_of_a_shared_task_never_deadlock() {
    const ROUNDS: usize = 200;
    // A round unfinished this long whose threads only wait on locks is
    // deadlocked, and fails at once.
    const DEADLOCK_DEADLINE: Duration = Duration::from_secs(5);
    // A round whose threads are still doing disk I/O (slow syncs under load)
    // gets this long in all, so a stuck round can never hang the suite.
    const ROUND_CAP: Duration = Duration::from_secs(30);
    for round in 0..ROUNDS {
        let dir = tempfile::tempdir().unwrap();
        let scope = owner_scope(&format!("stop-registration-race-{round}"));
        let (_daemon, _clock, task_id, _json) =
            saved_due_draining_retry(dir.path(), &scope, u32::MAX).await;
        let (recovered, errors) =
            BgTaskRegistry::recover_saved_scope_tasks(dir.path(), &scope, &[]);
        assert!(errors.is_empty(), "round {round}: {errors:?}");
        let recovery = recovered.into_iter().next().unwrap();
        let shared = recovery.task(&task_id).unwrap();
        let project = registry();
        let start = Arc::new(std::sync::Barrier::new(2));
        let (finished, results) = std::sync::mpsc::channel::<(&str, Result<(), String>)>();
        {
            let (start, finished, recovery, scope) = (
                start.clone(),
                finished.clone(),
                recovery.clone(),
                scope.clone(),
            );
            std::thread::Builder::new()
                .name("stress-stop".into())
                .spawn(move || {
                    start.wait();
                    let result = recovery
                        .begin_scope_pause(&scope)
                        .and_then(|members| recovery.pause_scope(&scope, true, &members))
                        .map(|_| ());
                    let _ = finished.send(("stop", result));
                })
                .unwrap();
        }
        {
            let (project, storage) = (project.clone(), dir.path().to_path_buf());
            std::thread::Builder::new()
                .name("stress-register".into())
                .spawn(move || {
                    start.wait();
                    let result = project.replay_session(&storage, "session");
                    let _ = finished.send(("registration", result));
                })
                .unwrap();
        }
        let started = Instant::now();
        let mut finished_paths = Vec::new();
        while finished_paths.len() < 2 {
            let deadline = if started.elapsed() < DEADLOCK_DEADLINE {
                DEADLOCK_DEADLINE
            } else {
                // Past the deadlock deadline, keep waiting while the snapshot
                // reports possible progress; look again each second.
                started.elapsed() + Duration::from_secs(1)
            };
            match results.recv_timeout(deadline.saturating_sub(started.elapsed())) {
                Ok((path, result)) => {
                    result.unwrap_or_else(|error| panic!("round {round}: {path} failed: {error}"));
                    finished_paths.push(path);
                }
                Err(_) => {
                    let snapshot = round_snapshot(&shared, &recovery, &project);
                    if !snapshot.progressing {
                        panic!(
                            "round {round}: DEADLOCK: a concurrent stop and project registration of one shared task did not both finish within {:?}, and their threads only wait on locks (finished: {finished_paths:?}); {}",
                            started.elapsed(),
                            snapshot.report
                        );
                    }
                    if started.elapsed() >= ROUND_CAP {
                        panic!(
                            "round {round}: a concurrent stop and project registration of one shared task did not both finish within {ROUND_CAP:?} (finished: {finished_paths:?}); {}",
                            snapshot.report
                        );
                    }
                }
            }
        }
        let completions = project
            .drain_completions_for_session(Some("session"))
            .into_iter()
            .filter(|completion| completion.task_id == task_id)
            .count();
        assert_eq!(completions, 1, "round {round}: completions for {task_id}");
        assert_paused(&project, &task_id);
        recovery.detach();
        project.detach();
    }
}

/// A writer can replace metadata during every optimistic replay open while
/// an owner stops the task. A busy writer is not a corrupt task: replay must
/// still register it and deliver its stop, even when the bounded open retries
/// are exhausted.
#[tokio::test]
async fn a_stop_racing_every_replay_metadata_open_still_delivers_one_completion() {
    use std::os::unix::fs::MetadataExt;

    // Disk discovery resolves then reads metadata (skip 0/1); the batch's
    // revalidation repeats those opens (skip 2/3). Project-scoped discovery
    // reads metadata directly after resolving directories (skip 0, true).
    for (skip_metadata_opens, project_scoped) in
        [(1, false), (0, false), (2, false), (3, false), (0, true)]
    {
        let open_attempts = crate::bash_background::persistence::metadata_open_attempts_for_test();
        let dir = tempfile::tempdir().unwrap();
        let scope = owner_scope(&format!(
            "stop-during-replay-metadata-open-{skip_metadata_opens}-{project_scoped}"
        ));
        let (_daemon, _clock, task_id, json) =
            saved_due_draining_retry(dir.path(), &scope, u32::MAX).await;
        let (recovered, errors) =
            BgTaskRegistry::recover_saved_scope_tasks(dir.path(), &scope, &[]);
        assert!(errors.is_empty(), "{errors:?}");
        let recovery = recovered.into_iter().next().unwrap();
        let metadata_dir =
            crate::bash_background::persistence::PinnedDir::open(json.parent().unwrap()).unwrap();
        let metadata_name = json.file_name().unwrap().to_os_string();
        let metadata_path = json.clone();
        let project = registry();
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let registration = {
            let (project, storage) = (project.clone(), dir.path().to_path_buf());
            std::thread::spawn(move || {
                let mut metadata_opens = 0;
                crate::bash_background::persistence::with_artifact_validation_hook_for_test(
                    move |file| {
                        let opened = file.metadata().unwrap();
                        let current = fs::metadata(&json).unwrap();
                        if (opened.dev(), opened.ino()) != (current.dev(), current.ino()) {
                            return;
                        }
                        metadata_opens += 1;
                        // Pause with the old metadata open, before its link count
                        // is checked. Each owner write unlinks that exact handle.
                        if (skip_metadata_opens + 1..=skip_metadata_opens + open_attempts)
                            .contains(&metadata_opens)
                        {
                            reached_tx
                                .send(metadata_opens - skip_metadata_opens)
                                .unwrap();
                            release_rx
                                .recv_timeout(Duration::from_secs(30))
                                .expect("the owner must release the metadata reader");
                        }
                    },
                    || {
                        if project_scoped {
                            project.replay_session_for_project(&storage, "session", &storage)
                        } else {
                            project.replay_session(&storage, "session")
                        }
                    },
                )
            })
        };
        for attempt in 1..=open_attempts {
            assert_eq!(
                reached_rx.recv_timeout(Duration::from_secs(30)).unwrap(),
                attempt
            );
            if attempt < open_attempts {
                recovery
                    .record_owner_pause_intent(&recovery.task(&task_id).unwrap())
                    .unwrap();
                // Repeated pause intents have identical bytes and can skip
                // persistence. Force the inode replacement this race needs,
                // without changing the task's logical cancellation state.
                crate::bash_background::persistence::randomized_atomic_replace(
                    &metadata_dir,
                    &metadata_name,
                    &fs::read(&metadata_path).unwrap(),
                )
                .unwrap();
            } else {
                let members = recovery.begin_scope_pause(&scope).unwrap();
                recovery.pause_scope(&scope, true, &members).unwrap();
            }
            release_tx.send(()).unwrap();
        }
        registration.join().unwrap().unwrap();
        assert_project_told_of_the_stop_once(&project, &task_id);
        assert_paused(&project, &task_id);
        recovery.detach();
        project.detach();
    }
}

/// `bash.kill_scope` recovered a saved task and holds it without a worker.
/// The task's project then binds and registers that same task. The pause
/// records no cancellation (as when it fails part way), so the held task must
/// go to the project's registry, whose one worker resumes it as the project's
/// replay would have.
#[tokio::test]
async fn an_unpaused_held_task_is_handed_to_the_project_that_registered_it() {
    let dir = tempfile::tempdir().unwrap();
    let scope = owner_scope("held-handoff");
    let (daemon, _clock, task_id, _json) = saved_due_draining_retry(dir.path(), &scope, 1).await;
    let (recovered, errors) = BgTaskRegistry::recover_saved_scope_tasks(dir.path(), &scope, &[]);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(recovered.len(), 1);
    let project = registry();
    project.replay_session(dir.path(), "session").unwrap();
    assert_eq!(
        remote_workers(&task_id),
        (1, 1),
        "nothing may run while the task is held"
    );
    recovered[0].start_held_remote_workers();
    recovered[0].detach();
    let done = terminal(&project, &task_id).await;
    assert_eq!(done.exit_code, Some(0), "{done:?}");
    wait_for("the project's worker must stop", || {
        (remote_workers(&task_id) == (2, 2)).then_some(())
    })
    .await;
    assert_eq!(
        exec_runs(&daemon).len(),
        2,
        "the first, refused run and one resubmission after the restart"
    );
}

/// A registry restored a saved task, and its worker stopped without finishing
/// it. Another registry that restores the same task registers it and must run
/// it with one worker of its own; both registries see the outcome.
#[tokio::test]
async fn a_later_registry_runs_a_restored_task_whose_worker_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let scope = owner_scope("late-restore");
    let (daemon, mut clock, task_id, _json) = saved_due_draining_retry(dir.path(), &scope, 1).await;
    stopped_draining_roots()
        .lock()
        .unwrap()
        .insert(dir.path().into());
    let first = registry();
    first.replay_session(dir.path(), "session").unwrap();
    clock.expect_event(DrainingClockEvent::Stopped).await;
    wait_for("the first restored worker must stop", || {
        (remote_workers(&task_id) == (2, 2)).then_some(())
    })
    .await;
    assert!(!first.task(&task_id).unwrap().is_terminal());
    let later = registry();
    later.replay_session(dir.path(), "session").unwrap();
    let done = terminal(&later, &task_id).await;
    assert_eq!(done.exit_code, Some(0), "{done:?}");
    wait_for("the later registry's worker must stop", || {
        (remote_workers(&task_id) == (3, 3)).then_some(())
    })
    .await;
    assert_eq!(
        exec_runs(&daemon).len(),
        2,
        "the first, refused run and one resubmission after the restart"
    );
    assert!(first.task(&task_id).unwrap().is_terminal());
}

#[tokio::test]
async fn runon_missing_project_folder_is_refused_before_connecting() {
    let daemon = daemon(Script::Plain, "exec-remote/v1").await;
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let missing_root = project.path().join("does-not-exist");
    let marker = project.path().join("must-not-run-locally");
    let registry = registry();
    let mut policy = launch(daemon.connection.with_extension("missing"));
    policy.explicit_runon = true;
    let task_id = registry
        .spawn_remote(
            policy,
            SpawnPlan::Unsandboxed,
            &format!("printf local-proof > '{}'", marker.display()),
            PathBuf::from("/bin/sh"),
            "session".into(),
            project.path().to_path_buf(),
            HashMap::new(),
            crate::bash_background::HardKill::After(Duration::from_secs(30)),
            None,
            storage.path().to_path_buf(),
            crate::bash_background::TaskSlot::Foreground,
            false,
            false,
            Some(missing_root.clone()),
        )
        .unwrap();
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Failed, "{done:?}");
    let expected = format!(
        "runon refused: project folder does not exist: {}; command did not run (os error 2)",
        missing_root.display()
    );
    assert_eq!(
        done.info.status_reason.as_deref(),
        Some(expected.as_str()),
        "{done:?}"
    );
    assert_eq!(done.child_pid, None);
    assert_eq!(done.remote_job_id, None);
    let task = registry.task(&task_id).unwrap();
    assert!(!task.state.lock().unwrap().metadata.local_fallback_started);
    assert!(!marker.exists());
    assert!(exec_runs(&daemon).is_empty());
}

// A declared sibling must already be readable: runon never asks for access.
// Grant only that sibling so these cd tests reach directory validation without
// depending on the host's temp exemptions or allowing other outside paths.

fn runon_cd_snapshot_fixture() -> (tempfile::TempDir, RunRequest) {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap())
        .canonicalize()
        .unwrap();
    let dir = tempfile::Builder::new()
        .prefix("aft-runon-snapshot-cwd-")
        .tempdir_in(home)
        .unwrap();
    let checkout = dir.path().join("workspace/checkout");
    git_fixture(&checkout);
    let request = build_remote_request(
        &checkout,
        &checkout,
        "true",
        BTreeMap::new(),
        30,
        &exec::PresetParams::default(),
    )
    .unwrap();
    (dir, request)
}

#[test]
fn runon_write_shapes_respect_snapshot_membership() {
    let (_dir, request) = runon_cd_snapshot_fixture();
    let root = Path::new(&request.workspace_key);
    fs::write(root.join(".gitignore"), "ignored/\ntracked.rs\n").unwrap();
    fs::write(root.join("tracked.rs"), "source\n").unwrap();
    hermetic_git(root, &["add", "-f", "tracked.rs"]);
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::write(root.join("sub/file.rs"), "source\n").unwrap();
    let shapes = [
        "echo text > PATH",
        "echo text >> PATH",
        "echo text &> PATH",
        "echo text >| PATH",
        "cat > PATH <<'EOF'\nsource\nEOF",
        "tee PATH",
        "tee -a PATH",
        "sed -i 's/a/b/' PATH",
        "sed -i '' 's/a/b/' PATH",
        "perl -i -pe 's/a/b/' PATH",
        "ruby -i -pe 'gsub(/a/, \"b\")' PATH",
        "cp input PATH",
        "mv input PATH",
        "install input PATH",
        "ln input PATH",
        "rm PATH",
        "rm -rf PATH",
        "unlink PATH",
        "truncate -s 0 PATH",
        "touch PATH",
        "patch -p0 <<'EOF'\n--- PATH\n+++ PATH\n@@ -1 +1 @@\n-a\n+b\nEOF",
        "git apply -p0 --unsafe-paths <<'EOF'\n--- PATH\n+++ PATH\n@@ -1 +1 @@\n-a\n+b\nEOF",
        "git checkout -- PATH",
        "git restore PATH",
        "rustfmt PATH",
        "prettier --write PATH",
        "biome format --write PATH",
        "biome check --write PATH",
        "gofmt -w PATH",
        "black PATH",
        "ruff format PATH",
        "python -c 'open(\"PATH\", \"w\").write(\"source\")'",
        "python3 -c 'open(\"PATH\", \"a\").write(\"source\")'",
        "python3 - <<'PY'\nfrom pathlib import Path\nPath(\"PATH\").write_text(\"source\")\nPY",
        "node -e 'require(\"fs\").writeFileSync(\"PATH\", \"source\")'",
    ];
    for shape in shapes {
        for path in [
            "tracked.rs",
            "new.rs",
            "target/out.rs",
            "ignored/out.rs",
            "/tmp/aft-runon-write-proof.rs",
            "/dev/null",
            "$FILE",
        ] {
            // touch only changes timestamps on an existing file; no source edit
            // can be lost unless it creates a new snapshot member.
            let expected = matches!(path, "tracked.rs" | "new.rs")
                && !(shape == "touch PATH" && path == "tracked.rs");
            let command = shape.replace("PATH", path);
            assert_eq!(
                runon_write_refusal(&command, &request).is_some(),
                expected,
                "{command}"
            );
        }
    }
    assert!(
        runon_write_refusal("cd sub && sed -i 's/a/b/' file.rs", &request)
            .unwrap()
            .contains("sub/file.rs")
    );
    assert!(runon_write_refusal("cargo fmt", &request).is_some());
    assert!(runon_write_refusal("cp $SOURCE tracked.rs", &request).is_some());
    assert!(runon_write_refusal("cp input $DESTINATION", &request).is_none());
    assert!(runon_write_refusal("black $UNKNOWN tracked.rs", &request).is_some());
    assert!(runon_write_refusal(
        "git apply <<'EOF'\n--- a/tracked.rs\n+++ b/tracked.rs\n@@ -1 +1 @@\n-a\n+b\nEOF",
        &request
    )
    .is_some());
    for command in [
        "cargo fmt --check",
        "cargo fmt -- --check",
        "ruff format --check tracked.rs",
        "black --check tracked.rs",
        "rustfmt --check tracked.rs",
        "cargo fmt --help",
        "rustfmt --emit stdout tracked.rs",
        "black --diff tracked.rs",
        "black --code 'print(1)'",
        "ruff format --diff tracked.rs",
        "cat tracked.rs",
        "python3 -c 'open(\"tracked.rs\", \"r\").read()'",
        "python3 -c 'print(\"open(\\\"tracked.rs\\\", \\\"w\\\")\")'",
        "python3 -c 'open(path, \"w\").write(\"source\")'",
    ] {
        assert!(
            runon_write_refusal(command, &request).is_none(),
            "{command}"
        );
    }
    let expected = format!("runon refused: this command writes {} (and 1 more), and a build-server job's changes to checkout files are not copied back. Edit it in a local call, or name it in return_paths to regenerate it remotely.", root.join("tracked.rs").display());
    assert_eq!(
        runon_write_refusal("tee tracked.rs new.rs", &request).unwrap(),
        expected
    );
}

include!("remote_write_guard_tests.rs");

#[test]
fn runon_write_formatter_workdir_accepts_one_returned_file() {
    let (_dir, mut request) = runon_cd_snapshot_fixture();
    let root = Path::new(&request.workspace_key);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "source\n").unwrap();
    hermetic_git(root, &["add", "src/a.rs"]);
    for command in [
        "cargo fmt",
        "cargo fmt --all",
        "prettier --write .",
        "gofmt -w .",
        "ruff format",
        "black .",
    ] {
        request.return_paths.clear();
        assert!(
            runon_write_refusal(command, &request).is_some(),
            "{command}"
        );
        request.return_paths = vec!["./src/a.rs".into()];
        assert!(
            runon_write_refusal(command, &request).is_none(),
            "{command}"
        );
    }
    request.cwd = root.join("src").to_string_lossy().into_owned();
    request.return_paths = vec!["./a.rs".into()];
    assert!(runon_write_refusal("cargo fmt --all", &request).is_none());
}

#[test]
fn runon_write_formatter_folder_requires_a_returned_descendant() {
    let (_dir, mut request) = runon_cd_snapshot_fixture();
    let root = Path::new(&request.workspace_key);
    for path in ["src/a.ts", "docs/a.md", "src-extra/a.ts"] {
        fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
        fs::write(root.join(path), "source\n").unwrap();
    }
    hermetic_git(root, &["add", "src/a.ts", "docs/a.md", "src-extra/a.ts"]);
    for command in [
        "cd src && cargo fmt",
        "cd src && cargo fmt --all",
        "rustfmt --edition 2021 src/",
        "prettier --write src/",
        "gofmt -w src/",
        "cd src && ruff format",
        "black src/",
        "biome format --write src/",
        "biome check --write src/",
    ] {
        request.return_paths.clear();
        assert!(
            runon_write_refusal(command, &request).is_some(),
            "{command}"
        );
        request.return_paths = vec!["./src/a.ts".into()];
        assert!(
            runon_write_refusal(command, &request).is_none(),
            "{command}"
        );
        for outside in ["docs/a.md", "src-extra/a.ts"] {
            request.return_paths = vec![outside.into()];
            assert!(
                runon_write_refusal(command, &request).is_some(),
                "{command}, return_paths: {outside}"
            );
        }
    }
}

#[test]
fn runon_write_formatter_files_and_other_edits_keep_exact_return_paths() {
    let (_dir, mut request) = runon_cd_snapshot_fixture();
    let root = Path::new(&request.workspace_key);
    fs::create_dir_all(root.join("src")).unwrap();
    for path in ["src/a.ts", "src/b.ts"] {
        fs::write(root.join(path), "source\n").unwrap();
    }
    hermetic_git(root, &["add", "src/a.ts", "src/b.ts"]);
    request.return_paths = vec!["src/a.ts".into()];
    assert!(runon_write_refusal("prettier --write src/a.ts", &request).is_none());
    assert!(runon_write_refusal("echo new > src/a.ts", &request).is_none());
    for command in [
        "prettier --write src/a.ts src/b.ts",
        "prettier --write src/ && echo new > src/b.ts",
        "prettier --write src/ && sed -i 's/a/b/' src/b.ts",
        "prettier --write src/ && rm -rf src",
        "rm -rf src && prettier --write src/",
        "prettier --write src/ > src",
    ] {
        assert!(
            runon_write_refusal(command, &request).is_some(),
            "{command}"
        );
    }
    request.return_paths.push("src/b.ts".into());
    assert!(runon_write_refusal("prettier --write src/a.ts src/b.ts", &request).is_none());
}

#[test]
fn runon_cd_snapshot_ignored_existing_directories_keep_failure_branch() {
    let (_dir, mut request) = runon_cd_snapshot_fixture();
    let checkout = PathBuf::from(&request.workspace_key);
    fs::write(checkout.join(".gitignore"), "node_modules/\ndeleted/\n").unwrap();
    fs::write(checkout.join(".git/info/exclude"), "excluded/\n").unwrap();
    let global = checkout.join("global-ignore");
    fs::write(&global, "globalignored/\n").unwrap();
    hermetic_git(
        &checkout,
        &["config", "core.excludesFile", global.to_str().unwrap()],
    );
    fs::create_dir_all(checkout.join("nested")).unwrap();
    fs::write(checkout.join("nested/.gitignore"), "cache/\n").unwrap();
    for folder in [
        "node_modules/x",
        "excluded/x",
        "globalignored/x",
        "nested/cache/x",
        "deleted/x",
    ] {
        fs::create_dir_all(checkout.join(folder)).unwrap();
        if folder != "deleted/x" {
            fs::write(checkout.join(folder).join("source.txt"), "not copied\n").unwrap();
        }
    }
    fs::write(checkout.join("deleted/x/tracked.txt"), "deleted\n").unwrap();
    hermetic_git(&checkout, &["add", "-f", "deleted/x/tracked.txt"]);
    fs::remove_file(checkout.join("deleted/x/tracked.txt")).unwrap();
    let sibling = checkout.parent().unwrap().join("sibling");
    git_fixture(&sibling);
    fs::write(sibling.join(".gitignore"), "node_modules/\n").unwrap();
    fs::create_dir_all(sibling.join("node_modules/x")).unwrap();
    fs::write(sibling.join("node_modules/x/source.txt"), "not copied\n").unwrap();
    request.siblings = vec![sibling.to_str().unwrap().to_owned()];
    for line in [
        "cd node_modules/x; cd ../..",
        "cd excluded/x; cd ../..",
        "cd globalignored/x; cd ../..",
        "cd nested/cache/x; cd ../../..",
        "cd deleted/x; cd ../..",
        "cd ../sibling/node_modules/x; cd ../../../checkout",
    ] {
        assert!(
            runon_cd_refusal(line, &request).unwrap().is_some(),
            "{line}"
        );
    }
}

#[test]
fn runon_cd_snapshot_directories_without_copied_files_keep_failure_branch() {
    let (_dir, request) = runon_cd_snapshot_fixture();
    let checkout = Path::new(&request.workspace_key);
    fs::write(checkout.join(".gitignore"), "*.cache\n").unwrap();
    fs::create_dir_all(checkout.join("empty/x")).unwrap();
    fs::create_dir_all(checkout.join("onlyignored/x")).unwrap();
    fs::write(checkout.join("onlyignored/x/file.cache"), "not copied\n").unwrap();
    for line in ["cd empty/x; cd ../..", "cd onlyignored/x; cd ../.."] {
        assert!(
            runon_cd_refusal(line, &request).unwrap().is_some(),
            "{line}"
        );
    }
}

#[test]
fn runon_cd_snapshot_nonignored_and_tracked_directories_are_known() {
    let (_dir, mut request) = runon_cd_snapshot_fixture();
    let checkout = PathBuf::from(&request.workspace_key);
    fs::write(checkout.join(".gitignore"), "forced/\n").unwrap();
    for folder in ["tracked/x", "untracked/x", "forced/x", "src/target"] {
        fs::create_dir_all(checkout.join(folder)).unwrap();
        fs::write(checkout.join(folder).join("file.txt"), "source\n").unwrap();
    }
    hermetic_git(&checkout, &["add", "tracked/x/file.txt"]);
    hermetic_git(&checkout, &["add", "-f", "forced/x/file.txt"]);
    let sibling = checkout.parent().unwrap().join("sibling");
    git_fixture(&sibling);
    fs::write(sibling.join(".gitignore"), "forced/\n").unwrap();
    fs::create_dir_all(sibling.join("forced/x")).unwrap();
    fs::write(sibling.join("forced/x/file.txt"), "source\n").unwrap();
    hermetic_git(&sibling, &["add", "-f", "forced/x/file.txt"]);
    request.siblings = vec![sibling.to_str().unwrap().to_owned()];
    for line in [
        "cd tracked/x; cd ../..",
        "cd untracked/x; cd ../..",
        "cd forced/x; cd ../..",
        "cd src/target; cd ../..",
        "cd ../sibling/forced/x; cd ../../../checkout",
    ] {
        assert_eq!(runon_cd_refusal(line, &request).unwrap(), None, "{line}");
    }
}

#[test]
fn runon_cd_snapshot_mc_package_sequence_is_allowed() {
    let (_dir, request) = runon_cd_snapshot_fixture();
    let checkout = Path::new(&request.workspace_key);
    for package in ["pi-plugin", "plugin"] {
        let path = checkout.join("packages").join(package);
        fs::create_dir_all(&path).unwrap();
        for index in 0..200 {
            fs::write(path.join(format!("source-{index:03}.ts")), "source\n").unwrap();
        }
    }
    let line =
        "cd packages/pi-plugin && bun test src/__tests__; cd ../plugin && bun test src/__tests__";
    assert_eq!(runon_cd_refusal(line, &request).unwrap(), None);
}

#[test]
fn runon_cd_snapshot_missing_git_keeps_failure_branch() {
    let (_dir, request) = runon_cd_snapshot_fixture();
    let checkout = Path::new(&request.workspace_key);
    fs::create_dir_all(checkout.join("source/x")).unwrap();
    fs::write(checkout.join("source/x/source.txt"), "source\n").unwrap();
    assert_eq!(
        runon_cd_refusal("cd source/x; cd ../..", &request).unwrap(),
        None
    );
    assert!(runon_cd_refusal_with_git(
        "cd source/x; cd ../..",
        &request,
        &checkout.join("missing-git"),
    )
    .unwrap()
    .is_some());
}

#[test]
fn runon_cd_sequential_command_after_setup_is_allowed() {
    let (_dir, request) = runon_cd_snapshot_fixture();
    let checkout = Path::new(&request.workspace_key);
    for package in ["plugin", "e2e-tests"] {
        let path = checkout.join("packages").join(package);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("source.ts"), "source\n").unwrap();
    }
    let command = "printf setup; cd packages/plugin && bun test src/__tests__; cd ../e2e-tests && bun test src/__tests__";
    assert_eq!(runon_cd_refusal(command, &request).unwrap(), None);
}

#[test]
fn runon_cd_scoped_and_unknown_directories_preserve_refusals() {
    let (_dir, request) = runon_cd_snapshot_fixture();
    let checkout = Path::new(&request.workspace_key);
    for folder in ["sub", "a", "b"] {
        let path = checkout.join(folder);
        fs::create_dir(&path).unwrap();
        fs::write(path.join("source.txt"), "source\n").unwrap();
    }
    for command in [
        "cd a && cd ../b",
        "cd a; cd ../b",
        "false || cd a && cd ../b",
        "printf setup; cd a && bun test\ncd ../b",
        "cd \"$DIR\" && cd ../x",
        "cd \"$DIR\"; cd ../x",
        "cd && cd ../x",
        "cd ~ && cd ../x",
        "cd ~/src && cd ../x",
        "cd - && cd ../x",
        "cd */sub && cd ../x",
    ] {
        assert_eq!(
            runon_cd_refusal(command, &request).unwrap(),
            None,
            "{command}"
        );
    }
    let outside = checkout.parent().unwrap().join("outside");
    let expected = format!(
        "runon refused: the command changes into {}, which is outside the checkout the build server copies ({}); the command did not run. Set the bash tool's workdir to that folder instead of using cd.",
        outside.display(), checkout.display()
    );
    for command in [
        "(cd sub && printf nested); cd ../outside".to_owned(),
        "cd sub | cat; cd ../outside".to_owned(),
        "printf '%s' \"$(cd sub && pwd)\"; cd ../outside".to_owned(),
        "cd sub && printf nested & cd ../outside".to_owned(),
        "cd sub; cd ../../outside".to_owned(),
        "(cd sub && cd ../../outside); printf outer".to_owned(),
        "printf '%s' \"$(cd sub && cd ../../outside)\"".to_owned(),
        format!("cd '{}'", outside.display()),
        format!("cd \"$DIR\"; cd ../x; cd '{}'", outside.display()),
    ] {
        assert_eq!(
            runon_cd_refusal(&command, &request).unwrap(),
            Some(expected.clone()),
            "{command}"
        );
    }
}

#[test]
fn runon_cd_snapshot_rule_invalid_cd_keeps_failure_branch() {
    let (_dir, request) = runon_cd_snapshot_fixture();
    let checkout = Path::new(&request.workspace_key);
    fs::create_dir(checkout.join("a")).unwrap();
    fs::write(checkout.join("a/source.txt"), "source\n").unwrap();
    for line in [
        "cd a extra || cd ..",
        "cd -Z a || cd ..",
        "cd -PZ a || cd ..",
        "cd -- a extra || cd ..",
    ] {
        assert!(
            runon_cd_refusal(line, &request).unwrap().is_some(),
            "{line}"
        );
    }
}

#[test]
fn runon_cd_snapshot_rule_prior_command_keeps_failure_branch() {
    let (_dir, request) = runon_cd_snapshot_fixture();
    let checkout = Path::new(&request.workspace_key);
    fs::create_dir(checkout.join("a")).unwrap();
    fs::write(checkout.join("a/source.txt"), "source\n").unwrap();
    for line in [
        "mv a moved; cd a || cd ..",
        "rm -rf a; cd a || cd ..",
        "git clean -fd; cd a || cd ..",
        "./script; cd a || cd ..",
        "true; cd a || cd ..",
        "set +P; cd a || cd ..",
        "pushd .; cd a || cd ..",
        "rm -rf a & cd a || cd ..",
        "cd \"$(rm -rf a)\" || cd a || cd ..",
    ] {
        assert!(
            runon_cd_refusal(line, &request).unwrap().is_some(),
            "{line}"
        );
    }
}

#[test]
fn runon_cd_snapshot_rule_shared_deadline_bounds_git_lookups() {
    let (_dir, request) = runon_cd_snapshot_fixture();
    let checkout = Path::new(&request.workspace_key);
    let counter = checkout.join("git-spawns");
    let git = checkout.join("fake-git");
    crate::executable_cache::write_executable(
        &git,
        format!(
            "#!/bin/sh\nprintf 'spawn\\n' >> '{}'\nsleep 0.4\nfor target; do :; done\nprintf '%s/source.txt\\000' \"$target\"\n",
            counter.display()
        )
        .as_bytes(),
    )
    .unwrap();
    let mut line = String::from("cd d0");
    for index in 0..8 {
        let folder = checkout.join(format!("d{index}"));
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("source.txt"), "source\n").unwrap();
        if index > 0 {
            line.push_str(&format!("; cd ../d{index}"));
        }
    }
    let start = Instant::now();
    assert_eq!(
        runon_cd_refusal_with_git(&line, &request, &git).unwrap(),
        None
    );
    let elapsed = start.elapsed();
    let spawns = fs::read_to_string(counter).unwrap().lines().count();
    assert!((2..=6).contains(&spawns), "spawned Git {spawns} times");
    assert!(
        elapsed < Duration::from_millis(2500),
        "elapsed: {elapsed:?}"
    );
}

#[test]
fn runon_cd_snapshot_top_level_target_is_not_source() {
    let (_dir, mut request) = runon_cd_snapshot_fixture();
    let checkout = PathBuf::from(&request.workspace_key);
    let sibling = checkout.parent().unwrap().join("sibling");
    git_fixture(&sibling);
    for root in [&checkout, &sibling] {
        fs::create_dir_all(root.join("target/x")).unwrap();
        fs::write(root.join("target/x/tracked.txt"), "not copied\n").unwrap();
        hermetic_git(root, &["add", "-f", "target/x/tracked.txt"]);
    }
    request.siblings = vec![sibling.to_str().unwrap().to_owned()];
    for line in [
        "cd target/x; cd ../..",
        "cd ../sibling/target/x; cd ../../../checkout",
    ] {
        assert!(
            runon_cd_refusal(line, &request).unwrap().is_some(),
            "{line}"
        );
    }
}

#[tokio::test]
async fn exec_remote_run_scope_revocation_reports_reason() {
    let daemon = daemon(Script::Cancel, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let task = start(&registry, dir.path(), daemon.connection.clone());
    wait_for(
        "acceptance must be consumed before revoking the scope",
        || {
            registry
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
                .map(|_| ())
        },
    )
    .await;
    let killing = registry.clone();
    let done = tokio::task::spawn_blocking(move || {
        killing.kill_for_scope_revocation(&task, "session").unwrap()
    })
    .await
    .unwrap();
    assert_eq!(done.info.status, BgTaskStatus::Killed);
    assert_eq!(
        done.info.status_reason.as_deref(),
        Some(crate::bash_background::registry::SCOPE_REVOKED_REASON)
    );
    assert!(daemon
        .log
        .lock()
        .unwrap()
        .iter()
        .any(|(_, body)| body["method"] == "exec.cancel"));
}

#[cfg(test)]
#[path = "remote_tests/fallback_worker_broker.rs"]
mod fallback_worker_broker;

#[test]
fn hinted_retry_needs_the_named_refusal_ran_none_and_a_hint() {
    use crate::exec_remote::wire_fixture::signer_refusal;
    assert_eq!(
        hinted_retry_after_ms(&signer_refusal("runner_sign_unavailable", Some(2000))),
        Some(2000)
    );
    assert_eq!(
        hinted_retry_after_ms(&signer_refusal("runner_sign_invalid_response", Some(5))),
        Some(5)
    );
    // A refusal that needs an operator is never resubmitted, even with a hint.
    assert_eq!(
        hinted_retry_after_ms(&signer_refusal("runner_sign_missing", Some(2000))),
        None
    );
    assert_eq!(
        hinted_retry_after_ms(&signer_refusal("runner_sign_unavailable", None)),
        None
    );
    // Only `ran: "none"` proves the command did not start.
    for ran in [
        None,
        Some(Ran::Remote),
        Some(Ran::Unknown("elsewhere".into())),
    ] {
        let mut record = signer_refusal("runner_sign_unavailable", Some(2000));
        record.ran = ran.clone();
        assert_eq!(hinted_retry_after_ms(&record), None, "{ran:?}");
    }
    // Other hinted refusals keep their own handling.
    for reason in [RefusalReason::RunnerDraining, RefusalReason::RunnerDiskFull] {
        let mut record = signer_refusal("runner_sign_unavailable", Some(2000));
        record.outcome = Outcome::RefusedBeforeStart { reason };
        assert_eq!(hinted_retry_after_ms(&record), None);
    }
}

#[tokio::test]
async fn runon_signer_refusal_is_resubmitted_after_its_hint_and_only_success_is_seen() {
    use crate::exec_remote::wire_fixture::{signer_refusal, SIGNER_REFUSAL_VECTOR};
    // The fake runner sends ck-motor's record exactly, losing no field.
    assert_eq!(
        serde_json::to_value(StreamRecord::Terminal(signer_refusal(
            "runner_sign_unavailable",
            Some(2000)
        )))
        .unwrap(),
        serde_json::from_str::<serde_json::Value>(SIGNER_REFUSAL_VECTOR).unwrap()
    );
    let daemon = daemon(
        Script::SignerRefused {
            refusals: 1,
            detail: "runner_sign_unavailable",
            retry_after_ms: Some(2000),
        },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut clock = PausedDrainingClock::new(dir.path());
    let registry = registry();
    let task = start_runon_retry(&registry, dir.path(), daemon.connection.clone());
    let first = clock.now();
    clock.expect_sleep(first, first + 5000).await;
    let waiting = registry.observed_status(&task, "session", 8192).unwrap();
    assert!(!waiting.info.status.is_terminal(), "{waiting:?}");
    assert!(waiting.remote_refusal.is_none(), "{waiting:?}");
    assert_eq!(
        registry.runon_launch_pending(&task, "session"),
        Some(true),
        "a foreground launch keeps waiting while the retry is pending"
    );
    assert_eq!(exec_runs(&daemon).len(), 1);
    clock.advance(5000);
    let done = terminal(&registry, &task).await;
    assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");
    assert_eq!(done.exit_code, Some(0), "{done:?}");
    assert!(done.remote_refusal.is_none(), "{done:?}");
    for hidden in ["runner_authentication_failed", "refused", "local-proof"] {
        assert!(!done.output_preview.contains(hidden), "{done:?}");
    }
    let requests = exec_runs(&daemon);
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0], requests[1], "the same request is sent again");
    assert_no_retry_sleep(&mut clock);
}

#[tokio::test]
async fn runon_kill_during_a_signer_retry_wait_ends_the_task_without_resubmitting() {
    let daemon = daemon(
        Script::SignerRefused {
            refusals: u32::MAX,
            detail: "runner_sign_unavailable",
            retry_after_ms: Some(2000),
        },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut clock = PausedDrainingClock::new(dir.path());
    let registry = registry();
    let task_id = start_runon_retry(&registry, dir.path(), daemon.connection.clone());
    let first = clock.now();
    clock.expect_sleep(first, first + 5000).await;
    let killing = registry.clone();
    let killed_id = task_id.clone();
    // With the retry clock frozen, a terminal kill proves cancellation woke
    // the worker; the wall-clock timeout only guards against a deadlock.
    let killed = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || killing.kill(&killed_id, "session")),
    )
    .await
    .expect("kill must interrupt the retry wait without advancing the clock")
    .unwrap()
    .unwrap();
    assert_eq!(killed.info.status, BgTaskStatus::Killed);
    assert_eq!(clock.now(), first);
    clock.advance(5000);
    // Give a worker that wrongly kept waiting the chance to resubmit.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        exec_runs(&daemon).len(),
        1,
        "kill must veto any new exec.run"
    );
}

/// A draining resubmission is a fresh submission: a signer refusal with a
/// hint on it is resubmitted too, within an admission time counted from that
/// resubmission. The shortened budget is spent long before the draining wait
/// ends, so counting from the first submission would surface the refusal.
#[tokio::test]
async fn runon_signer_retry_follows_a_live_draining_resubmission() {
    let daemon = daemon(
        Script::DrainingThenSigner {
            signer_refusals: 1,
            draining_after_ms: 10_000,
            signer_after_ms: 100,
        },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    shorten_hinted_retry_budget(dir.path(), 6000);
    let mut clock = PausedDrainingClock::new(dir.path());
    let registry = registry();
    let task = start_runon_retry(&registry, dir.path(), daemon.connection.clone());
    let first = clock.now();
    clock.expect_sleep(first, first + 1000).await;
    clock.advance(10_000);
    clock.expect_sleep(first + 10_000, first + 15_000).await;
    assert_eq!(exec_runs(&daemon).len(), 2);
    clock.advance(5000);
    let done = terminal(&registry, &task).await;
    assert_eq!(done.exit_code, Some(0), "{done:?}");
    assert!(done.remote_refusal.is_none(), "{done:?}");
    let requests = exec_runs(&daemon);
    assert_eq!(requests.len(), 3);
    assert!(requests.windows(2).all(|pair| pair[0] == pair[1]));
    assert_no_retry_sleep(&mut clock);
}

/// The same after a restart: the saved draining refusal is resumed by a new
/// daemon, whose draining resubmission meets a hinted signer refusal.
#[tokio::test]
async fn runon_signer_retry_follows_a_draining_resubmission_resumed_after_restart() {
    let daemon = crate::exec_remote::wire_fixture::daemon_with_clients(
        Script::DrainingThenSigner {
            signer_refusals: 1,
            draining_after_ms: 10_000,
            signer_after_ms: 100,
        },
        "exec-remote/v1",
        2,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    shorten_hinted_retry_budget(dir.path(), 6000);
    let mut clock = PausedDrainingClock::new(dir.path());
    stopped_draining_roots()
        .lock()
        .unwrap()
        .insert(dir.path().into());
    let original = registry();
    let task = start_runon_retry(&original, dir.path(), daemon.connection.clone());
    clock.expect_event(DrainingClockEvent::Stopped).await;
    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    let first = clock.now();
    clock.expect_sleep(first, first + 1000).await;
    clock.advance(10_000);
    clock.expect_sleep(first + 10_000, first + 15_000).await;
    assert_eq!(exec_runs(&daemon).len(), 2);
    clock.advance(5000);
    let done = terminal(&restarted, &task).await;
    assert_eq!(done.exit_code, Some(0), "{done:?}");
    assert!(done.remote_refusal.is_none(), "{done:?}");
    let requests = exec_runs(&daemon);
    assert_eq!(requests.len(), 3);
    assert!(requests.windows(2).all(|pair| pair[0] == pair[1]));
    assert_no_retry_sleep(&mut clock);
}

#[test]
fn precommit_remote_handle_failure_removes_layout() {
    use crate::bash_background::persistence::{with_task_io_fault, TaskIoFault};
    let dir = tempfile::tempdir().unwrap();
    ensure_git_fixture(dir.path());
    let registry = registry();
    let result = with_task_io_fault(TaskIoFault::OutputHandlesEnospc, || {
        registry.spawn_remote(
            launch(dir.path().join("unused-connection")),
            SpawnPlan::Unsandboxed,
            "exit 0",
            resolve_posix_shell(),
            "session".into(),
            dir.path().into(),
            HashMap::new(),
            crate::bash_background::HardKill::Default,
            None,
            dir.path().into(),
            crate::bash_background::TaskSlot::Foreground,
            true,
            false,
            Some(dir.path().into()),
        )
    });
    assert!(result.unwrap_err().contains("No space left on device"));
    let session = session_tasks_dir(dir.path(), "session");
    assert_eq!(
        fs::read_dir(session).unwrap().count(),
        0,
        "uncommitted remote layout leaked"
    );
    assert!(registry.list(0).is_empty());
}

/// Without an owner pause, a reconnect that still finds the runner away is an
/// ordinary transport failure: the worker keeps reconnecting, never sends a
/// cancel, and delivers the job's result once the runner is back.
#[tokio::test]
async fn exec_remote_reconnect_failure_without_owner_pause_keeps_retrying() {
    let daemon = crate::exec_remote::wire_fixture::outage_daemon().await;
    let dir = tempfile::tempdir().unwrap();
    let clock = PausedReattachClock::new(dir.path());
    let registry = registry();
    let task = start(&registry, dir.path(), daemon.connection.clone());
    daemon.after_refused_connects(1).await;
    daemon.hold_next_connect();
    daemon.after_held_connect().await;
    let refused = *daemon.refused_connects.borrow();
    // The held reconnect is answered while the runner is still away.
    daemon.release_held_connect();
    daemon.after_refused_connects(refused + 3).await;
    daemon.restore();
    let done = terminal(&registry, &task).await;
    assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");
    assert_eq!(done.exit_code, Some(0));
    let stdout = registry.task(&task).unwrap().paths.stdout.clone();
    assert_eq!(fs::read(stdout).unwrap(), b"AB");
    remote_worker_exited(&task).await;
    assert!(daemon.methods("exec.cancel").is_empty());
    assert_eq!(daemon.methods("exec.run").len(), 1, "never resubmitted");
    drop(clock);
}

async fn submission_daemon(
    never_accepted: bool,
    mismatch: bool,
    always_unknown: bool,
    clients: usize,
) -> crate::exec_remote::wire_fixture::Daemon {
    crate::exec_remote::wire_fixture::daemon_with_capabilities(
        Script::Submission {
            never_accepted,
            mismatch,
            always_unknown,
            disconnect: false,
        },
        &["exec-remote/v1", "exec-remote-submission-key/v1"],
        clients,
    )
    .await
}

fn submission_runs(daemon: &crate::exec_remote::wire_fixture::Daemon) -> Vec<serde_json::Value> {
    daemon
        .log
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, body)| body["method"] == "exec.run")
        .map(|(_, body)| body["params"].clone())
        .collect()
}

async fn preaccepted_recovery_fixture(
    answer: crate::exec_remote::wire_fixture::SubmissionOutage,
) -> (
    tempfile::TempDir,
    crate::exec_remote::wire_fixture::OutageDaemon,
    BgTaskRegistry,
    String,
) {
    let daemon = crate::exec_remote::wire_fixture::preaccepted_outage_daemon(answer).await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let mut launch = launch(daemon.connection.clone());
    launch.explicit_runon = true;
    let task_id = start_with_launch(
        &registry,
        dir.path(),
        launch,
        crate::bash_background::HardKill::After(Duration::from_secs(30)),
    );
    wait_for(
        "the lost submission must retry while the provider is away",
        || {
            let task = registry.observed_status(&task_id, "session", 0).unwrap();
            assert!(
                !task.info.status.is_terminal(),
                "abandoned before the provider returned: {task:?}"
            );
            (*daemon.refused_connects.borrow() >= 4).then_some(())
        },
    )
    .await;
    (dir, daemon, registry, task_id)
}

fn assert_no_preaccepted_local_run(registry: &BgTaskRegistry, task_id: &str) {
    let task = registry.task(task_id).unwrap();
    let metadata = read_task(&task.paths.json).unwrap();
    assert!(!metadata.local_fallback_started);
    assert!(metadata.child_pid.is_none());
}

#[tokio::test]
async fn runon_preaccepted_separate_interruptions_each_get_a_recovery_window() {
    use crate::exec_remote::wire_fixture::SubmissionOutage;
    let (_dir, daemon, registry, task_id) =
        preaccepted_recovery_fixture(SubmissionOutage::Repeated).await;
    daemon.restore();
    let task = registry.task(&task_id).unwrap();
    wait_for(
        "the recovered stream must report placement progress",
        || {
            let state = task.state.lock().unwrap();
            state.remote_admission_note.is_some().then_some(())
        },
    )
    .await;
    {
        let mut state = task.state.lock().unwrap();
        assert!(!state.metadata.remote.as_ref().unwrap().accepted);
        // Age the live submission by an hour without waiting in real time.
        // Any uncleared interruption timestamp ages with it, reproducing a
        // second outage long after the first recovery window would expire.
        let elapsed = 60 * 60 * 1000;
        state.metadata.started_at -= elapsed;
        let submission = state
            .metadata
            .remote
            .as_mut()
            .unwrap()
            .submission
            .as_mut()
            .unwrap();
        submission.created_at_ms -= elapsed;
        if let Some(started) = &mut submission.recovery_started_at_ms {
            *started -= elapsed;
        }
        registry.persist_task(&task.paths, &state.metadata).unwrap();
    }
    let refused_before = *daemon.refused_connects.borrow();
    daemon.interrupt_submission();
    wait_for(
        "the second interruption must retry, not reuse the expired first window",
        || {
            let snapshot = registry.observed_status(&task_id, "session", 0).unwrap();
            assert!(
                !snapshot.info.status.is_terminal(),
                "second interruption was abandoned: {snapshot:?}"
            );
            (*daemon.refused_connects.borrow() > refused_before).then_some(())
        },
    )
    .await;
    daemon.restore();
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");
    let saved = read_task(&task.paths.json).unwrap();
    let submission = saved.remote.unwrap().submission.unwrap();
    assert_eq!(
        (submission.recovery_started_at_ms, submission.attempts),
        (None, 0),
        "successful recovery must durably retire both its deadline and attempts"
    );
    let runs = daemon.methods("exec.run");
    assert_eq!(
        runs.len(),
        3,
        "one dispatch and one keyed recovery per interruption"
    );
    assert_eq!(runs[0]["params"], runs[1]["params"]);
    assert_eq!(runs[1]["params"], runs[2]["params"]);
    assert_eq!(daemon.methods("exec.attach").len(), 1);
    assert_eq!(fs::read(&task.paths.stdout).unwrap(), b"AB");
    assert_no_preaccepted_local_run(&registry, &task_id);
}

#[tokio::test]
async fn runon_preaccepted_route_reopens_after_long_admission_wait() {
    use crate::exec_remote::wire_fixture::SubmissionOutage;
    let (_dir, daemon, registry, task_id) =
        preaccepted_recovery_fixture(SubmissionOutage::NeverAccepted).await;
    let task = registry.task(&task_id).unwrap();
    {
        let mut state = task.state.lock().unwrap();
        // Windows placement can outlive AFT's estimate of the runner's queue
        // limit. Recovery needs its own window after the route closes.
        state.metadata.started_at = unix_millis() - 20 * 60 * 1000;
        registry.persist_task(&task.paths, &state.metadata).unwrap();
    }
    daemon.restore();
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");
    assert_eq!(fs::read(&task.paths.stdout).unwrap(), b"AB");
    assert_no_preaccepted_local_run(&registry, &task_id);
}

#[tokio::test]
async fn runon_preaccepted_accepted_answer_attaches_without_fresh_submission() {
    use crate::exec_remote::wire_fixture::SubmissionOutage;
    let (_dir, daemon, registry, task_id) =
        preaccepted_recovery_fixture(SubmissionOutage::Accepted).await;
    daemon.restore();
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");
    let runs = daemon.methods("exec.run");
    assert_eq!(runs.len(), 2, "one dispatch and one same-key recovery");
    assert_eq!(runs[0]["params"], runs[1]["params"]);
    let attaches = daemon.methods("exec.attach");
    assert_eq!(attaches.len(), 1);
    assert_eq!(attaches[0]["params"]["job_id"], id().to_string());
    assert_eq!(attaches[0]["params"]["from_seq"], 0);
    assert_eq!(
        fs::read(&registry.task(&task_id).unwrap().paths.stdout).unwrap(),
        b"AB"
    );
    assert_no_preaccepted_local_run(&registry, &task_id);
}

#[tokio::test]
async fn runon_preaccepted_never_accepted_resubmits_exactly_once_with_fresh_key() {
    use crate::exec_remote::wire_fixture::SubmissionOutage;
    let (_dir, daemon, registry, task_id) =
        preaccepted_recovery_fixture(SubmissionOutage::NeverAccepted).await;
    daemon.restore();
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");
    let runs = daemon.methods("exec.run");
    assert_eq!(runs.len(), 3);
    assert_eq!(runs[0]["params"], runs[1]["params"]);
    assert_ne!(
        runs[1]["params"]["submission_key"],
        runs[2]["params"]["submission_key"]
    );
    assert!(runs[2]["params"].get("resume_from_seq").is_none());
    assert!(daemon.methods("exec.attach").is_empty());
    assert_no_preaccepted_local_run(&registry, &task_id);
}

#[tokio::test]
async fn runon_preaccepted_exhausted_recovery_is_fate_unknown() {
    use crate::exec_remote::wire_fixture::SubmissionOutage;
    let daemon = crate::exec_remote::wire_fixture::preaccepted_outage_daemon(
        SubmissionOutage::NeverAccepted,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let mut launch = launch(daemon.connection.clone());
    launch.explicit_runon = true;
    let task_id = start_with_launch(
        &registry,
        dir.path(),
        launch,
        crate::bash_background::HardKill::After(Duration::from_secs(30)),
    );
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::FateUnknown, "{done:?}");
    assert!(
        *daemon.refused_connects.borrow() >= 4,
        "a restarting provider must get more than a single catalog probe"
    );
    assert!(
        done.output_preview
            .contains("remote outcome unknown; keyed recovery unavailable or exhausted"),
        "{done:?}"
    );
    assert_eq!(daemon.methods("exec.run").len(), 1);
    assert!(daemon.methods("exec.attach").is_empty());
    assert_no_preaccepted_local_run(&registry, &task_id);
}

#[tokio::test]
async fn runon_preaccepted_ambiguous_keyed_answer_is_not_resubmitted() {
    use crate::exec_remote::wire_fixture::SubmissionOutage;
    let (_dir, daemon, registry, task_id) =
        preaccepted_recovery_fixture(SubmissionOutage::Ambiguous).await;
    daemon.restore();
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::FateUnknown, "{done:?}");
    let runs = daemon.methods("exec.run");
    assert_eq!(
        runs.len(),
        2,
        "an ambiguous answer cannot authorize another run"
    );
    assert_eq!(runs[0]["params"], runs[1]["params"]);
    assert!(daemon.methods("exec.attach").is_empty());
    assert!(done
        .output_preview
        .contains("remote outcome unknown; keyed recovery unavailable or exhausted"));
    assert_no_preaccepted_local_run(&registry, &task_id);
}

#[tokio::test]
async fn runon_preaccepted_recovery_window_is_persisted_and_retention_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, paths) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused-connection"), "true");
    let task = registry.task(&task_id).unwrap();
    {
        let mut state = task.state.lock().unwrap();
        let remote = state.metadata.remote.as_mut().unwrap();
        remote.accepted = false;
        remote.submission = Some(KeyedSubmission {
            key: "aft:interrupted".into(),
            created_at_ms: 100_000,
            request_digest: "unused".into(),
            attempts: 1,
            recovery_started_at_ms: Some(200_000),
            fresh_key_used: false,
            resuming: true,
            reattached: false,
            refusal: None,
        });
    }
    let mut sink = TaskSink::new(&registry, task.clone()).unwrap();
    sink.begin_submission_recovery().unwrap();
    let metadata = read_task(&paths.json).unwrap();
    let mut submission = metadata
        .remote
        .as_ref()
        .unwrap()
        .submission
        .clone()
        .unwrap();
    assert_eq!(submission.recovery_started_at_ms, Some(200_000));
    assert_eq!(submission.expires_at(&metadata), 230_000);
    submission.created_at_ms = 1;
    submission.recovery_started_at_ms = Some(86_400_000);
    assert_eq!(submission.expires_at(&metadata), 86_400_001);
    sink.accepted(&Accepted::new(id(), 1)).unwrap();
    let accepted = read_task(&paths.json).unwrap().remote.unwrap();
    let submission = accepted.submission.unwrap();
    assert_eq!(submission.recovery_started_at_ms, None);
    assert_eq!(submission.attempts, 0);
}

#[tokio::test]
async fn submission_key_attach_resumes_output_exactly_once() {
    let daemon = submission_daemon(false, false, false, 1).await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");
    let task = registry.task(&task_id).unwrap();
    assert_eq!(
        fs::read_to_string(&task.paths.stdout).unwrap(),
        "line-0\nline-1\nline-2\nline-3\n"
    );
    let runs = submission_runs(&daemon);
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0]["submission_key"], format!("aft:{task_id}"));
    assert_eq!(runs[1]["resume_from_seq"], 1);
    let mut resumed = runs[1].clone();
    resumed.as_object_mut().unwrap().remove("resume_from_seq");
    assert_eq!(resumed, runs[0]);
    let durable = read_task(&task.paths.json).unwrap();
    assert_eq!(durable.remote.as_ref().unwrap().job_id, Some(id()));
    assert!(
        durable
            .remote
            .as_ref()
            .unwrap()
            .submission
            .as_ref()
            .unwrap()
            .reattached
    );
}

#[tokio::test]
async fn submission_key_never_accepted_uses_one_new_key() {
    let daemon = submission_daemon(true, false, false, 1).await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");
    let runs = submission_runs(&daemon);
    assert_eq!(runs.len(), 3);
    assert_eq!(runs[0]["submission_key"], runs[1]["submission_key"]);
    assert_ne!(runs[1]["submission_key"], runs[2]["submission_key"]);
    assert!(runs[2].get("resume_from_seq").is_none());
    let task = registry.task(&task_id).unwrap();
    let durable = serde_json::to_value(read_task(&task.paths.json).unwrap()).unwrap();
    assert_eq!(
        durable["remote"]["submission"]["key"],
        runs[2]["submission_key"]
    );
}

#[tokio::test]
async fn submission_key_disconnect_before_acceptance_keeps_keyed_recovery() {
    let daemon = crate::exec_remote::wire_fixture::daemon_with_capabilities(
        Script::Submission {
            never_accepted: true,
            mismatch: false,
            always_unknown: false,
            disconnect: true,
        },
        &["exec-remote/v1", "exec-remote-submission-key/v1"],
        2,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");
    let runs = submission_runs(&daemon);
    assert_eq!(runs.len(), 3);
    assert_eq!(runs[0]["submission_key"], runs[1]["submission_key"]);
    assert_ne!(runs[1]["submission_key"], runs[2]["submission_key"]);
    assert!(!daemon
        .log
        .lock()
        .unwrap()
        .iter()
        .any(|(_, body)| body["method"] == "exec.attach"));
}

#[tokio::test]
async fn submission_key_without_capability_is_never_sent_or_retried() {
    let daemon = daemon(Script::Lost, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    assert_eq!(
        terminal(&registry, &task_id).await.info.status,
        BgTaskStatus::FateUnknown
    );
    let runs = submission_runs(&daemon);
    assert_eq!(runs.len(), 1);
    assert!(runs[0].get("submission_key").is_none(), "{runs:?}");
    assert!(runs[0].get("resume_from_seq").is_none());
}

#[tokio::test]
async fn submission_key_mismatch_preserves_existing_job_for_cancel() {
    let daemon = submission_daemon(false, true, false, 2).await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::FateUnknown);
    assert_eq!(submission_runs(&daemon).len(), 2);
    assert!(
        done.remote_refusal.is_none(),
        "the retry's refusal is not proof about the existing job"
    );
    assert!(
        done.output_preview.contains("submission_key_mismatch"),
        "{done:?}"
    );
    let task = registry.task(&task_id).unwrap();
    assert_eq!(
        read_task(&task.paths.json).unwrap().remote.unwrap().job_id,
        Some(id())
    );
    let recorded_id = read_task(&task.paths.json)
        .unwrap()
        .remote
        .unwrap()
        .job_id
        .unwrap();
    let client = ExecRemoteClient::connect(
        &daemon.connection,
        subc_protocol::BindIdentity::new("broca", "session"),
    )
    .await
    .unwrap();
    client.cancel_job(recorded_id).await.unwrap();
    daemon.wait_for_requests("exec.cancel", 1).await;
    let log = daemon.log.lock().unwrap();
    assert_eq!(
        log.iter()
            .find(|(_, b)| b["method"] == "exec.cancel")
            .unwrap()
            .1["params"]["job_id"],
        id().to_string()
    );
}

async fn stopped_submission(
    dir: &Path,
    daemon: &crate::exec_remote::wire_fixture::Daemon,
) -> (BgTaskRegistry, String) {
    stopped_draining_roots().lock().unwrap().insert(dir.into());
    let registry = registry();
    let task_id = start(&registry, dir, daemon.connection.clone());
    remote_worker_exited(&task_id).await;
    (registry, task_id)
}

#[tokio::test]
async fn submission_key_restart_restores_request_key_and_output_cursor() {
    let daemon = submission_daemon(false, false, false, 2).await;
    let dir = tempfile::tempdir().unwrap();
    let (old, task_id) = stopped_submission(dir.path(), &daemon).await;
    let task = old.task(&task_id).unwrap();
    let saved = read_task(&task.paths.json).unwrap();
    assert_eq!(saved.remote.as_ref().unwrap().last_seq, Some(1));
    let value = serde_json::to_value(&saved).unwrap();
    assert_eq!(
        value["remote"]["submission"]["key"],
        format!("aft:{task_id}")
    );
    drop(task);
    drop(old);
    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    assert_eq!(
        terminal(&restarted, &task_id).await.info.status,
        BgTaskStatus::Completed
    );
    let task = restarted.task(&task_id).unwrap();
    assert_eq!(
        fs::read_to_string(&task.paths.stdout).unwrap(),
        "line-0\nline-1\nline-2\nline-3\n"
    );
    let runs = submission_runs(&daemon);
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[1]["resume_from_seq"], 1);
    assert_eq!(runs[1]["submission_key"], runs[0]["submission_key"]);
}

#[tokio::test]
async fn submission_key_older_than_retention_is_not_resubmitted() {
    let daemon = submission_daemon(false, false, false, 2).await;
    let dir = tempfile::tempdir().unwrap();
    let (old, task_id) = stopped_submission(dir.path(), &daemon).await;
    let task = old.task(&task_id).unwrap();
    let mut value = serde_json::to_value(read_task(&task.paths.json).unwrap()).unwrap();
    assert!(value["remote"]["submission"].is_object());
    value["remote"]["submission"]["created_at_ms"] =
        serde_json::json!(unix_millis() - 24 * 60 * 60 * 1000);
    write_task(&task.paths.json, &serde_json::from_value(value).unwrap()).unwrap();
    drop(task);
    drop(old);
    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    assert_eq!(
        terminal(&restarted, &task_id).await.info.status,
        BgTaskStatus::FateUnknown
    );
    assert_eq!(submission_runs(&daemon).len(), 1);
}

#[test]
fn submission_key_reattachment_copy_is_not_enabled_without_approval() {
    assert_eq!(REATTACHED_RESULT_SENTENCE, None);
}

#[tokio::test]
async fn submission_key_recovery_attempts_are_bounded() {
    let daemon = submission_daemon(false, false, true, 1).await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    assert_eq!(
        terminal(&registry, &task_id).await.info.status,
        BgTaskStatus::FateUnknown
    );
    let runs = submission_runs(&daemon);
    assert_eq!(runs.len(), 4);
    let task = registry.task(&task_id).unwrap();
    assert_eq!(
        read_task(&task.paths.json).unwrap().remote.unwrap().job_id,
        Some(id())
    );
    for run in &runs[1..] {
        assert_eq!(run["submission_key"], runs[0]["submission_key"]);
        assert_eq!(run["resume_from_seq"], 1);
    }
}

#[tokio::test]
async fn submission_key_closed_runner_time_budget_is_not_resubmitted() {
    let daemon = submission_daemon(false, false, false, 2).await;
    let dir = tempfile::tempdir().unwrap();
    let (old, task_id) = stopped_submission(dir.path(), &daemon).await;
    let task = old.task(&task_id).unwrap();
    let mut saved = read_task(&task.paths.json).unwrap();
    saved.started_at =
        unix_millis() - saved.timeout_ms.unwrap() - duration_ms(RUNNER_QUEUE_WAIT_LIMIT) - 1;
    write_task(&task.paths.json, &saved).unwrap();
    drop(task);
    drop(old);
    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    assert_eq!(
        terminal(&restarted, &task_id).await.info.status,
        BgTaskStatus::FateUnknown
    );
    assert_eq!(submission_runs(&daemon).len(), 1);
}

#[tokio::test]
async fn submission_key_daemon_restart_reconnects_and_attaches_without_resubmission() {
    let daemon = crate::exec_remote::wire_fixture::daemon_with_capabilities(
        Script::Submission {
            never_accepted: false,
            mismatch: false,
            always_unknown: false,
            disconnect: true,
        },
        &["exec-remote/v1", "exec-remote-submission-key/v1"],
        2,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    let done = terminal(&registry, &task_id).await;
    let operations: Vec<_> = daemon
        .log
        .lock()
        .unwrap()
        .iter()
        .map(|(_, body)| {
            body.get("method")
                .or_else(|| body.get("op"))
                .cloned()
                .unwrap_or_default()
        })
        .collect();
    assert_eq!(
        done.info.status,
        BgTaskStatus::Completed,
        "{done:?}; operations: {operations:?}"
    );
    let task = registry.task(&task_id).unwrap();
    assert_eq!(
        fs::read_to_string(&task.paths.stdout).unwrap(),
        "line-0\nline-1\nline-2\nline-3\n"
    );
    let runs = submission_runs(&daemon);
    assert_eq!(runs.len(), 1);
    assert!(runs[0]["submission_key"].is_string());
    let log = daemon.log.lock().unwrap();
    let attaches: Vec<_> = log
        .iter()
        .filter(|(_, body)| body["method"] == "exec.attach")
        .collect();
    assert_eq!(attaches.len(), 1);
    assert_eq!(attaches[0].1["params"]["job_id"], id().to_string());
    assert_eq!(attaches[0].1["params"]["from_seq"], 2);
}

#[tokio::test]
async fn remote_restart_unaccepted_keyed_without_job_id_is_not_resubmitted() {
    let daemon = submission_daemon(false, false, false, 2).await;
    let dir = tempfile::tempdir().unwrap();
    let (old, task_id) = stopped_submission(dir.path(), &daemon).await;
    let task = old.task(&task_id).unwrap();
    let mut saved = read_task(&task.paths.json).unwrap();
    let remote = saved.remote.as_mut().unwrap();
    remote.accepted = false;
    remote.job_id = None;
    remote.last_seq = None;
    remote.stdout_len = 0;
    remote.output_received = false;
    remote.terminal = None;
    write_task(&task.paths.json, &saved).unwrap();
    drop(task);
    drop(old);
    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    let done = terminal(&restarted, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::FateUnknown, "{done:?}");
    assert!(
        done.output_preview.contains("command was not resubmitted"),
        "{done:?}"
    );
    let task = restarted.task(&task_id).unwrap();
    assert!(fs::read(&task.paths.stdout).unwrap().is_empty());
    let runs = submission_runs(&daemon);
    assert_eq!(
        runs.len(),
        1,
        "restart must not redispatch the saved request"
    );
    assert!(!daemon
        .log
        .lock()
        .unwrap()
        .iter()
        .any(|(_, body)| body["method"] == "exec.attach"));
}

#[tokio::test]
async fn submission_key_queue_wait_does_not_consume_execution_timeout() {
    let daemon = submission_daemon(false, false, false, 2).await;
    let dir = tempfile::tempdir().unwrap();
    let (old, task_id) = stopped_submission(dir.path(), &daemon).await;
    let task = old.task(&task_id).unwrap();
    let mut saved = read_task(&task.paths.json).unwrap();
    saved.started_at = unix_millis() - saved.timeout_ms.unwrap() - 1;
    let remote = saved.remote.as_mut().unwrap();
    remote.submission.as_mut().unwrap().created_at_ms = saved.started_at;
    // Recover the checkpoint from admission, before any execution was observed.
    remote.last_seq = None;
    remote.stdout_len = 0;
    remote.output_received = false;
    remote.terminal = None;
    write_task(&task.paths.json, &saved).unwrap();
    drop(task);
    drop(old);
    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    assert_eq!(
        terminal(&restarted, &task_id).await.info.status,
        BgTaskStatus::Completed
    );
    assert_eq!(submission_runs(&daemon).len(), 1);
    let log = daemon.log.lock().unwrap();
    let attach = log
        .iter()
        .find(|(_, body)| body["method"] == "exec.attach")
        .unwrap();
    assert_eq!(attach.1["params"]["job_id"], id().to_string());
    assert_eq!(attach.1["params"]["from_seq"], 0);
}

struct LostSubmissionReplies(PathBuf);

impl LostSubmissionReplies {
    fn new(root: &Path) -> Self {
        let root = test_clock_key(root);
        assert!(lost_submission_roots().lock().unwrap().insert(root.clone()));
        Self(root)
    }
}

impl Drop for LostSubmissionReplies {
    fn drop(&mut self) {
        lost_submission_roots().lock().unwrap().remove(&self.0);
    }
}

async fn review_daemon(script: Script, clients: usize) -> crate::exec_remote::wire_fixture::Daemon {
    crate::exec_remote::wire_fixture::daemon_with_capabilities(
        script,
        &["exec-remote/v1", "exec-remote-submission-key/v1"],
        clients,
    )
    .await
}

async fn fresh_reply_loss_never_runs_locally(restored: bool) {
    let daemon = review_daemon(
        Script::SubmissionFreshReplyLost,
        if restored { 2 } else { 1 },
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    ensure_git_fixture(dir.path());
    let _loss = LostSubmissionReplies::new(dir.path());
    if restored {
        stopped_draining_roots()
            .lock()
            .unwrap()
            .insert(dir.path().into());
    }
    let old = registry();
    let counter = dir.path().join("local-spawn-counter");
    let task_id = old
        .spawn_remote(
            launch(daemon.connection.clone()),
            SpawnPlan::Unsandboxed,
            "printf x >> \"$LOCAL_SPAWN_COUNTER\"",
            resolve_posix_shell(),
            "session".into(),
            dir.path().into(),
            HashMap::from([("LOCAL_SPAWN_COUNTER".into(), counter.display().to_string())]),
            crate::bash_background::HardKill::After(Duration::from_secs(30)),
            None,
            dir.path().into(),
            crate::bash_background::TaskSlot::Background { max: 10 },
            true,
            false,
            Some(dir.path().into()),
        )
        .unwrap();
    let registry = if restored {
        remote_worker_exited(&task_id).await;
        drop(old);
        let restarted = registry();
        restarted.replay_session(dir.path(), "session").unwrap();
        restarted
    } else {
        old
    };
    let done = terminal(&registry, &task_id).await;
    let local_spawns = match fs::read(&counter) {
        Ok(bytes) => bytes.len(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
        Err(error) => panic!("read local spawn counter: {error}"),
    };
    assert_eq!(
        local_spawns, 0,
        "an uncertain fresh submission must not also run locally"
    );
    assert_eq!(done.info.status, BgTaskStatus::FateUnknown, "{done:?}");
    let runs = submission_runs(&daemon);
    assert!(
        runs.len() >= 2,
        "fresh dispatch must reach the fake executor"
    );
    assert_ne!(runs[0]["submission_key"], runs[1]["submission_key"]);
    for request in &runs[1..] {
        assert_eq!(request["submission_key"], runs[1]["submission_key"]);
    }
    let task = registry.task(&task_id).unwrap();
    let durable = read_task(&task.paths.json).unwrap();
    assert!(!durable.local_fallback_started);
    assert!(durable.child_pid.is_none());
    let remote = durable.remote.as_ref().unwrap();
    assert_eq!(
        serde_json::to_value(remote).unwrap()["submission"]["key"],
        runs[1]["submission_key"]
    );
    assert!(remote.terminal.is_none());
    assert!(remote.submission.as_ref().unwrap().refusal.is_none());
    assert!(remote.submission.as_ref().unwrap().fresh_key_used);
}

#[tokio::test]
async fn submission_review_fresh_reply_loss_never_runs_locally() {
    fresh_reply_loss_never_runs_locally(false).await;
}

#[tokio::test]
async fn submission_review_restored_fresh_reply_loss_never_runs_locally() {
    fresh_reply_loss_never_runs_locally(true).await;
}

async fn expired_gap_keeps_confirmed_exit(restored: bool) {
    for unknown in [false, true] {
        let daemon = review_daemon(
            Script::SubmissionGapExpired { unknown },
            if restored { 2 } else { 1 },
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let (registry, task_id) = if restored {
            let (old, task_id) = stopped_submission(dir.path(), &daemon).await;
            let task = old.task(&task_id).unwrap();
            let saved = read_task(&task.paths.json).unwrap();
            assert!(saved.remote.as_ref().unwrap().gap_recovery.is_some());
            drop(task);
            drop(old);
            let restarted = registry();
            restarted.replay_session(dir.path(), "session").unwrap();
            (restarted, task_id)
        } else {
            let registry = registry();
            let task_id = start(&registry, dir.path(), daemon.connection.clone());
            (registry, task_id)
        };
        let done = terminal(&registry, &task_id).await;
        assert_eq!(
            done.info.status,
            BgTaskStatus::Completed,
            "unknown={unknown}, {done:?}"
        );
        assert_eq!(done.exit_code, Some(0));
        let task = registry.task(&task_id).unwrap();
        assert_eq!(fs::read(&task.paths.stdout).unwrap(), b"AC");
        let durable = read_task(&task.paths.json).unwrap();
        assert_eq!(durable.incomplete_output, vec![(1, 1)]);
        assert!(
            done.output_preview
                .contains("output lost between seq 1 and 1"),
            "{done:?}"
        );
        assert!(!durable.local_fallback_started);
        let remote = durable.remote.unwrap();
        assert!(remote.gap_recovery.is_none());
        assert_eq!(remote.terminal.unwrap().outcome, Outcome::Exit { code: 0 });
    }
}

#[tokio::test]
async fn submission_review_expired_gap_keeps_confirmed_exit() {
    expired_gap_keeps_confirmed_exit(false).await;
}

#[tokio::test]
async fn submission_review_restored_expired_gap_keeps_confirmed_exit() {
    expired_gap_keeps_confirmed_exit(true).await;
}

#[tokio::test]
async fn submission_review_current_never_accepted_allows_local_fallback() {
    let daemon = review_daemon(Script::SubmissionFreshReplyLost, 2).await;
    let dir = tempfile::tempdir().unwrap();
    let (old, task_id) = stopped_submission(dir.path(), &daemon).await;
    let task = old.task(&task_id).unwrap();
    let mut saved = read_task(&task.paths.json).unwrap();
    saved
        .remote
        .as_mut()
        .unwrap()
        .submission
        .as_mut()
        .unwrap()
        .attempts = PRE_ACCEPTANCE_RECOVERY_ATTEMPTS;
    write_task(&task.paths.json, &saved).unwrap();
    drop(task);
    drop(old);
    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    let done = terminal(&restarted, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");
    assert!(done.output_preview.contains("local-proof"));
    let task = restarted.task(&task_id).unwrap();
    assert!(read_task(&task.paths.json).unwrap().local_fallback_started);
    assert_eq!(submission_runs(&daemon).len(), 1);
}

/// A runner that offers chunked file return beside ordinary runs.

#[tokio::test]
async fn local_fallback_with_return_paths_says_the_files_stayed_in_place() {
    // Prefix routing (not runon) may still fall back to a local run when the
    // runner refuses before start. A task that asked for files back says
    // they were written here and nothing was copied.
    let daemon = crate::exec_remote::wire_fixture::daemon_with_capabilities(
        Script::KnownRefused,
        RETURN_CLAIMS,
        1,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    ensure_git_fixture(dir.path());
    let registry = registry();
    let mut policy = launch(daemon.connection.clone());
    policy.return_paths = vec!["out.txt".into()];
    let task = registry
        .spawn_remote(
            policy,
            SpawnPlan::Unsandboxed,
            "printf local > out.txt",
            resolve_posix_shell(),
            "session".into(),
            dir.path().into(),
            HashMap::new(),
            crate::bash_background::HardKill::After(Duration::from_secs(30)),
            None,
            dir.path().into(),
            crate::bash_background::TaskSlot::Background { max: 10 },
            true,
            false,
            Some(dir.path().into()),
        )
        .unwrap();
    let done = terminal(&registry, &task).await;
    assert_eq!(exec_runs(&daemon).len(), 1);
    assert_eq!(fs::read(dir.path().join("out.txt")).unwrap(), b"local");
    assert!(
        done.output_preview
            .contains(crate::bash_background::registry::return_files::LOCAL_RUN_NOTE),
        "{}",
        done.output_preview
    );
}

#[test]
fn copied_back_files_leave_the_changed_list_on_linux_and_windows_runners() {
    use crate::bash_background::registry::return_files::{ReturnOutcome, ReturnedPath};
    let mut terminal = TerminalRecord::new(id(), Outcome::Exit { code: 0 }, 1, 0, 0);
    terminal.workspace_changes = Some(vec!["dist/hello.txt".into(), "other.txt".into()]);
    let mut remote: RemoteTask = serde_json::from_value(serde_json::json!({
        "harness": "broca", "session": "session", "stdout_len": 0,
        "stderr_len": 0, "cancel_requested": false,
    }))
    .unwrap();
    remote.accepted = true;
    remote.terminal = Some(terminal);
    remote.return_paths = vec!["dist/hello.txt".into()];
    remote.returned_files = Some(vec![ReturnedPath {
        path: "dist/hello.txt".into(),
        checkout_path: Some("dist/hello.txt".into()),
        outcome: ReturnOutcome::Returned { bytes: 5 },
    }]);
    let mut metadata = PersistedTask::starting(
        "task".into(),
        "session".into(),
        "make dist".into(),
        "/work".into(),
        Some("/work".into()),
        Some(30000),
        true,
        false,
    );
    // ck-motor names changed files the same way on both runners.
    for platform in [None, Some(Platform::Linux), Some(Platform::Windows)] {
        let mut remote = remote.clone();
        remote.requested_platform = platform.clone();
        remote.accepted_platform = platform.clone();
        metadata.remote = Some(remote);
        let report = remote_report(&metadata).unwrap();
        assert!(
            report.starts_with(
                "Files requested back from the server (return_paths):\n  \
                 dist/hello.txt: returned (5 bytes)\n\
                 These files changed on the server and were NOT copied back:\n  \
                 other.txt"
            ),
            "{platform:?}: {report}"
        );
        assert_eq!(report.matches("dist/hello.txt").count(), 1, "{report}");
    }
}

#[tokio::test]
async fn runon_return_paths_restart_before_the_manifest_replays_chunks_and_places_once() {
    let daemon = return_daemon("three-chunks-short-last").await;
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    let original = registry();
    let db = Arc::new(Mutex::new(crate::db::open(&root.join("aft.db")).unwrap()));
    original.set_harness(crate::harness::Harness::Runner);
    original.set_db_pool(db.clone());
    let layout = allocate_task_layout(&root, "session").unwrap();
    let task_id = layout.paths.task_id.clone();
    let mut metadata = PersistedTask::starting(
        task_id.clone(),
        "session".into(),
        "make dist".into(),
        root.clone(),
        Some(root.clone()),
        Some(30000),
        true,
        false,
    );
    metadata.status = BgTaskStatus::Running;
    metadata.harness = Some("runner".into());
    metadata.execution_note = Some("ran remotely on ck-motor".into());
    metadata.remote = Some(RemoteTask {
        copy_warning: false,
        submission: None,
        accepted_platform: None,
        explicit_runon: true,
        connection_file: Some(daemon.connection.clone()),
        harness: "broca".into(),
        session: "session".into(),
        requested_platform: None,
        requested_vcpus: None,
        requested_network: false,
        granted_network: false,
        accepted: false,
        started: None,
        job_id: None,
        last_seq: None,
        gap_recovery: None,
        undelivered_output: Vec::new(),
        stdout_len: 0,
        stderr_len: 0,
        unknown_len: 0,
        output_limit_dropped_bytes: 0,
        cancel_requested: false,
        cancel_pending: false,
        owner_pause_cancel: None,
        terminal: None,
        pre_start_refusal: None,
        fallback_digest: None,
        env_not_forwarded: None,
        queue_position: None,
        queue_wait_ms: None,
        output_received: false,
        retry: None,
        output_incomplete: None,
        workspace_holder: None,
        return_paths: vec!["dist/app.bin".into()],
        returned_files: None,
        return_first_seq: None,
        return_staging: Default::default(),
        return_max_bytes: exec::return_max_bytes(),
        return_cap_retried: false,
        expired_gap_unverified: Vec::new(),
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
    consumer
        .consume(
            StreamRecord::Output(Output::new(
                0,
                OutputStream::Stdout,
                BytePayload(Vec::new()),
            )),
            &mut sink,
        )
        .unwrap();
    // Only the first of the file's three chunks arrives before the restart.
    let first_chunk =
        crate::exec_remote::return_vectors::return_vector_records("three-chunks-short-last")
            .into_iter()
            .find(|record| matches!(record, StreamRecord::ReturnChunk(_)))
            .unwrap();
    consumer.consume(first_chunk, &mut sink).unwrap();
    // Checkpoint the cursor past the chunk, so only the saved first-chunk
    // seq can bring the chunk back after the restart.
    sink.checkpoint_progress().unwrap();
    // Received bytes are staged in the task's private control folder.
    let staging = layout
        .dirs
        .control
        .path()
        .join(format!(".aft-return-{task_id}-0.part"));
    assert_eq!(fs::read(&staging).unwrap(), b"0123");
    let saved = read_task_at(&layout).unwrap();
    let remote = saved.remote.as_ref().unwrap();
    assert_eq!(remote.return_first_seq, Some(1));
    assert_eq!(
        remote.last_seq,
        Some(1),
        "the saved cursor is past the chunk"
    );
    assert_eq!(remote.return_staging.files, [staging.clone()]);
    assert!(remote.return_staging.dirs.is_empty());
    assert!(
        !root.join("dist").exists(),
        "nothing in the project before the manifest"
    );
    // A crash leaves the staging file behind: its receiver never cleans up.
    std::mem::forget(sink.returns.take());
    drop(sink);
    drop(consumer);
    original.inner.shutdown.store(true, Ordering::SeqCst);
    drop(task);
    drop(original);

    let restarted = registry();
    restarted.set_harness(crate::harness::Harness::Runner);
    restarted.set_db_pool(db);
    restarted.replay_session(&root, "session").unwrap();
    let done = terminal(&restarted, &task_id).await;
    let log = daemon.log.lock().unwrap();
    let attaches: Vec<_> = log
        .iter()
        .filter(|(_, body)| body["method"] == "exec.attach")
        .map(|(_, body)| body["params"]["from_seq"].clone())
        .collect();
    assert_eq!(
        attaches,
        [serde_json::json!(1)],
        "one attach, from the first chunk"
    );
    assert!(!log.iter().any(|(_, body)| body["method"] == "exec.run"));
    assert_eq!(fs::read(root.join("dist/app.bin")).unwrap(), b"0123456789");
    assert_eq!(
        done.output_preview.matches("dist/app.bin:").count(),
        1,
        "{}",
        done.output_preview
    );
    assert!(
        done.output_preview
            .contains("  dist/app.bin: returned (10 bytes)"),
        "{}",
        done.output_preview
    );
    assert!(!staging.exists(), "the stale staging file is gone");
    let dist: Vec<_> = fs::read_dir(root.join("dist"))
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name())
        .collect();
    assert_eq!(dist, ["app.bin"]);
    let saved = read_task_at(&layout).unwrap();
    assert!(saved.remote.unwrap().return_staging.is_empty());
}

#[tokio::test]
async fn return_paths_unsupported_never_falls_back_even_for_prefix_routing() {
    let daemon = crate::exec_remote::wire_fixture::daemon_with_capabilities(
        Script::ReturnPathsUnsupported,
        RETURN_CLAIMS,
        1,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    ensure_git_fixture(dir.path());
    let registry = registry();
    let mut policy = launch(daemon.connection.clone());
    policy.return_paths = vec!["out.txt".into()];
    let task = registry
        .spawn_remote(
            policy,
            SpawnPlan::Unsandboxed,
            "printf local > out.txt",
            resolve_posix_shell(),
            "session".into(),
            dir.path().into(),
            HashMap::new(),
            crate::bash_background::HardKill::After(Duration::from_secs(30)),
            None,
            dir.path().into(),
            crate::bash_background::TaskSlot::Background { max: 10 },
            true,
            false,
            Some(dir.path().into()),
        )
        .unwrap();
    let done = terminal(&registry, &task).await;
    assert_eq!(exec_runs(&daemon).len(), 1);
    assert!(!dir.path().join("out.txt").exists(), "never a local run");
    assert!(
        done.output_preview
            .contains(RETURN_PATHS_UNSUPPORTED_MESSAGE),
        "{}",
        done.output_preview
    );
}

#[tokio::test]
async fn runon_return_paths_restart_during_gap_recovery_rewinds_to_the_first_chunk() {
    // Chunks 1 and 2 of the file arrive, chunk 3 does not, then the manifest
    // and terminal do: the terminal proof is saved for gap recovery. After a
    // restart the staged bytes are gone, so recovery must attach from the
    // first chunk, not from the gap, while keeping the saved terminal proof.
    let daemon = return_daemon("three-chunks-short-last").await;
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    let original = registry();
    let db = Arc::new(Mutex::new(crate::db::open(&root.join("aft.db")).unwrap()));
    original.set_harness(crate::harness::Harness::Runner);
    original.set_db_pool(db.clone());
    let layout = allocate_task_layout(&root, "session").unwrap();
    let task_id = layout.paths.task_id.clone();
    let mut metadata = PersistedTask::starting(
        task_id.clone(),
        "session".into(),
        "make dist".into(),
        root.clone(),
        Some(root.clone()),
        Some(30000),
        true,
        false,
    );
    metadata.status = BgTaskStatus::Running;
    metadata.harness = Some("runner".into());
    metadata.execution_note = Some("ran remotely on ck-motor".into());
    let mut remote: RemoteTask = serde_json::from_value(serde_json::json!({
        "harness": "broca", "session": "session", "stdout_len": 0,
        "stderr_len": 0, "cancel_requested": false,
    }))
    .unwrap();
    remote.explicit_runon = true;
    remote.connection_file = Some(daemon.connection.clone());
    remote.return_paths = vec!["dist/app.bin".into()];
    metadata.remote = Some(remote);
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
    consumer
        .consume(
            StreamRecord::Output(Output::new(
                0,
                OutputStream::Stdout,
                BytePayload(Vec::new()),
            )),
            &mut sink,
        )
        .unwrap();
    let mut gap = None;
    for record in
        crate::exec_remote::return_vectors::return_vector_records("three-chunks-short-last")
    {
        if matches!(&record, StreamRecord::ReturnChunk(chunk) if chunk.seq == 3) {
            continue;
        }
        if let Err(error) = consumer.consume(record, &mut sink) {
            gap = Some(error);
        }
    }
    assert!(
        matches!(gap, Some(exec::Error::RecoveryRequired { .. })),
        "{gap:?}"
    );
    let saved = read_task_at(&layout).unwrap();
    let remote = saved.remote.as_ref().unwrap();
    assert_eq!(remote.return_first_seq, Some(1));
    assert_eq!(remote.last_seq, Some(2), "the cursor stops at the gap");
    assert!(remote.gap_recovery.is_some(), "the terminal proof is saved");
    // A crash leaves the staged bytes behind; the restart removes them.
    std::mem::forget(sink.returns.take());
    drop(sink);
    drop(consumer);
    original.inner.shutdown.store(true, Ordering::SeqCst);
    drop(task);
    drop(original);

    let restarted = registry();
    restarted.set_harness(crate::harness::Harness::Runner);
    restarted.set_db_pool(db);
    restarted.replay_session(&root, "session").unwrap();
    let done = terminal(&restarted, &task_id).await;
    let attaches: Vec<_> = daemon
        .log
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, body)| body["method"] == "exec.attach")
        .map(|(_, body)| body["params"]["from_seq"].clone())
        .collect();
    assert_eq!(
        attaches.first(),
        Some(&serde_json::json!(1)),
        "recovery attaches from the first chunk, not the gap: {attaches:?}"
    );
    assert_eq!(done.exit_code, Some(0), "{}", done.output_preview);
    assert_eq!(fs::read(root.join("dist/app.bin")).unwrap(), b"0123456789");
    assert!(
        done.output_preview
            .contains("  dist/app.bin: returned (10 bytes)"),
        "{}",
        done.output_preview
    );
}

#[test]
fn expired_gap_unverified_line_is_the_approved_text() {
    assert_eq!(
        expired_gap_unverified_line(4, 6),
        "records 4 to 6 could not be verified: they may be expired returned-file data or lost output, and the runner did not report chunk counts"
    );
    // A single record keeps the range form, like the lost-output line.
    assert_eq!(
        expired_gap_unverified_line(4, 4),
        "records 4 to 4 could not be verified: they may be expired returned-file data or lost output, and the runner did not report chunk counts"
    );
}

/// Requests the daemon received to list its catalog.
fn catalog_lists(daemon: &crate::exec_remote::wire_fixture::Daemon) -> usize {
    daemon
        .log
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, body)| body["op"] == "catalog.list")
        .count()
}

/// Task record writes and daemon catalog lists so far, for one task.
fn monitor_counts(
    daemon: &crate::exec_remote::wire_fixture::Daemon,
    task_id: &str,
) -> (usize, usize) {
    (
        crate::bash_background::persistence::record_writes::get(task_id),
        catalog_lists(daemon),
    )
}

/// An accepted build-server job that is queued, then running, then quiet
/// writes its task record and lists the daemon catalog only when its state
/// changes: the worker's 50 ms polls in between cost neither.
#[tokio::test]
async fn runon_monitor_polls_without_a_change_write_nothing_and_list_no_catalog() {
    let daemon = crate::exec_remote::wire_fixture::daemon_with_capabilities(
        Script::GatedStages { position: 2 },
        &[
            "exec-remote/v1",
            exec::types::SUBMISSION_KEY_CAPABILITY,
            exec::types::PROGRESS_CAPABILITY,
        ],
        1,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = start_with_launch(
        &registry,
        dir.path(),
        crate::bash_background::RemoteLaunch {
            explicit_runon: true,
            ..launch(daemon.connection.clone())
        },
        crate::bash_background::HardKill::After(Duration::from_secs(30)),
    );
    wait_for("the runner must accept the job", || {
        registry
            .remote_progress(&task_id, "session")
            .filter(|progress| progress.phase == RemotePhase::Queued { position: 2 })
    })
    .await;
    let launch = monitor_counts(&daemon, &task_id);
    // Many polls of a queued job with nothing new from the runner.
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let queued = monitor_counts(&daemon, &task_id);
    daemon.output_gate.notify_one();
    wait_for("the first output must mark the job running", || {
        registry
            .remote_progress(&task_id, "session")
            .filter(|progress| progress.phase == RemotePhase::Running)
    })
    .await;
    let started = monitor_counts(&daemon, &task_id);
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let running = monitor_counts(&daemon, &task_id);
    daemon.completion_gate.notify_one();
    assert_eq!(
        terminal(&registry, &task_id).await.info.status,
        BgTaskStatus::Completed
    );
    eprintln!(
        "monitor counts (record writes, catalog lists): launch {launch:?}, queued {queued:?}, started {started:?}, running {running:?}"
    );
    assert_eq!(queued, launch, "a queued job's polls changed nothing");
    assert_eq!(
        running, started,
        "a quiet running job's polls changed nothing"
    );
    // Launching resolves the executor, then reads its capabilities once for
    // every check the submission makes.
    assert_eq!(launch.1, 2, "catalog lists at launch");
}

/// A running job that streams output steadily saves its output cursor at
/// most once per checkpoint interval and never lists the catalog.
#[tokio::test]
async fn runon_monitor_steady_output_writes_only_checkpoints_and_lists_no_catalog() {
    let daemon = crate::exec_remote::wire_fixture::daemon_with_capabilities(
        Script::Continuous,
        &[
            "exec-remote/v1",
            exec::types::SUBMISSION_KEY_CAPABILITY,
            exec::types::PROGRESS_CAPABILITY,
        ],
        1,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = start_with_launch(
        &registry,
        dir.path(),
        crate::bash_background::RemoteLaunch {
            explicit_runon: true,
            ..launch(daemon.connection.clone())
        },
        crate::bash_background::HardKill::After(Duration::from_secs(30)),
    );
    let task = registry.task(&task_id).unwrap();
    wait_for("the job must start streaming output", || {
        registry
            .remote_progress(&task_id, "session")
            .filter(|progress| progress.phase == RemotePhase::Running)
    })
    .await;
    let started = monitor_counts(&daemon, &task_id);
    let bytes = fs::metadata(&task.paths.stdout).unwrap().len();
    tokio::time::sleep(Duration::from_millis(3_000)).await;
    let streamed = monitor_counts(&daemon, &task_id);
    let streamed_bytes = fs::metadata(&task.paths.stdout).unwrap().len() - bytes;
    assert!(registry.record_remote_cancel(&task).unwrap());
    assert_eq!(
        terminal(&registry, &task_id).await.info.status,
        BgTaskStatus::Killed
    );
    eprintln!(
        "steady output counts (record writes, catalog lists): started {started:?}, after 3 s {streamed:?}, {streamed_bytes} output bytes"
    );
    assert!(
        streamed_bytes > 100,
        "the runner kept streaming: {streamed_bytes}"
    );
    assert_eq!(streamed.1, started.1, "streaming output lists no catalog");
    assert!(
        streamed.0 - started.0 <= 1,
        "at most one output checkpoint in 3 s: {started:?} -> {streamed:?}"
    );
}

/// A draining wait polls once a second and only the elapsed time in its note
/// changes. The record is saved when the wait begins and then at most once
/// per `DRAINING_NOTE_SAVE_INTERVAL_MS`, while the note shown in this process
/// still moves every second.
#[tokio::test]
async fn runon_draining_wait_saves_its_record_at_a_coarse_step() {
    let daemon = daemon(
        Script::Draining {
            refusals: 1,
            retry_after_ms: Some(60_000),
        },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut clock = PausedDrainingClock::new(dir.path());
    let registry = registry();
    let task_id = start_with_launch(
        &registry,
        dir.path(),
        crate::bash_background::RemoteLaunch {
            explicit_runon: true,
            ..launch(daemon.connection.clone())
        },
        crate::bash_background::HardKill::After(Duration::from_secs(30)),
    );
    let first = clock.now();
    clock.expect_sleep(first, first + 1000).await;
    let wait_began = crate::bash_background::persistence::record_writes::get(&task_id);
    // A simulated minute of the wait: 59 more one-second passes.
    for second in 1..60 {
        clock.advance(1000);
        clock
            .expect_sleep(first + second * 1000, first + (second + 1) * 1000)
            .await;
    }
    let waited = crate::bash_background::persistence::record_writes::get(&task_id);
    let waiting = registry.observed_status(&task_id, "session", 8192).unwrap();
    assert!(
        waiting
            .output_preview
            .contains("draining for maintenance; retrying (attempt 2, 59s of 10m used)"),
        "the note shown here still moves every second: {waiting:?}"
    );
    clock.advance(1000);
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.exit_code, Some(0), "{done:?}");
    assert_eq!(exec_runs(&daemon).len(), 2);
    eprintln!(
        "draining wait: {} record writes over 59 one-second passes",
        waited - wait_began
    );
    assert!(
        waited - wait_began <= 59 / (DRAINING_NOTE_SAVE_INTERVAL_MS / 1000) as usize,
        "59 one-second passes saved the record {} times",
        waited - wait_began
    );
}

#[path = "remote_scope_tests.rs"]
mod scope_tests;
