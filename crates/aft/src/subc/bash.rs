//! Deferred bash orchestration and trust-gated shell helpers for subc route calls.

use super::*;

#[derive(Clone)]
pub(super) struct BashWaitCancel {
    pub(super) connection: PersistentCancelSignal,
    pub(super) route: PersistentCancelSignal,
}

impl BashWaitCancel {
    async fn cancelled(&self) {
        tokio::select! {
            _ = self.connection.cancelled() => {}
            _ = self.route.cancelled() => {}
        }
    }
}

pub(super) struct RouteBashCancel {
    pub(super) token: PersistentCancelSignal,
    pub(super) active_waits: usize,
}

pub(super) struct BashElicitationPlan {
    pub(super) command: String,
    pub(super) asks: Vec<crate::bash_permissions::PermissionAsk>,
    pub(super) grants: Vec<String>,
}

pub(super) struct BashDeferredCompletion {
    route: RouteChannel,
    corr: u64,
    flags: Flags,
    ver: u8,
    root: ProjectRootId,
    request_id: String,
    result: Option<ToolCallResult>,
    fatal: bool,
}

#[derive(Clone, Copy, Debug, Default)]
struct BashTranslatedSettings {
    background: bool,
    pty: bool,
    wait: bool,
    block_to_completion: bool,
    timeout: Option<u64>,
    /// Client-bounded foreground wait window (ms). Pi sends this so the
    /// Rust-side promotion window cannot exceed the client's request budget.
    foreground_wait_ms: Option<u64>,
}

enum BashSpawnControl {
    Immediate,
    Foreground {
        task_id: String,
        session_id: String,
        project_root: Option<PathBuf>,
        storage_dir: PathBuf,
        deadline: Instant,
        block_to_completion: bool,
        timeout: Option<u64>,
        wait_window_ms: u64,
        detach_on_user_message: bool,
    },
}

enum BashPollControl {
    Done,
    Promote,
    Wait,
}

fn bash_settings_from_translated(args: &serde_json::Map<String, Value>) -> BashTranslatedSettings {
    BashTranslatedSettings {
        background: args
            .get("background")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        pty: args.get("pty").and_then(Value::as_bool).unwrap_or(false),
        wait: args.get("wait").and_then(Value::as_bool).unwrap_or(false),
        block_to_completion: args
            .get("block_to_completion")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        timeout: args.get("timeout").and_then(Value::as_u64),
        foreground_wait_ms: args
            .get("foreground_wait_ms")
            .and_then(Value::as_u64)
            .filter(|ms| *ms > 0),
    }
}

pub(super) fn prepare_bash_elicitation_plan(
    arguments: &Value,
    project_root: &Path,
) -> Result<BashElicitationPlan, crate::subc_translate::TranslateError> {
    let translated = crate::subc_translate::subc_translate("bash", arguments, project_root)?;
    let command = translated
        .args
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let workdir = translated
        .args
        .get("workdir")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| project_root.to_path_buf());
    // PowerShell is not POSIX shell. Its AST must never be fed to the bash
    // scanner, so untrusted routes require a conservative exact-command ask.
    let asks = if translated.args.get("shell").and_then(Value::as_str) == Some("powershell") {
        vec![crate::bash_permissions::PermissionAsk {
            kind: crate::bash_permissions::PermissionKind::Bash,
            patterns: vec![command.clone()],
            always: Vec::new(),
        }]
    } else {
        crate::bash_permissions::scan::scan_with_project_root(&command, project_root, &workdir)
    };
    let grants = permission_grants_for_retry(&asks);
    Ok(BashElicitationPlan {
        command,
        asks,
        grants,
    })
}

fn permission_grants_for_retry(asks: &[crate::bash_permissions::PermissionAsk]) -> Vec<String> {
    asks.iter()
        .flat_map(|ask| {
            if ask.always.is_empty() {
                ask.patterns.iter()
            } else {
                ask.always.iter()
            }
        })
        .cloned()
        .collect()
}

fn finalized_bash_result(
    mut response: Response,
    ctx: &AppContext,
    session_id: &str,
    format_context: &crate::subc_format::FormatContext,
    allow_bg_completions: bool,
) -> ToolCallResult {
    crate::response_finalize::finalize_response_with_bg_completions(
        &mut response,
        ctx,
        session_id,
        "bash",
        allow_bg_completions,
    );
    bash_result_from_response(response, format_context)
}

fn bash_result_from_response(
    response: Response,
    format_context: &crate::subc_format::FormatContext,
) -> ToolCallResult {
    let text = crate::subc_format::format_response_with_context("bash", &response, format_context);
    ToolCallResult { text, response }
}

fn bash_background_launch_response(request_id: &str, task_id: &str, is_pty: bool) -> Response {
    Response::success(
        request_id,
        json!({
            "output": crate::commands::bash_orchestrate::format_background_launch(task_id, is_pty),
            "task_id": task_id,
            "status": "running",
            "mode": if is_pty { "pty" } else { "pipes" },
        }),
    )
}

fn finish_bash_spawn_immediate(
    response: Response,
    ctx: &AppContext,
    session_id: &str,
    format_context: &crate::subc_format::FormatContext,
    text_tx: &mut Option<oneshot::Sender<String>>,
    control_tx: &mut Option<oneshot::Sender<BashSpawnControl>>,
    allow_bg_completions: bool,
) -> Response {
    let result = finalized_bash_result(
        response,
        ctx,
        session_id,
        format_context,
        allow_bg_completions,
    );
    let ToolCallResult { text, response } = result;
    if let Some(tx) = text_tx.take() {
        let _ = tx.send(text);
    }
    if let Some(tx) = control_tx.take() {
        let _ = tx.send(BashSpawnControl::Immediate);
    }
    response
}

fn finish_bash_poll_done(
    response: Response,
    ctx: &AppContext,
    session_id: &str,
    format_context: &crate::subc_format::FormatContext,
    text_tx: &mut Option<oneshot::Sender<String>>,
    control_tx: &mut Option<oneshot::Sender<BashPollControl>>,
) -> Response {
    let result = finalized_bash_result(response, ctx, session_id, format_context, true);
    let ToolCallResult { text, response } = result;
    if let Some(tx) = text_tx.take() {
        let _ = tx.send(text);
    }
    if let Some(tx) = control_tx.take() {
        let _ = tx.send(BashPollControl::Done);
    }
    response
}

#[allow(clippy::too_many_arguments)]
pub(super) fn submit_deferred_bash(
    executor: &Arc<Executor>,
    completion_tx: &mpsc::Sender<BashDeferredCompletion>,
    poll_touch_tx: &mpsc::Sender<ProjectRootId>,
    metrics: &Arc<DispatchPathMetrics>,
    dispatch: DispatchFn,
    root: ProjectRootId,
    project_root: PathBuf,
    session_id: String,
    request_id: String,
    route: RouteChannel,
    corr: u64,
    flags: Flags,
    ver: u8,
    arguments: Value,
    format_context: crate::subc_format::FormatContext,
    cancel: BashWaitCancel,
    bind_trust: BindTrust,
    spawn_principal: crate::sandbox_spawn::AuthenticatedPrincipal,
    edit_slot_survives: Option<bool>,
    permissions_granted: Option<Vec<String>>,
    // Absolute request deadline captured at ingress. Checked before spawn
    // bookkeeping; each queued executor phase carries the same deadline and a
    // phase already running may finish after the deadline (queue-scoped rule).
    request_deadline: Option<Instant>,
) {
    let (spawn_control_tx, spawn_control_rx) = oneshot::channel::<BashSpawnControl>();
    let (spawn_text_tx, spawn_text_rx) = oneshot::channel::<String>();
    let root_for_spawn = root.clone();
    let request_id_for_spawn = request_id.clone();
    let session_for_spawn = session_id.clone();
    let project_root_for_spawn = project_root.clone();
    let format_context_for_spawn = format_context.clone();
    // The spawn submission carries the exact absolute ingress deadline; the
    // executor rejects or prunes the queued phase when it elapses.
    let spawn_rx = executor.submit_async_with_deadline(
        root_for_spawn,
        Lane::Mutating,
        request_id.clone(),
        Box::new(move |ctx| {
            log_ctx::with_session(Some(session_for_spawn.clone()), || {
                let mut spawn_text_tx = Some(spawn_text_tx);
                let mut spawn_control_tx = Some(spawn_control_tx);

                // Queue-scoped rule: if the budget expired before the spawn
                // phase even started, no command may begin.
                if request_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                    let response = crate::protocol::Response::error_with_data(
                        request_id_for_spawn.clone(),
                        "request_deadline_exceeded",
                        "request deadline elapsed before bash could start",
                        serde_json::json!({
                            "retryable": false,
                            "phase": "queue",
                        }),
                    );
                    return finish_bash_spawn_immediate(
                        response,
                        ctx,
                        &session_for_spawn,
                        &format_context_for_spawn,
                        &mut spawn_text_tx,
                        &mut spawn_control_tx,
                        false,
                    );
                }

                if matches!(bind_trust, BindTrust::Untrusted) && permissions_granted.is_none() {
                    let response = bash_denied_untrusted_response(request_id_for_spawn.clone());
                    return finish_bash_spawn_immediate(
                        response,
                        ctx,
                        &session_for_spawn,
                        &format_context_for_spawn,
                        &mut spawn_text_tx,
                        &mut spawn_control_tx,
                        false,
                    );
                }

                // Native bash skips the normal tool-call registration flow, so update the
                // registration before rewriting. Rewritten reads then match ordinary reads.
                if edit_slot_survives.is_some() {
                    crate::run_tool_call::ensure_hashline_registration(
                        ctx,
                        &project_root_for_spawn,
                        &session_for_spawn,
                        edit_slot_survives,
                        false,
                    );
                }

                let mut translated = match crate::subc_translate::subc_translate_owned(
                    "bash",
                    arguments,
                    &project_root_for_spawn,
                ) {
                    Ok(translated) => translated,
                    Err(error) => {
                        let response = Response::error(
                            request_id_for_spawn.clone(),
                            error.code,
                            error.message,
                        );
                        return finish_bash_spawn_immediate(
                            response,
                            ctx,
                            &session_for_spawn,
                            &format_context_for_spawn,
                            &mut spawn_text_tx,
                            &mut spawn_control_tx,
                            true,
                        );
                    }
                };
                if let Some(grants) = permissions_granted {
                    translated
                        .args
                        .insert("permissions_requested".to_string(), Value::Bool(true));
                    translated.args.insert(
                        "permissions_granted".to_string(),
                        Value::Array(grants.into_iter().map(Value::String).collect()),
                    );
                }
                let settings = bash_settings_from_translated(&translated.args);
                let raw_req = RawRequest {
                    id: request_id_for_spawn.clone(),
                    command: "bash".to_string(),
                    lsp_hints: None,
                    session_id: Some(session_for_spawn.clone()),
                    params: Value::Object(translated.args),
                };
                let response =
                    crate::sandbox_spawn::with_authenticated_principal(spawn_principal, || {
                        dispatch(raw_req, ctx)
                    });
                if !response.success {
                    return finish_bash_spawn_immediate(
                        response,
                        ctx,
                        &session_for_spawn,
                        &format_context_for_spawn,
                        &mut spawn_text_tx,
                        &mut spawn_control_tx,
                        true,
                    );
                }

                let Some(task_id) = response
                    .data
                    .get("task_id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                else {
                    return finish_bash_spawn_immediate(
                        response,
                        ctx,
                        &session_for_spawn,
                        &format_context_for_spawn,
                        &mut spawn_text_tx,
                        &mut spawn_control_tx,
                        true,
                    );
                };
                if response.data.get("status").and_then(Value::as_str) != Some("running") {
                    return finish_bash_spawn_immediate(
                        response,
                        ctx,
                        &session_for_spawn,
                        &format_context_for_spawn,
                        &mut spawn_text_tx,
                        &mut spawn_control_tx,
                        true,
                    );
                }

                let mode = response
                    .data
                    .get("mode")
                    .and_then(Value::as_str)
                    .unwrap_or("pipes");
                let is_pty = mode == "pty" || settings.pty;
                if is_pty || settings.background {
                    let response =
                        bash_background_launch_response(&request_id_for_spawn, &task_id, is_pty);
                    return finish_bash_spawn_immediate(
                        response,
                        ctx,
                        &session_for_spawn,
                        &format_context_for_spawn,
                        &mut spawn_text_tx,
                        &mut spawn_control_tx,
                        true,
                    );
                }

                let wait_window_ms =
                    crate::commands::bash_orchestrate::select_foreground_wait_window_ms(
                        ctx.config().foreground_wait_window_ms,
                        settings.timeout,
                        settings.wait,
                        settings.foreground_wait_ms,
                    );
                let deadline = Instant::now() + Duration::from_millis(wait_window_ms);
                let storage_dir =
                    crate::bash_background::storage_dir(ctx.config().storage_dir.as_deref());
                let project_root = ctx.config().project_root.clone();
                // Register the session as detachable exactly like the
                // standalone path (bash_orchestrate) does: without this, a
                // bash_wait_detach signal finds no active wait and wait:true
                // blocks through user messages.
                let detach_on_user_message = settings.wait;
                ctx.bash_background()
                    .register_foreground_task(&session_for_spawn, &task_id);
                if detach_on_user_message {
                    ctx.bash_background()
                        .begin_wait_mode_session(&session_for_spawn, &task_id);
                }
                if let Some(tx) = spawn_control_tx.take() {
                    let _ = tx.send(BashSpawnControl::Foreground {
                        task_id,
                        session_id: session_for_spawn.clone(),
                        project_root,
                        storage_dir,
                        deadline,
                        block_to_completion: settings.block_to_completion || settings.wait,
                        timeout: settings.timeout,
                        wait_window_ms,
                        detach_on_user_message,
                    });
                }
                response
            })
        }),
        request_deadline,
    );

    let executor = Arc::clone(executor);
    let completion_tx = completion_tx.clone();
    let poll_touch_tx = poll_touch_tx.clone();
    let task_metrics = Arc::clone(metrics);
    let root_for_task = root.clone();
    tokio::spawn(async move {
        let _response_task = ResponseTaskGuard::new(&task_metrics);
        let spawn_response = await_executor_response(spawn_rx, request_id.clone()).await;
        let spawn_control = spawn_control_rx.await;
        match spawn_control {
            Ok(BashSpawnControl::Immediate) => {
                let text = spawn_text_rx.await.unwrap_or_else(|_| {
                    crate::subc_format::format_response_with_context(
                        "bash",
                        &spawn_response,
                        &format_context,
                    )
                });
                let result = ToolCallResult {
                    text,
                    response: spawn_response,
                };
                let fatal = response_is_fatal_panic(&result.response);
                send_bash_deferred_completion(
                    &completion_tx,
                    &task_metrics,
                    route,
                    corr,
                    flags,
                    ver,
                    root_for_task,
                    request_id,
                    Some(result),
                    fatal,
                )
                .await;
            }
            Ok(BashSpawnControl::Foreground {
                task_id,
                session_id,
                project_root,
                storage_dir,
                deadline,
                block_to_completion,
                timeout,
                wait_window_ms,
                detach_on_user_message,
            }) => {
                run_deferred_bash_wait(
                    executor,
                    completion_tx,
                    poll_touch_tx,
                    task_metrics,
                    route,
                    corr,
                    flags,
                    ver,
                    root_for_task,
                    request_id,
                    task_id,
                    session_id,
                    project_root,
                    storage_dir,
                    deadline,
                    block_to_completion,
                    timeout,
                    wait_window_ms,
                    detach_on_user_message,
                    format_context,
                    cancel,
                    request_deadline,
                )
                .await;
            }
            Err(_) => {
                let result = bash_result_from_response(spawn_response, &format_context);
                let fatal = response_is_fatal_panic(&result.response);
                send_bash_deferred_completion(
                    &completion_tx,
                    &task_metrics,
                    route,
                    corr,
                    flags,
                    ver,
                    root_for_task,
                    request_id,
                    Some(result),
                    fatal,
                )
                .await;
            }
        }
    });
}

#[allow(clippy::too_many_arguments)]
async fn run_deferred_bash_wait(
    executor: Arc<Executor>,
    completion_tx: mpsc::Sender<BashDeferredCompletion>,
    poll_touch_tx: mpsc::Sender<ProjectRootId>,
    metrics: Arc<DispatchPathMetrics>,
    route: RouteChannel,
    corr: u64,
    flags: Flags,
    ver: u8,
    root: ProjectRootId,
    request_id: String,
    task_id: String,
    session_id: String,
    project_root: Option<PathBuf>,
    storage_dir: PathBuf,
    deadline: Instant,
    block_to_completion: bool,
    timeout: Option<u64>,
    wait_window_ms: u64,
    detach_on_user_message: bool,
    format_context: crate::subc_format::FormatContext,
    cancel: BashWaitCancel,
    request_deadline: Option<Instant>,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                if let Some(ctx) = executor.actor_context(&root) {
                    if detach_on_user_message {
                        ctx.bash_background()
                            .end_wait_mode_session(&session_id, &task_id);
                    } else {
                        ctx.bash_background()
                            .unregister_foreground_task(&session_id, &task_id);
                    }
                }
                send_bash_deferred_completion(
                    &completion_tx,
                    &metrics,
                    route,
                    corr,
                    flags,
                    ver,
                    root,
                    request_id,
                    None,
                    false,
                )
                .await;
                break;
            }
            _ = tokio::time::sleep(PENDING_POLL_INTERVAL) => {
                let (poll_control_tx, poll_control_rx) = oneshot::channel::<BashPollControl>();
                let (poll_text_tx, poll_text_rx) = oneshot::channel::<String>();
                let root_for_poll = root.clone();
                let request_id_for_poll = request_id.clone();
                let task_id_for_poll = task_id.clone();
                let session_for_poll = session_id.clone();
                let storage_for_poll = storage_dir.clone();
                let project_root_for_poll = project_root.clone();
                let format_context_for_poll = format_context.clone();
                // Each queued poll phase carries the same absolute request
                // deadline; a phase already running may finish after it.
                let poll_rx = executor.submit_async_with_deadline(
                    root_for_poll,
                    Lane::PureRead,
                    request_id.clone(),
                    Box::new(move |ctx| {
                        log_ctx::with_session(Some(session_for_poll.clone()), || {
                            let mut poll_text_tx = Some(poll_text_tx);
                            let mut poll_control_tx = Some(poll_control_tx);

                            // Foreground polls only need task state. Terminal snapshots still
                            // return cached output.
                            let Some(snapshot) = crate::commands::bash_orchestrate::poll_bash_status(
                                ctx,
                                &task_id_for_poll,
                                &session_for_poll,
                                project_root_for_poll.as_deref(),
                                &storage_for_poll,
                                0,
                            ) else {
                                if detach_on_user_message {
                                    ctx.bash_background().end_wait_mode_session(
                                        &session_for_poll,
                                        &task_id_for_poll,
                                    );
                                } else {
                                    ctx.bash_background().unregister_foreground_task(
                                        &session_for_poll,
                                        &task_id_for_poll,
                                    );
                                }
                                return finish_bash_poll_done(
                                    crate::commands::bash_orchestrate::task_not_found_response(
                                        &request_id_for_poll,
                                        &task_id_for_poll,
                                    ),
                                    ctx,
                                    &session_for_poll,
                                    &format_context_for_poll,
                                    &mut poll_text_tx,
                                    &mut poll_control_tx,
                                );
                            };

                            if detach_on_user_message
                                && !snapshot.info.status.is_terminal()
                                && ctx
                                    .bash_background()
                                    .take_wait_mode_detach(&session_for_poll)
                            {
                                let response = crate::commands::bash_orchestrate::detach_wait_mode_bash(
                                    ctx,
                                    &task_id_for_poll,
                                    &session_for_poll,
                                    &request_id_for_poll,
                                );
                                ctx.bash_background().end_wait_mode_session(
                                    &session_for_poll,
                                    &task_id_for_poll,
                                );
                                return finish_bash_poll_done(
                                    response,
                                    ctx,
                                    &session_for_poll,
                                    &format_context_for_poll,
                                    &mut poll_text_tx,
                                    &mut poll_control_tx,
                                );
                            }
                            match crate::commands::bash_orchestrate::decide_bash_step(
                                snapshot,
                                deadline,
                                block_to_completion,
                                Instant::now(),
                                &request_id_for_poll,
                            ) {
                                crate::commands::bash_orchestrate::BashStep::Done(response) => {
                                    if detach_on_user_message {
                                        ctx.bash_background().end_wait_mode_session(
                                            &session_for_poll,
                                            &task_id_for_poll,
                                        );
                                    } else {
                                        ctx.bash_background().unregister_foreground_task(
                                            &session_for_poll,
                                            &task_id_for_poll,
                                        );
                                    }
                                    finish_bash_poll_done(
                                        response,
                                        ctx,
                                        &session_for_poll,
                                        &format_context_for_poll,
                                        &mut poll_text_tx,
                                        &mut poll_control_tx,
                                    )
                                }
                                crate::commands::bash_orchestrate::BashStep::Promote => {
                                    if detach_on_user_message {
                                        ctx.bash_background().end_wait_mode_session(
                                            &session_for_poll,
                                            &task_id_for_poll,
                                        );
                                    } else {
                                        ctx.bash_background().unregister_foreground_task(
                                            &session_for_poll,
                                            &task_id_for_poll,
                                        );
                                    }
                                    if let Some(tx) = poll_control_tx.take() {
                                        let _ = tx.send(BashPollControl::Promote);
                                    }
                                    Response::success(
                                        request_id_for_poll,
                                        json!({ "subc_bash_step": "promote" }),
                                    )
                                }
                                crate::commands::bash_orchestrate::BashStep::Wait => {
                                    if let Some(tx) = poll_control_tx.take() {
                                        let _ = tx.send(BashPollControl::Wait);
                                    }
                                    Response::success(
                                        request_id_for_poll,
                                        json!({ "subc_bash_step": "wait" }),
                                    )
                                }
                            }
                        })
                    }),
                    request_deadline,
                );
                let poll_response = await_executor_response(poll_rx, request_id.clone()).await;
                let _ = send_counted_channel(
                    &poll_touch_tx,
                    &metrics.bash_poll_touch_queued,
                    root.clone(),
                )
                .await;
                match poll_control_rx.await.unwrap_or(BashPollControl::Done) {
                    BashPollControl::Done => {
                        // If the executor rejected the poll after the request
                        // deadline elapsed, the command is still running with
                        // wait-mode bookkeeping registered. Promote it to a
                        // background task instead of leaking the registration
                        // and delivering a bare deadline error.
                        if poll_was_deadline_rejected(&poll_response) {
                            if let Some(ctx) = executor.actor_context(&root) {
                                if detach_on_user_message {
                                    ctx.bash_background()
                                        .end_wait_mode_session(&session_id, &task_id);
                                } else {
                                    ctx.bash_background()
                                        .unregister_foreground_task(&session_id, &task_id);
                                }
                            }
                            let result = submit_bash_promote(
                                &executor,
                                root.clone(),
                                request_id.clone(),
                                task_id.clone(),
                                session_id.clone(),
                                timeout,
                                wait_window_ms,
                                format_context.clone(),
                                None,
                            )
                            .await;
                            let fatal = response_is_fatal_panic(&result.response);
                            send_bash_deferred_completion(
                                &completion_tx,
                                &metrics,
                                route,
                                corr,
                                flags,
                                ver,
                                root,
                                request_id,
                                Some(result),
                                fatal,
                            )
                            .await;
                            break;
                        }
                        let text = poll_text_rx.await.unwrap_or_else(|_| {
                            crate::subc_format::format_response_with_context(
                                "bash",
                                &poll_response,
                                &format_context,
                            )
                        });
                        let result = ToolCallResult {
                            text,
                            response: poll_response,
                        };
                        let fatal = response_is_fatal_panic(&result.response);
                        send_bash_deferred_completion(
                            &completion_tx,
                            &metrics,
                            route,
                            corr,
                            flags,
                            ver,
                            root,
                            request_id,
                            Some(result),
                            fatal,
                        )
                        .await;
                        break;
                    }
                    BashPollControl::Promote => {
                        let result = submit_bash_promote(
                            &executor,
                            root.clone(),
                            request_id.clone(),
                            task_id.clone(),
                            session_id.clone(),
                            timeout,
                            wait_window_ms,
                            format_context.clone(),
                            request_deadline,
                        )
                        .await;
                        let fatal = response_is_fatal_panic(&result.response);
                        send_bash_deferred_completion(
                            &completion_tx,
                            &metrics,
                            route,
                            corr,
                            flags,
                            ver,
                            root,
                            request_id,
                            Some(result),
                            fatal,
                        )
                        .await;
                        break;
                    }
                    BashPollControl::Wait => {}
                }
            }
        }
    }
}

async fn submit_bash_promote(
    executor: &Arc<Executor>,
    root: ProjectRootId,
    request_id: String,
    task_id: String,
    session_id: String,
    timeout: Option<u64>,
    wait_window_ms: u64,
    format_context: crate::subc_format::FormatContext,
    request_deadline: Option<Instant>,
) -> ToolCallResult {
    let (text_tx, text_rx) = oneshot::channel::<String>();
    let request_id_for_promote = request_id.clone();
    let task_id_for_promote = task_id.clone();
    let session_for_promote = session_id.clone();
    let format_context_for_promote = format_context.clone();
    // The promote phase carries the same absolute request deadline. If it
    // elapsed while queued, the executor settles with request_deadline_exceeded.
    let promote_rx = executor.submit_async_with_deadline(
        root,
        Lane::Mutating,
        request_id.clone(),
        Box::new(move |ctx| {
            log_ctx::with_session(Some(session_for_promote.clone()), || {
                let response = if let Some(value) =
                    std::env::var_os("AFT_TEST_FORCE_SUBC_BASH_PROMOTE_ERROR")
                {
                    if value.to_string_lossy() == "panic" {
                        panic!("forced subc bash promote panic");
                    }
                    Response::error(
                        &request_id_for_promote,
                        "execution_failed",
                        "forced subc bash promote failure",
                    )
                } else {
                    crate::commands::bash_orchestrate::promote_bash(
                        ctx,
                        &task_id_for_promote,
                        &session_for_promote,
                        timeout,
                        wait_window_ms,
                        &request_id_for_promote,
                    )
                };
                let result = finalized_bash_result(
                    response,
                    ctx,
                    &session_for_promote,
                    &format_context_for_promote,
                    true,
                );
                let ToolCallResult { text, response } = result;
                let _ = text_tx.send(text);
                response
            })
        }),
        request_deadline,
    );
    let response = await_executor_response(promote_rx, request_id).await;
    let text = text_rx.await.unwrap_or_else(|_| {
        crate::subc_format::format_response_with_context("bash", &response, &format_context)
    });
    ToolCallResult { text, response }
}

/// True when a poll-phase executor response proves the request budget
/// elapsed before the poll could be admitted (queued rejection or prune).
/// The command may still be running, so the caller must promote it.
fn poll_was_deadline_rejected(response: &Response) -> bool {
    response.data.get("code").and_then(Value::as_str) == Some("request_deadline_exceeded")
}

#[allow(clippy::too_many_arguments)]
async fn send_bash_deferred_completion(
    completion_tx: &mpsc::Sender<BashDeferredCompletion>,
    metrics: &DispatchPathMetrics,
    route: RouteChannel,
    corr: u64,
    flags: Flags,
    ver: u8,
    root: ProjectRootId,
    request_id: String,
    result: Option<ToolCallResult>,
    fatal: bool,
) {
    let _ = send_counted_channel(
        completion_tx,
        &metrics.bash_deferred_queued,
        BashDeferredCompletion {
            route,
            corr,
            flags,
            ver,
            root,
            request_id,
            result,
            fatal,
        },
    )
    .await;
}

pub(super) async fn handle_bash_deferred_completion(
    tx: &WriterSender,
    done: BashDeferredCompletion,
    routes: &HashMap<RouteChannel, RouteIdentity>,
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    route_bash_cancels: &mut HashMap<RouteChannel, RouteBashCancel>,
    shutdown: &Arc<Notify>,
    metrics: &DispatchPathMetrics,
) -> Result<(), SubcError> {
    if let Some(meta) = live_roots.get_mut(&done.root) {
        meta.active_bash_waits = meta.active_bash_waits.saturating_sub(1);
        meta.note_activity();
    }
    let route_id = done.route;
    let remove_route_cancel = if let Some(cancel) = route_bash_cancels.get_mut(&route_id) {
        cancel.active_waits = cancel.active_waits.saturating_sub(1);
        cancel.active_waits == 0
    } else {
        false
    };
    if remove_route_cancel {
        route_bash_cancels.remove(&route_id);
    }

    if let Some(result) = done.result {
        if let Some(identity) = routes.get(&route_id) {
            let frame = build_tool_response_frame(
                done.ver,
                done.route,
                done.corr,
                done.flags,
                &result,
                identity.trust,
            )?;
            send_reliable_writer_frame(tx, metrics, frame, "deferred bash response").await?;
        } else {
            log::debug!(
                "subc attach: dropping deferred bash response {} for unbound route {}",
                done.request_id,
                done.route
            );
        }
    } else {
        log::debug!(
            "subc attach: deferred bash wait {} cancelled before delivery on route {}",
            done.request_id,
            done.route
        );
    }

    if done.fatal {
        signal_fatal_teardown(tx, Some(done.route), done.ver, done.corr, shutdown, metrics).await;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn bash_denied_untrusted_completion(
    route: RouteChannel,
    corr: u64,
    flags: Flags,
    ver: u8,
    root: ProjectRootId,
    request_id: String,
    format_context: crate::subc_format::FormatContext,
) -> BashDeferredCompletion {
    let response = bash_denied_untrusted_response(request_id.clone());
    BashDeferredCompletion {
        route,
        corr,
        flags,
        ver,
        root,
        request_id,
        result: Some(bash_result_from_response(response, &format_context)),
        fatal: false,
    }
}

/// A deadline-expired deferred bash completion: the request budget elapsed
/// before any command could start, so the response proves non-execution.
#[allow(clippy::too_many_arguments)]
pub(super) fn bash_deadline_exceeded_completion(
    route: RouteChannel,
    corr: u64,
    flags: Flags,
    ver: u8,
    root: ProjectRootId,
    request_id: String,
    format_context: crate::subc_format::FormatContext,
    response: crate::protocol::Response,
) -> BashDeferredCompletion {
    BashDeferredCompletion {
        route,
        corr,
        flags,
        ver,
        root,
        request_id,
        result: Some(bash_result_from_response(response, &format_context)),
        fatal: false,
    }
}

pub(super) fn bash_denied_untrusted_response(request_id: impl Into<String>) -> Response {
    Response::error(
        request_id.into(),
        "bash_denied_untrusted",
        "remote/MCP-facade binds cannot run shell commands",
    )
}

#[cfg(test)]
mod grant_path_tests {
    #[test]
    fn foreground_background_and_pty_share_submit_deferred_spawn_path() {
        let source = include_str!("bash.rs");
        let submit = source
            .split("pub(super) fn submit_deferred_bash")
            .nth(1)
            .and_then(|source| source.split("async fn run_deferred_bash_wait").next())
            .expect("submit_deferred_bash source");
        assert_eq!(
            submit.matches("dispatch(raw_req, ctx)").count(),
            1,
            "all bash modes must use the same authenticated dispatch"
        );
        let dispatch = submit.find("dispatch(raw_req, ctx)").unwrap();
        let mode_branch = submit
            .find("if is_pty || settings.background")
            .expect("background/PTY branch");
        let foreground_wait = submit
            .find("select_foreground_wait_window_ms")
            .expect("foreground branch");
        assert!(dispatch < mode_branch && mode_branch < foreground_wait);
        assert!(submit.contains("with_authenticated_principal"));
    }
}
