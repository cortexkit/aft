use aft_engine::test_bridge::bash_remote::{bash_background::registry::remote::*, resolve_posix_shell, BgTaskRegistryTestExt as _, TaskSinkTestExt as _};
use aft_engine::test_bridge::bash_remote::BgTaskStateTestExt as _;
use aft_engine::test_bridge::bash_remote::KeyedSubmissionTestExt as _;
fn read_only_result_case(
    locked: Option<bool>,
    outcome: Outcome,
    error: &str,
) -> (String, String, String) {
    read_only_result_with_stdout(locked, outcome, error, b"command output\n", true)
}

fn read_only_result_with_stdout(
    locked: Option<bool>,
    outcome: Outcome,
    error: &str,
    stdout: &[u8],
    compressed: bool,
) -> (String, String, String) {
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, _) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf remote");
    let task = registry.test_task(&task_id).unwrap();
    {
        let mut state = task.test_state().lock().unwrap();
        state.metadata_mut().workdir = dir.path().join("package");
        state.metadata_mut().compressed = compressed;
        // The synthetic environment has no secrets, so notices may retain its captured output.
        state.metadata_mut().completion_has_env_secrets = Some(false);
        state.metadata_mut().completion_has_non_ticket_env_secrets = Some(false);
        state.metadata_mut().completion_has_secret_placeholders = Some(false);
    }
    let error = error.replace("$ROOT", &dir.path().to_string_lossy());
    let mut record = TerminalRecord::new(id(), outcome, 2, 0, 0)
        .with_ran(Ran::Remote)
        .with_workspace_changes(Vec::new());
    if let Some(locked) = locked {
        record = record.with_source_read_only(locked);
    }
    let verdict = exec::grade(&record);
    let mut sink = TaskSink::new(&registry, task.clone()).unwrap();
    sink.output(1, OutputStream::Stdout, stdout).unwrap();
    sink.output(2, OutputStream::Stderr, error.as_bytes())
        .unwrap();
    sink.terminal(&record, &verdict).unwrap();
    let readonly = self::registry();
    let (disk, _) = readonly
        .terminal_status_from_disk(&task_id, "session", Some(dir.path()), dir.path())
        .unwrap();
    registry.remote_terminal(&task, verdict, None);
    let done = registry.observed_status(&task_id, "session", 8192).unwrap();
    let foreground = crate::commands::bash_orchestrate::foreground_result_response("lock", done);
    let completion = registry
        .drain_completions()
        .into_iter()
        .find(|completion| completion.task_id == task_id)
        .expect("remote completion notice");
    assert_eq!(
        serde_json::to_value(completion.clone().into_push_frame()).unwrap()["output_preview"],
        completion.output_preview
    );
    (
        foreground.data["output"].as_str().unwrap().to_owned(),
        completion.output_preview,
        disk.output_preview,
    )
}

fn assert_read_only_note(error: &str) {
    let (foreground, completion, disk) =
        read_only_result_case(Some(true), Outcome::Exit { code: 1 }, error);
    for text in [foreground, completion, disk] {
        assert_eq!(text.matches(READ_ONLY_NOTE).count(), 1, "{text}");
        assert!(text.lines().any(|line| line == READ_ONLY_NOTE), "{text}");
        assert!(
            text.find("Operation not permitted").unwrap() < text.find(READ_ONLY_NOTE).unwrap(),
            "{text}"
        );
    }
}

#[test]
fn runon_read_only_note_quoted_path_foreground_and_background() {
    for quote in ['\'', '"'] {
        assert_read_only_note(&format!(
            "cp: cannot create regular file {quote}$ROOT/src/x.rs{quote}: Operation not permitted\n"
        ));
    }
}

#[test]
fn runon_read_only_note_bsd_and_redirect_relative_paths() {
    for prefix in ["touch: ", "bash: line 1: "] {
        assert_read_only_note(&format!("{prefix}../src/x.rs: Operation not permitted\n"));
    }
}

#[test]
fn runon_read_only_note_python_permission_error() {
    assert_read_only_note("PermissionError: [Errno 1] Operation not permitted: '../src/x.rs'\n");
}

#[test]
fn runon_read_only_note_precedes_the_single_final_list_trailer() {
    let stdout = "ordinary output\n".repeat(10_000);
    let (foreground, completion, _) = read_only_result_with_stdout(
        Some(true),
        Outcome::Exit { code: 1 },
        "../src/x.rs: Operation not permitted\n",
        stdout.as_bytes(),
        true,
    );
    for text in [foreground, completion] {
        assert_eq!(text.matches(READ_ONLY_NOTE).count(), 1, "{text}");
        let trailers: Vec<_> = text
            .lines()
            .filter(|line| line.starts_with("shown "))
            .collect();
        assert_eq!(trailers.len(), 1, "{text}");
        assert_eq!(text.lines().last(), trailers.first().copied(), "{text}");
        assert!(text.find(READ_ONLY_NOTE).unwrap() < text.find(trailers[0]).unwrap());
    }
}

#[test]
fn runon_read_only_note_killed_job_completion() {
    let (foreground, completion, disk) = read_only_result_case(
        Some(true),
        Outcome::Cancelled,
        "../src/x.rs: Operation not permitted\n",
    );
    for text in [foreground, completion, disk] {
        assert!(text.contains(READ_ONLY_NOTE), "{text}");
    }
}

fn assert_remote_oom_terminal(cause: Option<&str>, expected: &str) {
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, paths) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf remote");
    let task = registry.test_task(&task_id).unwrap();
    let mut record = TerminalRecord::new(id(), Outcome::Signal { signal: 9 }, 1, 0, 0)
        .with_ran(Ran::Remote)
        .with_workspace_changes(Vec::new())
        .with_memory_stated_mib(8192)
        .with_memory_peak_working_mib(7798);
    if let Some(cause) = cause {
        record = record.with_oom_kill(OomKill::new(cause));
    }
    let verdict = exec::grade(&record);
    let mut sink = TaskSink::new(&registry, task.clone()).unwrap();
    sink.terminal(&record, &verdict).unwrap();

    // A crash after the terminal record is saved must give the same advice as live completion.
    let readonly = self::registry();
    let (disk, _) = readonly
        .terminal_status_from_disk(&task_id, "session", Some(dir.path()), dir.path())
        .unwrap();
    assert_eq!(disk.info.status_reason.as_deref(), Some(expected));
    if cause.is_some() {
        assert!(disk.output_preview.contains(expected), "{disk:?}");
    }
    registry.remote_terminal(&task, verdict, None);
    let done = registry.observed_status(&task_id, "session", 8192).unwrap();
    assert_eq!(done.info.status, BgTaskStatus::Failed);
    assert_eq!(done.exit_code, Some(137));
    assert_eq!(done.info.status_reason.as_deref(), Some(expected));
    assert!(done.output_preview.contains(expected), "{done:?}");
    assert_eq!(
        read_task(&paths.json).unwrap().status_reason.as_deref(),
        Some(expected)
    );
    let completion = registry
        .drain_completions()
        .into_iter()
        .find(|completion| completion.task_id == task_id)
        .expect("remote completion notice");
    assert_eq!(completion.status_reason.as_deref(), Some(expected));
    assert!(
        completion.output_preview.contains(expected),
        "{completion:?}"
    );
    let foreground =
        crate::commands::bash_orchestrate::foreground_result_response("oom", done.clone());
    assert!(foreground.data["output"]
        .as_str()
        .unwrap()
        .contains(expected));
    for value in [
        serde_json::to_value(&disk).unwrap(),
        serde_json::to_value(&done).unwrap(),
        serde_json::to_value(&completion).unwrap(),
        serde_json::to_value(completion.into_push_frame()).unwrap(),
        foreground.data,
    ] {
        if let Some(cause) = cause {
            assert_eq!(
                value["remote_oom_kill"],
                serde_json::json!({
                    "cause": cause,
                    "memory_stated_mib": 8192,
                    "memory_peak_working_mib": 7798,
                })
            );
        } else {
            assert!(value.get("remote_oom_kill").is_none(), "{value}");
            assert!(!value.to_string().contains("memory"));
        }
    }
}

#[test]
fn exec_remote_bash_oom_job_limit() {
    assert_remote_oom_terminal(
        Some("job_limit"),
        "killed: it went over its memory limit of 8 GiB (peak 7798 MiB); rerun with a larger limit",
    );
}

#[test]
fn exec_remote_bash_oom_host_pressure() {
    assert_remote_oom_terminal(Some("host_pressure"), "killed: the build server ran out of memory, not this job's limit (peak 7798 MiB); rerun as is");
}

#[test]
fn exec_remote_bash_oom_host_oom() {
    assert_remote_oom_terminal(Some("host_oom"), "killed: the build server ran out of memory, not this job's limit (peak 7798 MiB); rerun as is");
}

#[test]
fn exec_remote_bash_oom_absent_keeps_signal_text() {
    assert_remote_oom_terminal(None, "remote signal 9");
}

#[tokio::test]
async fn every_local_spawn_entrypoint_executes_command_once() {
    use crate::bash_background::{BashShell, HardKill, TaskSlot};
    for entry in [
        "foreground",
        "background",
        "pty",
        "deferred-wait",
        "replay-running",
        "remote-fallback",
        "replay-fallback",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let counter = dir.path().join("executions");
        let release = dir.path().join("release");
        let _release_on_failure = ReleaseOnDrop::new(release.clone());
        let command = format!(
            "printf x >> '{}'; {}; printf entrypoint-proof",
            counter.display(),
            crate::test_gate::gate(&format!("'{}'", release.display()), "/bin/sleep 0.01")
        );
        if entry == "deferred-wait" {
            crate::subc::test_support::assert_deferred_wait_entrypoint(&command, &release).await;
            assert_eq!(
                fs::read(&counter).unwrap(),
                b"x",
                "deferred wait must run its command once"
            );
            eprintln!(
                "spawn entrypoint deferred-wait: command executed and frame-loop lock check completed"
            );
            continue;
        }
        let mut registry = registry();
        let mut fake = None;
        let task_id = match entry {
            "foreground" => registry
                .spawn_with_shell_in_slot(
                    SpawnPlan::Unsandboxed,
                    &command,
                    BashShell::Bash,
                    resolve_posix_shell(),
                    "session".into(),
                    dir.path().into(),
                    HashMap::new(),
                    HardKill::After(Duration::from_secs(30)),
                    None,
                    dir.path().into(),
                    TaskSlot::Foreground,
                    true,
                    false,
                    Some(dir.path().into()),
                )
                .unwrap(),
            "background" | "replay-running" => registry
                .spawn(
                    SpawnPlan::Unsandboxed,
                    &command,
                    "session".into(),
                    dir.path().into(),
                    HashMap::new(),
                    HardKill::After(Duration::from_secs(30)),
                    None,
                    dir.path().into(),
                    10,
                    true,
                    false,
                    Some(dir.path().into()),
                )
                .unwrap(),
            "pty" => registry
                .spawn_pty(
                    SpawnPlan::Unsandboxed,
                    &command,
                    "session".into(),
                    dir.path().into(),
                    HashMap::new(),
                    HardKill::After(Duration::from_secs(30)),
                    None,
                    dir.path().into(),
                    10,
                    true,
                    false,
                    Some(dir.path().into()),
                    24,
                    80,
                )
                .unwrap(),
            "remote-fallback" => {
                ensure_git_fixture(dir.path());
                let daemon = daemon(Script::Refused, "exec-remote/v1").await;
                let id = registry
                    .spawn_remote(
                        launch(daemon.connection().clone()),
                        SpawnPlan::Unsandboxed,
                        &command,
                        resolve_posix_shell(),
                        "session".into(),
                        dir.path().into(),
                        HashMap::new(),
                        HardKill::After(Duration::from_secs(30)),
                        None,
                        dir.path().into(),
                        TaskSlot::Background { max: 10 },
                        true,
                        false,
                        Some(dir.path().into()),
                    )
                    .unwrap();
                fake = Some(daemon);
                id
            }
            "replay-fallback" => {
                let daemon = daemon(Script::AttachRefused, "exec-remote/v1").await;
                let (original, id, _) =
                    persist_accepted_with_snapshot(dir.path(), daemon.connection().clone(), &command);
                drop(original);
                registry.replay_session(dir.path(), "session").unwrap();
                fake = Some(daemon);
                id
            }
            _ => unreachable!(),
        };
        wait_for("command must cross its startup boundary", || {
            fs::read(&counter).ok().filter(|bytes| bytes == b"x")
        })
        .await;
        if entry == "replay-running" {
            registry.test_shutdown_flag().store(true, Ordering::SeqCst);
            let replacement = aft_engine::test_bridge::bash_remote::remote_support::registry();
            replacement.replay_session(dir.path(), "session").unwrap();
            registry = replacement;
        }
        fs::write(&release, b"release").unwrap();
        let done = terminal(&registry, &task_id).await;
        assert_eq!(
            done.info.status,
            BgTaskStatus::Completed,
            "{entry}: {done:?}"
        );
        assert_eq!(
            fs::read(&counter).unwrap(),
            b"x",
            "{entry}: duplicate execution"
        );
        let replay = aft_engine::test_bridge::bash_remote::remote_support::registry();
        replay.replay_session(dir.path(), "session").unwrap();
        assert_eq!(
            terminal(&replay, &task_id).await.info.status,
            BgTaskStatus::Completed,
            "{entry}: terminal replay"
        );
        assert_eq!(
            fs::read(&counter).unwrap(),
            b"x",
            "{entry}: replay ran command again"
        );
        eprintln!("spawn entrypoint {entry}: command executed once and replay completed");
        registry.test_shutdown_flag().store(true, Ordering::SeqCst);
        replay.test_shutdown_flag().store(true, Ordering::SeqCst);
        drop(fake);
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

#[tokio::test]
async fn runon_pre_acceptance_refusal_ends_standalone_foreground_wait_promptly() {
    for (reason, detail) in [
        ("runner_full", Some("hosted_host_full")),
        ("network_unsupported", None),
        ("queue_wait_exceeded", None),
    ] {
        let daemon = daemon(
            Script::GatedPreStartRefusal { reason, detail },
            "exec-remote/v1",
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = Arc::new(remote_test_context(dir.path()));
        let request = orchestrated_request(serde_json::json!({
            "command": "printf must-not-run",
            "workdir": dir.path(),
            "runon": "linux",
            "wait": true,
            "foreground_orchestrate": true,
            "compressed": false,
        }));
        let spawned = crate::bash_background::with_remote_policy(
            Some(launch(daemon.connection().clone())),
            || crate::commands::bash::handle(&request, &ctx),
        );
        let task_id = spawned.data["task_id"].as_str().unwrap().to_owned();
        let crate::response_finalize::DispatchOutcome::Deferred(mut pending) =
            crate::commands::bash_orchestrate::build_bash_outcome(&request, &ctx, spawned)
        else {
            panic!("foreground must wait while the executor holds its refusal");
        };
        let crate::response_finalize::DispatchOutcome::Deferred(mut watch) =
            crate::commands::bash_watch::handle_deferred(
                &crate::protocol::RawRequest {
                    id: "refusal-watch".into(),
                    command: "bash_watch".into(),
                    session_id: Some("session".into()),
                    lsp_hints: None,
                    params: serde_json::json!({"task_id": task_id, "worker_session": true}),
                },
                ctx.clone(),
            )
        else {
            panic!("watch must wait while the executor holds its refusal");
        };
        daemon.wait_for_requests("exec.run", 1).await;
        daemon.acceptance_gate().notify_one();
        let (response, watch_response) = tokio::time::timeout(Duration::from_secs(1), async {
            let mut foreground_reply = None;
            let mut watch_reply = None;
            loop {
                foreground_reply = foreground_reply.or_else(|| (pending.poll)(&ctx));
                watch_reply = watch_reply.or_else(|| (watch.poll)(&ctx));
                if foreground_reply.is_some() && watch_reply.is_some() {
                    return (foreground_reply.unwrap(), watch_reply.unwrap());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("a pre-start refusal must not consume the thirty-minute wait budget");
        assert_runon_refusal(&response, &rendered_refusal_reason(reason, detail));
        assert_eq!(
            ctx.bash_background()
                .runon_inputs_captured(&task_id, "session"),
            Some(false)
        );
        let status = crate::commands::bash_status::handle(
            &crate::protocol::RawRequest {
                id: "refusal-status".into(),
                command: "bash_status".into(),
                session_id: Some("session".into()),
                lsp_hints: None,
                params: serde_json::json!({"task_id": task_id}),
            },
            &ctx,
        );
        assert_eq!(response.data["code"], status.data["code"]);
        assert_eq!(response.data["message"], status.data["message"]);
        assert_eq!(watch_response.data["waited"]["reason"], "exited");
        assert_eq!(
            watch_response.data["remote_refusal"]["code"],
            status.data["code"]
        );
        assert_eq!(
            watch_response.data["remote_refusal"]["message"],
            status.data["message"]
        );
    }
}

#[tokio::test]
async fn runon_pre_submission_failures_end_standalone_foreground_wait_promptly() {
    for unavailable_connection in [false, true] {
        let daemon = daemon(Script::Plain, "unrelated/v1").await;
        if unavailable_connection {
            std::fs::remove_file(&daemon.connection()).unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        let ctx = remote_test_context(dir.path());
        let (response, _) = tokio::time::timeout(
            Duration::from_secs(1),
            orchestrated_reply_with_probe(
                &ctx,
                &daemon,
                serde_json::json!({
                    "command": "printf must-not-run", "runon": "linux", "wait": true,
                    "foreground_orchestrate": true, "compressed": false,
                }),
                |_| true,
            ),
        )
        .await
        .expect("a capability or connection refusal must end the foreground wait");
        assert!(!response.success, "{response:?}");
        assert_eq!(response.data["code"], "remote_unavailable", "{response:?}");
        assert!(
            response.data["message"]
                .as_str()
                .unwrap()
                .contains("command did not run"),
            "{response:?}"
        );
        assert!(exec_runs(&daemon).is_empty());
    }
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
            Some(launch(daemon.connection().clone())),
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
        let task = ctx.bash_background().test_task(task_id).unwrap();
        assert!(!task.test_state().lock().unwrap().metadata().local_fallback_started);
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
    shorten_draining_budget(&std::fs::canonicalize(dir.path()).unwrap(), 100);
    let marker = dir.path().join("must-not-run-locally");
    let ctx = restarted_context(dir.path());
    let response = handle_with_policy(
        &ctx,
        Some(launch(daemon.connection().clone())),
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
    let response = refusal_response(done);
    let message = response.data["message"].as_str().unwrap();
    assert!(message.starts_with("runon refused: remote refused: runner_draining (runner says: maintenance window) after retrying for "), "{message}");
    assert!(
        message.ends_with("; command was not run; retry, or omit runon to run locally; remote job 0192a64a-1234-7000-8000-000000000001"),
        "{message}"
    );
    assert!(
        message.contains("remote job 0192a64a-1234-7000-8000-000000000001"),
        "{message}"
    );
    assert!(!message.contains(['\n', '\t']), "{message}");
}

#[tokio::test]
async fn local_fallback_refusal_detail_is_rendered_and_sanitized() {
    let daemon = daemon(Script::RefusedWithDetail, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("local-fallback");
    let ctx = restarted_context(dir.path());
    let mut policy = launch(daemon.connection().clone());
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
        let mut policy = launch(daemon.connection().clone());
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

fn remote_terminal_preview_with_job(
    verdict: Verdict,
    accepted_refusal: bool,
) -> (String, Option<RemoteRefusal>) {
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, _) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf unused");
    let task = registry.test_task(&task_id).unwrap();
    {
        let mut state = task.test_state().lock().unwrap();
        let remote = state.metadata_mut().remote.as_mut().unwrap();
        remote.explicit_runon = true;
        remote.job_id = Some(id());
        remote.requested_vcpus = Some(4);
        if accepted_refusal {
            remote.terminal = Some(TerminalRecord::new(
                id(),
                Outcome::RefusedBeforeStart {
                    reason: RefusalReason::Unreachable,
                },
                0,
                0,
                0,
            ));
        }
    }
    registry.remote_terminal(&task, verdict, None);
    let snapshot = registry.observed_status(&task_id, "session", 8192).unwrap();
    let expected_header = if accepted_refusal {
        "remote executor refused before start (job 0192a64a-1234-7000-8000-000000000001)"
    } else {
        EXPECTED_REMOTE_JOB_HEADER
    };
    assert!(
        snapshot.output_preview.starts_with(expected_header),
        "{}",
        snapshot.output_preview
    );
    let foreground = crate::commands::bash_orchestrate::format_foreground_result(&snapshot);
    assert!(foreground.contains(expected_header), "{foreground}");
    let completion = registry.completion_snapshot_for_task(&task).unwrap();
    assert!(
        completion.output_preview.starts_with(expected_header),
        "{}",
        completion.output_preview
    );
    (snapshot.output_preview, snapshot.remote_refusal)
}

#[test]
fn remote_success_result_names_the_full_job_id() {
    let (output, _) = remote_terminal_preview_with_job(Verdict::Exited { code: 0 }, false);
    assert_eq!(output.lines().next(), Some(EXPECTED_REMOTE_JOB_HEADER));
}

#[test]
fn remote_nonzero_exit_result_names_the_full_job_id() {
    let (output, _) = remote_terminal_preview_with_job(Verdict::Exited { code: 7 }, false);
    assert_eq!(output.lines().next(), Some(EXPECTED_REMOTE_JOB_HEADER));
}

#[test]
fn remote_refusal_after_acceptance_names_the_full_job_id() {
    let (output, refusal) = remote_terminal_preview_with_job(
        Verdict::RunLocally {
            reason: RefusalReason::Unreachable,
            refusal_detail: None,
        },
        true,
    );
    assert_eq!(
        output.lines().next(),
        Some("remote executor refused before start (job 0192a64a-1234-7000-8000-000000000001)")
    );
    assert!(refusal
        .unwrap()
        .message
        .contains("remote job 0192a64a-1234-7000-8000-000000000001"));
}

#[test]
fn remote_timeout_result_names_the_full_job_id() {
    let (output, _) = remote_terminal_preview_with_job(Verdict::DeadlineKilled, false);
    assert_eq!(output.lines().next(), Some(EXPECTED_REMOTE_JOB_HEADER));
}

#[test]
fn remote_cancel_result_names_the_full_job_id() {
    let (output, _) = remote_terminal_preview_with_job(Verdict::Cancelled, false);
    assert_eq!(output.lines().next(), Some(EXPECTED_REMOTE_JOB_HEADER));
}

#[test]
fn remote_kill_result_names_the_full_job_id() {
    let (output, _) = remote_terminal_preview_with_job(Verdict::CancelKilled, false);
    assert_eq!(output.lines().next(), Some(EXPECTED_REMOTE_JOB_HEADER));
}

/// Promotion, wait-mode detach, remote hand-back, module-drain detach and the
/// module loop's held-call answer all pass their live-task reply through
/// `with_runon_capture_state`. Before the runner's `Accepted`, a live task
/// reads `starting`; once the task has ended (here: outcome unknown after the
/// connection dropped), the reply must carry that saved outcome and its rerun
/// advice instead.
#[test]
fn runon_uncaptured_reply_reports_terminal_outcome_not_starting() {
    use crate::commands::bash_orchestrate::{
        with_runon_capture_state, REMOTE_INPUT_CAPTURE_WARNING,
    };
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, _) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf unused");
    let task = registry.test_task(&task_id).unwrap();
    {
        let mut state = task.test_state().lock().unwrap();
        let remote = state.metadata_mut().remote.as_mut().unwrap();
        remote.explicit_runon = true;
        remote.accepted = false;
        remote.job_id = None;
    }
    let live_reply = || {
        crate::protocol::Response::success(
            "promote",
            serde_json::json!({
                "output": "moved to the background",
                "task_id": task_id,
                "status": "running",
            }),
        )
    };
    let pending = with_runon_capture_state(&registry, &task_id, "session", live_reply());
    assert_eq!(pending.data["status"], "starting", "{pending:?}");
    assert_eq!(pending.data["phase"], "submitting", "{pending:?}");

    registry.remote_terminal(
        &task,
        Verdict::OutcomeUnknown,
        Some("remote route interrupted before acceptance".into()),
    );
    let ended = with_runon_capture_state(&registry, &task_id, "session", live_reply());
    assert_eq!(ended.data["status"], "fate_unknown", "{ended:?}");
    assert!(ended.data.get("phase").is_none(), "{ended:?}");
    assert_eq!(ended.data["task_id"], task_id.as_str());
    let output = ended.data["output"].as_str().unwrap();
    assert!(
        output.contains("remote outcome unknown; the remote job could not affect this machine or the network, so rerunning is safe"),
        "{output}"
    );
    assert!(!output.contains("moved to the background"), "{output}");
    // The local task is terminal (the remote outcome is unknown), so this
    // reply will never move to queued or running: the in-flight capture
    // warning would be a false instruction here.
    assert!(!output.contains(REMOTE_INPUT_CAPTURE_WARNING), "{output}");

    let recovered = self::registry();
    // Startup can publish a detach target before replay inserts its task.
    // The reserved handle must receive the canonical recovered reply too.
    let reserved = recovered.drain_reply_cache(&task_id, "session");
    let metadata = task.test_state().lock().unwrap().metadata().clone();
    recovered
        .insert_rehydrated_task(metadata, task.paths().clone(), true)
        .unwrap();
    let expected = with_runon_capture_state(&recovered, &task_id, "session", live_reply());
    let recovered_task = recovered.test_task(&task_id).unwrap();
    let _state = recovered_task.test_state().lock().unwrap();
    let _no_task_locks = aft_engine::test_bridge::bash_remote::bash_background::registry::forbid_task_state_locks_for_test();
    let cached = reserved
        .snapshot()
        .uncaptured_terminal
        .expect("replayed terminal drain reply");
    let cached =
        crate::commands::bash_orchestrate::foreground_result_response("", (*cached).clone());
    assert_eq!(cached.data, expected.data);
    assert_eq!(cached.success, expected.success);
}

#[tokio::test]
async fn exec_remote_bash_keyed_disconnect_reattaches_while_job_is_running() {
    use aft_engine::test_bridge::bash_remote::remote_wire::{outage_daemon_with_replay, OutageReplay};
    let daemon = outage_daemon_with_replay(OutageReplay::Running).await;
    let dir = tempfile::tempdir().unwrap();
    ensure_git_fixture(dir.path());
    let ctx = Arc::new(restarted_context(dir.path()));
    let registry = ctx.bash_background().clone();
    let request = orchestrated_request(serde_json::json!({
        "command": "printf local-proof", "workdir": dir.path(), "runon": "linux",
        "wait": true, "timeout": 30_000, "foreground_orchestrate": true, "compressed": false,
    }));
    let spawned =
        crate::bash_background::with_remote_policy(Some(launch(daemon.connection().clone())), || {
            crate::commands::bash::handle(&request, &ctx)
        });
    assert!(spawned.success, "{spawned:?}");
    let task_id = spawned.data["task_id"].as_str().unwrap().to_string();
    let crate::response_finalize::DispatchOutcome::Deferred(mut pending) =
        crate::commands::bash_orchestrate::build_bash_outcome(&request, &ctx, spawned)
    else {
        panic!("foreground wait must remain pending while the remote job runs")
    };
    daemon.after_refused_connects(1).await;
    let task = registry.test_task(&task_id).unwrap();
    let detached = read_task(&task.paths().json).unwrap();
    let remote = detached.remote.unwrap();
    assert!(remote.submission.is_some());
    assert_eq!(remote.job_id, Some(id()));
    assert_eq!(remote.last_seq, Some(1), "checkpoint before reconnect");
    daemon.restore();
    wait_for(
        "attached output must arrive before the job finishes",
        || (fs::read(&task.paths().stdout).unwrap() == b"AB").then_some(()),
    )
    .await;
    assert!(!registry
        .observed_status(&task_id, "session", 0)
        .unwrap()
        .info
        .status
        .is_terminal());
    daemon.finish_job();
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");
    assert_eq!(done.exit_code, Some(0));
    assert_eq!(fs::read(&task.paths().stdout).unwrap(), b"AB");
    assert_eq!(daemon.methods("exec.run").len(), 1);
    let attaches = daemon.methods("exec.attach");
    assert_eq!(attaches.len(), 1);
    assert_eq!(attaches[0]["params"]["from_seq"], 2);
    let durable = read_task(&task.paths().json).unwrap();
    assert_eq!(durable.status, BgTaskStatus::Completed);
    assert_eq!(durable.remote.unwrap().last_seq, Some(2));
    let reply = wait_for(
        "foreground wait must return the reattached terminal",
        || (pending.poll)(&ctx),
    )
    .await;
    assert!(reply.success, "{reply:?}");
    assert_eq!(reply.data["exit_code"], 0, "{reply:?}");
    assert!(
        reply.data["output"].as_str().unwrap().contains("AB"),
        "{reply:?}"
    );
    let watch = rendered_watch(ctx, &task_id).await;
    assert!(watch.contains("AB"), "{watch}");
    assert!(!watch.contains("outcome unknown"), "{watch}");
}

async fn runon_restart_first_watch_reattaches(
    watch_before_replay: bool,
    relaxed_lookup: Option<bool>,
    auth_refusals: u32,
    lost: bool,
) {
    let daemon = aft_engine::test_bridge::bash_remote::remote_wire::daemon_with_capabilities(
        Script::RestartRunning {
            auth_refusals,
            lost,
        },
        &["exec-remote/v1", "exec-remote-submission-key/v1"],
        2,
    )
    .await;
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    fs::create_dir(&project).unwrap();
    ensure_git_fixture(&project);
    let fixture_root = root.path().to_path_buf();
    let connection = daemon.connection().clone();
    let child = tokio::task::spawn_blocking(move || {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "bash_background::registry::remote::tests::runon_restart_reattach_drain_child",
                "--nocapture",
            ])
            .env("AFT_RUNON_RESTART_FIXTURE", fixture_root)
            .env("AFT_RUNON_RESTART_CONNECTION", connection)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(child.status.success(), "{child:?}");
    let task_id = fs::read_to_string(root.path().join("task-id")).unwrap();
    let ctx = runon_restart_context(&project, &root.path().join("storage"));
    ctx.bash_background()
        .set_harness(crate::harness::Harness::Runner);
    let storage = crate::bash_background::task_storage_dir(&ctx);
    let capture = Arc::new(Mutex::new(Vec::new()));
    remote_launch_log_captures()
        .lock()
        .unwrap()
        .insert(project.canonicalize().unwrap(), capture.clone());
    let saved = resolve_task_layout(&session_tasks_dir(&storage, "session"), &task_id).unwrap();
    let saved = read_task_at(&saved).unwrap();
    assert!(saved.scope.is_some());
    assert!(!saved.remote.unwrap().submission.unwrap().reattached());
    if let Some(database) = relaxed_lookup {
        if database {
            let db = Arc::new(Mutex::new(
                crate::db::open(&root.path().join("aft.db")).unwrap(),
            ));
            ctx.bash_background().set_db_pool(db);
            let resolved =
                resolve_task_layout(&session_tasks_dir(&storage, "session"), &task_id).unwrap();
            let mut old = read_task_at(&resolved).unwrap();
            // The remote stream checkpoint on disk may be newer than its row.
            old.remote.as_mut().unwrap().last_seq = None;
            old.remote.as_mut().unwrap().started = None;
            ctx.bash_background().dual_write_task(&resolved.paths, &old);
        }
        assert!(ctx
            .bash_background()
            .status_relaxed(
                &task_id,
                "foreign-session",
                &project,
                &storage,
                0,
                false,
                Duration::ZERO,
            )
            .is_none());
        assert!(ctx
            .bash_background()
            .kill_relaxed(&task_id, &project, &storage)
            .is_err());
        assert!(!ctx.bash_background().holds_task(&task_id));
        assert_eq!(remote_workers(&task_id), (0, 0));
        let snapshot = ctx.bash_background().status_relaxed(
            &task_id,
            "session",
            &project,
            &storage,
            0,
            false,
            Duration::ZERO,
        );
        assert!(snapshot.is_some(), "relaxed lookup must find the saved job");
    }
    let executor = crate::context::Executor::new();
    let root_id = crate::path_identity::ProjectRootId::from_path(&project).unwrap();
    assert!(executor.register_actor(root_id.clone(), ctx.clone()));
    let bind_request = serde_json::from_value::<crate::protocol::RawRequest>(serde_json::json!({
        "id": "subc-bind-restart-runner",
        "command": "configure",
        "project_root": project,
        "storage_dir": root.path().join("storage"),
        "harness": "runner",
        "session_id": "session",
        "config": [{
            "tier": "user", "source": "/u/aft.jsonc",
            "doc": "{\"indexes\":{\"semantic\":false,\"trigram\":false,\"callgraph\":false}}"
        }],
    }))
    .unwrap();
    let principal = runon_restart_principal(&project);
    let bound = executor
        .submit_bind_cancellable_async(
            root_id,
            bind_request.id.clone(),
            Arc::new(move |ctx| {
                crate::sandbox_spawn::with_authenticated_principal(principal.clone(), || {
                    crate::commands::configure::handle_configure(&bind_request, ctx)
                })
            }),
        )
        .0
        .await
        .unwrap();
    assert!(bound.success, "{bound:?}");
    let mut gate = watch_before_replay.then(|| {
        crate::commands::configure::gate_configure_tail_stage_for_test(
            ctx.canonical_cache_root(),
            "replay_claimed",
        )
    });
    let tail_ctx = ctx.clone();
    let mut tail = tokio::task::spawn_blocking(move || {
        crate::commands::configure::drain_deferred_configure_maintenance(&tail_ctx);
    });
    if let Some((_, reached, _)) = &gate {
        let reached = reached.clone();
        tokio::task::spawn_blocking(move || reached.recv_timeout(Duration::from_secs(30)).unwrap())
            .await
            .unwrap();
    } else {
        // The session bind's real configure tail, not a direct registry replay,
        // must restore the job before this order's first watch is dispatched.
        (&mut tail).await.unwrap();
    }
    let request = serde_json::from_value(serde_json::json!({
        "id": "first-watch-after-drain",
        "command": "bash_watch",
        "session_id": "session",
        "worker_session": true,
        "task_id": task_id,
        "timeout_ms": 30_000,
    }))
    .unwrap();
    let outcome = crate::sandbox_spawn::with_authenticated_principal(
        runon_restart_principal(&project),
        || crate::commands::bash_watch::handle_deferred(&request, ctx.clone()),
    );
    daemon
        .wait_for_requests("exec.attach", auth_refusals as usize + 1)
        .await;
    if let Some((guard, _, release)) = gate.take() {
        drop(guard);
        release.send(()).unwrap();
        tail.await.unwrap();
    }
    let response = match outcome {
        crate::response_finalize::DispatchOutcome::Immediate(response) => response,
        crate::response_finalize::DispatchOutcome::Deferred(mut pending) => {
            wait_for("the first watch must return the recovered result", || {
                (pending.poll)(&ctx)
            })
            .await
        }
    };
    if lost {
        assert_eq!(response.data["status"], "fate_unknown", "{response:?}");
    } else {
        assert_eq!(response.data["status"], "completed", "{response:?}");
        assert_eq!(response.data["exit_code"], 0, "{response:?}");
        assert!(
            response.data["output"]
                .as_str()
                .unwrap()
                .contains("remote-restart-proof"),
            "{response:?}"
        );
    }
    assert_eq!(response.data["waited"]["reason"], "exited", "{response:?}");
    remote_worker_exited(&task_id).await;
    assert_eq!(remote_workers(&task_id), (1, 1));
    let task = ctx.bash_background().test_task(&task_id).unwrap();
    let completed = read_task(&task.paths().json).unwrap();
    if lost {
        assert!(completed
            .status_reason
            .as_deref()
            .unwrap()
            .contains("remote outcome unknown"));
    }
    assert!(completed.remote.unwrap().submission.unwrap().reattached());
    let lines = capture.lock().unwrap();
    let warnings: Vec<_> = lines
        .iter()
        .filter(|line| line.contains("runon: remote attach failed "))
        .collect();
    assert_eq!(warnings.len(), auth_refusals as usize, "{lines:?}");
    for (index, warning) in warnings.iter().enumerate() {
        assert!(
            warning.contains(&format!("task_id={task_id} job_id={}", id())),
            "{warning}"
        );
        assert!(
            warning.contains(&format!(
                "attempt={} next_backoff_ms={}",
                index + 1,
                100 << index
            )),
            "{warning}"
        );
        assert!(warning.contains("auth"), "{warning}");
    }
    let successes: Vec<_> = lines
        .iter()
        .filter(|line| line.contains("runon: remote attach succeeded "))
        .collect();
    assert_eq!(successes.len(), 1, "{lines:?}");
    assert!(
        successes[0].contains(&format!(
            "task_id={task_id} job_id={} attempt={}",
            id(),
            auth_refusals + 1
        )),
        "{successes:?}"
    );
    let log = daemon.log().lock().unwrap();
    let attaches: Vec<_> = log
        .iter()
        .filter(|(_, body)| body["method"] == "exec.attach")
        .collect();
    assert_eq!(attaches.len(), auth_refusals as usize + 1);
    for (_, attach) in attaches {
        assert_eq!(attach["params"]["job_id"], id().to_string());
        assert_eq!(attach["params"]["from_seq"], 1);
    }
    assert_eq!(
        log.iter()
            .filter(|(_, body)| body["method"] == "exec.run")
            .count(),
        1
    );
    assert!(!log.iter().any(|(_, body)| body["method"] == "exec.cancel"));
}

#[tokio::test]
async fn runon_restart_reattach_first_watch_before_replay() {
    runon_restart_first_watch_reattaches(true, None, 0, false).await;
}

#[tokio::test]
async fn runon_restart_reattach_first_watch_after_replay() {
    runon_restart_first_watch_reattaches(false, None, 0, false).await;
}

#[tokio::test]
async fn runon_restart_reattach_relaxed_lookup_before_first_watch() {
    for database in [false, true] {
        runon_restart_first_watch_reattaches(true, Some(database), 0, false).await;
    }
}

#[tokio::test]
async fn runon_restart_reattach_relaxed_lookup_before_replay() {
    for database in [false, true] {
        runon_restart_first_watch_reattaches(false, Some(database), 0, false).await;
    }
}

#[tokio::test]
async fn runon_restart_reattach_logs_each_auth_refusal_then_success() {
    runon_restart_first_watch_reattaches(true, None, 2, false).await;
}

#[tokio::test]
async fn runon_restart_reattach_unknown_job_ends_the_first_watch() {
    runon_restart_first_watch_reattaches(true, None, 0, true).await;
}

#[tokio::test]
async fn held_call_startup_reattaches_remote_job_without_a_route_bind() {
    held_call_startup_remote_result(false, "").await;
}

#[tokio::test]
async fn held_call_startup_lost_remote_job_settles_as_unknown_outcome() {
    held_call_startup_remote_result(true, "").await;
}

#[tokio::test]
async fn held_call_startup_lost_remote_job_preserves_output() {
    held_call_startup_remote_result(true, "partial output\n").await;
}

async fn held_call_startup_remote_result(lost: bool, output: &str) {
    use crate::subc::held_task_recovery::tests as recovery;
    let daemon = daemon(
        if lost { Script::Lost } else { Script::Restart },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("storage");
    let root = storage.join("runner");
    fs::create_dir_all(&root).unwrap();
    let old = recovery::context(&root, &storage);
    let (registry, task_id, _) = persist_accepted_with_snapshot(
        &root,
        daemon.connection().clone(),
        "printf must-not-run-locally",
    );
    registry.detach();
    drop(registry);
    let layout = resolve_task_layout(&session_tasks_dir(&root, "session"), &task_id).unwrap();
    let mut metadata = read_task(&layout.paths.json).unwrap();
    let job_id = metadata.remote.as_ref().unwrap().job_id.unwrap();
    if !output.is_empty() {
        fs::write(&layout.paths.stdout, output).unwrap();
        metadata.compressed = false;
        metadata.completion_has_secret_placeholders = Some(false);
        metadata.completion_has_env_secrets = Some(false);
        metadata.completion_has_non_ticket_env_secrets = Some(false);
        let remote = metadata.remote.as_mut().unwrap();
        remote.last_seq = Some(1);
        remote.stdout_len = output.len() as u64;
        remote.output_received = true;
        write_task_at(&layout, &metadata).unwrap();
    }
    let key = recovery::hold_task(&old, &layout, "remote-held");
    drop(old);
    let restarted = recovery::context(&root, &storage);
    let executor = recovery::startup(&restarted).await;
    let frame = recovery::settled(&restarted, &key).await;
    if lost {
        let result: serde_json::Value = serde_json::from_slice(&frame.body).unwrap();
        let preview = if output.is_empty() {
            String::new()
        } else {
            format!("Output received before that:\n{output}")
        };
        assert_eq!(
            result["content"][0]["text"],
            format!("Task {task_id}: outcome unknown (AFT lost the result of build-server job {job_id}).\n{preview}Check this task with bash_status before running the command again.")
        );
    } else {
        let result: serde_json::Value = serde_json::from_slice(&frame.body).unwrap();
        assert!(result["content"].to_string().contains("ABCD"), "{result}");
        assert_eq!(result["structuredContent"]["exit_code"], 0, "{result}");
    }
    let log = daemon.log().lock().unwrap();
    assert!(log.iter().any(|(_, body)| body["method"] == "exec.attach"));
    assert!(!log.iter().any(|(_, body)| body["method"] == "exec.run"));
    drop(log);
    for actor in executor.actor_contexts() {
        actor.bash_background().detach();
    }
}

async fn restart_unaccepted_submission(
    connection: PathBuf,
    job_id: Option<Uuid>,
    keyed: bool,
) -> (
    tempfile::TempDir,
    Arc<crate::context::AppContext>,
    String,
    TaskPaths,
) {
    use crate::subc::held_task_recovery::tests as recovery;
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("storage");
    let root = storage.join("runner");
    fs::create_dir_all(&root).unwrap();
    let old = recovery::context(&root, &storage);
    let (registry, task_id, paths) =
        persist_accepted_with_snapshot(&root, connection, "printf must-not-run-locally");
    registry.detach();
    drop(registry);
    let mut metadata = read_task(&paths.json).unwrap();
    metadata.harness = Some("runner".into());
    metadata.execution_note = None;
    let remote = metadata.remote.as_mut().unwrap();
    remote.accepted = false;
    remote.job_id = job_id;
    remote.queue_position = None;
    remote.explicit_runon = true;
    remote.submission = keyed.then(|| preacceptance_submission_for_test(&task_id));
    write_task(&paths.json, &metadata).unwrap();
    let db = old.db().unwrap();
    crate::db::bash_tasks::upsert_bash_task(
        &db.lock().unwrap(),
        &metadata.to_bash_task_row("runner", &paths).unwrap(),
    )
    .unwrap();
    drop(db);
    drop(old);
    let restarted = recovery::context(&root, &storage);
    let executor = recovery::startup(&restarted).await;
    let context = executor
        .actor_contexts()
        .into_iter()
        .find(|actor| actor.bash_background().test_task(&task_id).is_some())
        .expect("startup must adopt the saved submission without a held call or route bind");
    (dir, context, task_id, paths)
}

#[tokio::test]
async fn remote_restart_unaccepted_running_job_attaches_and_finishes() {
    use aft_engine::test_bridge::bash_remote::remote_wire::{outage_daemon_with_replay, OutageReplay};
    let daemon = outage_daemon_with_replay(OutageReplay::Running).await;
    let (_dir, ctx, task_id, paths) =
        restart_unaccepted_submission(daemon.connection().clone(), Some(id()), true).await;
    let registry = ctx.bash_background();
    wait_for(
        "attached output must arrive while the job is running",
        || (fs::read(&paths.stdout).unwrap() == b"AB").then_some(()),
    )
    .await;
    assert!(!registry
        .observed_status(&task_id, "session", 0)
        .unwrap()
        .info
        .status
        .is_terminal());
    assert_eq!(
        registry.remote_progress(&task_id, "session").unwrap().phase,
        RemotePhase::Running
    );
    assert!(!rendered_status(&ctx, &task_id).contains("capturing inputs"));
    daemon.finish_job();
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");
    assert_eq!(done.exit_code, Some(0));
    assert_eq!(fs::read(&paths.stdout).unwrap(), b"AB");
    assert!(done.output_preview.contains("AB"), "{done:?}");
    let attaches = daemon.methods("exec.attach");
    assert_eq!(attaches.len(), 1);
    assert_eq!(attaches[0]["params"]["job_id"], id().to_string());
    assert_eq!(attaches[0]["params"]["from_seq"], 0);
    assert!(daemon.methods("exec.run").is_empty());
    assert!(!read_task(&paths.json).unwrap().local_fallback_started);
}

#[tokio::test]
async fn remote_restart_unaccepted_completed_job_replays_output() {
    use aft_engine::test_bridge::bash_remote::remote_wire::{outage_daemon_with_replay, OutageReplay};
    let daemon = outage_daemon_with_replay(OutageReplay::Finished).await;
    let (_dir, ctx, task_id, paths) =
        restart_unaccepted_submission(daemon.connection().clone(), Some(id()), false).await;
    let registry = ctx.bash_background();
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");
    assert_eq!(done.exit_code, Some(0));
    assert_eq!(fs::read(&paths.stdout).unwrap(), b"AB");
    assert!(done.output_preview.contains("AB"), "{done:?}");
    let attaches = daemon.methods("exec.attach");
    assert_eq!(attaches.len(), 1);
    assert_eq!(attaches[0]["params"]["job_id"], id().to_string());
    assert_eq!(attaches[0]["params"]["from_seq"], 0);
    assert!(daemon.methods("exec.run").is_empty());
    assert!(!read_task(&paths.json).unwrap().local_fallback_started);
}

#[tokio::test]
async fn remote_restart_unaccepted_never_ran_job_settles() {
    let daemon = daemon(Script::PlacementWait { job_id: true }, "exec-remote/v1").await;
    let (_dir, ctx, task_id, paths) =
        restart_unaccepted_submission(daemon.connection().clone(), Some(id()), false).await;
    let registry = ctx.bash_background();
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::FateUnknown, "{done:?}");
    assert!(
        done.output_preview
            .contains("command never started on ck-motor"),
        "{done:?}"
    );
    let saved = read_task(&paths.json).unwrap();
    assert_eq!(saved.remote.unwrap().terminal.unwrap().ran, Some(Ran::None));
    assert!(!saved.local_fallback_started);
    let log = daemon.log().lock().unwrap();
    let attaches: Vec<_> = log
        .iter()
        .filter(|(_, body)| body["method"] == "exec.attach")
        .collect();
    assert_eq!(attaches.len(), 1);
    assert_eq!(attaches[0].1["params"]["job_id"], id().to_string());
    assert!(!log.iter().any(|(_, body)| body["method"] == "exec.run"));
}

#[tokio::test]
async fn remote_restart_unaccepted_without_job_id_settles_without_resubmission() {
    for keyed in [false, true] {
        let daemon = daemon(Script::PlacementWait { job_id: false }, "exec-remote/v1").await;
        let (_dir, ctx, task_id, paths) =
            restart_unaccepted_submission(daemon.connection().clone(), None, keyed).await;
        let registry = ctx.bash_background();
        let done = terminal(&registry, &task_id).await;
        assert_eq!(done.info.status, BgTaskStatus::FateUnknown, "{done:?}");
        assert!(
            done.output_preview.contains("command was not resubmitted"),
            "{done:?}"
        );
        assert!(!read_task(&paths.json).unwrap().local_fallback_started);
        let log = daemon.log().lock().unwrap();
        assert!(!log
            .iter()
            .any(|(_, body)| matches!(body["method"].as_str(), Some("exec.run" | "exec.attach"))));
    }
}

#[tokio::test]
async fn remote_restart_unaccepted_queued_progress_reaches_bash_status() {
    use aft_engine::test_bridge::bash_remote::remote_wire::{outage_daemon_with_replay, OutageReplay};
    let daemon = outage_daemon_with_replay(OutageReplay::Queued).await;
    let (_dir, ctx, task_id, paths) =
        restart_unaccepted_submission(daemon.connection().clone(), Some(id()), true).await;
    let task = ctx.bash_background().test_task(&task_id).unwrap();
    wait_for(
        "attach queue progress must finish publishing its status",
        || {
            let mut state = task.test_state().lock().unwrap();
            if state.remote_admission_note().as_deref() != Some("queued on the runner (42 s)") {
                return None;
            }
            // Metadata is saved before the ephemeral line is published. Wait for
            // both, then remove the line so bash_status must render the phase note.
            assert_eq!(
                state.metadata().remote.as_ref().unwrap().queue_wait_ms,
                Some(42_000)
            );
            *state.remote_admission_note_mut() = None;
            assert_eq!(
                remote_phase_note(&state.metadata(), unix_millis()).as_deref(),
                Some("queued on the runner (42 s)")
            );
            Some(())
        },
    )
    .await;
    let request: crate::protocol::RawRequest = serde_json::from_value(serde_json::json!({
        "id": "status", "command": "bash_status", "session_id": "session",
        "params": { "task_id": task_id },
    }))
    .unwrap();
    let response = crate::commands::bash_status::handle(&request, &ctx);
    assert!(response.success, "{response:?}");
    assert_eq!(response.data["phase"], "queued", "{response:?}");
    let status = rendered_status(&ctx, &task_id);
    assert!(status.contains("queued on the runner (42 s)"), "{status}");
    assert!(!status.contains("capturing inputs"), "{status}");
    let saved = read_task(&paths.json).unwrap();
    let remote = saved.remote.unwrap();
    assert!(!remote.accepted);
    assert_eq!(remote.last_seq, None);
    assert_eq!(remote.queue_position, None);
    assert_eq!(remote.stdout_len, 0);
    assert!(daemon.methods("exec.run").is_empty());
    assert_eq!(daemon.methods("exec.attach").len(), 1);
    daemon.finish_job();
    assert_eq!(
        terminal(ctx.bash_background(), &task_id).await.info.status,
        BgTaskStatus::Completed
    );
}

#[tokio::test]
async fn remote_queue_wait_wire_vectors_reach_bash_status_as_queued_progress() {
    use aft_engine::test_bridge::bash_remote::remote_wire::{outage_daemon_with_replay, OutageReplay};
    use sha2::{Digest, Sha256};
    // Published 0.2.22 progress vectors, including an unknown waiting reason.
    // Send their bytes unchanged: enum construction can conceal pin changes.
    for (bytes, digest) in [
        (
            include_bytes!("../../../../../aft-engine/src/exec_remote/fixtures/progress/queue-wait.jcs").as_slice(),
            include_str!("../../../../../aft-engine/src/exec_remote/fixtures/progress/queue-wait.sha256"),
        ),
        (
            include_bytes!("../../../../../aft-engine/src/exec_remote/fixtures/progress/queue-wait-unknown-reason.jcs")
                .as_slice(),
            include_str!("../../../../../aft-engine/src/exec_remote/fixtures/progress/queue-wait-unknown-reason.sha256"),
        ),
    ] {
        assert_eq!(format!("{:x}", Sha256::digest(bytes)), digest.trim());
        let daemon = outage_daemon_with_replay(OutageReplay::QueuedWire(bytes)).await;
        let (_dir, ctx, task_id, paths) =
            restart_unaccepted_submission(daemon.connection().clone(), Some(id()), true).await;
        let task = ctx.bash_background().test_task(&task_id).unwrap();
        wait_for("published queue-wait bytes must reach bash status", || {
            let mut state = task.test_state().lock().unwrap();
            if state.remote_admission_note().as_deref() != Some("queued on the runner (0 s)") {
                return None;
            }
            assert_eq!(
                state.metadata().remote.as_ref().unwrap().queue_wait_ms,
                Some(42)
            );
            *state.remote_admission_note_mut() = None;
            Some(())
        })
        .await;
        let request: crate::protocol::RawRequest = serde_json::from_value(serde_json::json!({
            "id":"status", "command":"bash_status", "session_id":"session", "params":{"task_id":task_id},
        })).unwrap();
        let response = crate::commands::bash_status::handle(&request, &ctx);
        assert!(response.success, "{response:?}");
        assert_eq!(response.data["phase"], "queued", "{response:?}");
        assert!(rendered_status(&ctx, &task_id).contains("queued on the runner (0 s)"));
        let remote = read_task(&paths.json).unwrap().remote.unwrap();
        assert_eq!(remote.queue_wait_ms, Some(42));
        assert!(!remote.accepted);
        assert_eq!(remote.last_seq, None);
        assert_eq!(remote.stdout_len, 0);
        assert!(daemon.methods("exec.run").is_empty());
        daemon.finish_job();
        assert_eq!(
            terminal(ctx.bash_background(), &task_id).await.info.status,
            BgTaskStatus::Completed
        );
    }
}

#[tokio::test]
async fn remote_restart_unaccepted_quiet_attach_waits_without_claiming_capture() {
    use aft_engine::test_bridge::bash_remote::remote_wire::{outage_daemon_with_replay, OutageReplay};
    let daemon = outage_daemon_with_replay(OutageReplay::Silent).await;
    let (_dir, ctx, task_id, paths) =
        restart_unaccepted_submission(daemon.connection().clone(), Some(id()), false).await;
    wait_for("startup must subscribe to the saved job", || {
        (!daemon.methods("exec.attach").is_empty()).then_some(())
    })
    .await;
    let status = rendered_status(&ctx, &task_id);
    assert!(
        status.contains(
            "waiting on the runner (ck-motor has not reported a queue position or any output yet)"
        ),
        "{status}"
    );
    assert!(!status.contains("capturing inputs"), "{status}");
    assert!(!status.contains("queued on the runner"), "{status}");
    let saved = read_task(&paths.json).unwrap();
    assert!(!saved.remote.as_ref().unwrap().accepted);
    assert!(serde_json::to_value(saved).unwrap()["remote"]
        .get("queue_wait_ms")
        .is_none());
    assert!(daemon.methods("exec.run").is_empty());
    assert_eq!(daemon.methods("exec.attach").len(), 1);
    daemon.finish_job();
    assert_eq!(
        terminal(ctx.bash_background(), &task_id).await.info.status,
        BgTaskStatus::Completed
    );
}

#[tokio::test]
async fn restart_delivered_unknown_kill_restores_owner_on_newer_epoch() {
    let _storage_env = crate::test_env::without_storage_override();
    let daemon = daemon(Script::Cancel, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let (task, paths, ctx) =
        delivered_unknown_restart_fixture(dir.path(), daemon.connection().clone(), false);
    let before = read_task(&paths.json).unwrap();
    let mut scope = before.scope.clone().unwrap();
    scope.scope_epoch += 1;
    let reply = kill_restarted_unknown(ctx, task, "session", Some(scope)).await;
    assert!(
        reply.success,
        "owning session must recover its scoped delivered task: {reply:?}"
    );
    assert_eq!(reply.data["status"], "fate_unknown");
    assert_eq!(reply.data["remote_cancel"], "runner confirmed cancellation");
    let requests = cancel_requests(&daemon);
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["params"]["job_id"], id().to_string());
    let after = read_task(&paths.json).unwrap();
    assert_eq!(after.status, before.status);
    assert_eq!(after.scope, before.scope);
    assert_eq!(
        after.remote.unwrap().terminal,
        before.remote.unwrap().terminal
    );
    assert!(after.completion_delivered);
}

#[tokio::test]
async fn restart_delivered_unknown_kill_refuses_other_scope_ref() {
    let _storage_env = crate::test_env::without_storage_override();
    let daemon = daemon(Script::Cancel, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let (task, _, ctx) =
        delivered_unknown_restart_fixture(dir.path(), daemon.connection().clone(), false);
    let mut scope = owner_scope("other-scope");
    scope.scope_epoch = 10;
    let reply = kill_restarted_unknown(ctx, task, "session", Some(scope)).await;
    assert!(
        !reply.success,
        "another scope ref must not restore the task: {reply:?}"
    );
    assert_eq!(reply.data["code"], "task_not_found");
    assert!(cancel_requests(&daemon).is_empty());
}

#[tokio::test]
async fn restart_delivered_unknown_kill_refuses_lower_epoch() {
    let _storage_env = crate::test_env::without_storage_override();
    let daemon = daemon(Script::Cancel, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let (task, _, ctx) =
        delivered_unknown_restart_fixture(dir.path(), daemon.connection().clone(), false);
    let mut scope = owner_scope("unknown-outcome");
    scope.scope_epoch = 8;
    let reply = kill_restarted_unknown(ctx, task, "session", Some(scope)).await;
    assert!(
        !reply.success,
        "an older scope epoch must not restore the task: {reply:?}"
    );
    assert_eq!(reply.data["code"], "task_not_found");
    assert!(cancel_requests(&daemon).is_empty());
}

#[tokio::test]
async fn restart_delivered_unknown_kill_refuses_foreign_callers() {
    let _storage_env = crate::test_env::without_storage_override();
    let mut other_owner = owner_scope("unknown-outcome");
    other_owner.scope_epoch = 10;
    other_owner.owner = subc_protocol::Principal::Reserved {
        module_id: "other-owner".into(),
    };
    let mut owner = owner_scope("unknown-outcome");
    owner.scope_epoch = 10;
    for (session, scope) in [
        ("other-session", Some(owner)),
        ("session", Some(other_owner)),
        ("session", None),
    ] {
        let daemon = daemon(Script::Cancel, "exec-remote/v1").await;
        let dir = tempfile::tempdir().unwrap();
        let (task, _, ctx) =
            delivered_unknown_restart_fixture(dir.path(), daemon.connection().clone(), false);
        let reply = kill_restarted_unknown(ctx, task, session, scope).await;
        assert!(
            !reply.success,
            "foreign caller must not restore the task: {reply:?}"
        );
        assert_eq!(reply.data["code"], "task_not_found");
        assert!(cancel_requests(&daemon).is_empty());
    }
}

#[tokio::test]
async fn restart_delivered_unknown_kill_refuses_other_project_or_harness() {
    let _storage_env = crate::test_env::without_storage_override();
    for other_harness in [false, true] {
        let daemon = daemon(Script::Cancel, "exec-remote/v1").await;
        let dir = tempfile::tempdir().unwrap();
        let (task, paths, ctx) =
            delivered_unknown_restart_fixture(dir.path(), daemon.connection().clone(), false);
        if other_harness {
            ctx.update_config(|config| config.harness = Some(crate::harness::Harness::Pi));
            ctx.bash_background()
                .set_harness(crate::harness::Harness::Pi);
        } else {
            let other_project = dir.path().join("other-project");
            fs::create_dir_all(&other_project).unwrap();
            ctx.update_config(|config| config.project_root = Some(other_project));
        }
        let scope = read_task(&paths.json).unwrap().scope;
        let reply = kill_restarted_unknown(ctx, task, "session", scope).await;
        assert!(
            !reply.success,
            "foreign namespace must not restore the task: {reply:?}"
        );
        assert_eq!(reply.data["code"], "task_not_found");
        assert!(cancel_requests(&daemon).is_empty());
    }
}

#[tokio::test]
async fn restart_delivered_unknown_kill_keeps_unscoped_session_recovery() {
    let _storage_env = crate::test_env::without_storage_override();
    let daemon = daemon(Script::Cancel, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let (task, paths, ctx) =
        delivered_unknown_restart_fixture(dir.path(), daemon.connection().clone(), false);
    let mut metadata = read_task(&paths.json).unwrap();
    metadata.scope = None;
    write_task(&paths.json, &metadata).unwrap();
    let reply = kill_restarted_unknown(ctx, task, "session", None).await;
    assert!(
        reply.success,
        "an unscoped saved task keeps session ownership: {reply:?}"
    );
    assert_eq!(reply.data["remote_cancel"], "runner confirmed cancellation");
    assert_eq!(cancel_requests(&daemon).len(), 1);
}

#[tokio::test]
async fn restart_delivered_unknown_kill_past_timeout_keeps_finished_answer() {
    let _storage_env = crate::test_env::without_storage_override();
    let daemon = daemon(Script::Cancel, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let (task, paths, ctx) =
        delivered_unknown_restart_fixture(dir.path(), daemon.connection().clone(), true);
    let scope = read_task(&paths.json).unwrap().scope;
    let reply = kill_restarted_unknown(ctx, task, "session", scope).await;
    assert!(
        reply.success,
        "an expired owned task still has its finished answer: {reply:?}"
    );
    assert_eq!(reply.data["status"], "fate_unknown");
    assert!(reply.data.get("remote_cancel").is_none());
    assert!(cancel_requests(&daemon).is_empty());
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
            persist_accepted_with_snapshot(dir.path(), daemon.connection().clone(), &command);
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
        let task = ctx.bash_background().test_task(task).unwrap();
        assert!(task.test_state().lock().unwrap().metadata().remote.is_none());
        outputs.push((
            fs::read(&task.paths().stdout).unwrap(),
            fs::read(&task.paths().stderr).unwrap(),
            crate::commands::bash_orchestrate::format_foreground_result(&done),
        ));
    }
    assert_eq!(outputs[0], outputs[1]);
    assert_eq!(outputs[0].0, b"local-proof");
    assert_eq!(outputs[0].2, "local-proof");
}

/// Run `params` through the bash command handler with `policy` installed as
/// the session's remote policy, as the subc tool-call path does.
fn handle_with_policy(
    ctx: &crate::context::AppContext,
    policy: Option<crate::bash_background::RemoteLaunch>,
    mut params: serde_json::Value,
) -> crate::protocol::Response {
    params
        .as_object_mut()
        .unwrap()
        .entry("workdir")
        .or_insert_with(|| serde_json::json!(ctx.config().project_root));
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

/// `message` is the exact refusal the bash response must carry.
pub(crate) fn assert_siblings_submit_refused_before_connect(
    ctx: &crate::context::AppContext,
    launch: crate::bash_background::RemoteLaunch,
    daemon: &aft_engine::test_bridge::bash_remote::remote_wire::Daemon,
    message: &str,
) {
    let marker = ctx
        .config()
        .project_root
        .as_ref()
        .unwrap()
        .join("must-not-run-locally");
    let response = handle_with_policy(
        ctx,
        Some(launch),
        serde_json::json!({
            "command": format!("touch '{}'", marker.display()), "runon":"linux",
            "background":true, "compressed":false,
        }),
    );
    assert!(!response.success, "{response:?}");
    assert_eq!(response.data["code"], "remote_run_refused", "{response:?}");
    assert!(
        ctx.bash_background().list(0).is_empty(),
        "a refused sibling must not register a remote or local task"
    );
    assert_eq!(
        response.data["message"].as_str().unwrap(),
        message,
        "{response:?}"
    );
    assert!(
        !marker.exists(),
        "a refused sibling must not trigger local fallback"
    );
    assert!(
        daemon.log().lock().unwrap().is_empty(),
        "no daemon frame may be sent"
    );
    assert_eq!(
        daemon.connections().load(Ordering::SeqCst),
        0,
        "the daemon must not be contacted"
    );
}

/// Submit a runon command under `launch`, wait until its task completes, and
/// return the parameters of the one `exec.run` the daemon received.
pub(crate) async fn submit_runon_for_test(
    ctx: &crate::context::AppContext,
    launch: crate::bash_background::RemoteLaunch,
    daemon: &aft_engine::test_bridge::bash_remote::remote_wire::Daemon,
) -> serde_json::Value {
    let response = handle_with_policy(
        ctx,
        Some(launch),
        serde_json::json!({"command":"printf remote", "runon":"linux", "compressed":false}),
    );
    assert!(response.success, "{response:?}");
    let done = terminal(
        ctx.bash_background(),
        response.data["task_id"].as_str().unwrap(),
    )
    .await;
    assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");
    let mut runs = exec_runs(daemon);
    assert_eq!(runs.len(), 1, "{runs:?}");
    runs.remove(0)
}

#[test]
fn runon_siblings_refusal_line_is_pinned_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    let folder = ctx
        .config()
        .project_root
        .as_ref()
        .unwrap()
        .join("not-a-checkout");
    std::fs::create_dir(&folder).unwrap();
    let mut policy = launch(dir.path().join("unused-connection.json"));
    policy.params.siblings = vec![folder.to_str().unwrap().into()];
    let response = handle_with_policy(
        &ctx,
        Some(policy),
        serde_json::json!({"command":"true", "runon":"linux", "background":true}),
    );
    assert!(!response.success, "{response:?}");
    assert_eq!(response.data["code"], "remote_run_refused");
    assert_eq!(
        response.data["message"].as_str().unwrap().as_bytes(),
        format!(
            "runon refused: sibling {} is not the root of a git checkout; the command was not run",
            folder.display()
        )
        .as_bytes()
    );
}

#[tokio::test]
async fn runon_windows_not_advertised_refuses_without_remote_or_local_spawn() {
    let daemon = daemon(Script::Plain, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    let marker = dir.path().join("local-spawn-proof");
    let (reply, _) = orchestrated_reply(
        &ctx,
        &daemon,
        serde_json::json!({
            "command": format!("touch '{}'", marker.display()),
            "runon": "windows", "wait": true, "foreground_orchestrate": true,
            "compressed": false,
        }),
    )
    .await;
    assert!(!reply.success, "{reply:?}");
    assert_eq!(reply.data["code"], "remote_unavailable", "{reply:?}");
    let message = reply.data["message"].as_str().unwrap();
    assert!(message.contains("windows"), "{message}");
    assert!(
        message.contains("doesn't offer Windows jobs yet"),
        "{message}"
    );
    assert!(
        exec_runs(&daemon).is_empty(),
        "unadvertised Windows must never be sent"
    );
    assert!(
        !marker.exists(),
        "explicit Windows must never fall back locally"
    );
}

#[tokio::test]
async fn runon_windows_advertised_preserves_request_mapping_and_full_exit_code() {
    for code in [256, 0xC000_0005, u32::MAX] {
        let daemon = windows_daemon(Some(code), true).await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = restarted_context(dir.path());
        let storage = dir.path().join("storage");
        ctx.update_config(|config| config.storage_dir = Some(storage.clone()));
        let (reply, _) = orchestrated_reply(
            &ctx,
            &daemon,
            serde_json::json!({
                "command": "cargo test", "runon": "windows", "wait": true,
                "foreground_orchestrate": true, "compressed": false,
            }),
        )
        .await;
        assert!(reply.success, "{reply:?}");
        assert_eq!(reply.data["status"], "failed", "{reply:?}");
        assert_eq!(reply.data["exit_code"], u64::from(code), "{reply:?}");
        assert!(
            reply.data["output"]
                .as_str()
                .unwrap()
                .contains(&format!("[exit code: {code}]")),
            "{reply:?}"
        );
        let runs = exec_runs(&daemon);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0]["platform"], "windows");
        assert!(runs[0].get("weight_hint").is_none());
        assert!(runs[0].get("network").is_none());
        let task_id = reply.data["task_id"].as_str().unwrap();
        let task = ctx.bash_background().test_task(task_id).unwrap();
        let saved = read_task(&task.paths().json).unwrap();
        assert_eq!(saved.exit_code, Some(i64::from(code)));
        let conn = crate::db::open(&dir.path().join("roundtrip.db")).unwrap();
        let row = saved.to_bash_task_row("broca", &task.paths()).unwrap();
        crate::db::bash_tasks::upsert_bash_task(&conn, &row).unwrap();
        let restored_row = crate::db::bash_tasks::get_bash_task(&conn, "broca", "session", task_id)
            .unwrap()
            .unwrap();
        assert_eq!(restored_row.exit_code, Some(i64::from(code)));
        assert_eq!(
            saved
                .remote
                .as_ref()
                .unwrap()
                .started
                .as_ref()
                .unwrap()
                .path_mapping,
            Some(PathMapping::new("/", r"C:\m"))
        );
        let restarted = registry();
        restarted.replay_session(&storage, "session").unwrap();
        let snapshot = restarted.observed_status(task_id, "session", 8192).unwrap();
        assert_eq!(snapshot.exit_code, Some(i64::from(code)));
        let completions = restarted.drain_completions();
        assert!(
            completions
                .iter()
                .any(|completion| completion.task_id == task_id
                    && completion.exit_code == Some(i64::from(code))),
            "{completions:?}"
        );
    }
}

#[tokio::test]
async fn runon_windows_unknown_offline_outcome_keeps_safe_rerun_advice() {
    let daemon = windows_daemon(None, true).await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    let (reply, _) = orchestrated_reply(
        &ctx,
        &daemon,
        serde_json::json!({
            "command": "cargo test", "runon": "windows", "wait": true,
            "foreground_orchestrate": true, "compressed": false,
        }),
    )
    .await;
    assert_eq!(reply.data["status"], "fate_unknown", "{reply:?}");
    assert!(
        reply.data["output"]
            .as_str()
            .unwrap()
            .contains("rerunning is safe"),
        "{reply:?}"
    );
    assert_eq!(exec_runs(&daemon).len(), 1);
}

#[tokio::test]
async fn runon_windows_unacknowledged_platform_cancels_and_distrusts_exit() {
    let daemon = windows_daemon(Some(0), false).await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    let (reply, _) = orchestrated_reply(
        &ctx,
        &daemon,
        serde_json::json!({
            "command": "cargo test", "runon": "windows", "wait": true,
            "foreground_orchestrate": true, "compressed": false,
        }),
    )
    .await;
    assert_eq!(reply.data["status"], "fate_unknown", "{reply:?}");
    assert!(reply.data["exit_code"].is_null(), "{reply:?}");
    assert!(
        reply.data["output"]
            .as_str()
            .unwrap()
            .contains("Windows platform was not acknowledged"),
        "{reply:?}"
    );
    assert!(daemon
        .log()
        .lock()
        .unwrap()
        .iter()
        .any(|(_, body)| body["method"] == "exec.cancel"));
    assert_eq!(exec_runs(&daemon).len(), 1);
}

#[tokio::test]
async fn runon_network_is_present_on_wire_only_when_requested() {
    for (runon, network, cpus) in [
        ("linux", None, None),
        ("linux,4c", None, Some(4)),
        ("linux,net", Some("outbound"), None),
        ("linux,4c,net", Some("outbound"), Some(4)),
        ("linux,net,4c", Some("outbound"), Some(4)),
        ("", Some("outbound"), Some(4)),
    ] {
        let daemon = daemon(Script::NetworkGranted, "exec-remote/v1").await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = restarted_context(dir.path());
        let mut policy = launch(daemon.connection().clone());
        policy.params.remote_exec.as_mut().unwrap().default_demand = Some("linux,net,4c".into());
        let response = handle_with_policy(
            &ctx,
            Some(policy),
            serde_json::json!({
                "command": "printf network", "runon": runon, "compressed": false
            }),
        );
        assert!(response.success, "{runon}: {response:?}");
        let done = terminal(
            ctx.bash_background(),
            response.data["task_id"].as_str().unwrap(),
        )
        .await;
        let runs = exec_runs(&daemon);
        assert_eq!(runs.len(), 1);
        let captured: RunRequest = serde_json::from_value(runs[0].clone()).unwrap();
        assert_eq!(
            captured.network,
            network.map(|_| Network::Outbound),
            "{runon}: {runs:?}"
        );
        assert_eq!(
            runs[0].get("network").and_then(serde_json::Value::as_str),
            network,
            "{runon}: {runs:?}"
        );
        assert_eq!(captured.weight_hint, cpus, "{runon}: {runs:?}");
        if network.is_some() {
            let expected = if cpus.is_some() {
                "ran remotely on ck-motor (4 vCPUs requested, job 0192a64a-1234-7000-8000-000000000001, network)"
            } else {
                "ran remotely on ck-motor (job 0192a64a-1234-7000-8000-000000000001, network)"
            };
            assert_eq!(done.output_preview.lines().next(), Some(expected));
            assert!(!done.output_preview.contains("OFFLINE"), "{done:?}");
        }
    }
}

#[tokio::test]
async fn runon_timeout_uses_the_task_limit_and_preserves_long_explicit_timeouts() {
    for (timeout_ms, expected_seconds) in [(None, 30 * 60), (Some(7_201_000), 7_201)] {
        let daemon = daemon(Script::Plain, "exec-remote/v1").await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = restarted_context(dir.path());
        let mut params = serde_json::json!({
            "command": "printf timeout-proof",
            "runon": "linux,4c",
            "compressed": false
        });
        if let Some(timeout_ms) = timeout_ms {
            params["timeout"] = serde_json::json!(timeout_ms);
        }

        let response = handle_with_policy(&ctx, Some(launch(daemon.connection().clone())), params);
        assert!(response.success, "{response:?}");
        let done = terminal(
            ctx.bash_background(),
            response.data["task_id"].as_str().unwrap(),
        )
        .await;
        assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");

        let runs = exec_runs(&daemon);
        assert_eq!(runs.len(), 1);
        let captured: crate::exec_remote::types::RunRequest =
            serde_json::from_value(runs[0].clone()).unwrap();
        assert_eq!(
            captured.timeout,
            Some(expected_seconds),
            "the protocol timeout is required and measured in seconds: {runs:?}"
        );
        assert_eq!(runs[0]["timeout"], expected_seconds);
        assert_eq!(captured.weight_hint, Some(4));
    }
}

#[tokio::test]
async fn runon_queue_wait_limit_follows_only_explicit_timeouts() {
    for background in [false, true] {
        for (timeout_ms, expected_seconds) in [
            (None, None),
            (Some(5_400_000), Some(5_400)),
            (Some(5_400_001), Some(5_401)),
            (Some(30_001), Some(31)),
        ] {
            let daemon = daemon(Script::Plain, "exec-remote/v1").await;
            let dir = tempfile::tempdir().unwrap();
            ensure_git_fixture(dir.path());
            let ctx = crate::test_context(
                Box::new(crate::parser::TreeSitterProvider::new()),
                crate::config::Config {
                    project_root: Some(dir.path().into()),
                    experimental_bash_background: true,
                    remote_exec: crate::config::RemoteExecConfig {
                        enabled: true,
                        ..Default::default()
                    },
                    ..Default::default()
                },
            );
            let mut params = serde_json::json!({
                "command": "printf queue-timeout-proof",
                "runon": "linux,4c",
                "background": background,
                "compressed": false
            });
            if let Some(timeout_ms) = timeout_ms {
                params["timeout"] = serde_json::json!(timeout_ms);
            }

            let response =
                handle_with_policy(&ctx, Some(launch(daemon.connection().clone())), params);
            assert!(response.success, "{response:?}");
            let done = terminal(
                ctx.bash_background(),
                response.data["task_id"].as_str().unwrap(),
            )
            .await;
            assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");

            let runs = exec_runs(&daemon);
            assert_eq!(runs.len(), 1);
            let captured: RunRequest = serde_json::from_value(runs[0].clone()).unwrap();
            assert_eq!(captured.queue_wait_limit_s, expected_seconds, "{runs:?}");
            assert_eq!(
                runs[0].get("queue_wait_limit_s").cloned(),
                expected_seconds.map(serde_json::Value::from),
                "background={background}, timeout_ms={timeout_ms:?}: {runs:?}"
            );
        }
    }
}

#[tokio::test]
async fn local_bash_keeps_its_caller_timeout_without_remote_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context_with_runon(dir.path(), false);
    let response = handle_with_policy(
        &ctx,
        None,
        serde_json::json!({
            "command": "printf local-timeout-proof",
            "timeout": 10_000,
            "compressed": false
        }),
    );
    assert!(response.success, "{response:?}");
    let task_id = response.data["task_id"].as_str().unwrap();
    let done = terminal(ctx.bash_background(), task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");
    assert_eq!(done.output_preview, "local-timeout-proof");

    let task = ctx.bash_background().test_task(task_id).unwrap();
    let state = task.test_state().lock().unwrap();
    assert!(state.metadata().remote.is_none());
    assert_eq!(state.metadata().timeout_ms, Some(10_000));
}

#[tokio::test]
async fn runon_network_not_granted_is_plainly_offline_in_reply_and_persistence() {
    for script in [Script::Plain, Script::NetworkUnknown] {
        let daemon = daemon(script, "exec-remote/v1").await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = restarted_context(dir.path());
        let response = handle_with_policy(
            &ctx,
            Some(launch(daemon.connection().clone())),
            serde_json::json!({
                "command": "printf offline", "runon": "linux,net", "compressed": false
            }),
        );
        assert!(response.success, "{response:?}");
        let task_id = response.data["task_id"].as_str().unwrap();
        let done = terminal(ctx.bash_background(), task_id).await;
        assert_eq!(done.info.status, BgTaskStatus::Completed);
        let expected = "OFFLINE: the runner did not grant the requested network access";
        assert!(done.output_preview.contains(expected), "{done:?}");
        assert!(
            crate::commands::bash_orchestrate::format_foreground_result(&done).contains(expected)
        );
        let task = ctx.bash_background().test_task(task_id).unwrap();
        let persisted = read_task(&task.paths().json).unwrap();
        assert!(persisted.execution_note.unwrap().contains(expected));
        let remote = persisted.remote.unwrap();
        assert!(remote.requested_network);
        assert!(!remote.granted_network);
    }
}

#[tokio::test]
async fn runon_network_unsupported_refuses_by_name_without_local_spawn() {
    let daemon = daemon(Script::NetworkUnsupported, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    let marker = dir.path().join("must-not-run");
    let response = handle_with_policy(
        &ctx,
        Some(launch(daemon.connection().clone())),
        serde_json::json!({
            "command": format!("touch '{}'", marker.display()), "runon": "linux,net"
        }),
    );
    assert!(response.success, "{response:?}");
    let done = terminal(
        ctx.bash_background(),
        response.data["task_id"].as_str().unwrap(),
    )
    .await;
    let refusal = refusal_response(done);
    assert_runon_refusal(
        &refusal,
        "network_unsupported (outbound access disabled by runner policy)",
    );
    assert!(
        refusal.data["message"]
            .as_str()
            .unwrap()
            .contains("outbound access disabled by runner policy"),
        "{refusal:?}"
    );
    assert!(!marker.exists());
    assert_eq!(exec_runs(&daemon).len(), 1);
}

#[tokio::test]
async fn runon_network_unknown_outcome_warns_only_for_granted_outbound() {
    for (script, runon, granted) in [
        (Script::NetworkLost, "linux,net", true),
        (Script::Lost, "linux,net", false),
        (Script::Lost, "linux", false),
    ] {
        let daemon = daemon(script, "exec-remote/v1").await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = restarted_context(dir.path());
        let response = handle_with_policy(
            &ctx,
            Some(launch(daemon.connection().clone())),
            serde_json::json!({
                "command": "printf uncertain", "runon": runon, "compressed": false
            }),
        );
        assert!(response.success, "{response:?}");
        let task_id = response.data["task_id"].as_str().unwrap();
        let done = terminal(ctx.bash_background(), task_id).await;
        assert_eq!(done.info.status, BgTaskStatus::FateUnknown);
        let text = crate::commands::bash_orchestrate::format_foreground_result(&done);
        assert!(
            text.contains(&format!("remote outcome unknown (job {})", id())),
            "{text}"
        );
        assert_eq!(text.contains("rerunning is safe"), !granted, "{text}");
        assert_eq!(
            text.contains("may have had outside effects"),
            granted,
            "{text}"
        );
        assert_eq!(text.contains("check before rerunning"), granted, "{text}");
        assert_eq!(
            exec_runs(&daemon).len(),
            1,
            "unknown outcomes must never be resubmitted"
        );
        let task = ctx.bash_background().test_task(task_id).unwrap();
        let persisted = read_task(&task.paths().json).unwrap();
        assert_eq!(persisted.remote.unwrap().granted_network, granted);
        assert_eq!(persisted.status_reason, done.info.status_reason);
    }
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
        Some(launch(daemon.connection().clone())),
        serde_json::json!({"command": line, "runon": "linux", "compressed": false}),
    );
    assert!(response.success, "{response:?}");
    let task_id = response.data["task_id"].as_str().unwrap().to_string();
    let done = terminal(ctx.bash_background(), &task_id).await;
    let runs = exec_runs(&daemon);
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0]["command"], line);
    assert!(runs[0].get("weight_hint").is_none(), "{runs:?}");
    assert_eq!(
        Path::new(runs[0]["cwd"].as_str().unwrap()),
        dir.path(),
        "the remote run uses the call's working directory"
    );
    let rendered = crate::commands::bash_orchestrate::format_foreground_result(&done);
    assert_eq!(
        rendered.lines().next(),
        Some("ran remotely on ck-motor (job 0192a64a-1234-7000-8000-000000000001)"),
        "{rendered}"
    );
    // A report without a changed-file list says so, after the output. The
    // other workspace reports stay silent when absent.
    assert!(
        rendered.ends_with("changed files: not reported by the runner"),
        "{rendered}"
    );
}

#[tokio::test]
async fn runon_macos_reaches_the_wire_request_as_macos_platform() {
    let daemon = daemon(Script::Plain, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    let response = handle_with_policy(
        &ctx,
        Some(launch(daemon.connection().clone())),
        serde_json::json!({"command": "uname -a", "runon": "macos", "compressed": false}),
    );
    assert!(response.success, "{response:?}");
    let task_id = response.data["task_id"].as_str().unwrap().to_string();
    let _done = terminal(ctx.bash_background(), &task_id).await;
    let runs = exec_runs(&daemon);
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0]["platform"], "macos");
    assert!(runs[0].get("weight_hint").is_none(), "{runs:?}");
    assert!(runs[0].get("network").is_none(), "{runs:?}");
}

#[tokio::test]
async fn runon_memory_is_sent_only_when_requested_and_supported() {
    use aft_engine::test_bridge::bash_remote::remote_wire::daemon_with_capabilities;
    for (runon, supports_memory, expected) in [
        ("linux", false, None),
        ("linux", true, None),
        ("linux,16gb", true, Some(16_384)),
        ("linux,net,16gb,2c", true, Some(16_384)),
        ("", true, Some(16_384)),
    ] {
        let claims = if supports_memory {
            vec!["exec-remote/v1", exec::types::MEMORY_CAPABILITY]
        } else {
            vec!["exec-remote/v1"]
        };
        let daemon = daemon_with_capabilities(Script::NetworkGranted, &claims, 1).await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = restarted_context(dir.path());
        let mut policy = launch(daemon.connection().clone());
        policy.params.remote_exec.as_mut().unwrap().default_demand = Some("linux,16gb".into());
        let response = handle_with_policy(
            &ctx,
            Some(policy),
            serde_json::json!({"command": "printf memory", "runon": runon, "compressed": false}),
        );
        assert!(response.success, "{runon}: {response:?}");
        let done = terminal(
            ctx.bash_background(),
            response.data["task_id"].as_str().unwrap(),
        )
        .await;
        assert_eq!(done.exit_code, Some(0), "{done:?}");
        let runs = exec_runs(&daemon);
        assert_eq!(runs.len(), 1, "{runon}: {runs:?}");
        assert_eq!(
            runs[0]
                .get("memory_mib")
                .and_then(serde_json::Value::as_u64),
            expected,
            "{runon}: {runs:?}"
        );
        if expected.is_none() {
            assert!(runs[0].get("memory_mib").is_none(), "{runs:?}");
        }
    }
}

#[tokio::test]
async fn runon_memory_without_capability_refuses_before_submission() {
    let daemon = daemon(Script::Plain, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    let marker = dir.path().join("local-spawn-proof");
    let (reply, _) = orchestrated_reply(
        &ctx,
        &daemon,
        serde_json::json!({
            "command": format!("touch '{}'", marker.display()), "runon": "linux,16gb",
        }),
    )
    .await;
    assert!(!reply.success, "{reply:?}");
    let message = reply.data["message"].as_str().unwrap();
    assert!(
        message
            .contains("This build server can't take a memory size yet; run the job without one."),
        "{message}"
    );
    assert!(exec_runs(&daemon).is_empty(), "nothing may be submitted");
    assert!(!marker.exists(), "a refused runon never runs locally");
}

#[tokio::test]
async fn runon_vcpu_count_is_sent_as_weight_hint_on_the_wire() {
    let daemon = daemon(Script::Plain, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    let response = handle_with_policy(
        &ctx,
        Some(launch(daemon.connection().clone())),
        serde_json::json!({"command": "printf vcpu", "runon": "linux,4c", "compressed": false}),
    );
    assert!(response.success, "{response:?}");
    let task_id = response.data["task_id"].as_str().unwrap().to_string();
    let done = terminal(ctx.bash_background(), &task_id).await;
    let runs = exec_runs(&daemon);
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0]["weight_hint"], 4);
    let rendered = crate::commands::bash_orchestrate::format_foreground_result(&done);
    assert_eq!(
        rendered.lines().next(),
        Some("ran remotely on ck-motor (4 vCPUs requested, job 0192a64a-1234-7000-8000-000000000001)"),
        "{rendered}"
    );
}

#[tokio::test]
async fn runon_uses_vcpu_count_from_plan_default_demand() {
    let daemon = daemon(Script::Plain, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    let mut policy = launch(daemon.connection().clone());
    policy.params.remote_exec.as_mut().unwrap().default_demand = Some("linux,4c".into());
    let response = handle_with_policy(
        &ctx,
        Some(policy),
        serde_json::json!({"command": "printf default", "runon": "", "compressed": false}),
    );
    assert!(response.success, "{response:?}");
    let task_id = response.data["task_id"].as_str().unwrap().to_string();
    let _done = terminal(ctx.bash_background(), &task_id).await;
    let runs = exec_runs(&daemon);
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0]["weight_hint"], 4);
}

#[tokio::test]
async fn malformed_runon_vcpu_requests_are_refused_before_dispatch() {
    let daemon = daemon(Script::Plain, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("ran-locally");
    let command = format!("printf x > '{}'", marker.display());
    for runon in [
        "linux,0c",
        "linux,4",
        "linux,4cores",
        "linux,c",
        "linux,4c,2c",
        "linux,net,net",
        "linux,unknown",
        "linux,0gb",
        "linux,1.5gb",
        "linux,16mb",
        "linux,16gb,16gb",
        "windows,16gb",
        "macos,16gb",
        "windows,4c",
        "windows,net",
        "windows,wat",
        "macos,4c",
        "macos,net",
        "macos,wat",
    ] {
        let response = handle_with_policy(
            &restarted_context(dir.path()),
            Some(launch(daemon.connection().clone())),
            serde_json::json!({"command": command.clone(), "runon": runon}),
        );
        assert!(!response.success, "{runon}: {response:?}");
        let message = response.data["message"].as_str().unwrap_or_default();
        assert!(message.contains(runon), "{runon}: {response:?}");
        let reason = if runon == "windows,4c" || runon == "macos,4c" {
            "fixed at 4 vCPUs"
        } else if runon == "windows,net" || runon == "macos,net" {
            "no network"
        } else {
            "not valid"
        };
        assert!(message.contains(reason), "{runon}: {response:?}");
    }
    assert!(exec_runs(&daemon).is_empty());
    assert!(!marker.exists());
}

#[tokio::test]
async fn old_shape_prefix_routing_survives_the_runon_kill_switch_and_runon_works_when_enabled() {
    for enabled in [false, true] {
        let daemon =
            aft_engine::test_bridge::bash_remote::remote_wire::daemon_with_clients(Script::Plain, "exec-remote/v1", 2)
                .await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = remote_test_context_with_runon(dir.path(), enabled);
        let mut old = launch(daemon.connection().clone());
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
            assert_eq!(
                whole.data["message"],
                "runon refused: runon is disabled by the user setting remote_exec.enabled"
            );
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
        Some(launch(daemon.connection().clone())),
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
    let mut policy = launch(daemon.connection().clone());
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
    let task = ctx.bash_background().test_task(&task_id).unwrap();
    assert!(task.test_state().lock().unwrap().metadata().remote.is_none());
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
    let enabled = || Some(launch(daemon.connection().clone()));
    let mut disabled = launch(daemon.connection().clone());
    disabled.params.remote_exec.as_mut().unwrap().enabled = false;
    let ctx = restarted_context(dir.path());
    let mut project_off = crate::config::Config {
        project_root: Some(dir.path().into()),
        ..Default::default()
    };
    project_off.sandbox.profile.head = crate::config::SandboxMode::Off;
    project_off.remote_exec.project_off = true;
    let project_off_ctx = crate::test_context(
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
            serde_json::json!({"runon": "plan9"}),
            "unknown runner demand \"plan9\"",
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
    // Each synchronous refusal must return without registering a worker that
    // could dispatch later, either remotely or on this host.
    assert!(ctx.bash_background().list(0).is_empty());
    assert!(project_off_ctx.bash_background().list(0).is_empty());
    assert!(exec_runs(&daemon).is_empty());
    assert!(!marker.exists());
}

#[tokio::test]
async fn a_reporting_runner_report_is_printed_after_the_output() {
    let daemon = daemon(Script::Reported, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = remote_test_context(dir.path());
    let response = handle_with_policy(
        &ctx,
        Some(launch(daemon.connection().clone())),
        serde_json::json!({"command": "bash build.sh", "runon": "linux", "compressed": false}),
    );
    assert!(response.success, "{response:?}");
    let task_id = response.data["task_id"].as_str().unwrap().to_string();
    let done = terminal(ctx.bash_background(), &task_id).await;
    let rendered = crate::commands::bash_orchestrate::format_foreground_result(&done);
    assert_eq!(
        rendered.lines().next(),
        Some("ran remotely on ck-motor (job 0192a64a-1234-7000-8000-000000000001)"),
        "{rendered}"
    );
    assert!(
        rendered.ends_with(
            &render_terminal_report(&aft_engine::test_bridge::bash_remote::remote_wire::report_vector("all"), &[])
                .unwrap()
        ),
        "{rendered}"
    );
    assert!(
        !rendered.contains("not reported by the runner"),
        "{rendered}"
    );
}

/// Spawn `params` through the bash handler and drive its foreground wait the
/// way the standalone request loop does, until the call answers. Returns the
/// reply and how long its foreground wait took.
async fn orchestrated_reply(
    ctx: &crate::context::AppContext,
    daemon: &aft_engine::test_bridge::bash_remote::remote_wire::Daemon,
    params: serde_json::Value,
) -> (crate::protocol::Response, Duration) {
    // These callers check terminal payloads, not the foreground wait cap.
    // Observe completion before starting that independent call budget.
    orchestrated_reply_with_probe(ctx, daemon, params, |task_id| {
        ctx.bash_background()
            .observed_status(task_id, "session", 8192)
            .is_some_and(|snapshot| snapshot.info.status.is_terminal())
    })
    .await
}

/// Wait for a remote-state probe before starting the foreground wait budget.
/// This separates daemon connection and stream-processing latency from tests
/// of the wait cap in a particular remote phase.
async fn orchestrated_reply_with_probe(
    ctx: &crate::context::AppContext,
    daemon: &aft_engine::test_bridge::bash_remote::remote_wire::Daemon,
    mut params: serde_json::Value,
    ready: impl Fn(&str) -> bool,
) -> (crate::protocol::Response, Duration) {
    // The handler resolves an omitted cwd against the process, not this
    // disposable test project. Keep the remote request inside its own root.
    params
        .as_object_mut()
        .unwrap()
        .entry("workdir")
        .or_insert_with(|| serde_json::json!(ctx.config().project_root));
    let request = orchestrated_request(params);
    let spawned =
        crate::bash_background::with_remote_policy(Some(launch(daemon.connection().clone())), || {
            crate::commands::bash::handle(&request, ctx)
        });
    assert!(spawned.success, "{spawned:?}");
    let task_id = spawned.data["task_id"].as_str().unwrap();
    wait_for(
        "remote task must satisfy the probe before its foreground wait",
        || ready(task_id).then_some(()),
    )
    .await;
    let received = std::time::Instant::now();
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

#[tokio::test]
async fn blocking_runon_queued_past_its_timeout_answers_with_task_job_and_queue_position() {
    // The runner accepts the job at queue position 11 and holds it queued well
    // past the call's `timeout` before running it.
    let daemon = daemon(Script::GatedStages { position: 11 }, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    let timeout_ms = 1_500;
    let (reply, waited) = orchestrated_reply_with_probe(
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
        |task_id| {
            ctx.bash_background()
                .remote_progress(task_id, "session")
                .is_some_and(|progress| progress.phase == RemotePhase::Queued { position: 11 })
        },
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
    daemon.output_gate().notify_one();
    wait_for("queued job must consume output before completing", || {
        ctx.bash_background()
            .remote_progress(&task_id, "session")
            .filter(|progress| progress.phase == RemotePhase::Running)
    })
    .await;
    daemon.completion_gate().notify_one();
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
    let daemon = daemon(Script::Started, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    let timeout_ms = 1_500;
    let (reply, waited) = orchestrated_reply_with_probe(
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
        |task_id| {
            ctx.bash_background()
                .remote_progress(task_id, "session")
                .is_some_and(|progress| progress.phase == RemotePhase::Running)
        },
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
    let task_id = reply.data["task_id"].as_str().unwrap().to_string();
    let registry = ctx.bash_background().clone();
    tokio::task::spawn_blocking(move || registry.kill(&task_id, "session"))
        .await
        .unwrap()
        .unwrap();
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
    let daemon = daemon(Script::GatedStages { position: 3 }, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    ctx.update_config(|config| config.foreground_wait_window_ms = 2_500);
    let (reply, waited) = orchestrated_reply_with_probe(
        &ctx,
        &daemon,
        serde_json::json!({
            "command": "uname -s",
            "runon": "linux",
            "timeout": 500,
            "foreground_orchestrate": true,
            "compressed": false,
        }),
        |task_id| {
            ctx.bash_background()
                .remote_progress(task_id, "session")
                .is_some_and(|progress| progress.phase == RemotePhase::Queued { position: 3 })
        },
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
    daemon.output_gate().notify_one();
    daemon.completion_gate().notify_one();
    let task_id = reply.data["task_id"].as_str().unwrap();
    assert_eq!(
        terminal(ctx.bash_background(), task_id).await.info.status,
        BgTaskStatus::Completed
    );
}

async fn remote_job_id_response(
    ctx: Arc<crate::context::AppContext>,
    tool: &str,
    reference: &str,
    session: &str,
) -> crate::protocol::Response {
    let request = serde_json::from_value(serde_json::json!({
        "id": "alias", "command": tool, "session_id": session,
        "params": { "task_id": reference, "timeout_ms": 1 },
    }))
    .unwrap();
    if tool == "bash_status" {
        return crate::commands::bash_status::handle(&request, &ctx);
    }
    if tool == "bash_kill" {
        return tokio::task::spawn_blocking(move || {
            crate::commands::bash_kill::handle(&request, &ctx)
        })
        .await
        .unwrap();
    }
    let wake = crate::response_finalize::DeferredResponseWake::default();
    let producer_wake = wake.clone();
    let producer_ctx = Arc::clone(&ctx);
    let outcome = tokio::task::spawn_blocking(move || {
        let _wake = producer_wake.install();
        crate::commands::bash_watch::handle_deferred(&request, producer_ctx)
    })
    .await
    .unwrap();
    match outcome {
        crate::response_finalize::DispatchOutcome::Immediate(response) => response,
        crate::response_finalize::DispatchOutcome::Deferred(mut pending) => {
            tokio::time::timeout(Duration::from_secs(10), wake.notified())
                .await
                .expect("watch answers");
            (pending.poll)(&ctx).expect("response queued before completion wake")
        }
    }
}

#[tokio::test]
async fn remote_job_id_own_status_watch_and_kill_use_persisted_task_ids() {
    // Replay follows an existing job with exec.attach, not exec.run. Keep that
    // job running until exec.cancel so a natural exit cannot race the kill.
    let daemon = daemon(Script::AttachUntilCancelled, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let (original, task_id, _) =
        persist_accepted_with_snapshot(dir.path(), daemon.connection().clone(), "cargo test");
    drop(original);
    let capture = Arc::new(Mutex::new(Vec::new()));
    remote_launch_log_captures()
        .lock()
        .unwrap()
        .insert(dir.path().canonicalize().unwrap(), capture.clone());
    let ctx = remote_job_id_context(dir.path());
    for tool in ["bash_status", "bash_watch", "bash_kill"] {
        let response =
            remote_job_id_response(Arc::clone(&ctx), tool, &id().to_string(), "session").await;
        assert!(response.success, "{tool}: {response:?}");
        assert_eq!(response.data["task_id"], task_id, "{tool}: {response:?}");
        assert_eq!(
            response.data["status"],
            if tool == "bash_kill" {
                "killed"
            } else {
                "running"
            },
            "{tool}: {response:?}; worker logs: {:?}",
            capture.lock().unwrap()
        );
        if tool == "bash_status" {
            daemon.wait_for_requests("exec.attach", 1).await;
        }
    }
    // Request-level Cancel only detaches a follower; require the job-id cancel
    // itself, addressed to the saved job, rather than accepting a stream drop.
    daemon.wait_for_requests("exec.cancel", 1).await;
    let log = daemon.log().lock().unwrap();
    let cancels: Vec<_> = log
        .iter()
        .filter(|(_, body)| body["method"] == "exec.cancel")
        .collect();
    assert_eq!(cancels.len(), 1, "{log:?}");
    assert_eq!(cancels[0].1["params"]["job_id"], id().to_string());
    assert_eq!(daemon.connections().load(Ordering::SeqCst), 1);
    drop(log);
    ctx.bash_background().detach();
}

#[tokio::test]
async fn remote_job_id_foreign_and_unknown_refuse_like_unknown_task_ids() {
    let dir = tempfile::tempdir().unwrap();
    let (original, _, paths) = persist_accepted_with_snapshot(
        dir.path(),
        dir.path().join("missing-connection"),
        "cargo test",
    );
    drop(original);
    let mut metadata = read_task(&paths.json).unwrap();
    metadata.mark_terminal(BgTaskStatus::Completed, Some(0), None);
    write_task(&paths.json, &metadata).unwrap();
    let ctx = remote_job_id_context(dir.path());
    for tool in ["bash_status", "bash_watch", "bash_kill"] {
        for (reference, session) in [
            (id().to_string(), "foreign"),
            (Uuid::from_bytes([0xaa; 16]).to_string(), "session"),
            ("bash-ffffffffffffffff".to_string(), "session"),
        ] {
            let response =
                remote_job_id_response(Arc::clone(&ctx), tool, &reference, session).await;
            assert!(!response.success, "{tool}: {response:?}");
            assert_eq!(response.data["code"], "task_not_found", "{response:?}");
            assert_eq!(
                response.data["message"],
                crate::commands::bash_status::format_unknown_polled_task_message(
                    &reference, "bash"
                )
            );
        }
    }
    ctx.bash_background().detach();
}

/// bash_status for `task_id` in `ctx`, rendered as the agent sees it.
fn rendered_status(ctx: &crate::context::AppContext, task_id: &str) -> String {
    let request: crate::protocol::RawRequest = serde_json::from_value(serde_json::json!({
        "id": "status", "command": "bash_status", "session_id": "session",
        "params": { "task_id": task_id },
    }))
    .unwrap();
    let response = crate::commands::bash_status::handle(&request, ctx);
    assert!(response.success, "{response:?}");
    crate::subc_format::format_response_with_context(
        "bash_status",
        &response,
        &crate::subc_format::FormatContext::default(),
    )
}

/// A short bash_watch on `task_id` in `ctx`; returns its rendered text.
async fn rendered_watch(ctx: Arc<crate::context::AppContext>, task_id: &str) -> String {
    let request: crate::protocol::RawRequest = serde_json::from_value(serde_json::json!({
        "id": "watch", "command": "bash_watch", "session_id": "session",
        "params": { "task_id": task_id, "timeout_ms": 200 },
    }))
    .unwrap();
    let wake = crate::response_finalize::DeferredResponseWake::default();
    let producer_wake = wake.clone();
    let producer_ctx = Arc::clone(&ctx);
    let outcome = tokio::task::spawn_blocking(move || {
        let _wake = producer_wake.install();
        crate::commands::bash_watch::handle_deferred(&request, producer_ctx)
    })
    .await
    .unwrap();
    let response = match outcome {
        crate::response_finalize::DispatchOutcome::Immediate(response) => response,
        crate::response_finalize::DispatchOutcome::Deferred(mut pending) => {
            tokio::time::timeout(Duration::from_secs(10), wake.notified())
                .await
                .expect("watch answers");
            (pending.poll)(&ctx).expect("response queued before completion wake")
        }
    };
    assert!(response.success, "{response:?}");
    response.data["output"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn bash_status_watch_and_task_line_distinguish_input_capture_from_queueing() {
    let daemon = daemon(Script::GatedAcceptance, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    ensure_git_fixture(dir.path());
    let ctx = Arc::new(restarted_context(dir.path()));
    let mut policy = launch(daemon.connection().clone());
    policy.explicit_runon = true;
    let task_id = ctx
        .bash_background()
        .spawn_remote(
            policy,
            SpawnPlan::Unsandboxed,
            "uname -s",
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
    daemon.wait_for_requests("exec.run", 1).await;
    let status = rendered_status(&ctx, &task_id);
    let watch = rendered_watch(ctx.clone(), &task_id).await;
    assert!(
        status.starts_with(&format!("Task {task_id}: starting · submitting")),
        "{status}"
    );
    assert!(
        watch.starts_with(&format!("Task {task_id}: starting")),
        "{watch}"
    );
    for text in [&status, &watch] {
        assert!(text.contains("capturing inputs"), "{text}");
        assert!(
            !text.contains("queued on the runner") && !text.contains("still running"),
            "{text}"
        );
    }
    let snapshot = ctx
        .bash_background()
        .observed_status(&task_id, "session", 0)
        .unwrap();
    assert_eq!(
        crate::commands::bash_orchestrate::foreground_task_line(&snapshot),
        format!("[task {task_id}]")
    );
    daemon.acceptance_gate().notify_one();
    wait_for("acceptance must end the input-capture phase", || {
        (ctx.bash_background()
            .runon_launch_pending(&task_id, "session")
            == Some(false))
        .then_some(())
    })
    .await;
    let status = rendered_status(&ctx, &task_id);
    let watch = rendered_watch(ctx.clone(), &task_id).await;
    assert!(status.contains("queued on the runner"), "{status}");
    assert!(watch.contains("Queued on the runner"), "{watch}");
    let snapshot = ctx
        .bash_background()
        .observed_status(&task_id, "session", 0)
        .unwrap();
    assert_eq!(
        crate::commands::bash_orchestrate::foreground_task_line(&snapshot),
        format!("[task {task_id} (job {})]", id())
    );
    let registry = ctx.bash_background().clone();
    tokio::task::spawn_blocking(move || registry.kill(&task_id, "session"))
        .await
        .unwrap()
        .unwrap();
}
