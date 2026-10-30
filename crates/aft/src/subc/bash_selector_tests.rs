use super::*;

#[tokio::test]
async fn exec_remote_v1_bash_runs_through_real_route_without_invalid_selector() {
    let (dir, root) = test_support::test_root("v1-bash-selector");
    let ctx = Arc::new(AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(root.as_path().into()),
            storage_dir: Some(dir.path().join("storage")),
            sandbox: crate::config::SandboxConfig {
                ..Default::default()
            },
            ..Default::default()
        },
    ));
    let executor = Arc::new(Executor::new());
    assert!(executor.register_actor(root.clone(), ctx));
    let identity = RouteIdentity(Arc::new(RouteIdentityData {
        root: root.clone(),
        project_root: root.as_path().into(),
        harness: "runner".into(),
        session: "selector-test".into(),
        role: tool_provider::RouteRole::ToolProviderV1,
        trust: BindTrust::FirstParty,
        spawn_principal: AuthenticatedPrincipal::FirstParty,
        consumer_elicitation_capable: false,
        tool_result_body: false,
        disabled_tools: Arc::new(vec![]),
        run_scope: OnceLock::new(),
        call_mode: subc_protocol::call_mode::CallMode::Classic,
        scope: None,
        made_tool_call: AtomicBool::new(false),
        run_owner: None,
    }));
    let routes = HashMap::from([(route_key(41, 1), identity)]);
    // The bash tool runs PowerShell on Windows, where `printf` does not exist.
    // Each command writes the proof text with no trailing newline, so the
    // exact output comparison below holds on every platform.
    let command = if cfg!(windows) {
        "[Console]::Out.Write('v1-bash-proof')"
    } else {
        "printf v1-bash-proof"
    };
    let frame = Frame::build(
        FrameType::Request,
        control_flags(),
        41,
        1,
        7,
        serde_json::to_vec(&json!({"name":"bash","arguments":{"command":command}})).unwrap(),
    )
    .unwrap();
    let (writer, _replies) = mpsc::channel(8);
    let (bash_tx, mut bash_rx) = mpsc::channel(8);
    let (touch_tx, _touch_rx) = mpsc::channel(8);
    let (deferred_tx, _deferred_rx) = mpsc::unbounded_channel();
    let deferred_tx = super::DeferredResponseSender {
        entries: deferred_tx,
        wake: crate::response_finalize::DeferredResponseWake::default(),
    };
    handle_tool_call(
        &writer,
        &frame,
        None,
        None,
        PhaseTrace::new(Instant::now()),
        &routes,
        &HashMap::new(),
        &ReclaimedRoutes::default(),
        &mut HashMap::new(),
        &executor,
        &Arc::default(),
        &Arc::new(PendingDeferredSetups::default()),
        &Arc::new(Notify::new()),
        &PersistentCancelSignal::new(),
        &bash_tx,
        &touch_tx,
        &Arc::new(DispatchPathMetrics::new()),
        &mut HashMap::new(),
        &mut HashMap::new(),
        &mut 1,
        &mut HashMap::new(),
        &mut HashMap::new(),
        &mut HashMap::new(),
        &mut HashMap::new(),
        |request, ctx| crate::commands::bash::handle(&request, ctx),
        &deferred_tx,
        false,
        1024 * 1024,
        &drain::ModuleDrainWindow::default(),
        None,
    )
    .await
    .unwrap();
    let done = tokio::time::timeout(Duration::from_secs(10), bash_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let response = done.response_for_test();
    assert!(response.success, "{response:?}");
    assert_eq!(response.data["exit_code"], 0, "{response:?}");
    // A command that finishes within the call still names its task, so the
    // caller can find it again with bash_status.
    let task_id = response.data["task_id"].as_str().unwrap();
    assert_eq!(
        response.data["output"],
        format!("v1-bash-proof\n[task {task_id}]"),
        "{response:?}"
    );
}

#[cfg(unix)]
const CANCEL_SESSION: &str = "cancelled-call-session";

#[cfg(unix)]
fn cancel_test_context(root: &ProjectRootId, storage: &Path) -> Arc<AppContext> {
    Arc::new(AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(root.as_path().into()),
            storage_dir: Some(storage.into()),
            sandbox: crate::config::SandboxConfig {
                ..Default::default()
            },
            ..Default::default()
        },
    ))
}

/// Sends one `bash` call through the subc route on a plugin (legacy) route.
/// A server-owned tool provider route kills a local command when its call is
/// cancelled; a plugin route keeps it running, which this test needs.
#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
async fn route_bash(
    executor: &Arc<Executor>,
    root: &ProjectRootId,
    connection_cancel: &PersistentCancelSignal,
    bash_tx: &mpsc::Sender<bash::BashDeferredCompletion>,
    corr: u64,
    arguments: Value,
) {
    route_cancel_test_tool(
        executor,
        root,
        connection_cancel,
        bash_tx,
        corr,
        "bash",
        arguments,
        false,
    )
    .await;
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
async fn route_cancel_test_tool(
    executor: &Arc<Executor>,
    root: &ProjectRootId,
    connection_cancel: &PersistentCancelSignal,
    bash_tx: &mpsc::Sender<bash::BashDeferredCompletion>,
    corr: u64,
    name: &str,
    arguments: Value,
    preview: bool,
) -> mpsc::Receiver<wire::WriterFrame> {
    let identity = RouteIdentity(Arc::new(RouteIdentityData {
        root: root.clone(),
        project_root: root.as_path().into(),
        harness: "runner".into(),
        session: CANCEL_SESSION.into(),
        role: tool_provider::RouteRole::Legacy,
        trust: BindTrust::FirstParty,
        spawn_principal: AuthenticatedPrincipal::FirstParty,
        consumer_elicitation_capable: false,
        tool_result_body: false,
        disabled_tools: Arc::new(vec![]),
        run_scope: OnceLock::new(),
        call_mode: subc_protocol::call_mode::CallMode::Classic,
        scope: None,
        made_tool_call: AtomicBool::new(false),
        run_owner: None,
    }));
    let routes = HashMap::from([(route_key(41, 1), identity)]);
    let frame = Frame::build(
        FrameType::Request,
        control_flags(),
        41,
        1,
        corr,
        serde_json::to_vec(&json!({"name": name, "arguments": arguments, "preview": preview}))
            .unwrap(),
    )
    .unwrap();
    let (writer, replies) = mpsc::channel(8);
    let (touch_tx, _touch_rx) = mpsc::channel(8);
    let (deferred_tx, _deferred_rx) = mpsc::unbounded_channel();
    let deferred_tx = super::DeferredResponseSender {
        entries: deferred_tx,
        wake: crate::response_finalize::DeferredResponseWake::default(),
    };
    handle_tool_call(
        &writer,
        &frame,
        None,
        None,
        PhaseTrace::new(Instant::now()),
        &routes,
        &HashMap::new(),
        &ReclaimedRoutes::default(),
        &mut HashMap::new(),
        executor,
        &Arc::default(),
        &Arc::new(PendingDeferredSetups::default()),
        &Arc::new(Notify::new()),
        connection_cancel,
        bash_tx,
        &touch_tx,
        &Arc::new(DispatchPathMetrics::new()),
        &mut HashMap::new(),
        &mut HashMap::new(),
        &mut 1,
        &mut HashMap::new(),
        &mut HashMap::new(),
        &mut HashMap::new(),
        &mut HashMap::new(),
        |request, ctx| match request.command.as_str() {
            "bash" => crate::commands::bash::handle(&request, ctx),
            "bash_status" => crate::commands::bash_status::handle(&request, ctx),
            "read" => crate::commands::read::handle_read(&request, ctx),
            other => panic!("unexpected cancelled-call test command: {other}"),
        },
        &deferred_tx,
        false,
        1024 * 1024,
        &drain::ModuleDrainWindow::default(),
        None,
    )
    .await
    .unwrap();
    replies
}

/// The text and response data of the next bash call answered on `bash_rx`.
#[cfg(unix)]
async fn next_bash_reply(
    bash_rx: &mut mpsc::Receiver<bash::BashDeferredCompletion>,
) -> (String, Value) {
    let done = tokio::time::timeout(Duration::from_secs(20), bash_rx.recv())
        .await
        .expect("bash reply within 20 s")
        .expect("bash reply");
    let text = done.text_for_test().expect("bash result").to_string();
    (text, done.response_for_test().data.clone())
}

#[cfg(unix)]
async fn next_cancel_test_tool_reply(replies: &mut mpsc::Receiver<wire::WriterFrame>) -> String {
    let mut reply = tokio::time::timeout(Duration::from_secs(10), replies.recv())
        .await
        .expect("tool call settles")
        .expect("tool call reply");
    reply.project_tool_result().unwrap();
    let body: Value = serde_json::from_slice(reply.body()).unwrap();
    body["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("tool reply has no text: {body}"))
        .to_string()
}

#[cfg(unix)]
#[tokio::test]
async fn cancelled_call_notice_appears_on_read_once() {
    cancelled_call_notice_read_case(NoticeInterveningCall::None, false).await;
}

#[cfg(unix)]
#[tokio::test]
async fn cancelled_call_notice_survives_native_status_and_appears_on_read_once() {
    cancelled_call_notice_read_case(NoticeInterveningCall::NativeStatus, false).await;
}

#[cfg(unix)]
#[tokio::test]
async fn cancelled_call_notice_appears_on_standalone_read_once() {
    cancelled_call_notice_read_case(NoticeInterveningCall::None, true).await;
}

#[cfg(unix)]
#[tokio::test]
async fn cancelled_call_notice_survives_argument_refusal_and_appears_on_read_once() {
    cancelled_call_notice_read_case(NoticeInterveningCall::ArgumentRefusal, false).await;
}

#[cfg(unix)]
enum NoticeInterveningCall {
    None,
    NativeStatus,
    ArgumentRefusal,
}

#[cfg(unix)]
async fn cancelled_call_notice_read_case(intervening: NoticeInterveningCall, standalone: bool) {
    struct Shutdown(Arc<AppContext>);
    impl Drop for Shutdown {
        fn drop(&mut self) {
            self.0.bash_background().shutdown();
        }
    }
    let (dir, root) = test_support::test_root("cancelled-call-read");
    let ctx = cancel_test_context(&root, &dir.path().join("storage"));
    let _shutdown = Shutdown(Arc::clone(&ctx));
    let executor = Arc::new(Executor::new());
    assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));
    let (bash_tx, _bash_rx) = mpsc::channel(8);
    let connection = PersistentCancelSignal::new();
    let task_id = ctx
        .bash_background()
        .spawn_with_shell_in_slot(
            crate::sandbox_spawn::SpawnPlan::Unsandboxed,
            "sleep 60",
            crate::bash_background::BashShell::Bash,
            crate::bash_background::registry::resolve_posix_shell(),
            CANCEL_SESSION.into(),
            root.as_path().into(),
            HashMap::new(),
            crate::bash_background::HardKill::After(Duration::from_secs(60)),
            None,
            dir.path().join("storage"),
            crate::bash_background::TaskSlot::Background { max: 10 },
            true,
            false,
            Some(root.as_path().into()),
        )
        .unwrap();
    ctx.bash_background()
        .note_cancelled_call(&task_id, CANCEL_SESSION);
    let notice = format!("task {task_id} from your cancelled call is still running; check it with bash_status or stop it with bash_kill");

    let arguments = match intervening {
        NoticeInterveningCall::None => None,
        NoticeInterveningCall::NativeStatus => Some(("bash_status", json!({"task_id": task_id}))),
        NoticeInterveningCall::ArgumentRefusal => Some(("read", json!([]))),
    };
    if let Some((name, arguments)) = arguments {
        let mut plumbing = route_cancel_test_tool(
            &executor,
            &root,
            &connection,
            &bash_tx,
            2,
            name,
            arguments,
            false,
        )
        .await;
        let text = next_cancel_test_tool_reply(&mut plumbing).await;
        assert!(!text.contains("cancelled call"), "{text}");
        if name == "read" {
            assert!(text.contains("tool arguments must be an object"), "{text}");
        }
    }

    let path = root.as_path().join("notice.txt");
    std::fs::write(&path, "file contents").unwrap();
    let mut held = Response::error("held", crate::access_policy::ACCESS_ASK_REQUIRED, "ask");
    let mut text = String::new();
    crate::response_finalize::finalize_tool_response(
        &mut held,
        &mut text,
        &ctx,
        CANCEL_SESSION,
        "read",
        false,
    );
    assert!(!text.contains("cancelled call"), "{text}");

    let mut preview = route_cancel_test_tool(
        &executor,
        &root,
        &connection,
        &bash_tx,
        6,
        "read",
        json!({"path": path}),
        true,
    )
    .await;
    let text = next_cancel_test_tool_reply(&mut preview).await;
    assert!(!text.contains("cancelled call"), "{text}");

    let text = if standalone {
        let mut request = RawRequest {
            id: "standalone".into(),
            command: "tool_call".into(),
            params: json!({"name": "read", "arguments": {"path": path}, "preview": true}),
            session_id: Some(CANCEL_SESSION.into()),
            lsp_hints: None,
        };
        let dispatch = |request: RawRequest, ctx: &AppContext| {
            crate::commands::read::handle_read(&request, ctx)
        };
        let preview = crate::commands::tool_call::handle_with_dispatch(&request, &ctx, &dispatch);
        assert!(!preview.data["text"]
            .as_str()
            .unwrap()
            .contains("cancelled call"));
        request.params["preview"] = json!(false);
        let read = crate::commands::tool_call::handle_with_dispatch(&request, &ctx, &dispatch);
        read.data["text"].as_str().unwrap().to_string()
    } else {
        let mut read = route_cancel_test_tool(
            &executor,
            &root,
            &connection,
            &bash_tx,
            3,
            "read",
            json!({"path": path}),
            false,
        )
        .await;
        next_cancel_test_tool_reply(&mut read).await
    };
    assert!(text.contains("file contents"), "{text}");
    assert_eq!(text.matches(&notice).count(), 1, "{text}");

    let mut status = route_cancel_test_tool(
        &executor,
        &root,
        &connection,
        &bash_tx,
        4,
        "bash_status",
        json!({"taskId": task_id}),
        false,
    )
    .await;
    let text = next_cancel_test_tool_reply(&mut status).await;
    assert!(!text.contains("cancelled call"), "{text}");

    // The agent's taskId spelling must receive notices; the plugin's task_id poll must not.
    ctx.bash_background()
        .note_cancelled_call(&task_id, CANCEL_SESSION);
    let mut status = route_cancel_test_tool(
        &executor,
        &root,
        &connection,
        &bash_tx,
        5,
        "bash_status",
        json!({"taskId": task_id}),
        false,
    )
    .await;
    let text = next_cancel_test_tool_reply(&mut status).await;
    assert_eq!(text.matches(&notice).count(), 1, "{text}");
    assert!(ctx
        .bash_background()
        .take_cancelled_call_notices(CANCEL_SESSION)
        .is_empty());
}

/// A `wait: true` call whose transport is lost while the command still runs
/// answers with only the cancellation. The command keeps running as a task;
/// across an AFT restart, the session's next bash call names it once, and its
/// completion is delivered through the ordinary completion queue.
#[cfg(unix)]
#[tokio::test]
async fn cancelled_wait_names_its_task_once_and_delivers_its_completion_after_restart() {
    let (dir, root) = test_support::test_root("cancelled-call");
    let storage = dir.path().join("storage");
    let started = dir.path().join("started");
    let release = dir.path().join("release");
    let command = format!(
        "touch '{}'; while [ ! -f '{}' ]; do sleep 0.05; done; printf cancelled-call-done",
        started.display(),
        release.display()
    );

    let first = cancel_test_context(&root, &storage);
    let executor = Arc::new(Executor::new());
    assert!(executor.register_actor(root.clone(), Arc::clone(&first)));
    let (bash_tx, mut bash_rx) = mpsc::channel(8);
    let connection = PersistentCancelSignal::new();
    route_bash(
        &executor,
        &root,
        &connection,
        &bash_tx,
        1,
        json!({"command": command, "wait": true, "timeout": 60_000}),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(20), async {
        while !started.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("command starts");
    // Let the call settle into its wait before the transport goes away.
    tokio::time::sleep(Duration::from_millis(300)).await;
    connection.cancel();
    let cancelled = tokio::time::timeout(Duration::from_secs(10), bash_rx.recv())
        .await
        .expect("cancelled call settles")
        .expect("cancelled call completion");
    assert!(
        cancelled.text_for_test().is_none(),
        "a cancelled call carries no result"
    );

    // Restart: the first registry lets go of the still-running command, and a
    // new one reloads the session's tasks from disk.
    first.bash_background().detach();
    let second = cancel_test_context(&root, &storage);
    second
        .bash_background()
        .replay_session(
            &crate::bash_background::task_storage_dir(&second),
            CANCEL_SESSION,
        )
        .unwrap();
    let executor = Arc::new(Executor::new());
    assert!(executor.register_actor(root.clone(), Arc::clone(&second)));
    let connection = PersistentCancelSignal::new();

    // The next bash call names the still-running task.
    route_bash(
        &executor,
        &root,
        &connection,
        &bash_tx,
        2,
        json!({"command": "printf next"}),
    )
    .await;
    let (text, _) = next_bash_reply(&mut bash_rx).await;
    let notice = text
        .lines()
        .find(|line| line.ends_with("from your cancelled call is still running; check it with bash_status or stop it with bash_kill"))
        .unwrap_or_else(|| panic!("no cancelled-call notice in: {text}"));
    let task_id = notice
        .strip_prefix("task ")
        .and_then(|rest| rest.split(' ').next())
        .unwrap()
        .to_string();
    assert_eq!(
        notice,
        format!("task {task_id} from your cancelled call is still running; check it with bash_status or stop it with bash_kill"),
        "a local task names no job"
    );
    assert!(second
        .bash_background()
        .observed_status(&task_id, CANCEL_SESSION, 0)
        .is_some_and(|snapshot| !snapshot.info.status.is_terminal()));

    // A further bash call does not name the task again: a still-running task
    // is named only once.
    route_bash(
        &executor,
        &root,
        &connection,
        &bash_tx,
        3,
        json!({"command": "printf again"}),
    )
    .await;
    let (text, _) = next_bash_reply(&mut bash_rx).await;
    assert!(!text.contains("cancelled call"), "{text}");

    // The command finishes after the restart; the session receives its
    // completion with a later reply.
    std::fs::write(&release, b"go").unwrap();
    let mut delivered = None;
    for corr in 4..200 {
        route_bash(
            &executor,
            &root,
            &connection,
            &bash_tx,
            corr,
            json!({"command": "printf poll"}),
        )
        .await;
        let (_, data) = next_bash_reply(&mut bash_rx).await;
        delivered = data["bg_completions"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|completion| completion["task_id"] == task_id.as_str())
            .cloned();
        if delivered.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let delivered = delivered.expect("the cancelled call's completion was never delivered");
    assert_eq!(delivered["exit_code"], 0, "{delivered}");
    second.bash_background().detach();
}

#[test]
fn exec_remote_v1_powershell_keeps_its_only_supported_selector() {
    let mut args = serde_json::Map::new();
    provider_shell_selector("powershell", &mut args);
    assert_eq!(args.get("shell"), Some(&json!("powershell")));
}

#[cfg(unix)]
async fn v1_bash_wait_limit_case(worker: bool) {
    let (dir, root) = test_support::test_root("v1-bash-wait-limit");
    let mut config = crate::config::Config::default();
    config.project_root = Some(root.as_path().into());
    config.storage_dir = Some(dir.path().join("storage"));
    config.sandbox.profile.head = crate::config::SandboxMode::Off;
    config.bash.worker_wait_max_ms = 1_000;
    let ctx = Arc::new(AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        config,
    ));
    let executor = Arc::new(Executor::new());
    assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));
    let identity = RouteIdentity(Arc::new(RouteIdentityData {
        root: root.clone(),
        project_root: root.as_path().into(),
        harness: "runner".into(),
        session: "wait-limit-test".into(),
        role: tool_provider::RouteRole::ToolProviderV1,
        trust: BindTrust::FirstParty,
        spawn_principal: AuthenticatedPrincipal::FirstParty,
        consumer_elicitation_capable: false,
        tool_result_body: false,
        disabled_tools: Arc::new(vec![]),
        run_scope: OnceLock::new(),
        call_mode: subc_protocol::call_mode::CallMode::Classic,
        scope: None,
        made_tool_call: AtomicBool::new(false),
        run_owner: None,
    }));
    let mut call = json!({"name":"bash","arguments":{
        "command":"sleep 30; printf completed", "wait":true, "timeout":120_000
    }});
    call["preset"] = json!(if worker { "worker" } else { "head" });
    let frame = Frame::build(
        FrameType::Request,
        control_flags(),
        41,
        1,
        7,
        serde_json::to_vec(&call).unwrap(),
    )
    .unwrap();
    let (writer, _replies) = mpsc::channel(8);
    let (bash_tx, mut bash_rx) = mpsc::channel(8);
    let (touch_tx, _touch_rx) = mpsc::channel(8);
    let (deferred_tx, _deferred_rx) = mpsc::unbounded_channel();
    let deferred_tx = DeferredResponseSender {
        entries: deferred_tx,
        wake: crate::response_finalize::DeferredResponseWake::default(),
    };
    let started = Instant::now();
    handle_tool_call(
        &writer,
        &frame,
        None,
        None,
        PhaseTrace::new(started),
        &HashMap::from([(route_key(41, 1), identity)]),
        &HashMap::new(),
        &ReclaimedRoutes::default(),
        &mut HashMap::new(),
        &executor,
        &Arc::default(),
        &Arc::new(PendingDeferredSetups::default()),
        &Arc::new(Notify::new()),
        &PersistentCancelSignal::new(),
        &bash_tx,
        &touch_tx,
        &Arc::new(DispatchPathMetrics::new()),
        &mut HashMap::new(),
        &mut HashMap::new(),
        &mut 1,
        &mut HashMap::new(),
        &mut HashMap::new(),
        &mut HashMap::new(),
        &mut HashMap::new(),
        |request, ctx| crate::commands::bash::handle(&request, ctx),
        &deferred_tx,
        false,
        1024 * 1024,
        &drain::ModuleDrainWindow::default(),
        None,
    )
    .await
    .unwrap();
    let done = tokio::time::timeout(Duration::from_secs(90), bash_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let elapsed = started.elapsed();
    let response = done.response_for_test();
    // Clean up before assertions so a failing handoff assertion leaves no child.
    if let Some(task_id) = response.data["task_id"].as_str() {
        let snapshot = ctx
            .bash_background()
            .status(task_id, "wait-limit-test", None, None, 0)
            .unwrap();
        let status = snapshot.info.status;
        let _ = ctx.bash_background().kill(task_id, "wait-limit-test");
        assert_eq!(status, crate::bash_background::BgTaskStatus::Running);
    }
    assert!(response.success, "{response:?}");
    assert_eq!(response.data["status"], "running", "{response:?}");
    assert!(response.data["task_id"].as_str().is_some(), "{response:?}");
    assert!(
        elapsed >= Duration::from_millis(800) && elapsed < Duration::from_secs(15),
        "{elapsed:?}"
    );
    assert!(
        response.data["output"]
            .as_str()
            .unwrap()
            .contains("was not killed"),
        "{response:?}"
    );
    assert!(
        response.data["output"]
            .as_str()
            .unwrap()
            .contains("timeout"),
        "{response:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn tool_provider_v1_worker_bash_hands_off_at_configured_cap() {
    v1_bash_wait_limit_case(true).await;
}

#[cfg(unix)]
#[tokio::test]
async fn tool_provider_v1_head_bash_hands_off_at_configured_cap() {
    v1_bash_wait_limit_case(false).await;
}
