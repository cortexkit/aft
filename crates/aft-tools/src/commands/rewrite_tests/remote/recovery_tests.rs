use aft_engine::test_bridge::bash_remote::DrainingRetryTestExt as _;
#[tokio::test]
async fn bash_status_and_watch_show_the_persisted_remote_phase_after_a_restart() {
    for started in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        // Accepted at queue position 1 and persisted; the connection file is
        // missing, so the restarted registry keeps trying to re-attach and
        // the task stays running with exactly the persisted stream records.
        let (original, task_id, paths) = persist_accepted_with_snapshot(
            dir.path(),
            dir.path().join("missing-connection.json"),
            "cargo test",
        );
        drop(original);
        if started {
            let mut metadata = read_task(&paths.json).unwrap();
            metadata.remote.as_mut().unwrap().started = Some(Started::new(0, 2_000, 1));
            write_task(&paths.json, &metadata).unwrap();
        }
        let ctx = Arc::new(restarted_context(dir.path()));
        ctx.bash_background()
            .replay_session(dir.path(), "session")
            .unwrap();

        let status = rendered_status(&ctx, &task_id);
        let watch = rendered_watch(Arc::clone(&ctx), &task_id).await;
        if started {
            assert!(
                status.starts_with(&format!(
                    "Task {task_id}: running · remote job {} · the runner stops it 30 s after it started\n",
                    id()
                )),
                "{status}"
            );
            assert!(
                watch.ends_with("\nThe runner stops it 30 s after it started."),
                "{watch}"
            );
        } else {
            let header = format!(
                "Task {task_id}: running · remote job {} · queued on the runner (",
                id()
            );
            assert!(status.starts_with(&header), "{status}");
            assert!(
                status.lines().next().unwrap().ends_with(" s, position 1)"),
                "{status}"
            );
            let last = watch.lines().last().unwrap();
            assert!(
                last.starts_with("Queued on the runner (") && last.ends_with(" s, position 1)."),
                "{watch}"
            );
        }
        assert!(!watch.contains("no kill deadline"), "{watch}");
        ctx.bash_background().detach();
    }
}

/// A recovered refusal is a finished task whether or not the runner sent
/// Accepted. Neither bash_status nor the finished-command reply may carry the
/// in-flight capture warning, which tells the caller to wait for queued or
/// running states the task will never reach.
#[tokio::test]
async fn runon_capture_recovered_terminal_status_omits_in_flight_warning() {
    use crate::commands::bash_orchestrate::REMOTE_INPUT_CAPTURE_WARNING;
    for accepted in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (original, task_id, paths) = persist_accepted_with_snapshot(
            dir.path(),
            dir.path().join("missing-connection.json"),
            "cargo test",
        );
        drop(original);
        let mut metadata = read_task(&paths.json).unwrap();
        let remote = metadata.remote.as_mut().unwrap();
        remote.explicit_runon = true;
        remote.accepted = accepted;
        remote.terminal = Some(TerminalRecord::new(
            id(),
            Outcome::RefusedBeforeStart {
                reason: RefusalReason::Unreachable,
            },
            1,
            0,
            0,
        ));
        metadata.status = BgTaskStatus::Failed;
        write_task(&paths.json, &metadata).unwrap();
        let ctx = restarted_context(dir.path());
        ctx.bash_background()
            .replay_session(dir.path(), "session")
            .unwrap();
        let request = crate::protocol::RawRequest {
            id: "recovered-capture-status".into(),
            command: "bash_status".into(),
            params: serde_json::json!({"task_id":task_id}),
            session_id: Some("session".into()),
            lsp_hints: None,
        };
        let response = crate::commands::bash_status::handle(&request, &ctx);
        assert!(!response.success, "{response:?}");
        assert_eq!(response.data["status"], "failed", "{response:?}");
        assert!(
            !response.data["output_preview"]
                .as_str()
                .unwrap()
                .contains(REMOTE_INPUT_CAPTURE_WARNING),
            "accepted={accepted}: {response:?}"
        );
        let (disk, _) = ctx
            .bash_background()
            .terminal_status_from_disk(&task_id, "session", Some(dir.path()), dir.path())
            .unwrap();
        assert_eq!(disk.remote_inputs_captured, Some(accepted));
        let response = crate::commands::bash_orchestrate::decide_bash_step(
            disk,
            Instant::now(),
            false,
            false,
            Instant::now(),
            "disk-capture-result",
        );
        let crate::commands::bash_orchestrate::BashStep::Done(response) = response else {
            panic!("recovered refusal must remain terminal");
        };
        assert!(!response.success, "{response:?}");
        assert!(
            !response.data["output"]
                .as_str()
                .unwrap()
                .contains(REMOTE_INPUT_CAPTURE_WARNING),
            "accepted={accepted}: {response:?}"
        );
    }
}

#[tokio::test]
async fn runon_explicit_timeout_prevents_a_hinted_retry_at_the_queue_deadline() {
    let daemon = daemon(retry_backoff_script(true, 30_000), "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let mut clock = PausedDrainingClock::new(dir.path());
    let registry = registry();
    let task = start_runon_retry(&registry, dir.path(), daemon.connection().clone());
    let first = clock.now();
    clock.expect_sleep(first, first + 30_000).await;
    let runs = exec_runs(&daemon);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["timeout"], 30);
    assert_eq!(runs[0]["queue_wait_limit_s"], 30);

    clock.advance(29_999);
    assert_eq!(exec_runs(&daemon).len(), 1, "must honour the runner's hint");
    clock.advance(1);
    let done = terminal(&registry, &task).await;
    assert_eq!(done.info.status, BgTaskStatus::Failed, "{done:?}");
    assert_runon_refusal(&refusal_response(done), SIGNER_REFUSED);
    assert_eq!(
        exec_runs(&daemon).len(),
        1,
        "no request at the queue deadline"
    );
    assert_no_retry_sleep(&mut clock);
}

#[tokio::test]
async fn runon_draining_retry_only_draining() {
    let daemon = daemon(
        Script::DiskFull {
            refusals: 2,
            retry_after_ms: Some(20),
        },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    shorten_draining_budget(dir.path(), 300);
    let registry = registry();
    let task = start_runon_retry(&registry, dir.path(), daemon.connection().clone());
    let done = terminal(&registry, &task).await;
    assert_eq!(done.info.status, BgTaskStatus::Failed, "{done:?}");
    assert_runon_refusal(&refusal_response(done), "runner_disk_full (maintenance)");
    assert_eq!(exec_runs(&daemon).len(), 1);
}

/// Start the owner's `bash.kill_scope` off the test runtime. It blocks until
/// every paused remote task settles its cancellation, or its own 10-second
/// deadline passes.
fn start_kill_scope(
    registry: &BgTaskRegistry,
    scope: &subc_protocol::scope::ScopeStamp,
) -> tokio::task::JoinHandle<crate::protocol::Response> {
    let registry = registry.clone();
    let scope = scope.clone();
    tokio::task::spawn_blocking(move || {
        crate::commands::bash_kill_scope::handle_bash_kill_scope(
            serde_json::json!({"scope":scope}),
            Some(&scope.owner),
            &[registry],
        )
    })
}

/// The owner pauses a worker whose `runon` task sleeps before retrying a
/// draining runner. The worker wakes, sees the cancellation, and must settle
/// the pause itself: nothing else ever will.
#[tokio::test]
async fn bash_kill_scope_settles_while_draining_retry_sleeps() {
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
    let registry = registry();
    let scope = owner_scope("draining-sleep");
    let task = start_scoped_runon_retry(&registry, dir.path(), daemon.connection().clone(), &scope);
    let first = clock.now();
    clock.expect_sleep(first, first + 1000).await;
    assert_pause_settles_promptly(&registry, start_kill_scope(&registry, &scope), &task).await;
    // The retry clock never moved: cancellation, not a retry deadline, woke
    // the worker.
    assert_eq!(clock.now(), first);
    remote_worker_exited(&task).await;
    assert_paused(&registry, &task);
    assert_eq!(exec_runs(&daemon).len(), 1, "never resubmitted");
}

/// The pause lands after the worker decided to resubmit but before it records
/// the dispatch. Its recheck there must settle the pause.
#[tokio::test]
async fn bash_kill_scope_settles_when_paused_just_before_draining_resubmit() {
    let daemon = daemon(
        Script::Draining {
            refusals: 1,
            retry_after_ms: Some(200),
        },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut clock = PausedDrainingClock::new(dir.path());
    let registry = registry();
    let scope = owner_scope("draining-resubmit");
    let task = start_scoped_runon_retry(&registry, dir.path(), daemon.connection().clone(), &scope);
    let first = clock.now();
    clock.expect_sleep(first, first + 1000).await;
    let mut gate = HeldDrainingGate::arm(dir.path(), DrainingGate::BeforeResubmit);
    clock.advance(5000);
    gate.reached().await;
    let operation = start_kill_scope(&registry, &scope);
    pause_recorded(&registry, &task).await;
    gate.release();
    assert_pause_settles_promptly(&registry, operation, &task).await;
    assert_eq!(clock.now(), first + 5000);
    remote_worker_exited(&task).await;
    assert_paused(&registry, &task);
    assert_eq!(exec_runs(&daemon).len(), 1, "never resubmitted");
}

/// The pause lands just before the worker finds its retry budget spent. The
/// worker then records the draining refusal, which cannot replace the pause's
/// terminal state, and must still settle the pause.
#[tokio::test]
async fn bash_kill_scope_settles_when_paused_as_draining_budget_runs_out() {
    let daemon = daemon(
        Script::Draining {
            refusals: u32::MAX,
            retry_after_ms: Some(10_000),
        },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    shorten_draining_budget(dir.path(), 600);
    let mut clock = PausedDrainingClock::new(dir.path());
    let registry = registry();
    let scope = owner_scope("draining-budget");
    let task = start_scoped_runon_retry(&registry, dir.path(), daemon.connection().clone(), &scope);
    let first = clock.now();
    clock.expect_sleep(first, first + 600).await;
    let mut gate = HeldDrainingGate::arm(dir.path(), DrainingGate::BeforeBudgetCheck);
    clock.advance(600);
    gate.reached().await;
    let operation = start_kill_scope(&registry, &scope);
    pause_recorded(&registry, &task).await;
    gate.release();
    assert_pause_settles_promptly(&registry, operation, &task).await;
    remote_worker_exited(&task).await;
    assert_paused(&registry, &task);
    assert_eq!(exec_runs(&daemon).len(), 1, "never resubmitted");
}

/// After a restart, a draining retry is due and the worker must reconnect to
/// resubmit. The pause lands before that reconnect fails; the worker records
/// that it could not dispatch and must still settle the pause.
#[tokio::test]
async fn bash_kill_scope_settles_when_restored_draining_retry_cannot_reconnect() {
    let daemon = daemon(
        Script::Draining {
            refusals: u32::MAX,
            retry_after_ms: Some(200),
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
    let scope = owner_scope("draining-reconnect");
    let task_id =
        start_scoped_runon_retry(&original, dir.path(), daemon.connection().clone(), &scope);
    clock.expect_event(DrainingClockEvent::Stopped).await;
    // The runner's connection file is gone by the time the module restarts.
    let json = original.test_task(&task_id).unwrap().paths().json.clone();
    let mut saved = read_task(&json).unwrap();
    saved.remote.as_mut().unwrap().connection_file = Some(dir.path().join("missing.json"));
    write_task(&json, &saved).unwrap();
    clock.advance(5000);
    let mut gate = HeldDrainingGate::arm(dir.path(), DrainingGate::BeforeBudgetCheck);
    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    gate.reached().await;
    let operation = start_kill_scope(&restarted, &scope);
    pause_recorded(&restarted, &task_id).await;
    gate.release();
    assert_pause_settles_promptly(&restarted, operation, &task_id).await;
    assert_paused(&restarted, &task_id);
    assert_eq!(exec_runs(&daemon).len(), 1, "never resubmitted");
}

/// Run the owner's `bash.kill_scope` as the module's frame loop does after a
/// restart: no live registry holds the scope's tasks, which are found in
/// their saved records under `storage`.
async fn kill_scope_with_saved_tasks(
    storage: &Path,
    scope: &subc_protocol::scope::ScopeStamp,
) -> crate::protocol::Response {
    tokio::time::timeout(
        Duration::from_secs(30),
        start_kill_scope_with_saved_tasks(storage, scope),
    )
    .await
    .expect("bash.kill_scope must reply")
    .unwrap()
}

/// Start the owner's `bash.kill_scope` as `kill_scope_with_saved_tasks`
/// runs it, off the test runtime, without waiting for its reply.
fn start_kill_scope_with_saved_tasks(
    storage: &Path,
    scope: &subc_protocol::scope::ScopeStamp,
) -> tokio::task::JoinHandle<crate::protocol::Response> {
    let storage = storage.to_path_buf();
    let scope = scope.clone();
    tokio::task::spawn_blocking(move || {
        crate::commands::bash_kill_scope::handle_bash_kill_scope_with_saved_tasks(
            serde_json::json!({"scope": scope}),
            Some(&scope.owner),
            &[],
            Some(&storage),
        )
    })
}

/// The production entry point: `configure` of a standalone process (here
/// for another project) starts the owed cancel pass for its storage root.
/// The runner gets the paused task's cancel although the task's own project
/// never binds.
#[tokio::test]
async fn configure_sends_an_owed_runner_cancel_without_binding_its_project() {
    let dir = tempfile::tempdir().unwrap();
    let (daemon, task_id, job_id, json) = saved_task_owing_a_cancel(dir.path(), 2).await;
    let other = tempfile::tempdir().unwrap();
    let ctx = Arc::new(restarted_context(other.path()));
    let configure = {
        let (ctx, other, storage) = (
            ctx.clone(),
            other.path().to_path_buf(),
            dir.path().to_path_buf(),
        );
        tokio::task::spawn_blocking(move || {
            // The configured storage root, not a worker-wide override.
            let _storage = crate::test_env::without_storage_override();
            crate::commands::configure::handle_configure(
                &crate::protocol::RawRequest {
                    id: "configure-other-project".into(),
                    command: "configure".into(),
                    lsp_hints: None,
                    session_id: None,
                    params: serde_json::json!({
                        "project_root": other,
                        "storage_dir": storage,
                        "harness": "opencode",
                    }),
                },
                &ctx,
            )
        })
    };
    daemon.wait_for_requests("exec.cancel", 1).await;
    let configured = configure.await.unwrap();
    assert!(configured.success, "{configured:?}");
    wait_for("the sent cancel must be saved", || {
        let remote = read_task(&json).ok()?.remote?;
        (!remote.cancel_pending).then_some(())
    })
    .await;
    assert_owed_cancel_sent_once(&daemon, job_id, &json);
    assert!(
        ctx.bash_background().test_task(&task_id).is_none(),
        "the task's project was not bound"
    );
    assert_eq!(remote_workers(&task_id), (1, 1), "no worker was started");
}

/// `bash.kill_scope` stops a scope's local tasks in one pass and its remote
/// tasks in a second. Here the runner refuses a remote task between the two
/// passes, and its worker would run the command locally instead: the first
/// pass saw it remote, the second would see it local, so neither would stop
/// it. The pause must cover it anyway: its local fallback never starts, and
/// the reply names it as stopped.
#[tokio::test]
async fn bash_kill_scope_stops_a_task_that_falls_back_to_local_between_its_passes() {
    let daemon = daemon(Script::GatedAdmission { refuse: true }, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("local-fallback-ran");
    let registry = registry();
    let scope = owner_scope("fallback-between-passes");
    ensure_git_fixture(dir.path());
    let task_id = reg::with_scope_spawn(Some(scope.clone()), || {
        registry.spawn_remote(
            launch(daemon.connection().clone()),
            SpawnPlan::Unsandboxed,
            &format!("printf x > '{}'", marker.display()),
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
    })
    .unwrap();
    daemon.wait_for_requests("exec.run", 1).await;
    // The runner refuses the job; hold the worker after it received the
    // refusal, before it decides on the local fallback.
    let mut refusal = HeldDrainingGate::arm(dir.path(), DrainingGate::BeforeRefusalReport);
    daemon.acceptance_gate().notify_one();
    refusal.reached().await;
    let (between_passes, release_remote_pass) =
        crate::commands::bash_kill_scope::saved_recovery_gate::arm(
            dir.path(),
            crate::commands::bash_kill_scope::saved_recovery_gate::Point::BetweenPasses,
        );
    let operation = {
        let registry = registry.clone();
        let scope = scope.clone();
        let storage = dir.path().to_path_buf();
        tokio::task::spawn_blocking(move || {
            crate::commands::bash_kill_scope::handle_bash_kill_scope_with_saved_tasks(
                serde_json::json!({"scope": scope}),
                Some(&scope.owner),
                &[registry],
                Some(&storage),
            )
        })
    };
    tokio::time::timeout(Duration::from_secs(30), between_passes)
        .await
        .expect("bash.kill_scope must finish its local pass")
        .unwrap();
    // Between the passes, the worker acts on the refusal.
    refusal.release();
    remote_worker_exited(&task_id).await;
    release_remote_pass.send(()).unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(30), operation)
        .await
        .expect("bash.kill_scope must reply")
        .unwrap();
    // Give a local child that wrongly started the time to write its marker.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!marker.exists(), "the local fallback ran: {reply:?}");
    assert!(reply.success, "{reply:?}");
    let killed = reply.data["killed"].as_array().unwrap();
    assert!(
        killed
            .iter()
            .any(|entry| entry["task_id"] == task_id.as_str()),
        "the stopped task must be reported: {reply:?}"
    );
    assert_paused(&registry, &task_id);
}

/// After a module restart, the owner pauses a worker whose saved draining
/// retry is already due. Recovering that saved task for the pause must not
/// let it resubmit: the resubmitted command is the one the owner is stopping.
/// The scope's saved local command keeps the pause busy long enough for a
/// worker started during recovery to reach the runner.
#[tokio::test]
async fn bash_kill_scope_never_resubmits_a_due_saved_draining_retry() {
    let daemon = aft_engine::test_bridge::bash_remote::remote_wire::daemon_with_clients(
        Script::Draining {
            refusals: u32::MAX,
            retry_after_ms: Some(200),
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
    let scope = owner_scope("saved-draining-retry");
    let original = registry();
    let task_id =
        start_scoped_runon_retry(&original, dir.path(), daemon.connection().clone(), &scope);
    clock.expect_event(DrainingClockEvent::Stopped).await;
    let json = original.test_task(&task_id).unwrap().paths().json.clone();
    original.detach();
    let (local_task, local_pid) = start_saved_term_ignoring_task(dir.path(), &scope).await;
    // The retry is due by the time the owner pauses the restarted module.
    clock.advance(5000);
    let reply = kill_scope_with_saved_tasks(dir.path(), &scope).await;
    assert_eq!(
        exec_runs(&daemon).len(),
        1,
        "the paused draining retry was resubmitted: {reply:?}"
    );
    assert_saved_pause_reply(&reply, &task_id, &local_task, local_pid).await;
    assert_saved_paused(&json);
    assert_eq!(exec_runs(&daemon).len(), 1, "never resubmitted");
}

/// After a module restart, the owner pauses a scope whose saved draining
/// retry is due, and the task's project binds while `bash.kill_scope` holds
/// the copy it recovered, not yet paused. The bind must register that same
/// task, held without a worker, not build a second copy whose worker would
/// resubmit the command the owner is stopping.
#[tokio::test]
async fn bash_kill_scope_recovery_then_a_bind_share_one_saved_task() {
    let dir = tempfile::tempdir().unwrap();
    let scope = owner_scope("saved-recovery-then-bind");
    let (daemon, _clock, task_id, json) =
        saved_due_draining_retry(dir.path(), &scope, u32::MAX).await;
    let (reached, release) = crate::commands::bash_kill_scope::saved_recovery_gate::arm(
        dir.path(),
        crate::commands::bash_kill_scope::saved_recovery_gate::Point::AfterRecovery,
    );
    let operation = start_kill_scope_with_saved_tasks(dir.path(), &scope);
    tokio::time::timeout(Duration::from_secs(30), reached)
        .await
        .expect("bash.kill_scope must recover the saved task")
        .unwrap();
    let project = registry();
    project.replay_session(dir.path(), "session").unwrap();
    assert_eq!(
        remote_workers(&task_id),
        (1, 1),
        "the bind started a worker before the pause"
    );
    release.send(()).unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(30), operation)
        .await
        .expect("bash.kill_scope must reply")
        .unwrap();
    assert_one_saved_task_paused(&reply, &task_id);
    // One worker since the restart: the one the pause started to settle it.
    assert_eq!(remote_workers(&task_id).0, 2, "{reply:?}");
    wait_for("the paused task's worker must stop", || {
        (remote_workers(&task_id) == (2, 2)).then_some(())
    })
    .await;
    // The bound project sees the owner's stop on the task it registered.
    assert_paused(&project, &task_id);
    assert_saved_paused(&json);
    assert_eq!(exec_runs(&daemon).len(), 1, "never resubmitted");
    assert_project_told_of_the_stop_once(&project, &task_id);
}

/// After a module restart, the task's project binds and its worker is about
/// to resubmit the saved, due draining retry when the owner's
/// `bash.kill_scope` recovers the scope's saved tasks, before the module
/// lists the project's registry as live. The pause must register the bound
/// project's task, so that its worker sees the stop; a private copy would be
/// paused while that worker resubmits.
#[tokio::test]
async fn a_bind_then_bash_kill_scope_recovery_share_one_saved_task() {
    let dir = tempfile::tempdir().unwrap();
    let scope = owner_scope("saved-bind-then-recovery");
    let (daemon, _clock, task_id, json) =
        saved_due_draining_retry(dir.path(), &scope, u32::MAX).await;
    let mut gate = HeldDrainingGate::arm(dir.path(), DrainingGate::BeforeBudgetCheck);
    let project = registry();
    project.replay_session(dir.path(), "session").unwrap();
    gate.reached().await;
    assert_eq!(remote_workers(&task_id), (2, 1));
    let operation = start_kill_scope_with_saved_tasks(dir.path(), &scope);
    wait_for("the pause must be saved", || {
        let saved = read_task(&json).ok()?;
        (saved.status == BgTaskStatus::Killed
            && saved.status_reason.as_deref()
                == Some(aft_engine::test_bridge::bash_remote::OWNER_PAUSED_REASON))
        .then_some(())
    })
    .await;
    gate.release();
    let reply = tokio::time::timeout(Duration::from_secs(30), operation)
        .await
        .expect("bash.kill_scope must reply")
        .unwrap();
    assert_one_saved_task_paused(&reply, &task_id);
    // The bound project's worker is the only one since the restart.
    assert_eq!(remote_workers(&task_id).0, 2, "{reply:?}");
    wait_for("the paused task's worker must stop", || {
        (remote_workers(&task_id) == (2, 2)).then_some(())
    })
    .await;
    assert_paused(&project, &task_id);
    assert_saved_paused(&json);
    assert_eq!(exec_runs(&daemon).len(), 1, "never resubmitted");
    assert_project_told_of_the_stop_once(&project, &task_id);
}

/// After a module restart, `bash.kill_scope` recovers and stops a saved
/// task. The task's project binds just as the stop is saved: its replay read
/// the task's record from before the stop, so it registers the task the
/// recovery registry holds, which is already stopped. The stop's notices,
/// which the recovery registry could not deliver, must reach the project's
/// session once.
#[tokio::test]
async fn a_bind_racing_a_saved_tasks_stop_gets_its_notices() {
    let dir = tempfile::tempdir().unwrap();
    let scope = owner_scope("saved-stop-then-bind");
    let (daemon, _clock, task_id, json) =
        saved_due_draining_retry(dir.path(), &scope, u32::MAX).await;
    let before_stop = fs::read(&json).unwrap();
    let (reached, release) = crate::commands::bash_kill_scope::saved_recovery_gate::arm(
        dir.path(),
        crate::commands::bash_kill_scope::saved_recovery_gate::Point::BeforeDetach,
    );
    let operation = start_kill_scope_with_saved_tasks(dir.path(), &scope);
    tokio::time::timeout(Duration::from_secs(30), reached)
        .await
        .expect("bash.kill_scope must stop the saved task")
        .unwrap();
    assert_saved_paused(&json);
    // Stand in for a replay that read the record just before the stop
    // saved it.
    fs::write(&json, &before_stop).unwrap();
    let project = registry();
    project.replay_session(dir.path(), "session").unwrap();
    release.send(()).unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(30), operation)
        .await
        .expect("bash.kill_scope must reply")
        .unwrap();
    assert_one_saved_task_paused(&reply, &task_id);
    wait_for("the paused task's worker must stop", || {
        (remote_workers(&task_id) == (2, 2)).then_some(())
    })
    .await;
    assert_eq!(
        remote_workers(&task_id),
        (2, 2),
        "the bind started a worker"
    );
    assert_paused(&project, &task_id);
    assert_eq!(exec_runs(&daemon).len(), 1, "never resubmitted");
    assert_project_told_of_the_stop_once(&project, &task_id);
}

/// After a module restart, the owner pauses a worker whose saved remote job
/// the runner refused, which a restored worker runs locally instead. Covers
/// a refusal the worker learns when it reattaches and one saved before the
/// restart. Recovering that saved task for the pause must not start the
/// local run.
#[tokio::test]
async fn bash_kill_scope_never_starts_a_saved_tasks_local_fallback() {
    for saved_refusal in [false, true] {
        let daemon = daemon(Script::AttachRefused, "exec-remote/v1").await;
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("local-fallback-ran");
        let (original, task_id, paths) = persist_accepted_with_snapshot(
            dir.path(),
            daemon.connection().clone(),
            &format!("printf x >> '{}'", marker.display()),
        );
        let scope = owner_scope("saved-local-fallback");
        let mut saved = read_task(&paths.json).unwrap();
        saved.scope = Some(scope.clone());
        if saved_refusal {
            saved.remote.as_mut().unwrap().terminal = Some(TerminalRecord::new(
                id(),
                Outcome::RefusedBeforeStart {
                    reason: RefusalReason::Unknown("future_refusal".into()),
                },
                1,
                0,
                0,
            ));
        }
        write_task(&paths.json, &saved).unwrap();
        original.detach();
        let (local_task, local_pid) = start_saved_term_ignoring_task(dir.path(), &scope).await;
        let reply = kill_scope_with_saved_tasks(dir.path(), &scope).await;
        assert!(
            !marker.exists(),
            "saved_refusal={saved_refusal}: the paused task ran locally: {reply:?}"
        );
        assert_saved_pause_reply(&reply, &task_id, &local_task, local_pid).await;
        assert_saved_paused(&paths.json);
        assert!(!marker.exists(), "saved_refusal={saved_refusal}");
    }
}

#[tokio::test]
async fn head_blocking_runon_uses_configured_cap_while_queued() {
    let daemon = daemon(Script::GatedStages { position: 9 }, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    ctx.update_config(|config| config.bash.worker_wait_max_ms = 250);
    let (reply, waited) = orchestrated_reply_with_probe(
        &ctx,
        &daemon,
        serde_json::json!({
            "command": "uname -s",
            "runon": "linux",
            "wait": true,
            "timeout": 5_000,
            "foreground_orchestrate": true,
            "compressed": false,
        }),
        |task_id| {
            ctx.bash_background()
                .remote_progress(task_id, "session")
                .is_some_and(|progress| progress.phase == RemotePhase::Queued { position: 9 })
        },
    )
    .await;
    assert!(reply.success, "{reply:?}");
    assert!(waited >= Duration::from_millis(250), "{waited:?}");
    assert!(waited < plugin_transport_deadline(250), "{waited:?}");
    assert_eq!(reply.data["status"], "running", "{reply:?}");
    assert_eq!(reply.data["remote_phase"], "queued", "{reply:?}");
    assert_eq!(reply.data["queue_position"], 9, "{reply:?}");
    assert_eq!(reply.data["remote_job_id"], id().to_string(), "{reply:?}");
    assert_eq!(runner_cancels(&daemon), 0);
    assert_eq!(exec_runs(&daemon).len(), 1);
    let registry = ctx.bash_background().clone();
    let task_id = reply.data["task_id"].as_str().unwrap().to_string();
    tokio::task::spawn_blocking(move || registry.kill(&task_id, "session"))
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn head_blocking_runon_uses_configured_cap_before_any_stream_record() {
    assert_blocking_runon_limit_before_acceptance(5_000, Some(250)).await;
}

#[tokio::test]
async fn blocking_runon_wait_limit_before_accepted_reports_input_capture() {
    assert_blocking_runon_limit_before_acceptance(250, None).await;
}

async fn assert_blocking_runon_limit_before_acceptance(timeout_ms: u64, cap_ms: Option<u64>) {
    let daemon = daemon(Script::GatedAcceptance, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    if let Some(cap_ms) = cap_ms {
        ctx.update_config(|config| config.bash.worker_wait_max_ms = cap_ms);
    }
    let limit_ms = cap_ms.unwrap_or(timeout_ms);
    let (reply, waited) = orchestrated_reply_with_probe(
        &ctx,
        &daemon,
        serde_json::json!({
            "command": "uname -s",
            "runon": "linux",
            "wait": true,
            "timeout": timeout_ms,
            "foreground_orchestrate": true,
            "compressed": false,
        }),
        |_| !exec_runs(&daemon).is_empty(),
    )
    .await;
    assert!(reply.success, "{reply:?}");
    assert!(waited >= Duration::from_millis(limit_ms), "{waited:?}");
    assert!(waited < plugin_transport_deadline(limit_ms), "{waited:?}");
    assert_eq!(reply.data["status"], "starting", "{reply:?}");
    assert_eq!(reply.data["remote_phase"], "submitting", "{reply:?}");
    assert_eq!(reply.data["queue_position"], serde_json::Value::Null);
    assert_eq!(reply.data["remote_job_id"], serde_json::Value::Null);
    assert!(
        reply.data["output"]
            .as_str()
            .unwrap()
            .contains("capturing inputs; not accepted yet"),
        "{reply:?}"
    );
    assert_eq!(runner_cancels(&daemon), 0);
    assert_eq!(exec_runs(&daemon).len(), 1);
    // Let AFT learn the job ID so cleanup can cancel the held-open stream.
    daemon.acceptance_gate().notify_one();
    let task_id = reply.data["task_id"].as_str().unwrap().to_string();
    wait_for(
        "released acceptance must be processed before cancellation",
        || {
            ctx.bash_background()
                .remote_progress(&task_id, "session")
                .filter(|progress| progress.phase == RemotePhase::Queued { position: 1 })
        },
    )
    .await;
    let registry = ctx.bash_background().clone();
    tokio::task::spawn_blocking(move || registry.kill(&task_id, "session"))
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn runon_missing_workdir_is_refused_by_name_before_dispatch() {
    let daemon = daemon(Script::Plain, "exec-remote/v1").await;
    let project = tempfile::tempdir().unwrap();
    let ctx = remote_test_context(project.path());
    let missing_workdir = project.path().join("does-not-exist");
    let marker = project.path().join("must-not-run-locally");
    let response = handle_with_policy(
        &ctx,
        Some(launch(daemon.connection().clone())),
        serde_json::json!({
            "command": format!("printf local-proof > '{}'", marker.display()),
            "runon": "linux",
            "workdir": missing_workdir,
            "compressed": false,
        }),
    );
    assert!(response.success, "{response:?}");
    let task_id = response.data["task_id"].as_str().unwrap();
    let done = terminal(ctx.bash_background(), task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Failed, "{done:?}");
    let expected = format!(
        "runon refused: working directory does not exist: {}; command did not run (os error 2)",
        missing_workdir.display()
    );
    assert_eq!(
        done.info.status_reason.as_deref(),
        Some(expected.as_str()),
        "{done:?}"
    );
    assert_eq!(done.child_pid, None);
    assert_eq!(done.remote_job_id, None);
    let task = ctx.bash_background().test_task(task_id).unwrap();
    assert!(!task.test_state().lock().unwrap().metadata().local_fallback_started);
    assert!(!marker.exists(), "a refused runon must never spawn locally");
    assert!(
        exec_runs(&daemon).is_empty(),
        "a missing cwd must never dispatch"
    );
}

#[tokio::test]
async fn runon_transport_failure_still_reports_remote_unavailable() {
    let daemon = daemon(Script::Plain, "exec.remote/v1").await;
    let project = tempfile::tempdir().unwrap();
    let ctx = remote_test_context(project.path());
    let marker = project.path().join("must-not-run-locally");
    let response = handle_with_policy(
        &ctx,
        Some(launch(daemon.connection().clone())),
        serde_json::json!({
            "command": format!("printf local-proof > '{}'", marker.display()),
            "runon": "linux",
            "compressed": false,
        }),
    );
    assert!(response.success, "{response:?}");
    let task_id = response.data["task_id"].as_str().unwrap();
    let done = terminal(ctx.bash_background(), task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Failed, "{done:?}");
    assert!(
        done.info
            .status_reason
            .as_deref()
            .unwrap()
            .starts_with("runon refused: remote execution is unavailable; command did not run: remote execution route: "),
        "{done:?}"
    );
    let task = ctx.bash_background().test_task(task_id).unwrap();
    assert!(!task.test_state().lock().unwrap().metadata().local_fallback_started);
    assert!(!marker.exists(), "a refused runon must never spawn locally");
    assert!(exec_runs(&daemon).is_empty());
}

#[tokio::test]
async fn runon_non_git_workdir_is_refused_by_name_before_dispatch() {
    let daemon = daemon(Script::Plain, "exec-remote/v1").await;
    let project = tempfile::tempdir().unwrap();
    let ctx = restarted_context(project.path());
    // The runner may place TMPDIR inside a checkout. Use the system's other
    // Unix temporary root so this workdir cannot inherit that repository.
    let workdir = tempfile::Builder::new()
        .prefix("aft-runon-non-git-")
        .tempdir_in("/var/tmp")
        .unwrap();
    assert!(
        worktree_root_containing(workdir.path(), exec::DirectoryKind::WorkingDirectory)
            .unwrap()
            .is_none()
    );
    let marker = workdir.path().join("must-not-run-locally");
    let response = handle_with_policy(
        &ctx,
        Some(launch(daemon.connection().clone())),
        serde_json::json!({
            "command": format!("printf local-proof > '{}'", marker.display()),
            "runon": "linux",
            "workdir": workdir.path(),
            "compressed": false,
        }),
    );
    assert!(response.success, "{response:?}");
    let done = terminal(
        ctx.bash_background(),
        response.data["task_id"].as_str().unwrap(),
    )
    .await;
    assert_eq!(done.info.status, BgTaskStatus::Failed, "{done:?}");
    for expected in [
        "runon refused before remote dispatch; command did not run",
        "runon refused: remote execution is unavailable; command did not run",
        "cwd resolves outside the workspace key",
    ] {
        assert!(done.output_preview.contains(expected), "{done:?}");
    }
    let named = format!(
        "workdir {} is not inside a git checkout; this session's checkout is ",
        workdir.path().canonicalize().unwrap().display()
    );
    assert!(done.output_preview.contains(&named), "{done:?}");
    assert!(!marker.exists(), "a refused runon must never spawn locally");
    assert!(
        exec_runs(&daemon).is_empty(),
        "a non-git cwd must never dispatch"
    );
}

#[tokio::test]
async fn runon_cd_refusal_has_distinct_code_and_no_execution() {
    let daemon = daemon(Script::Plain, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let checkout = dir.path().join("checkout");
    let other = dir.path().join("other");
    fs::create_dir_all(&checkout).unwrap();
    fs::create_dir_all(&other).unwrap();
    git_fixture(&other);
    let ctx = remote_test_context(&checkout);
    let marker = checkout.join("must-not-run-locally");
    let response = handle_with_policy(
        &ctx,
        Some(launch(daemon.connection().clone())),
        serde_json::json!({
            "command": format!("cd '{}'; printf local-proof > '{}'", other.display(), marker.display()),
            "runon": "linux", "compressed": false,
        }),
    );
    assert!(response.success, "{response:?}");
    let task_id = response.data["task_id"].as_str().unwrap();
    let done = terminal(ctx.bash_background(), task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Failed, "{done:?}");
    assert_eq!(
        done.exit_code, None,
        "refused before start, not a process exit"
    );
    assert_eq!(done.child_pid, None);
    let refusal = refusal_response(done);
    assert_eq!(
        refusal.data["code"], "runon_cd_outside_checkout",
        "{refusal:?}"
    );
    let expected = format!(
        "runon refused: the command changes into {}, which is outside the checkout the build server copies ({}); the command did not run. Set the bash tool's workdir to that folder instead of using cd.",
        other.canonicalize().unwrap().display(), checkout.canonicalize().unwrap().display()
    );
    assert_eq!(refusal.data["message"], expected);
    assert_eq!(
        crate::subc_format::format_response("bash", &refusal, false),
        expected
    );
    let task = ctx.bash_background().test_task(task_id).unwrap();
    let state = task.test_state().lock().unwrap();
    let persisted = serde_json::to_value(&state.metadata()).unwrap();
    assert_eq!(
        persisted["remote"]["pre_start_refusal"]["code"],
        "runon_cd_outside_checkout"
    );
    assert_eq!(persisted["remote"]["pre_start_refusal"]["ran"], "none");
    assert!(persisted["remote"]["terminal"].is_null());
    assert!(persisted["remote"]["job_id"].is_null());
    let restored: PersistedTask = serde_json::from_value(persisted).unwrap();
    let restored_refusal = remote_refusal(&restored).unwrap();
    assert_eq!(restored_refusal.code, "runon_cd_outside_checkout");
    assert_eq!(restored_refusal.message, expected);
    assert!(!restored.local_fallback_started);
    let storage = crate::bash_background::task_storage_dir(&ctx);
    let persisted_path = task.paths().json.clone();
    drop(state);
    drop(task);
    let restarted = registry();
    assert!(restarted.test_task(task_id).is_none());
    assert!(persisted_path.exists());
    restarted.replay_session(&storage, "session").unwrap();
    let replayed = terminal(&restarted, task_id).await;
    let replayed_refusal = refusal_response(replayed);
    assert_eq!(replayed_refusal.data["code"], "runon_cd_outside_checkout");
    assert_eq!(replayed_refusal.data["message"], expected);
    assert!(exec_runs(&daemon).is_empty(), "a refusal must never submit");
    assert!(!marker.exists(), "a refusal must never spawn locally");
}

#[tokio::test]
async fn runon_cd_declared_siblings_are_allowed() {
    let dir = tempfile::tempdir().unwrap();
    let checkout = dir.path().join("checkout");
    let commons = dir.path().join("commons");
    fs::create_dir_all(&checkout).unwrap();
    fs::create_dir_all(commons.join("nested")).unwrap();
    git_fixture(&commons);
    let ctx = remote_test_context(&checkout);
    // The grant arrives through a symlinked parent, while runon checks the
    // canonical sibling spelling. Grant storage must give both one identity.
    let alias = dir.path().join("parent-alias");
    std::os::unix::fs::symlink(dir.path(), &alias).unwrap();
    let granted = alias.join("commons");
    assert_ne!(granted, commons.canonicalize().unwrap());
    let _read = grant_sibling_read(&ctx, &granted);
    let commons = commons.canonicalize().unwrap();
    for (sibling, line) in [
        (&commons, "cd ../commons && printf remote"),
        (&granted, "pushd ../commons/nested; printf remote"),
    ] {
        let daemon = daemon(Script::Plain, "exec-remote/v1").await;
        let mut policy = launch(daemon.connection().clone());
        policy.params.siblings = vec![sibling.to_str().unwrap().to_owned()];
        let response = handle_with_policy(
            &ctx,
            Some(policy),
            serde_json::json!({"command": line, "runon": "linux", "compressed": false}),
        );
        assert!(response.success, "{line}: {response:?}");
        let done = terminal(
            ctx.bash_background(),
            response.data["task_id"].as_str().unwrap(),
        )
        .await;
        assert_eq!(
            done.info.status,
            BgTaskStatus::Completed,
            "{line}: {done:?}"
        );
        let runs = exec_runs(&daemon);
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(runs[0]["command"], line);
        assert_eq!(runs[0]["siblings"], serde_json::json!([commons]));
    }
}

#[tokio::test]
async fn runon_cd_linked_worktree_sibling_alias_is_allowed() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main");
    let commons = dir.path().join("commons");
    git_fixture(&main);
    git_fixture(&commons);
    let worktree = dir.path().join("worktrees/worker");
    add_git_worktree(&main, &worktree, "cd-sibling-alias");
    let ctx = remote_test_context(&worktree);
    let _read = grant_sibling_read(&ctx, &commons);
    let worktree = worktree.canonicalize().unwrap();
    let commons = commons.canonicalize().unwrap();
    assert!(!worktree.parent().unwrap().join("commons").exists());
    for line in [
        "cd ../commons && printf remote",
        "cd ../commons/nested && printf remote",
    ] {
        let daemon = daemon(Script::Plain, "exec-remote/v1").await;
        let mut policy = launch(daemon.connection().clone());
        policy.params.siblings = vec![commons.to_str().unwrap().to_owned()];
        let response = handle_with_policy(
            &ctx,
            Some(policy),
            serde_json::json!({
                "command": line, "runon": "linux", "compressed": false,
            }),
        );
        assert!(response.success, "{response:?}");
        let done = terminal(
            ctx.bash_background(),
            response.data["task_id"].as_str().unwrap(),
        )
        .await;
        assert_eq!(
            done.info.status,
            BgTaskStatus::Completed,
            "{line}: {done:?}"
        );
        let runs = exec_runs(&daemon);
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(runs[0]["workspace_key"], worktree.to_str().unwrap());
        assert_eq!(runs[0]["siblings"], serde_json::json!([commons]));
        assert_eq!(runs[0]["command"], line);
    }
}

#[tokio::test]
async fn runon_cd_windows_absolute_targets_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let checkout = dir.path().join("checkout");
    let commons = dir.path().join("commons");
    git_fixture(&checkout);
    git_fixture(&commons);
    let ctx = remote_test_context(&checkout);
    let _read = grant_sibling_read(&ctx, &commons);
    let checkout = checkout.canonicalize().unwrap();
    let commons = commons.canonicalize().unwrap();
    let marker = checkout.join("must-not-run-locally");
    for target in [&checkout, &commons, &PathBuf::from("/tmp")] {
        let daemon = windows_daemon(Some(0), true).await;
        let mut policy = launch(daemon.connection().clone());
        policy.params.siblings = vec![commons.to_str().unwrap().to_owned()];
        let response = handle_with_policy(
            &ctx,
            Some(policy),
            serde_json::json!({
                "command": format!("cd '{}'; printf local-proof > '{}'", target.display(), marker.display()),
                "runon": "windows", "compressed": false,
            }),
        );
        assert!(response.success, "{response:?}");
        let done = terminal(
            ctx.bash_background(),
            response.data["task_id"].as_str().unwrap(),
        )
        .await;
        assert_eq!(
            done.info.status,
            BgTaskStatus::Failed,
            "{target:?}: {done:?}"
        );
        let refusal = refusal_response(done);
        assert_eq!(refusal.data["code"], "runon_cd_outside_checkout");
        let expected = format!(
            "runon refused: the command changes into {}, which is not valid on the Windows runner because absolute command paths are not translated (checkout {}); the command did not run. Set the bash tool's workdir to that folder instead of using cd.",
            target.canonicalize().unwrap().display(), checkout.display()
        );
        assert_eq!(refusal.data["message"], expected);
        assert!(exec_runs(&daemon).is_empty(), "a refusal must never submit");
        assert!(!marker.exists(), "a refusal must never spawn locally");
    }
}

#[tokio::test]
async fn runon_cd_windows_relative_targets_are_allowed() {
    let dir = tempfile::tempdir().unwrap();
    let checkout = dir.path().join("checkout");
    let commons = dir.path().join("commons");
    git_fixture(&checkout);
    git_fixture(&commons);
    fs::create_dir(checkout.join("nested")).unwrap();
    let ctx = remote_test_context(&checkout);
    let _read = grant_sibling_read(&ctx, &commons);
    let commons = commons.canonicalize().unwrap();
    for line in [
        "cd nested && printf remote",
        "cd ../commons && printf remote",
    ] {
        let daemon = windows_daemon(Some(0), true).await;
        let mut policy = launch(daemon.connection().clone());
        policy.params.siblings = vec![commons.to_str().unwrap().to_owned()];
        let response = handle_with_policy(
            &ctx,
            Some(policy),
            serde_json::json!({
                "command": line, "runon": "windows", "compressed": false,
            }),
        );
        assert!(response.success, "{response:?}");
        let done = terminal(
            ctx.bash_background(),
            response.data["task_id"].as_str().unwrap(),
        )
        .await;
        assert_eq!(
            done.info.status,
            BgTaskStatus::Completed,
            "{line}: {done:?}"
        );
        let runs = exec_runs(&daemon);
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(runs[0]["command"], line);
        assert_eq!(runs[0]["platform"], "windows");
    }
}

#[tokio::test]
async fn runon_cd_system_and_missing_non_home_paths_are_allowed() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = remote_test_context(dir.path());
    let outside = tempfile::Builder::new()
        .prefix("aft-runon-system-")
        .tempdir_in("/var/tmp")
        .unwrap();
    let outside = outside.path().canonicalize().unwrap();
    let home = PathBuf::from(std::env::var_os("HOME").unwrap())
        .canonicalize()
        .unwrap();
    assert!(!outside.starts_with(home));
    assert!(aft_engine::test_bridge::search_misc::nearest_git_marker(&outside)
        .unwrap()
        .is_none());
    for target in [
        PathBuf::from("/tmp"),
        PathBuf::from("/usr"),
        outside.join("missing"),
    ] {
        let daemon = daemon(Script::Plain, "exec-remote/v1").await;
        let line = format!("cd '{}' && printf remote", target.display());
        let response = handle_with_policy(
            &ctx,
            Some(launch(daemon.connection().clone())),
            serde_json::json!({"command": line, "runon": "linux", "compressed": false}),
        );
        assert!(response.success, "{response:?}");
        let done = terminal(
            ctx.bash_background(),
            response.data["task_id"].as_str().unwrap(),
        )
        .await;
        assert_eq!(
            done.info.status,
            BgTaskStatus::Completed,
            "{target:?}: {done:?}"
        );
        let runs = exec_runs(&daemon);
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(runs[0]["command"], line);
    }
}

#[tokio::test]
async fn runon_cd_other_local_checkout_outside_home_is_refused() {
    let daemon = daemon(Script::Plain, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = remote_test_context(dir.path());
    let outside = tempfile::Builder::new()
        .prefix("aft-runon-other-repo-")
        .tempdir_in("/var/tmp")
        .unwrap();
    let outside = outside.path().canonicalize().unwrap();
    let home = PathBuf::from(std::env::var_os("HOME").unwrap())
        .canonicalize()
        .unwrap();
    assert!(!outside.starts_with(home));
    git_fixture(&outside);
    let marker = dir.path().join("must-not-run-locally");
    let response = handle_with_policy(
        &ctx,
        Some(launch(daemon.connection().clone())),
        serde_json::json!({
            "command": format!("cd '{}'; printf local-proof > '{}'", outside.join("missing").display(), marker.display()),
            "runon": "linux", "compressed": false,
        }),
    );
    assert!(response.success, "{response:?}");
    let done = terminal(
        ctx.bash_background(),
        response.data["task_id"].as_str().unwrap(),
    )
    .await;
    assert_eq!(done.info.status, BgTaskStatus::Failed, "{done:?}");
    let refusal = refusal_response(done);
    assert_eq!(refusal.data["code"], "runon_cd_outside_checkout");
    assert!(exec_runs(&daemon).is_empty(), "a refusal must never submit");
    assert!(!marker.exists(), "a refusal must never spawn locally");
}

#[tokio::test]
async fn runon_cd_outside_checkout_is_refused_before_submit() {
    let daemon = daemon(Script::Plain, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let checkout = dir.path().join("checkout");
    let sibling = dir.path().join("sibling-repo");
    fs::create_dir_all(&checkout).unwrap();
    fs::create_dir_all(&sibling).unwrap();
    git_fixture(&sibling);
    let ctx = remote_test_context(&checkout);
    let checkout = checkout.canonicalize().unwrap();
    let sibling = sibling.canonicalize().unwrap();
    let marker = checkout.join("must-not-run-locally");
    let home_target =
        PathBuf::from(std::env::var_os("HOME").unwrap()).join("aft-runon-outside-checkout");
    let cases = [
        (format!("cd '{}'", sibling.display()), sibling.clone()),
        (format!("cd '{}'", home_target.display()), home_target),
        ("cd ../sibling-repo".into(), sibling.clone()),
        (
            "cd ../sibling-repo && printf remote; printf continuation".into(),
            sibling.clone(),
        ),
        (format!("pushd '{}'", sibling.display()), sibling),
    ];
    for (line, target) in cases {
        let command = format!("{line}; printf local-proof > '{}'", marker.display());
        let response = handle_with_policy(
            &ctx,
            Some(launch(daemon.connection().clone())),
            serde_json::json!({"command": command, "runon": "linux", "compressed": false}),
        );
        assert!(response.success, "{line}: {response:?}");
        let done = terminal(
            ctx.bash_background(),
            response.data["task_id"].as_str().unwrap(),
        )
        .await;
        assert_eq!(done.info.status, BgTaskStatus::Failed, "{line}: {done:?}");
        let refusal = refusal_response(done);
        assert_eq!(
            refusal.data["code"], "runon_cd_outside_checkout",
            "{refusal:?}"
        );
        let expected = format!(
            "runon refused: the command changes into {}, which is outside the checkout the build server copies ({}); the command did not run. Set the bash tool's workdir to that folder instead of using cd.",
            target.display(), checkout.display()
        );
        assert!(
            refusal.data["message"]
                .as_str()
                .unwrap()
                .contains(&expected),
            "{line}: {refusal:?}"
        );
        assert!(
            exec_runs(&daemon).is_empty(),
            "{line}: a refused command reached the runner"
        );
        assert!(!marker.exists(), "a refused runon must never spawn locally");
    }
}

#[tokio::test]
async fn runon_cd_unknown_set_keeps_known_folder_for_definite_exits() {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap())
        .canonicalize()
        .unwrap();
    let checkout = tempfile::Builder::new()
        .prefix("aft-runon-unknown-set-")
        .tempdir_in(&home)
        .unwrap();
    let ctx = remote_test_context(checkout.path());
    fs::create_dir(checkout.path().join("nested")).unwrap();
    let marker = checkout.path().join("must-not-run-locally");
    for line in [
        "set \"$OPTS\"; cd ..",
        "set -P; set \"$OPTS\"; cd ..",
        "set -o \"$OPT\"; cd ..",
        "set \"$OPTS\"; cd nested && cd .. && cd ..",
        "set \"$OPTS\" && cd ..",
        "set \"$OPTS\" || cd ..",
    ] {
        let daemon = daemon(Script::Plain, "exec-remote/v1").await;
        let response = handle_with_policy(
            &ctx,
            Some(launch(daemon.connection().clone())),
            serde_json::json!({
                "command": format!("{line}; printf local-proof > '{}'", marker.display()), "runon": "linux", "compressed": false,
            }),
        );
        assert!(response.success, "{line}: {response:?}");
        let done = terminal(
            ctx.bash_background(),
            response.data["task_id"].as_str().unwrap(),
        )
        .await;
        assert_eq!(done.info.status, BgTaskStatus::Failed, "{line}: {done:?}");
        let refusal = refusal_response(done);
        assert_eq!(refusal.data["code"], "runon_cd_outside_checkout");
        assert!(
            refusal.data["message"]
                .as_str()
                .unwrap()
                .contains(&format!("changes into {},", home.display())),
            "{line}: {refusal:?}"
        );
        assert!(exec_runs(&daemon).is_empty(), "a refusal must never submit");
        assert!(!marker.exists(), "runon must never spawn locally");
    }
}

#[tokio::test]
async fn runon_cd_unknown_mode_requires_both_resolutions_to_be_invalid() {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap())
        .canonicalize()
        .unwrap();
    let checkout = tempfile::Builder::new()
        .prefix("aft-runon-mode-pair-")
        .tempdir_in(&home)
        .unwrap();
    let outside = tempfile::Builder::new()
        .prefix("aft-runon-mode-target-")
        .tempdir_in(&home)
        .unwrap();
    let deep = checkout.path().join("deep");
    fs::create_dir(&deep).unwrap();
    fs::create_dir_all(outside.path().join("a/b/c")).unwrap();
    fs::create_dir_all(deep.join("a/b/c")).unwrap();
    std::os::unix::fs::symlink("..", deep.join("link")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("a/b/c"), deep.join("jump")).unwrap();
    std::os::unix::fs::symlink("a/b/c", deep.join("reenter")).unwrap();
    let ctx = remote_test_context(checkout.path());
    for (line, refused) in [
        ("set \"$OPTS\"; cd link/..", false),
        ("set \"$OPTS\"; cd link/.. && cd ../..", false),
        ("set \"$OPTS\"; cd reenter/../../..", false),
        ("set \"$OPTS\"; cd reenter/../../.. && cd ../../..", false),
        ("set \"$OPTS\"; cd jump/../../..", true),
        ("set \"$OPTS\"; cd -P link/..", true),
        ("set \"$OPTS\"; cd -L link/..", false),
    ] {
        let daemon = daemon(Script::Plain, "exec-remote/v1").await;
        let response = handle_with_policy(
            &ctx,
            Some(launch(daemon.connection().clone())),
            serde_json::json!({
                "command": line, "runon": "linux", "workdir": deep, "compressed": false,
            }),
        );
        assert!(response.success, "{line}: {response:?}");
        let done = terminal(
            ctx.bash_background(),
            response.data["task_id"].as_str().unwrap(),
        )
        .await;
        if refused {
            assert_eq!(done.info.status, BgTaskStatus::Failed, "{line}: {done:?}");
            assert_eq!(
                refusal_response(done).data["code"],
                "runon_cd_outside_checkout"
            );
            assert!(exec_runs(&daemon).is_empty(), "a refusal must never submit");
        } else {
            assert_eq!(
                done.info.status,
                BgTaskStatus::Completed,
                "{line}: {done:?}"
            );
            let runs = exec_runs(&daemon);
            assert_eq!(runs.len(), 1, "{line}: {runs:?}");
            assert_eq!(runs[0]["command"], line);
        }
    }
}

#[tokio::test]
async fn runon_cd_background_jobs_preserve_foreground_directory() {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap())
        .canonicalize()
        .unwrap();
    let checkout = tempfile::Builder::new()
        .prefix("aft-runon-background-")
        .tempdir_in(&home)
        .unwrap();
    let ctx = remote_test_context(checkout.path());
    fs::create_dir(checkout.path().join("nested")).unwrap();
    fs::write(checkout.path().join("nested/source.txt"), "source\n").unwrap();
    let marker = checkout.path().join("must-not-run-locally");
    for (line, refused) in [
        ("sleep 1 & cd ..", true),
        ("cd nested & cd ..", true),
        ("cd nested && sleep 1 & cd ..", true),
        ("false && cd nested & cd ..", true),
        ("source ./script & cd ..", true),
        ("set \"$OPTS\" & cd ..", true),
        ("set -- \"$OPTS\"; sleep 1 & cd ..", true),
        ("sleep 1 & cd nested && cd ..", false),
        ("cd nested && { sleep 1 & cd ..; }", false),
        ("false & cd ..", true),
        ("sleep 1 & false && cd ..", false),
        ("cd nested; sleep 1 & cd ..", false),
    ] {
        let daemon = daemon(Script::Plain, "exec-remote/v1").await;
        let command = format!("{line}; printf local-proof > '{}'", marker.display());
        let response = handle_with_policy(
            &ctx,
            Some(launch(daemon.connection().clone())),
            serde_json::json!({
                "command": command, "runon": "linux", "compressed": false,
            }),
        );
        assert!(response.success, "{line}: {response:?}");
        let done = terminal(
            ctx.bash_background(),
            response.data["task_id"].as_str().unwrap(),
        )
        .await;
        if refused {
            assert_eq!(done.info.status, BgTaskStatus::Failed, "{line}: {done:?}");
            let refusal = refusal_response(done);
            assert_eq!(refusal.data["code"], "runon_cd_outside_checkout");
            assert!(exec_runs(&daemon).is_empty(), "a refusal must never submit");
        } else {
            assert_eq!(
                done.info.status,
                BgTaskStatus::Completed,
                "{line}: {done:?}"
            );
            let runs = exec_runs(&daemon);
            assert_eq!(runs.len(), 1, "{line}: {runs:?}");
            assert_eq!(runs[0]["command"], command);
        }
        assert!(!marker.exists(), "runon must never spawn locally");
    }
}

#[tokio::test]
async fn runon_cd_shell_physical_option_tracks_later_changes() {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap())
        .canonicalize()
        .unwrap();
    let checkout = tempfile::Builder::new()
        .prefix("aft-runon-set-mode-")
        .tempdir_in(&home)
        .unwrap();
    let deep = checkout.path().join("deep");
    fs::create_dir(&deep).unwrap();
    std::os::unix::fs::symlink("..", deep.join("link")).unwrap();
    let ctx = remote_test_context(checkout.path());
    let marker = checkout.path().join("must-not-run-locally");
    for (line, refused) in [
        ("set -P; cd link && cd ..", true),
        ("set -o physical; cd link && cd ..", true),
        ("set -P; set +P; cd link && cd ..", false),
        ("set -o physical; set +o physical; cd link && cd ..", false),
        ("set -P; cd -L link && cd ..", true),
        ("set -P; cd -L link && cd -L ..", false),
        ("set +P; cd -P link && cd ..", true),
        ("set -- -P; cd link && cd ..", false),
        ("set value -P; cd link && cd ..", false),
        ("set -eP; cd link && cd ..", true),
        ("set -P +P; cd link && cd ..", false),
        ("set +P -P; cd link && cd ..", true),
        ("false && set -P; cd link && cd ..", false),
        ("true || set -P; cd link && cd ..", false),
        ("maybe && set -P; cd link && cd ..", true),
        ("false || set -P; cd link && cd ..", true),
        ("set -P & cd link && cd ..", false),
        ("set -P; set +P & cd link && cd ..", true),
        ("source ./script; cd link && cd ..", false),
        ("set \"$OPTS\"; cd link && cd ..", false),
        ("set -P; set \"$OPTS\"; cd link && cd ..", false),
    ] {
        let daemon = daemon(Script::Plain, "exec-remote/v1").await;
        let command = format!("{line}; printf local-proof > '{}'", marker.display());
        let response = handle_with_policy(
            &ctx,
            Some(launch(daemon.connection().clone())),
            serde_json::json!({
                "command": command, "runon": "linux", "workdir": deep, "compressed": false,
            }),
        );
        assert!(response.success, "{line}: {response:?}");
        let done = terminal(
            ctx.bash_background(),
            response.data["task_id"].as_str().unwrap(),
        )
        .await;
        if refused {
            assert_eq!(done.info.status, BgTaskStatus::Failed, "{line}: {done:?}");
            let refusal = refusal_response(done);
            assert_eq!(refusal.data["code"], "runon_cd_outside_checkout");
            assert!(exec_runs(&daemon).is_empty(), "a refusal must never submit");
        } else {
            assert_eq!(
                done.info.status,
                BgTaskStatus::Completed,
                "{line}: {done:?}"
            );
            let runs = exec_runs(&daemon);
            assert_eq!(runs.len(), 1, "{line}: {runs:?}");
            assert_eq!(runs[0]["command"], command);
        }
        assert!(!marker.exists(), "runon must never spawn locally");
    }
}

#[tokio::test]
async fn runon_cd_logical_symlinks_and_physical_mode_are_distinct() {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap())
        .canonicalize()
        .unwrap();
    let checkout = tempfile::Builder::new()
        .prefix("aft-runon-logical-")
        .tempdir_in(&home)
        .unwrap();
    let deep = checkout.path().join("deep");
    fs::create_dir(&deep).unwrap();
    std::os::unix::fs::symlink("..", deep.join("link")).unwrap();
    let ctx = remote_test_context(checkout.path());
    let marker = checkout.path().join("must-not-run-locally");
    for (line, refused) in [
        ("cd link && cd ..", false),
        ("pushd link && cd ..", false),
        ("cd link/..", false),
        ("cd -L link && cd ..", false),
        ("cd -P link && cd ..", true),
        ("cd -P link/..", true),
        ("cd -P -L link && cd ..", false),
        ("cd -L -P link && cd ..", true),
        ("cd -PL link && cd ..", false),
        ("cd -LP link && cd ..", true),
        ("cd '-P' link && cd ..", true),
        ("cd -- link && cd ..", false),
        ("maybe && cd link; cd ..", false),
        ("cd link || cd .; cd ..", false),
        ("cd -P link || cd .; cd ..", true),
    ] {
        let daemon = daemon(Script::Plain, "exec-remote/v1").await;
        let command = format!("{line}; printf local-proof > '{}'", marker.display());
        let response = handle_with_policy(
            &ctx,
            Some(launch(daemon.connection().clone())),
            serde_json::json!({
                "command": command, "runon": "linux", "workdir": deep, "compressed": false,
            }),
        );
        assert!(response.success, "{line}: {response:?}");
        let done = terminal(
            ctx.bash_background(),
            response.data["task_id"].as_str().unwrap(),
        )
        .await;
        if refused {
            assert_eq!(done.info.status, BgTaskStatus::Failed, "{line}: {done:?}");
            let refusal = refusal_response(done);
            assert_eq!(refusal.data["code"], "runon_cd_outside_checkout");
            assert!(
                refusal.data["message"]
                    .as_str()
                    .unwrap()
                    .contains(&format!("changes into {},", home.display())),
                "{line}: {refusal:?}"
            );
            assert!(exec_runs(&daemon).is_empty(), "a refusal must never submit");
        } else {
            assert_eq!(
                done.info.status,
                BgTaskStatus::Completed,
                "{line}: {done:?}"
            );
            let runs = exec_runs(&daemon);
            assert_eq!(runs.len(), 1, "{line}: {runs:?}");
            assert_eq!(runs[0]["command"], command);
        }
        assert!(!marker.exists(), "runon must never spawn locally");
    }
}

#[tokio::test]
async fn runon_cd_skipped_changes_do_not_hide_checkout_exits() {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap())
        .canonicalize()
        .unwrap();
    let checkout = tempfile::Builder::new()
        .prefix("aft-runon-branches-")
        .tempdir_in(&home)
        .unwrap();
    let ctx = remote_test_context(checkout.path());
    fs::create_dir(checkout.path().join("nested")).unwrap();
    fs::write(checkout.path().join("nested/source.txt"), "source\n").unwrap();
    let marker = checkout.path().join("must-not-run-locally");
    for (line, refused) in [
        ("false && cd nested; cd ..", true),
        ("maybe && cd nested; cd ..", false),
        ("cd nested; cd ..", false),
        ("true || cd nested; cd ..", true),
        ("cd \"$X\" || cd ..", true),
        ("cd nested && cd ..", false),
        ("false || cd nested && cd ..", false),
        ("false && cd nested || cd nested && cd ..", false),
        ("false && cd ../must-not-execute", false),
        ("true || cd ..", false),
        ("cd \"$X\" && cd ..", false),
    ] {
        let daemon = daemon(Script::Plain, "exec-remote/v1").await;
        let command = format!("{line}; printf local-proof > '{}'", marker.display());
        let response = handle_with_policy(
            &ctx,
            Some(launch(daemon.connection().clone())),
            serde_json::json!({
                "command": command, "runon": "linux", "compressed": false,
            }),
        );
        assert!(response.success, "{line}: {response:?}");
        let done = terminal(
            ctx.bash_background(),
            response.data["task_id"].as_str().unwrap(),
        )
        .await;
        if refused {
            assert_eq!(done.info.status, BgTaskStatus::Failed, "{line}: {done:?}");
            let refusal = refusal_response(done);
            assert_eq!(refusal.data["code"], "runon_cd_outside_checkout");
            assert!(
                refusal.data["message"]
                    .as_str()
                    .unwrap()
                    .contains(&format!("changes into {},", home.display())),
                "{line}: {refusal:?}"
            );
            assert!(exec_runs(&daemon).is_empty(), "a refusal must never submit");
        } else {
            assert_eq!(
                done.info.status,
                BgTaskStatus::Completed,
                "{line}: {done:?}"
            );
            let runs = exec_runs(&daemon);
            assert_eq!(runs.len(), 1, "{line}: {runs:?}");
            assert_eq!(runs[0]["command"], command);
        }
        assert!(!marker.exists(), "runon must never spawn locally");
    }
}

#[tokio::test]
async fn runon_cd_sequences_use_existing_copied_directories() {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap())
        .canonicalize()
        .unwrap();
    let dir = tempfile::Builder::new()
        .prefix("aft-runon-copied-cwd-")
        .tempdir_in(&home)
        .unwrap();
    let checkout = dir.path().join("checkout");
    let sibling = dir.path().join("commons");
    for folder in [
        checkout.join("packages/a"),
        checkout.join("packages/b"),
        sibling.join("a"),
        sibling.join("b"),
    ] {
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("source.txt"), "source\n").unwrap();
    }
    git_fixture(&sibling);
    let ctx = remote_test_context(&checkout);
    let _read = grant_sibling_read(&ctx, &sibling);
    let marker = checkout.join("must-not-run-locally");
    for (line, refused) in [
        ("cd packages/a && true; cd ../b", false),
        ("pushd packages/a && true; cd ../b", false),
        ("cd ../commons/a && true; cd ../b", false),
        ("cd packages/missing && true; cd ../b", true),
        ("cd ../../outside", true),
        ("cd missing || cd ../../outside", true),
    ] {
        let daemon = daemon(Script::Plain, "exec-remote/v1").await;
        let mut policy = launch(daemon.connection().clone());
        policy.params.siblings = vec![sibling.to_str().unwrap().to_owned()];
        let command = format!("{line}; printf local-proof > '{}'", marker.display());
        let response = handle_with_policy(
            &ctx,
            Some(policy),
            serde_json::json!({"command": command, "runon": "linux", "compressed": false}),
        );
        assert!(response.success, "{line}: {response:?}");
        let done = terminal(
            ctx.bash_background(),
            response.data["task_id"].as_str().unwrap(),
        )
        .await;
        if refused {
            assert_eq!(done.info.status, BgTaskStatus::Failed, "{line}: {done:?}");
            let refusal = refusal_response(done);
            assert_eq!(refusal.data["code"], "runon_cd_outside_checkout");
            assert!(exec_runs(&daemon).is_empty(), "a refusal must never submit");
        } else {
            assert_eq!(
                done.info.status,
                BgTaskStatus::Completed,
                "{line}: {done:?}"
            );
            let runs = exec_runs(&daemon);
            assert_eq!(runs.len(), 1, "{line}: {runs:?}");
            assert_eq!(runs[0]["command"], command);
        }
        assert!(!marker.exists(), "runon must never spawn locally");
    }
}

#[tokio::test]
async fn runon_write_python_heredoc_refusal_never_submits_or_spawns() {
    let daemon = daemon(Script::Plain, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = remote_test_context(dir.path());
    let command = "python3 - <<'PY' && cargo nextest run\np = 'source.rs'\nt = 'source'\nopen(p, 'w').write(t)\nPY";
    let response = handle_with_policy(
        &ctx,
        Some(launch(daemon.connection().clone())),
        serde_json::json!({
            "command": command, "runon": "linux", "compressed": false,
        }),
    );
    assert!(response.success, "{response:?}");
    let task_id = response.data["task_id"].as_str().unwrap();
    let done = terminal(ctx.bash_background(), task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Failed, "{done:?}");
    assert_eq!(done.exit_code, None);
    assert_eq!(done.child_pid, None);
    let refusal = refusal_response(done);
    assert_eq!(refusal.data["code"], "runon_edits_checkout");
    let expected = format!("runon refused: this command writes {}, and a build-server job's changes to checkout files are not copied back. Edit it in a local call, or name it in return_paths to regenerate it remotely.", dir.path().canonicalize().unwrap().join("source.rs").display());
    assert_eq!(refusal.data["message"], expected);
    let task = ctx.bash_background().test_task(task_id).unwrap();
    let state = task.test_state().lock().unwrap();
    let persisted = serde_json::to_value(&state.metadata()).unwrap();
    assert_eq!(persisted["remote"]["pre_start_refusal"]["ran"], "none");
    assert!(persisted["remote"]["job_id"].is_null());
    assert!(persisted["remote"]["terminal"].is_null());
    let restored: PersistedTask = serde_json::from_value(persisted).unwrap();
    assert_eq!(
        remote_refusal(&restored).unwrap().code,
        "runon_edits_checkout"
    );
    assert!(
        exec_runs(&daemon).is_empty(),
        "a refused edit must never submit"
    );
    assert!(
        !dir.path().join("source.rs").exists(),
        "a refused edit must never run locally"
    );
}

#[tokio::test]
async fn runon_write_check_never_refuses_without_runon() {
    let daemon = daemon(Script::Plain, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = remote_test_context(dir.path());
    let response = handle_with_policy(
        &ctx,
        Some(launch(daemon.connection().clone())),
        serde_json::json!({
            "command": "printf source > source.rs", "compressed": false,
        }),
    );
    assert!(response.success, "{response:?}");
    let done = terminal(
        ctx.bash_background(),
        response.data["task_id"].as_str().unwrap(),
    )
    .await;
    assert!(done.remote_refusal.is_none(), "{done:?}");
    assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");
    assert!(exec_runs(&daemon).is_empty());
    assert_eq!(
        fs::read_to_string(dir.path().join("source.rs")).unwrap(),
        "source"
    );
}

#[tokio::test]
async fn runon_cd_sequential_targets_follow_the_shell_cwd() {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap())
        .canonicalize()
        .unwrap();
    let checkout = tempfile::Builder::new()
        .prefix("aft-runon-cwd-")
        .tempdir_in(&home)
        .unwrap();
    let ctx = remote_test_context(checkout.path());
    fs::create_dir(checkout.path().join("nested")).unwrap();
    let marker = checkout.path().join("must-not-run-locally");
    for (line, refused) in [
        ("cd nested && cd .. && printf remote", false),
        ("pushd nested && cd .. && printf remote", false),
        ("cd nested && cd ../.. && printf remote", true),
        ("cd \"$X\" && cd .. && printf remote", false),
        ("cd - && cd .. && printf remote", false),
        ("popd && cd .. && printf remote", false),
        ("(cd nested) && cd .. && printf remote", true),
    ] {
        let daemon = daemon(Script::Plain, "exec-remote/v1").await;
        let command = format!("{line}; printf local-proof > '{}'", marker.display());
        let response = handle_with_policy(
            &ctx,
            Some(launch(daemon.connection().clone())),
            serde_json::json!({"command": command, "runon": "linux", "compressed": false}),
        );
        assert!(response.success, "{line}: {response:?}");
        let done = terminal(
            ctx.bash_background(),
            response.data["task_id"].as_str().unwrap(),
        )
        .await;
        if refused {
            assert_eq!(done.info.status, BgTaskStatus::Failed, "{line}: {done:?}");
            let refusal = refusal_response(done);
            assert_eq!(refusal.data["code"], "runon_cd_outside_checkout");
            let expected = format!(
                "runon refused: the command changes into {}, which is outside the checkout the build server copies ({}); the command did not run. Set the bash tool's workdir to that folder instead of using cd.",
                home.display(), checkout.path().canonicalize().unwrap().display()
            );
            assert_eq!(refusal.data["message"], expected);
            assert!(exec_runs(&daemon).is_empty(), "a refusal must never submit");
        } else {
            assert_eq!(
                done.info.status,
                BgTaskStatus::Completed,
                "{line}: {done:?}"
            );
            let runs = exec_runs(&daemon);
            assert_eq!(runs.len(), 1, "{line}: {runs:?}");
            assert_eq!(runs[0]["command"], command);
        }
        assert!(!marker.exists(), "runon must never spawn locally");
    }
}

#[tokio::test]
async fn runon_cd_inside_checkout_and_unresolved_or_nested_targets_are_allowed() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = remote_test_context(dir.path());
    fs::create_dir(dir.path().join("nested")).unwrap();
    let lines = [
        "cd nested && printf remote",
        "cd ./nested/..; printf remote",
        "cd \"$OTHER_CHECKOUT\"; printf remote",
        "cd -; printf remote",
        "printf '%s' \"$(cd /outside-checkout; pwd)\"",
        "(cd /outside-checkout; printf remote)",
    ];
    for line in lines {
        let daemon = daemon(Script::Plain, "exec-remote/v1").await;
        let response = handle_with_policy(
            &ctx,
            Some(launch(daemon.connection().clone())),
            serde_json::json!({"command": line, "runon": "linux", "compressed": false}),
        );
        assert!(response.success, "{line}: {response:?}");
        let done = terminal(
            ctx.bash_background(),
            response.data["task_id"].as_str().unwrap(),
        )
        .await;
        assert_eq!(
            done.info.status,
            BgTaskStatus::Completed,
            "{line}: {done:?}"
        );
        let runs = exec_runs(&daemon);
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(runs[0]["command"], line);
    }
}

#[tokio::test]
async fn runon_signer_hint_longer_than_the_admission_time_left_surfaces_the_refusal() {
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
    // Room for one floored 5 s wait, counted from the first submission; the
    // second refusal's delay is longer than the 1 s then left.
    shorten_hinted_retry_budget(dir.path(), 6000);
    let mut clock = PausedDrainingClock::new(dir.path());
    let registry = registry();
    let task = start_runon_retry(&registry, dir.path(), daemon.connection().clone());
    let first = clock.now();
    clock.expect_sleep(first, first + 5000).await;
    clock.advance(5000);
    let done = terminal(&registry, &task).await;
    assert_eq!(done.info.status, BgTaskStatus::Failed, "{done:?}");
    assert_runon_refusal(&refusal_response(done), SIGNER_REFUSED);
    assert_eq!(exec_runs(&daemon).len(), 2);
    assert_eq!(clock.now(), first + 5000);
    assert_no_retry_sleep(&mut clock);
}

#[tokio::test]
async fn runon_operator_needed_signer_refusal_surfaces_at_once() {
    let daemon = daemon(
        Script::SignerRefused {
            refusals: u32::MAX,
            detail: "runner_sign_missing",
            retry_after_ms: None,
        },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut clock = PausedDrainingClock::new(dir.path());
    let registry = registry();
    let task = start_runon_retry(&registry, dir.path(), daemon.connection().clone());
    let done = terminal(&registry, &task).await;
    assert_eq!(done.info.status, BgTaskStatus::Failed, "{done:?}");
    assert_runon_refusal(
        &refusal_response(done),
        "runner_authentication_failed (runner_sign_missing)",
    );
    assert_eq!(exec_runs(&daemon).len(), 1);
    assert_no_retry_sleep(&mut clock);
}

#[tokio::test]
async fn runon_signer_retries_stop_after_five_resubmissions() {
    let daemon = daemon(
        Script::SignerRefused {
            refusals: u32::MAX,
            detail: "runner_sign_unavailable",
            retry_after_ms: Some(100),
        },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut clock = PausedDrainingClock::new(dir.path());
    let registry = registry();
    let task = start_runon_retry(&registry, dir.path(), daemon.connection().clone());
    let first = clock.now();
    for resubmission in 0..5 {
        let from = first + resubmission * 5000;
        clock.expect_sleep(from, from + 5000).await;
        clock.advance(5000);
    }
    let done = terminal(&registry, &task).await;
    assert_eq!(done.info.status, BgTaskStatus::Failed, "{done:?}");
    assert_runon_refusal(&refusal_response(done), SIGNER_REFUSED);
    assert_eq!(
        exec_runs(&daemon).len(),
        6,
        "one submission plus five resubmissions"
    );
    assert_no_retry_sleep(&mut clock);
}

/// A retry wait that ends after the job's admission time has run out sends
/// nothing: the clock moves past the deadline while the worker waits.
#[tokio::test]
async fn runon_signer_retry_that_wakes_after_the_deadline_surfaces_the_refusal() {
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
    shorten_hinted_retry_budget(dir.path(), 6000);
    let mut clock = PausedDrainingClock::new(dir.path());
    let registry = registry();
    let task = start_runon_retry(&registry, dir.path(), daemon.connection().clone());
    let first = clock.now();
    clock.expect_sleep(first, first + 5000).await;
    // The worker wakes 10 s after the first submission, 4 s past its deadline.
    clock.advance(10_000);
    let done = terminal(&registry, &task).await;
    assert_eq!(done.info.status, BgTaskStatus::Failed, "{done:?}");
    assert_runon_refusal(&refusal_response(done), SIGNER_REFUSED);
    assert_eq!(exec_runs(&daemon).len(), 1, "no request after the deadline");
    assert_no_retry_sleep(&mut clock);
}

/// A zero hint on a job whose admission time is already used up sends
/// nothing either: the minimum retry delay exceeds the admission time left.
#[tokio::test]
async fn runon_signer_zero_hint_after_the_deadline_surfaces_the_refusal() {
    let daemon = daemon(
        Script::SignerRefused {
            refusals: u32::MAX,
            detail: "runner_sign_unavailable",
            retry_after_ms: Some(0),
        },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    // No admission time at all: the deadline is the first submission itself.
    shorten_hinted_retry_budget(dir.path(), 0);
    let mut clock = PausedDrainingClock::new(dir.path());
    let registry = registry();
    let task = start_runon_retry(&registry, dir.path(), daemon.connection().clone());
    let done = terminal(&registry, &task).await;
    assert_eq!(done.info.status, BgTaskStatus::Failed, "{done:?}");
    assert_runon_refusal(&refusal_response(done), SIGNER_REFUSED);
    assert_eq!(exec_runs(&daemon).len(), 1, "no request after the deadline");
    assert_no_retry_sleep(&mut clock);
}

/// The owner pauses a worker whose `runon` task waits out a signer refusal's
/// retry hint. The worker wakes, sees the cancellation, and must settle the
/// pause itself without resubmitting, as a draining retry does.
#[tokio::test]
async fn bash_kill_scope_settles_while_signer_retry_waits() {
    let daemon = daemon(signer_retry_script(u32::MAX), "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let mut clock = PausedDrainingClock::new(dir.path());
    let registry = registry();
    let scope = owner_scope("signer-wait");
    let task = start_scoped_runon_retry(&registry, dir.path(), daemon.connection().clone(), &scope);
    let first = clock.now();
    clock.expect_sleep(first, first + 5000).await;
    assert_pause_settles_promptly(&registry, start_kill_scope(&registry, &scope), &task).await;
    // The retry clock never moved: cancellation, not the hint, woke the worker.
    assert_eq!(clock.now(), first);
    remote_worker_exited(&task).await;
    assert_paused(&registry, &task);
    // Give a worker that wrongly kept waiting its chance to resubmit.
    clock.advance(5000);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(exec_runs(&daemon).len(), 1, "never resubmitted");
    assert_signer_retry_pause_saved(&registry.test_task(&task).unwrap().paths().json);
}

/// The pause lands after the signer-retry wait ended but before the worker
/// records the resubmission. Its recheck there must settle the pause.
#[tokio::test]
async fn bash_kill_scope_settles_when_paused_just_before_signer_resubmit() {
    let daemon = daemon(signer_retry_script(1), "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let mut clock = PausedDrainingClock::new(dir.path());
    let registry = registry();
    let scope = owner_scope("signer-resubmit");
    let task = start_scoped_runon_retry(&registry, dir.path(), daemon.connection().clone(), &scope);
    let first = clock.now();
    clock.expect_sleep(first, first + 5000).await;
    let mut gate = HeldDrainingGate::arm(dir.path(), DrainingGate::BeforeHintedResubmit);
    clock.advance(5000);
    gate.reached().await;
    let operation = start_kill_scope(&registry, &scope);
    pause_recorded(&registry, &task).await;
    gate.release();
    assert_pause_settles_promptly(&registry, operation, &task).await;
    assert_eq!(clock.now(), first + 5000);
    remote_worker_exited(&task).await;
    assert_paused(&registry, &task);
    assert_eq!(exec_runs(&daemon).len(), 1, "never resubmitted");
    assert_signer_retry_pause_saved(&registry.test_task(&task).unwrap().paths().json);
}

/// A signer-refusal retry is not resumable: its wait lives only in the worker,
/// so a daemon that stops during the wait saves just the runner's refusal.
/// After a module restart, the owner pauses that saved task. Recovering it
/// for the pause must neither resubmit it nor leave the pause unsettled. The
/// scope's saved local command keeps the pause busy long enough for a worker
/// started during recovery to reach the runner.
#[tokio::test]
async fn bash_kill_scope_never_resubmits_a_saved_signer_refusal() {
    let daemon = aft_engine::test_bridge::bash_remote::remote_wire::daemon_with_clients(
        signer_retry_script(1),
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
    let scope = owner_scope("saved-signer-refusal");
    let original = registry();
    let task_id =
        start_scoped_runon_retry(&original, dir.path(), daemon.connection().clone(), &scope);
    clock.expect_event(DrainingClockEvent::Stopped).await;
    remote_worker_exited(&task_id).await;
    let json = original.test_task(&task_id).unwrap().paths().json.clone();
    // What survives the stop: the refusal, and no saved retry wait to resume.
    let saved = read_task(&json).unwrap();
    assert!(!saved.status.is_terminal(), "{saved:?}");
    let remote = saved.remote.as_ref().unwrap();
    assert_eq!(
        remote
            .terminal
            .as_ref()
            .and_then(|refusal| refusal.ran.clone()),
        Some(Ran::None),
        "{saved:?}"
    );
    let retry = remote.retry.as_ref().unwrap();
    assert_eq!(
        (retry.first_refusal_at_ms(), retry.next_retry_at_ms()),
        (None, None),
        "{saved:?}"
    );
    original.detach();
    let (local_task, local_pid) = start_saved_term_ignoring_task(dir.path(), &scope).await;
    // The hint has passed by the time the owner pauses the restarted module.
    clock.advance(5000);
    let reply = kill_scope_with_saved_tasks(dir.path(), &scope).await;
    assert_eq!(
        exec_runs(&daemon).len(),
        1,
        "the paused signer refusal was resubmitted: {reply:?}"
    );
    assert_saved_pause_reply(&reply, &task_id, &local_task, local_pid).await;
    assert_signer_retry_pause_saved(&json);
    assert_eq!(exec_runs(&daemon).len(), 1, "never resubmitted");
}

/// The runner refuses a job in a way AFT does not retry, and the owner's pause
/// lands after the refusal arrived but before the worker reports it. The
/// worker's report cannot replace the pause's terminal state, and it must
/// still settle the pause: the runner refused the job, so nothing runs.
#[tokio::test]
async fn bash_kill_scope_settles_when_paused_as_a_refusal_is_reported() {
    let daemon = daemon(
        Script::SignerRefused {
            refusals: u32::MAX,
            detail: "runner_sign_missing",
            retry_after_ms: None,
        },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut clock = PausedDrainingClock::new(dir.path());
    let registry = registry();
    let scope = owner_scope("refusal-report");
    let mut gate = HeldDrainingGate::arm(dir.path(), DrainingGate::BeforeRefusalReport);
    let task = start_scoped_runon_retry(&registry, dir.path(), daemon.connection().clone(), &scope);
    gate.reached().await;
    let operation = start_kill_scope(&registry, &scope);
    pause_recorded(&registry, &task).await;
    gate.release();
    assert_pause_settles_promptly(&registry, operation, &task).await;
    remote_worker_exited(&task).await;
    assert_paused(&registry, &task);
    assert_eq!(exec_runs(&daemon).len(), 1, "never resubmitted");
    assert_refused_pause_saved(
        &registry.test_task(&task).unwrap().paths().json,
        "runner_sign_missing",
    );
    assert_no_retry_sleep(&mut clock);
}

/// A signer refusal's resubmission fails without reaching the runner, and the
/// owner's pause lands before the worker reports that. The worker must still
/// settle the pause: no submission of the command is left that could run.
#[tokio::test]
async fn bash_kill_scope_settles_when_paused_as_a_signer_resubmit_fails_unsent() {
    let daemon = daemon(signer_retry_script(1), "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let mut clock = PausedDrainingClock::new(dir.path());
    let registry = registry();
    let scope = owner_scope("signer-unsent");
    refuse_next_resubmission(dir.path());
    let mut gate = HeldDrainingGate::arm(dir.path(), DrainingGate::BeforeNoStartRefusal);
    let task = start_scoped_runon_retry(&registry, dir.path(), daemon.connection().clone(), &scope);
    let first = clock.now();
    clock.expect_sleep(first, first + 5000).await;
    clock.advance(5000);
    gate.reached().await;
    let operation = start_kill_scope(&registry, &scope);
    pause_recorded(&registry, &task).await;
    gate.release();
    assert_pause_settles_promptly(&registry, operation, &task).await;
    remote_worker_exited(&task).await;
    assert_paused(&registry, &task);
    assert_eq!(exec_runs(&daemon).len(), 1, "never resubmitted");
    assert_unsent_pause_saved(&registry.test_task(&task).unwrap().paths().json);
    assert_no_retry_sleep(&mut clock);
}

/// The same for a draining retry: its resubmission fails without reaching the
/// runner, and the pause lands before the worker reports that.
#[tokio::test]
async fn bash_kill_scope_settles_when_paused_as_a_draining_resubmit_fails_unsent() {
    let daemon = daemon(
        Script::Draining {
            refusals: 1,
            retry_after_ms: Some(200),
        },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut clock = PausedDrainingClock::new(dir.path());
    let registry = registry();
    let scope = owner_scope("draining-unsent");
    refuse_next_resubmission(dir.path());
    let mut gate = HeldDrainingGate::arm(dir.path(), DrainingGate::BeforeNoStartRefusal);
    let task = start_scoped_runon_retry(&registry, dir.path(), daemon.connection().clone(), &scope);
    let first = clock.now();
    clock.expect_sleep(first, first + 1000).await;
    clock.advance(5000);
    gate.reached().await;
    let operation = start_kill_scope(&registry, &scope);
    pause_recorded(&registry, &task).await;
    gate.release();
    assert_pause_settles_promptly(&registry, operation, &task).await;
    remote_worker_exited(&task).await;
    assert_paused(&registry, &task);
    assert_eq!(exec_runs(&daemon).len(), 1, "never resubmitted");
    assert_unsent_pause_saved(&registry.test_task(&task).unwrap().paths().json);
    assert_no_retry_sleep(&mut clock);
}

#[tokio::test]
async fn bash_kill_scope_runon_cancels_remote_job_and_releases_local_task() {
    assert_bash_kill_scope_runon_cancel(Script::Cancel, "sent").await;
}

#[tokio::test]
async fn bash_kill_scope_runon_cancel_failure_still_kills_local_task() {
    assert_bash_kill_scope_runon_cancel(Script::CancelRefused, "failed:").await;
}

#[tokio::test]
async fn bash_kill_scope_cancel_ack_is_local_stop_not_remote_proof() {
    assert_bash_kill_scope_runon_cancel(
        Script::CancelTerminal {
            terminal: cancel_unknown_terminal,
            stderr: "runner monitoring: outcome_unknown error=cancelled unit already exited",
        },
        "sent",
    )
    .await;
}

async fn assert_bash_kill_scope_runon_cancel(script: Script, expected: &str) {
    let daemon = daemon(script, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let scope = subc_protocol::scope::ScopeStamp::new(
        subc_protocol::Principal::Reserved {
            module_id: "prefrontal-core".into(),
        },
        "worker",
        1,
        subc_protocol::scope::ScopeKind::Worker,
    );
    let task = reg::with_scope_spawn(Some(scope.clone()), || {
        let mut launch = launch(daemon.connection().clone());
        launch.explicit_runon = true;
        start_with_launch(
            &registry,
            dir.path(),
            launch,
            crate::bash_background::HardKill::After(Duration::from_secs(30)),
        )
    });
    let job = wait_for("runner acceptance before pause", || {
        registry
            .status(&task, "session", None, None, 0)
            .unwrap()
            .remote_job_id
    })
    .await;
    let paused_registry = registry.clone();
    let reply = tokio::task::spawn_blocking(move || {
        crate::commands::bash_kill_scope::handle_bash_kill_scope(
            serde_json::json!({"scope":scope}),
            Some(&scope.owner),
            &[paused_registry],
        )
    })
    .await
    .unwrap();
    assert!(reply.success, "{reply:?}");
    assert_eq!(reply.data["killed"][0]["remote"]["job_id"], job);
    assert!(
        reply.data["killed"][0]["remote"]["cancel"]
            .as_str()
            .unwrap()
            .starts_with(expected),
        "{reply:?}"
    );
    if expected == "failed:" {
        assert!(
            reply.data["killed"][0]["remote"]["cancel"]
                .as_str()
                .unwrap()
                .contains("runner unavailable"),
            "{reply:?}"
        );
    }
    assert_eq!(
        daemon
            .log()
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, body)| body["method"] == "exec.cancel")
            .count(),
        1
    );
    let done = registry.status(&task, "session", None, None, 0).unwrap();
    assert_eq!(done.info.status, BgTaskStatus::Killed);
    assert_eq!(done.info.status_reason.as_deref(), Some("paused_by_owner"));
    // Owner pause stops the local task after the cancel acknowledgement; it
    // does not assert that the runner supplied a terminal or proved process exit.
    let saved = read_task(&registry.test_task(&task).unwrap().paths().json).unwrap();
    assert!(saved.remote.unwrap().terminal.is_none());
}

/// The scope's owner calls `bash.kill_scope` while the worker of an accepted
/// job in that scope is reconnecting after the runner's route closed. The worker sends the cancel
/// once it is connected again, and that settles the pause. From then on the
/// worker must stop: no further reconnect or `exec.cancel`, and the recorded
/// `sent` must still stand after the whole recovery deadline has passed.
#[tokio::test]
async fn bash_kill_scope_during_reconnect_settles_once_and_stops_the_worker() {
    let daemon = aft_engine::test_bridge::bash_remote::remote_wire::outage_daemon().await;
    let dir = tempfile::tempdir().unwrap();
    let clock = PausedReattachClock::new(dir.path());
    let registry = registry();
    let scope = subc_protocol::scope::ScopeStamp::new(
        subc_protocol::Principal::Reserved {
            module_id: "prefrontal-core".into(),
        },
        "worker",
        1,
        subc_protocol::scope::ScopeKind::Worker,
    );
    let task = reg::with_scope_spawn(Some(scope.clone()), || {
        let mut launch = launch(daemon.connection().clone());
        launch.explicit_runon = true;
        start_with_launch(
            &registry,
            dir.path(),
            launch,
            crate::bash_background::HardKill::After(Duration::from_secs(30)),
        )
    });
    // The route closed after acceptance and the runner is away, so the worker
    // is reconnecting. Hold one reconnect open and call `bash.kill_scope` as
    // the scope's owner during it.
    daemon.after_refused_connects(1).await;
    daemon.hold_next_connect();
    daemon.after_held_connect().await;
    let pausing = registry.clone();
    let pause_scope = scope.clone();
    let operation = tokio::task::spawn_blocking(move || {
        crate::commands::bash_kill_scope::handle_bash_kill_scope(
            serde_json::json!({"scope":pause_scope}),
            Some(&pause_scope.owner),
            &[pausing],
        )
    });
    wait_for("the pause must record its cancellation intent", || {
        registry
            .test_task(&task)
            .unwrap()
            .test_state()
            .lock()
            .unwrap()
            .metadata()
            .remote
            .as_ref()
            .unwrap()
            .cancel_requested
            .then_some(())
    })
    .await;
    // The runner is back when the held reconnect completes, so the worker's
    // `exec.cancel` reaches it and the pause records `sent`.
    daemon.restore();
    daemon.release_held_connect();
    let reply = tokio::time::timeout(Duration::from_secs(30), operation)
        .await
        .expect("the pause must settle once the cancel is sent")
        .unwrap();
    assert!(reply.success, "{reply:?}");
    assert_eq!(
        reply.data["killed"][0]["remote"]["cancel"], "sent",
        "{reply:?}"
    );
    let connects_at_settlement = *daemon.connects().borrow();
    // Pass the job's own limits plus grace: a worker still trying to re-attach
    // gives up here and records its recovery failure.
    clock.advance(Duration::from_secs(12 * 60));
    remote_worker_exited(&task).await;
    assert_eq!(
        daemon.methods("exec.cancel").len(),
        1,
        "the settled pause must not send the cancel again"
    );
    assert_eq!(
        *daemon.connects().borrow(),
        connects_at_settlement,
        "the worker must not reconnect after the pause settled"
    );
    assert!(daemon.methods("exec.attach").is_empty());
    assert_eq!(daemon.methods("exec.run").len(), 1, "never resubmitted");
    let recorded = registry
        .test_task(&task)
        .unwrap()
        .test_state()
        .lock()
        .unwrap()
        .metadata()
        .remote
        .as_ref()
        .unwrap()
        .owner_pause_cancel
        .clone();
    assert_eq!(recorded.as_deref(), Some("sent"));
    let done = registry.status(&task, "session", None, None, 0).unwrap();
    assert_eq!(done.info.status, BgTaskStatus::Killed);
    assert_eq!(done.info.status_reason.as_deref(), Some("paused_by_owner"));
    drop(clock);
}

#[tokio::test]
async fn bash_kill_scope_unaccepted_tasks_share_one_deadline_and_local_kills_are_first() {
    use crate::commands::bash_kill_scope::{
        handle_bash_kill_scope, with_remote_pause_observer, RemotePauseEvent,
    };

    let dirs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
    // Prepare repositories before either remote worker connects: synchronous
    // Git setup must not starve the fake daemon's authentication handshake.
    for dir in &dirs {
        ensure_git_fixture(dir.path());
    }
    let first = daemon(Script::GatedRoute, "exec-remote/v1").await;
    let second = daemon(Script::GatedRoute, "exec-remote/v1").await;
    let registries = [registry(), registry()];
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../../aft/tests/fixtures/bash-kill-scope/wire.json"
    ))
    .unwrap();
    let scope: subc_protocol::scope::ScopeStamp =
        serde_json::from_value(fixture["unaccepted"]["request"]["params"]["scope"].clone())
            .unwrap();
    let mut tasks = Vec::new();
    for (index, connection) in [first.connection().clone(), second.connection().clone()]
        .into_iter()
        .enumerate()
    {
        let task = reg::with_scope_spawn(Some(scope.clone()), || {
            let mut launch = launch(connection);
            launch.explicit_runon = true;
            start_with_launch(
                &registries[index],
                dirs[index].path(),
                launch,
                crate::bash_background::HardKill::After(Duration::from_secs(30)),
            )
        });
        tasks.push(task);
    }
    for daemon in [&first, &second] {
        wait_for("remote route setup must be in flight", || {
            daemon
                .log()
                .lock()
                .unwrap()
                .iter()
                .any(|(_, body)| body["op"] == "route.open")
                .then_some(())
        })
        .await;
    }
    let locals = registries
        .iter()
        .zip(&dirs)
        .map(|(registry, dir)| {
            let task = reg::with_scope_spawn(Some(scope.clone()), || {
                registry.spawn(
                    SpawnPlan::Unsandboxed,
                    "sleep 30",
                    "local".into(),
                    dir.path().into(),
                    HashMap::new(),
                    crate::bash_background::HardKill::After(Duration::from_secs(30)),
                    None,
                    dir.path().into(),
                    10,
                    true,
                    false,
                    Some(dir.path().into()),
                )
            })
            .unwrap();
            (registry.clone(), task)
        })
        .collect::<Vec<_>>();
    let pausing = registries.clone();
    let operation = tokio::task::spawn_blocking(move || {
        let (observed, events) = std::sync::mpsc::channel();
        let reply = with_remote_pause_observer(
            move |event| {
                if matches!(event, RemotePauseEvent::Wait(_)) {
                    for (registry, task) in &locals {
                        assert_eq!(
                            registry.status(task, "local", None, None, 0).unwrap().info.status,
                            BgTaskStatus::Killed,
                            "all local tasks must be killed before waiting on any runner"
                        );
                    }
                }
                observed.send(event).unwrap();
            },
            || handle_bash_kill_scope(serde_json::json!({"scope":scope}), Some(&scope.owner), &pausing),
        );
        (reply, events.into_iter().collect::<Vec<_>>())
    });
    // The observed wait deadlines, not the suite's scheduling, prove that
    // both runners share one window. This timeout only catches a stuck call.
    let (reply, events) = tokio::time::timeout(Duration::from_secs(60), operation)
        .await
        .expect("scope pause must settle")
        .unwrap();
    assert!(matches!(
        events.as_slice(),
        [RemotePauseEvent::Deadline(deadline), RemotePauseEvent::Wait(first), RemotePauseEvent::Wait(second)]
            if first == deadline && second == deadline
    ));
    assert!(reply.success, "{reply:?}");
    assert_eq!(reply.data["killed"].as_array().unwrap().len(), 4);
    for (index, task) in tasks.iter().enumerate() {
        let entry = reply.data["killed"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["task_id"] == *task)
            .unwrap();
        assert_eq!(
            entry["remote"],
            fixture["unaccepted"]["reply"]["data"]["killed"][0]["remote"]
        );
        assert_eq!(
            registries[index]
                .status(&task, "session", None, None, 0)
                .unwrap()
                .info
                .status,
            BgTaskStatus::Killed
        );
    }
    first.acceptance_gate().notify_one();
    second.acceptance_gate().notify_one();
    for (registry, task) in registries.iter().zip(&tasks) {
        wait_for(
            "paused submission must be withdrawn after route setup",
            || {
                registry
                    .test_task(task)
                    .unwrap()
                    .test_state()
                    .lock()
                    .unwrap()
                    .metadata()
                    .remote
                    .as_ref()
                    .unwrap()
                    .owner_pause_cancel
                    .clone()
            },
        )
        .await;
    }
    assert!(exec_runs(&first).is_empty());
    assert!(exec_runs(&second).is_empty());
}

#[tokio::test]
async fn bash_kill_scope_cancels_submission_after_pre_acceptance_pause() {
    let daemon = daemon(Script::GatedAcceptance, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../../aft/tests/fixtures/bash-kill-scope/wire.json"
    ))
    .unwrap();
    let case = &fixture["withdrawn_before_acceptance"];
    let scope: subc_protocol::scope::ScopeStamp =
        serde_json::from_value(case["request"]["params"]["scope"].clone()).unwrap();
    let task = reg::with_scope_spawn(Some(scope.clone()), || {
        let mut launch = launch(daemon.connection().clone());
        launch.explicit_runon = true;
        start_with_launch(
            &registry,
            dir.path(),
            launch,
            crate::bash_background::HardKill::After(Duration::from_secs(30)),
        )
    });
    daemon.wait_for_requests("exec.run", 1).await;
    let pausing = registry.clone();
    let operation = tokio::task::spawn_blocking(move || {
        crate::commands::bash_kill_scope::handle_bash_kill_scope(
            serde_json::json!({"scope":scope}),
            Some(&scope.owner),
            &[pausing],
        )
    });
    wait_for("owner pause intent must precede acceptance", || {
        registry
            .test_task(&task)
            .unwrap()
            .test_state()
            .lock()
            .unwrap()
            .metadata()
            .remote
            .as_ref()
            .unwrap()
            .cancel_requested
            .then_some(())
    })
    .await;
    // Detaching a caller does not cancel the job. Retain the observation until
    // Accepted supplies an id, then send a real exec.cancel for the owner pause.
    assert!(!daemon
        .log()
        .lock()
        .unwrap()
        .iter()
        .any(|(header, _)| header.ty == subc_protocol::FrameType::Cancel));
    assert!(exec_cancels(&daemon).is_empty());
    daemon.acceptance_gate().notify_one();
    let reply = tokio::time::timeout(Duration::from_secs(2), operation)
        .await
        .expect("a real cancellation must settle after acceptance")
        .unwrap();
    assert!(reply.success, "{reply:?}");
    let mut expected = case["reply"]["data"].clone();
    expected["killed"][0]["task_id"] = serde_json::json!(task);
    expected["killed"][0]["remote"]["job_id"] = serde_json::json!(id());
    assert_eq!(reply.data, expected);
    let cancels = exec_cancels(&daemon);
    assert_eq!(cancels.len(), 1);
    assert_eq!(cancels[0]["job_id"], id().to_string());
}

#[tokio::test]
async fn runon_return_paths_side_worktree_uses_copied_checkout_for_submit_and_completion() {
    let temp = tempfile::tempdir().unwrap();
    let main = temp.path().join("main");
    git_fixture(&main);
    let main = main.canonicalize().unwrap();
    let linked = temp.path().join("linked");
    add_git_worktree(&main, &linked, "returned-files");
    let linked = linked.canonicalize().unwrap();
    assert!(!linked.starts_with(&main));
    assert!(linked.join(".git").is_file());
    let ctx = restarted_context(&main);

    // Both checkouts use the same project context. A subdirectory cwd must
    // select its checkout's top level, not the session root or the cwd itself.
    for checkout in [&main, &linked] {
        let cwd = checkout.join("build");
        fs::create_dir(&cwd).unwrap();
        let daemon = return_daemon("failed-missing").await;
        let mut call = return_call(&["dist/hello.txt", "dist/absent.txt"]);
        call["workdir"] = serde_json::json!(cwd);
        let (reply, _) = orchestrated_reply(&ctx, &daemon, call).await;
        assert!(reply.success, "{reply:?}");
        let runs = exec_runs(&daemon);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0]["workspace_key"], checkout.to_str().unwrap());
        assert_eq!(runs[0]["repository_root"], main.to_str().unwrap());
        assert_eq!(runs[0]["cwd"], cwd.to_str().unwrap());
        assert_eq!(fs::read(cwd.join("dist/hello.txt")).unwrap(), b"hello");
        assert!(!cwd.join("dist/absent.txt").exists());
        let task_id = reply.data["task_id"].as_str().unwrap();
        let saved = read_task(&ctx.bash_background().test_task(task_id).unwrap().paths().json).unwrap();
        let report = saved.remote.unwrap().returned_files.unwrap();
        assert_eq!(
            report[0].checkout_path.as_deref(),
            Some("build/dist/hello.txt")
        );
        assert_eq!(
            report[0].outcome,
            aft_engine::test_bridge::bash_remote::ReturnOutcome::Returned { bytes: 5 }
        );
        let mut refused = return_call(&["../../outside.txt"]);
        refused["workdir"] = serde_json::json!(cwd);
        let response = crate::bash_background::with_remote_policy(
            Some(launch(daemon.connection().clone())),
            || crate::commands::bash::handle(&orchestrated_request(refused), &ctx),
        );
        assert!(!response.success, "{response:?}");
        assert_eq!(response.data["code"], "return_paths_refused");
        assert_eq!(response.data["message"], "return_paths refused: return_paths path ../../outside.txt must not use ..; the command did not run");

        if checkout == &linked {
            // This points back into the session project: project containment
            // alone would incorrectly allow a return outside the copied tree.
            std::os::unix::fs::symlink(&main, cwd.join("link-to-main")).unwrap();
            let mut refused = return_call(&["link-to-main/escaped.txt"]);
            refused["workdir"] = serde_json::json!(cwd);
            let response = crate::bash_background::with_remote_policy(
                Some(launch(daemon.connection().clone())),
                || crate::commands::bash::handle(&orchestrated_request(refused), &ctx),
            );
            assert!(!response.success, "{response:?}");
            assert_eq!(response.data["code"], "return_paths_refused");
            assert_eq!(response.data["message"], "return_paths refused: link-to-main/escaped.txt resolves outside the project; the command did not run");
        }
        assert_eq!(exec_runs(&daemon).len(), 1, "refused calls must not submit");
        assert!(!main.join("escaped.txt").exists());
    }
    assert!(!temp.path().join("outside.txt").exists());
}

#[tokio::test]
async fn runon_return_paths_places_verified_files_and_lists_every_path() {
    // One path returned, one missing on the server, in one job.
    let daemon = return_daemon("failed-missing").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    let (reply, _) = orchestrated_reply(
        &ctx,
        &daemon,
        return_call(&["dist/hello.txt", "dist/absent.txt"]),
    )
    .await;
    assert!(reply.success, "{reply:?}");
    let output = reply.data["output"].as_str().unwrap();
    let report = "Files requested back from the server (return_paths):\n  \
                  dist/hello.txt: returned (5 bytes)\n  \
                  dist/absent.txt: not returned (no such file on the server)";
    assert!(output.contains(report), "{output}");
    // The job id stays in the result header, with the copy-back list below.
    let header = output.lines().next().unwrap();
    assert!(header.contains(&format!("job {}", id())), "{output}");
    assert!(
        output.find(&format!("job {}", id())).unwrap() < output.find(report).unwrap(),
        "{output}"
    );
    assert_eq!(
        fs::read(dir.path().join("dist/hello.txt")).unwrap(),
        b"hello"
    );
    assert!(!dir.path().join("dist/absent.txt").exists());
    let leftovers: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".aft-return-")
        })
        .collect();
    assert!(leftovers.is_empty(), "staging files left: {leftovers:?}");
    // The request names the paths and always the cap.
    let runs = exec_runs(&daemon);
    assert_eq!(runs.len(), 1);
    assert_eq!(
        runs[0]["return_paths"],
        serde_json::json!(["dist/hello.txt", "dist/absent.txt"])
    );
    assert_eq!(runs[0]["return_chunked"], true);
    assert_eq!(runs[0]["return_max_bytes"], 10_485_760);
    // The completion notice prints the same saved report.
    let task_id = reply.data["task_id"].as_str().unwrap();
    let saved = read_task(&ctx.bash_background().test_task(task_id).unwrap().paths().json).unwrap();
    assert!(
        aft_engine::test_bridge::bash_remote::remote_report(&saved).unwrap().starts_with(report),
        "{saved:?}"
    );
}

#[tokio::test]
async fn runon_uncounted_expired_replay_reports_expired_after_bounded_recovery() {
    let daemon = aft_engine::test_bridge::bash_remote::remote_wire::daemon_with_capabilities(
        Script::MotorExpiredReplay,
        RETURN_CLAIMS,
        1,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    let (reply, _) = orchestrated_reply(
        &ctx,
        &daemon,
        return_call(&["dist/app.bin", "dist/absent.txt"]),
    )
    .await;
    assert!(reply.success, "{reply:?}");
    let output = reply.data["output"].as_str().unwrap();
    assert!(
        output.contains(
            "  dist/app.bin: not returned (the server no longer keeps this job's files)\n  \
             dist/absent.txt: not returned (no such file on the server)"
        ),
        "{output}"
    );
    assert!(output.contains("output lost"), "{output}");
    assert!(!dir.path().join("dist").exists());
    let attaches = daemon
        .log()
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, body)| body["method"] == "exec.attach")
        .count();
    assert_eq!(attaches, 3, "uncounted gaps require bounded recovery");
}

#[tokio::test]
async fn runon_return_paths_digest_mismatch_keeps_the_existing_file() {
    let daemon = return_daemon("refuse-digest-mismatch").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    fs::create_dir(dir.path().join("dist")).unwrap();
    fs::write(dir.path().join("dist/app.bin"), b"older build").unwrap();
    let (reply, _) = orchestrated_reply(&ctx, &daemon, return_call(&["dist/app.bin"])).await;
    assert!(reply.success, "{reply:?}");
    let output = reply.data["output"].as_str().unwrap();
    assert!(
        output.contains(
            "  dist/app.bin: not returned (the copy failed verification (digest_mismatch); nothing was written)"
        ),
        "{output}"
    );
    assert_eq!(
        fs::read(dir.path().join("dist/app.bin")).unwrap(),
        b"older build"
    );
}

#[tokio::test]
async fn runon_return_paths_without_the_return_capability_refuses_before_submit() {
    let daemon = daemon(Script::Plain, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    let marker = dir.path().join("local-spawn-proof");
    let mut call = return_call(&["dist/app.bin"]);
    call["command"] = serde_json::json!(format!("touch '{}'", marker.display()));
    let (reply, _) = orchestrated_reply(&ctx, &daemon, call).await;
    assert!(!reply.success, "{reply:?}");
    let message = reply.data["message"].as_str().unwrap();
    assert!(
        message.contains(
            "return_paths is unavailable: the build server doesn't offer copying files back yet (missing exec-remote-return/v2)"
        ),
        "{message}"
    );
    assert!(exec_runs(&daemon).is_empty(), "nothing may be submitted");
    assert!(!marker.exists(), "a refused runon never runs locally");
}

#[test]
fn runon_return_paths_outside_the_project_are_refused_before_anything_runs() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
    let marker = dir.path().join("local-spawn-proof");
    let cases: [(serde_json::Value, Option<&str>, &str); 4] = [
        (
            serde_json::json!(["../outside.txt"]),
            Some("linux"),
            "must not use ..",
        ),
        (
            serde_json::json!(["link/out.txt"]),
            Some("linux"),
            "return_paths refused: link/out.txt",
        ),
        (
            serde_json::json!([outside.path().join("x").display().to_string()]),
            Some("linux"),
            "must be relative to workdir",
        ),
        (
            serde_json::json!(["dist/app.bin"]),
            None,
            "return_paths needs runon",
        ),
    ];
    for (paths, runon, expected) in cases {
        let mut params = serde_json::json!({
            "command": format!("touch '{}'", marker.display()),
            "workdir": dir.path(),
            "return_paths": paths,
        });
        if let Some(runon) = runon {
            params["runon"] = serde_json::json!(runon);
        }
        let request = orchestrated_request(params);
        let response = crate::bash_background::with_remote_policy(
            Some(launch(dir.path().join("no-daemon"))),
            || crate::commands::bash::handle(&request, &ctx),
        );
        assert!(!response.success, "{response:?}");
        assert_eq!(
            response.data["code"], "return_paths_refused",
            "{response:?}"
        );
        let message = response.data["message"].as_str().unwrap();
        assert!(message.contains(expected), "{expected}: {message}");
    }
    assert!(!marker.exists(), "a refused call never runs");
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn runon_return_paths_copied_files_are_not_listed_as_left_on_the_server() {
    // The command runs in `sub/`, so the runner names its files relative to
    // the checkout (`sub/dist/hello.txt`) while return_paths names them
    // relative to the workdir (`dist/hello.txt`).
    let daemon = aft_engine::test_bridge::bash_remote::remote_wire::daemon_with_capabilities(
        Script::ReturnVectorReporting(
            "failed-missing",
            &["sub/dist/hello.txt", "sub/dist/absent.txt", "sub/other.txt"],
        ),
        RETURN_CLAIMS,
        1,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    let workdir = dir.path().join("sub");
    fs::create_dir(&workdir).unwrap();
    let mut call = return_call(&["dist/hello.txt", "dist/absent.txt"]);
    call["workdir"] = serde_json::json!(workdir);
    let (reply, _) = orchestrated_reply(&ctx, &daemon, call).await;
    assert!(reply.success, "{reply:?}");
    assert_eq!(fs::read(workdir.join("dist/hello.txt")).unwrap(), b"hello");
    let output = reply.data["output"].as_str().unwrap();
    assert!(
        output.contains(
            "These files changed on the server and were NOT copied back:\n  \
             sub/dist/absent.txt\n  sub/other.txt"
        ),
        "{output}"
    );
    // ck-motor sends no separate untracked list; its changed list holds them,
    // and an absent untracked report stays silent.
    assert!(!output.contains("untracked files"), "{output}");
    assert!(!output.contains("sub/dist/hello.txt"), "{output}");
}

#[test]
fn runon_windows_with_return_paths_is_refused_before_anything_runs() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    let marker = dir.path().join("local-spawn-proof");
    let request = orchestrated_request(serde_json::json!({
        "command": format!("touch '{}'", marker.display()),
        "workdir": dir.path(),
        "runon": "windows",
        "return_paths": ["dist/app.exe"],
    }));
    let response = crate::bash_background::with_remote_policy(
        Some(launch(dir.path().join("no-daemon"))),
        || crate::commands::bash::handle(&request, &ctx),
    );
    assert!(!response.success, "{response:?}");
    assert_eq!(
        response.data["code"], "return_paths_refused",
        "{response:?}"
    );
    assert_eq!(
        response.data["message"],
        "return_paths refused: return_paths is not supported on Windows runners; the command did not run"
    );
    assert!(!marker.exists(), "a refused call never runs");
}

#[tokio::test]
async fn runon_return_paths_unsupported_by_the_runner_is_final_and_never_runs_locally() {
    // ck-motor advertises chunked return, but the runner that would take the
    // job (a Windows runner or an older Linux one) cannot copy files back.
    let daemon = aft_engine::test_bridge::bash_remote::remote_wire::daemon_with_capabilities(
        Script::ReturnPathsUnsupported,
        RETURN_CLAIMS,
        1,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    let marker = dir.path().join("local-spawn-proof");
    let mut call = return_call(&["dist/app.bin"]);
    call["command"] = serde_json::json!(format!("touch '{}'", marker.display()));
    let (reply, _) = orchestrated_reply(&ctx, &daemon, call).await;
    assert!(!reply.success, "{reply:?}");
    assert_eq!(reply.data["code"], "remote_unavailable", "{reply:?}");
    // The refusal names the job like every other runon outcome.
    assert_eq!(
        reply.data["message"],
        format!("{RETURN_PATHS_UNSUPPORTED_MESSAGE}; remote job {}", id())
    );
    assert_eq!(
        exec_runs(&daemon).len(),
        1,
        "the refusal is final, never retried"
    );
    assert!(!marker.exists(), "never a local run");
}

#[tokio::test]
async fn runon_uncounted_expired_gap_reports_loss_and_the_manifest_expired_reason() {
    // Without explicit counts the gap cannot be proved to contain only file
    // chunks. Replaying the same missing records must still end honestly.
    let daemon = return_daemon("replay-expired").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    let (reply, _) = orchestrated_reply(
        &ctx,
        &daemon,
        return_call(&["dist/app.bin", "dist/absent.txt"]),
    )
    .await;
    assert!(reply.success, "{reply:?}");
    let output = reply.data["output"].as_str().unwrap();
    assert!(output.contains("output lost"), "{output}");
    assert!(
        output
            .contains("  dist/app.bin: not returned (the server no longer keeps this job's files)"),
        "{output}"
    );
}

#[tokio::test]
async fn runon_return_over_cap_report_names_the_requested_cap() {
    for (large, expected_bytes, expected_limit) in [
        (false, 10_485_760, "10 MiB"),
        (true, 134_217_728, "128 MiB"),
    ] {
        let mut claims = RETURN_CLAIMS.to_vec();
        if large {
            claims.push(exec::types::RETURN_LARGE_CAPABILITY);
        }
        let daemon = aft_engine::test_bridge::bash_remote::remote_wire::daemon_with_capabilities(
            Script::ReturnVector("failed-over-cap"),
            &claims,
            1,
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = restarted_context(dir.path());
        let (reply, _) = orchestrated_reply(
            &ctx,
            &daemon,
            return_call(&["dist/hello.txt", "dist/large.bin"]),
        )
        .await;
        assert!(reply.success, "{reply:?}");
        let runs = exec_runs(&daemon);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0]["return_max_bytes"], expected_bytes);
        let output = reply.data["output"].as_str().unwrap();
        let report = format!(
            "dist/large.bin: not returned (it would take the total past the {expected_limit} limit)"
        );
        assert!(output.contains(&report), "{output}");
        let task_id = reply.data["task_id"].as_str().unwrap();
        let saved = read_task(&ctx.bash_background().test_task(task_id).unwrap().paths().json).unwrap();
        assert!(aft_engine::test_bridge::bash_remote::remote_report(&saved).unwrap().contains(&report));
    }
}

#[tokio::test]
async fn runon_return_v3_uses_advertised_cap_for_new_and_existing_outputs() {
    for local_bytes in [None, Some(5), Some(5 * 1024 * 1024), Some(6 * 1024 * 1024)] {
        let daemon = aft_engine::test_bridge::bash_remote::remote_wire::daemon_with_capabilities(
            Script::ReturnVector("two-files-one-empty"),
            &[
                "exec-remote/v1",
                "exec-remote-return/v2",
                "exec-remote-return/v3",
                exec::types::SUBMISSION_KEY_CAPABILITY,
            ],
            1,
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = restarted_context(dir.path());
        if let Some(bytes) = local_bytes {
            fs::create_dir(dir.path().join("dist")).unwrap();
            fs::File::create(dir.path().join("dist/hello.txt"))
                .unwrap()
                .set_len(bytes)
                .unwrap();
        }
        let (reply, _) = orchestrated_reply(
            &ctx,
            &daemon,
            return_call(&["dist/hello.txt", "dist/empty.txt"]),
        )
        .await;
        assert!(reply.success, "{reply:?}");
        let runs = exec_runs(&daemon);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0]["return_max_bytes"], 134_217_728);
        let task_id = reply.data["task_id"].as_str().unwrap();
        let task = ctx.bash_background().test_task(task_id).unwrap();
        let saved = read_task(&task.paths().json).unwrap();
        let remote = saved.remote.unwrap();
        assert_eq!(remote.return_max_bytes, 134_217_728);
        let restored =
            restore_retry_request(&task, &remote.submission.unwrap().request_digest()).unwrap();
        assert_eq!(restored.return_max_bytes, Some(134_217_728));
    }
}

#[tokio::test]
async fn runon_return_v3_selects_large_cap_and_persists_a_single_cap_retry() {
    for retry in [false, true] {
        let script = if retry {
            Script::ReturnCapRefusal {
                code: "invalid_params",
                field: "return_max_bytes",
                max_bytes: Some(20 * 1024 * 1024),
                always: false,
                after_accepted: false,
            }
        } else {
            Script::ReturnVector("two-files-one-empty")
        };
        let daemon = aft_engine::test_bridge::bash_remote::remote_wire::daemon_with_capabilities(
            script,
            &[
                "exec-remote/v1",
                "exec-remote-return/v2",
                "exec-remote-return/v3",
                exec::types::SUBMISSION_KEY_CAPABILITY,
            ],
            1,
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = restarted_context(dir.path());
        seed_large_local_return_output(dir.path());
        let (reply, _) = orchestrated_reply(
            &ctx,
            &daemon,
            return_call(&["dist/hello.txt", "dist/empty.txt"]),
        )
        .await;
        assert!(reply.success, "{reply:?}");
        assert_eq!(
            fs::read(dir.path().join("dist/hello.txt")).unwrap(),
            b"hello"
        );
        let runs = exec_runs(&daemon);
        assert_eq!(runs.len(), if retry { 2 } else { 1 });
        assert_eq!(runs[0]["return_max_bytes"], 134_217_728);
        let task_id = reply.data["task_id"].as_str().unwrap();
        let task = ctx.bash_background().test_task(task_id).unwrap();
        let saved = read_task(&task.paths().json).unwrap();
        let remote = saved.remote.unwrap();
        let cap = if retry { 20 * 1024 * 1024 } else { 134_217_728 };
        assert_eq!(remote.return_max_bytes, cap);
        assert_eq!(remote.return_cap_retried, retry);
        let restored =
            restore_retry_request(&task, &remote.submission.unwrap().request_digest()).unwrap();
        assert_eq!(restored.return_max_bytes, Some(cap));
        if retry {
            assert_eq!(runs[1]["return_max_bytes"], cap);
            let mut sink = TaskSink::new(ctx.bash_background(), task).unwrap();
            assert!(
                !sink
                    .return_request(&restored.with_return_max_bytes(10_485_760), true)
                    .unwrap(),
                "restart cannot spend the retry twice"
            );
        }
    }
}

#[tokio::test]
async fn runon_return_cap_second_refusal_is_not_keyed_recovery() {
    let daemon = aft_engine::test_bridge::bash_remote::remote_wire::daemon_with_capabilities(
        Script::ReturnCapRefusal {
            code: "invalid_params",
            field: "return_max_bytes",
            max_bytes: Some(10_485_760),
            always: true,
            after_accepted: false,
        },
        &[
            "exec-remote/v1",
            "exec-remote-return/v2",
            "exec-remote-return/v3",
            exec::types::SUBMISSION_KEY_CAPABILITY,
        ],
        1,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    seed_large_local_return_output(dir.path());
    let (reply, _) = orchestrated_reply(
        &ctx,
        &daemon,
        return_call(&["dist/hello.txt", "dist/empty.txt"]),
    )
    .await;
    assert!(!reply.success, "{reply:?}");
    assert_eq!(exec_runs(&daemon).len(), 2);
    assert!(!dir.path().join("dist/hello.txt").exists());
}

#[tokio::test]
async fn agent_commands_runon_never_declares_a_local_root() {
    const CHILD: &str = "AFT_AGENT_COMMANDS_RUNON_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let name = concat!(
            module_path!(),
            "::agent_commands_runon_never_declares_a_local_root"
        );
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                name.strip_prefix(concat!(env!("CARGO_CRATE_NAME"), "::")).unwrap_or(name),
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "isolated regression did not run: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    crate::agent_commands::enable(dir.path());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let connection = crate::agent_commands::connect(move |batch| {
        tx.send(batch).unwrap();
        async { Ok(()) }
    });
    assert_eq!(rx.recv().await.unwrap().known, Some(vec![]));
    let daemon = daemon(Script::Plain, "exec-remote/v1").await;
    let ctx = restarted_context(dir.path());
    let response = handle_with_policy(
        &ctx,
        Some(launch(daemon.connection().clone())),
        serde_json::json!({"command":"uname -a","runon":"linux","compressed":false}),
    );
    assert!(response.success, "{response:?}");
    let task_id = response.data["task_id"].as_str().unwrap().to_string();
    let _done = terminal(ctx.bash_background(), &task_id).await;
    assert_eq!(exec_runs(&daemon).len(), 1);
    tokio::task::yield_now().await;
    assert!(
        rx.try_recv().is_err(),
        "remote jobs and internal git helpers have no declared local root"
    );
    connection.stop().await;
}

async fn kill_restarted_unknown(
    ctx: crate::context::AppContext,
    task: String,
    session: &str,
    scope: Option<subc_protocol::scope::ScopeStamp>,
) -> crate::protocol::Response {
    let request = crate::protocol::RawRequest {
        id: "restart-unknown-kill".into(),
        command: "bash_kill".into(),
        lsp_hints: None,
        session_id: Some(session.into()),
        params: serde_json::json!({"task_id": task}),
    };
    tokio::task::spawn_blocking(move || {
        reg::with_scope_spawn(scope, || crate::commands::bash_kill::handle(&request, &ctx))
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn runon_draining_network_retry_preserves_outbound_and_vcpu_request() {
    let daemon = daemon(
        Script::Draining {
            refusals: 1,
            retry_after_ms: Some(50),
        },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut clock = PausedDrainingClock::new(dir.path());
    let ctx = restarted_context(dir.path());
    let ((reply, _), ()) = tokio::join!(
        orchestrated_reply(
            &ctx,
            &daemon,
            serde_json::json!({
                "command": "printf remote-proof",
                "runon": "linux,4c,net",
                "wait": true,
                "timeout": 5_000,
                "foreground_orchestrate": true,
                "compressed": false,
            }),
        ),
        async {
            let first = clock.now();
            clock.expect_sleep(first, first + 1000).await;
            clock.advance(5000);
        },
    );
    assert!(reply.success, "{reply:?}");
    assert_eq!(reply.data["status"], "completed", "{reply:?}");
    assert_eq!(reply.data["exit_code"], 0, "{reply:?}");
    let requests = exec_runs(&daemon);
    assert_eq!(
        requests.len(),
        2,
        "a never-started draining refusal must retry"
    );
    assert_eq!(requests[0], requests[1]);
    for request in requests {
        let request: RunRequest = serde_json::from_value(request).unwrap();
        assert_eq!(request.network, Some(Network::Outbound));
        assert_eq!(request.weight_hint, Some(4));
    }
}
