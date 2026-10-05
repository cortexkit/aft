//! Deferred bash orchestration and trust-gated shell helpers for subc route calls.

use super::*;

#[derive(Clone)]
pub(super) struct BashWaitCancel {
    pub(super) connection: PersistentCancelSignal,
    pub(super) route: PersistentCancelSignal,
    /// Open while the daemon drains this module; a foreground wait that sees it
    /// detaches its command into a background task and answers at once.
    pub(super) drain: drain::ModuleDrainWindow,
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

#[cfg(test)]
impl BashDeferredCompletion {
    pub(super) fn response_for_test(&self) -> &Response {
        &self
            .result
            .as_ref()
            .expect("bash completion result")
            .response
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct BashTranslatedSettings {
    background: bool,
    pty: bool,
    wait: bool,
    block_to_completion: bool,
    timeout: Option<u64>,
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
        worker_session: bool,
        /// The worker wait limit bounding this wait, when it is a delegated
        /// worker's blocking call (`wait: true` or `block_to_completion`).
        worker_cap_ms: Option<u64>,
    },
}

enum BashPollControl {
    Done,
    Promote,
    Wait,
    /// The module loop already answered the call; the poll changed nothing.
    Abandoned,
}

/// Ends a poll job that found its call already answered by the module loop,
/// without touching the task or its wait registration (the loop owns both).
fn abandon_bash_poll(
    request_id: String,
    control_tx: &mut Option<oneshot::Sender<BashPollControl>>,
) -> Response {
    if let Some(tx) = control_tx.take() {
        let _ = tx.send(BashPollControl::Abandoned);
    }
    Response::success(request_id, json!({ "subc_bash_step": "abandoned" }))
}

/// Ends the wait registration a foreground bash call holds for its task.
fn release_wait_registration(
    registry: &crate::bash_background::BgTaskRegistry,
    session_id: &str,
    task_id: &str,
    wait_mode: bool,
) {
    if wait_mode {
        registry.end_wait_mode_session(session_id, task_id);
    } else {
        registry.unregister_foreground_task(session_id, task_id);
    }
}

/// What one wake of a deferred bash wait learned about its task.
struct DeferredWaitObservation {
    /// The task reached a terminal state, or is no longer in the registry.
    target_finished: bool,
    /// A `wait: true` call's session asked to detach the wait.
    detach_pending: bool,
}

/// Reads a deferred bash wait's task state on the blocking pool.
///
/// The wait loop runs on the subc frame loop's single-threaded runtime. The
/// registry snapshot takes the task's state mutex, which the bash watchdog can
/// hold while it persists the task (a file write plus an aft.db upsert), and a
/// terminal snapshot may render cached output from disk. Doing either inline
/// stops the frame loop, so no route's frames are read or written until the
/// lock frees. Here the frame loop only awaits.
///
/// `renew_window` is set for a delegated worker's capped wait: the worker is
/// waiting on its command, so each wake keeps the task's default hard kill at
/// least that far away, and the kill cannot fire before the cap hands control
/// back to the worker.
async fn observe_deferred_bash_wait(
    registry: crate::bash_background::BgTaskRegistry,
    task_id: String,
    session_id: String,
    wait_mode: bool,
    renew_window: Option<Duration>,
) -> DeferredWaitObservation {
    tokio::task::spawn_blocking(move || {
        if let Some(window) = renew_window {
            registry.renew_hard_kill(&task_id, &session_id, window);
        }
        let target_finished = registry
            .observed_status(&task_id, &session_id, 0)
            .is_none_or(|snapshot| snapshot.info.status.is_terminal());
        let detach_pending = wait_mode && registry.wait_mode_detach_pending(&session_id);
        DeferredWaitObservation {
            target_finished,
            detach_pending,
        }
    })
    .await
    // A panicked read learned nothing; hand the decision to the executor poll,
    // which reads the task again and answers the call either way.
    .unwrap_or(DeferredWaitObservation {
        target_finished: true,
        detach_pending: false,
    })
}

/// Hands a held call's command to the background the way a drain detach does
/// (the task keeps running and delivers its completion later), off the
/// executor and off the module loop's thread: promotion writes task metadata.
pub(super) fn detach_held_bash_in_background(target: drain::BashDetachTarget, cancelled: bool) {
    tokio::task::spawn_blocking(move || {
        if target.server_completion
            && (cancelled
                || !target
                    .registry
                    .is_remote_task(&target.task_id, &target.session_id))
        {
            let _ = target.registry.kill(&target.task_id, &target.session_id);
        } else if let Err(error) = target.registry.promote(&target.task_id, &target.session_id) {
            log::warn!(
                "subc attach: could not hand bash task {} to the background after answering its call: {error}",
                target.task_id
            );
        }
        release_wait_registration(
            &target.registry,
            &target.session_id,
            &target.task_id,
            target.wait_mode,
        );
    });
}

/// Answers the held bash calls the module loop must answer itself (see
/// [`drain::HeldBashCalls::claim_for_module_loop`]) and hands each command to
/// the background. A drained call gets the same "detached" response its own
/// wait loop would have sent; a cancelled call gets the `cancelled` error every
/// other cancelled tool call gets. Returns how many calls were answered.
pub(super) async fn answer_held_bash_calls_from_module_loop(
    tx: &WriterSender,
    routes: &HashMap<RouteChannel, RouteIdentity>,
    metrics: &DispatchPathMetrics,
    drain_due: bool,
) -> Result<usize, SubcError> {
    let claimed = metrics.held_bash_calls.claim_for_module_loop(drain_due);
    let answered = claimed.len();
    for (route, corr, target, reason) in claimed {
        log::info!(
            "subc attach: answered held bash call {} on route {route} corr={corr} from the module loop (reason={reason:?}); task {} keeps running in the background",
            target.request_id,
            target.task_id
        );
        let frame = match (reason, routes.get(&route)) {
            (_, None) => None,
            (drain::BashLoopAnswer::Drain, Some(_)) if target.server_completion => {
                Some(build_error_frame(
                    target.ver,
                    route.channel,
                    route.epoch,
                    corr,
                    target.flags,
                    if target
                        .registry
                        .is_remote_task(&target.task_id, &target.session_id)
                    {
                        "outcome_unknown_module_draining"
                    } else {
                        "cancelled"
                    },
                    &if target
                        .registry
                        .is_remote_task(&target.task_id, &target.session_id)
                    {
                        format!("remote task {} survives module drain; inspect bash_status after reconnecting, never rerun the command",target.task_id)
                    } else {
                        "server-owned call ended during module drain".into()
                    },
                )?)
            }
            (drain::BashLoopAnswer::Drain, Some(identity)) => {
                let response = Response::success(
                    &target.request_id,
                    json!({
                        "output": format!(
                        "{}{}",
                        crate::commands::bash_orchestrate::format_module_drain_detach_message(
                            &target.task_id,
                            target.worker_session,
                            target
                                .format_context
                                .bash_watch_available
                                .unwrap_or(target.worker_session),
                        ),
                        crate::commands::bash_orchestrate::kill_deadline_note(&target.registry, &target.task_id, &target.session_id, target.worker_session),
                    ),
                        "task_id": target.task_id,
                        "status": "running",
                    }),
                );
                let result = bash_result_from_response(response, &target.format_context);
                Some(build_tool_response_frame(
                    target.ver,
                    route,
                    corr,
                    target.flags,
                    &result,
                    identity.trust,
                )?)
            }
            (drain::BashLoopAnswer::Cancel, Some(_)) => Some(build_error_frame(
                target.ver,
                route.channel,
                route.epoch,
                corr,
                target.flags,
                "cancelled",
                "request cancelled",
            )?),
        };
        detach_held_bash_in_background(target, matches!(reason, drain::BashLoopAnswer::Cancel));
        if let Some(frame) = frame {
            send_reliable_writer_frame(tx, metrics, frame, "held bash answer").await?;
        }
    }
    Ok(answered)
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
    repeat: Option<crate::run_tool_call::RepeatObservation>,
) -> ToolCallResult {
    // The bash formatter never renders `bg_completions`, so formatting before finalization
    // yields the same text and lets the finalizer append the status bar to it.
    let mut text =
        crate::subc_format::format_response_with_context("bash", &response, format_context);
    // Observe before finalizing: the status bar the finalizer appends carries
    // moving counts that would make identical results hash differently.
    if let Some(repeat) = repeat {
        repeat.observe(ctx, &mut text);
    }
    crate::response_finalize::finalize_tool_response(
        &mut response,
        &mut text,
        ctx,
        session_id,
        "bash",
        allow_bg_completions,
    );
    ToolCallResult { text, response }
}

fn bash_result_from_response(
    response: Response,
    format_context: &crate::subc_format::FormatContext,
) -> ToolCallResult {
    let text = crate::subc_format::format_response_with_context("bash", &response, format_context);
    ToolCallResult { text, response }
}

fn bash_background_launch_response(
    request_id: &str,
    task_id: &str,
    is_pty: bool,
    worker_session: bool,
    bash_watch_available: bool,
    deadline_note: &str,
) -> Response {
    Response::success(
        request_id,
        json!({
            "output": format!(
                "{}{deadline_note}",
                crate::commands::bash_orchestrate::format_background_launch(
                    task_id,
                    is_pty,
                    worker_session,
                    bash_watch_available,
                )
            ),
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
    repeat: &mut Option<crate::run_tool_call::RepeatObservation>,
) -> Response {
    let result = finalized_bash_result(
        response,
        ctx,
        session_id,
        format_context,
        allow_bg_completions,
        repeat.take(),
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
    allow_bg_completions: bool,
    repeat: &mut Option<crate::run_tool_call::RepeatObservation>,
) -> Response {
    let result = finalized_bash_result(
        response,
        ctx,
        session_id,
        format_context,
        allow_bg_completions,
        repeat.take(),
    );
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
    call_key: Option<String>,
    permissions_granted: Option<Vec<String>>,
    repeat: Option<crate::run_tool_call::RepeatObservation>,
    worker_session: bool,
    server_completion: bool,
    remote_key: Option<crate::db::remote_exec::PolicyKey>,
    received_at: Instant,
) {
    // Leave room for writer queueing and the daemon relay under the shortest
    // supported client transport deadline (25 s). Explicit waits only extend
    // execution, never startup admission or control-file creation.
    let spawn_ctx = match executor.try_actor_context(&root) {
        Some(Some(ctx)) => ctx,
        state => {
            let code = if state.is_none() {
                "executor_busy"
            } else {
                "actor_not_registered"
            };
            let response = Response::error(
                &request_id,
                code,
                "bash refused before startup: executor admission state is unavailable",
            );
            let result = bash_result_from_response(response, &format_context);
            let completion_tx = completion_tx.clone();
            let metrics = metrics.clone();
            tokio::spawn(async move {
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
                    false,
                )
                .await;
            });
            return;
        }
    };
    let startup_window = if server_completion {
        20_000
    } else {
        crate::commands::bash_orchestrate::resolve_foreground_wait_window_ms(
            spawn_ctx.config().foreground_wait_window_ms,
        )
    }
    .min(20_000);
    let startup_deadline = received_at + Duration::from_millis(startup_window);
    let receipt = Arc::new(crate::bash_background::SpawnReceipt::new(startup_deadline));
    let receipt_for_spawn = Arc::clone(&receipt);
    let admitted = Arc::new(AtomicBool::new(false));
    let admitted_for_spawn = Arc::clone(&admitted);
    let claim = metrics.held_bash_calls.insert(route, corr);
    let (spawn_control_tx, spawn_control_rx) = oneshot::channel::<BashSpawnControl>();
    let (spawn_text_tx, spawn_text_rx) = oneshot::channel::<String>();
    let root_for_spawn = root.clone();
    let request_id_for_spawn = request_id.clone();
    let session_for_spawn = session_id.clone();
    let project_root_for_spawn = project_root.clone();
    let format_context_for_spawn = format_context.clone();
    // A call that returns its result ends in exactly one of three places: the
    // spawn job answers it at once (a rewritten command, an error, or a
    // background launch), a poll job answers it when the foreground wait
    // finishes, or the promote job answers it when the wait window closes. Each
    // gets its own copy; only the one that answers observes the call. A held
    // call the module loop answers instead (drain or cancel) carries no result
    // and is not observed.
    let mut repeat_for_spawn = repeat.clone();
    let ledger_key =
        call_key
            .as_ref()
            .filter(|_| server_completion)
            .map(|key| crate::db::call_ledger::Key {
                carrier: match &spawn_principal {
                    AuthenticatedPrincipal::RouteBind { principal_id, .. } => {
                        principal_id.clone().unwrap_or_else(|| "absent".into())
                    }
                    _ => "first-party".into(),
                },
                call_key: key.clone(),
            });
    let submit_executor = executor.clone();
    let spawn_cancel = JobCancellation::new();
    let submit_cancel = spawn_cancel.clone();
    let submit_request_id = request_id.clone();
    // Even submission can wait for a scheduler lock. The reply timer below
    // lives on the frame runtime; submission and every spawn syscall do not.
    let spawn_submission = tokio::task::spawn_blocking(move || {
        submit_executor.submit_tool_call_with_cancellation_async(
            root_for_spawn,
            Lane::Mutating,
            submit_request_id,
            "bash",
            Box::new(move |ctx| {
                log_ctx::with_session(Some(session_for_spawn.clone()), || {
                    admitted_for_spawn.store(true, Ordering::Relaxed);
                    if receipt_for_spawn.refused() {
                        return Response::error(
                            &request_id_for_spawn,
                            "bash_start_deadline",
                            "bash startup deadline expired while queued for executor admission",
                        );
                    }
                    let mut spawn_text_tx = Some(spawn_text_tx);
                    let mut spawn_control_tx = Some(spawn_control_tx);

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
                            &mut repeat_for_spawn,
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
                                &mut repeat_for_spawn,
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
                    // `worker_session` comes from the plugin-set field of the subc
                    // call body, never from the agent's arguments: an agent must not be able to claim it is a
                    // worker to lift the default hard kill on its command.
                    translated
                        .args
                        .remove(crate::protocol::WORKER_SESSION_FIELD);
                    if worker_session {
                        translated.args.insert(
                            crate::protocol::WORKER_SESSION_FIELD.to_string(),
                            Value::Bool(true),
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
                    let (response, storage_dir) =
                        crate::sandbox_spawn::with_authenticated_principal(spawn_principal, || {
                            crate::bash_background::with_call_key(call_key, || {
                                crate::bash_background::registry::with_ledger_spawn(ledger_key.clone(), || {
                                (
                                    crate::bash_background::with_spawn_receipt(
                                        Arc::clone(&receipt_for_spawn),
                                        || crate::bash_background::with_remote_policy(
                                            super::remote_policy::lookup(ctx, remote_key.as_ref()),
                                            || dispatch(raw_req, ctx),
                                        ),
                                    ),
                                    crate::bash_background::task_storage_dir(ctx),
                                )
                                })
                            })
                        });
                    if let (Some(key), Some(db)) = (&ledger_key, ctx.db()) {
                        if let Ok(conn) = db.lock() {
                            if let Err(error) =
                                crate::db::call_ledger::note_native_outcome(&conn, key, &response)
                            {
                                log::warn!("call ledger shell outcome recording failed: {error}");
                            }
                        }
                    }
                    if !response.success {
                        return finish_bash_spawn_immediate(
                            response,
                            ctx,
                            &session_for_spawn,
                            &format_context_for_spawn,
                            &mut spawn_text_tx,
                            &mut spawn_control_tx,
                            true,
                            &mut repeat_for_spawn,
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
                            &mut repeat_for_spawn,
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
                            &mut repeat_for_spawn,
                        );
                    }

                    let mode = response
                        .data
                        .get("mode")
                        .and_then(Value::as_str)
                        .unwrap_or("pipes");
                    let is_pty = mode == "pty" || settings.pty;
                    if !server_completion && (is_pty || settings.background) {
                        let response = bash_background_launch_response(
                            &request_id_for_spawn,
                            &task_id,
                            is_pty,
                            worker_session,
                            format_context_for_spawn
                                .bash_watch_available
                                .unwrap_or(worker_session),
                            &crate::commands::bash_orchestrate::kill_deadline_note(
                                ctx.bash_background(),
                                &task_id,
                                &session_for_spawn,
                                worker_session,
                            ),
                        );
                        return finish_bash_spawn_immediate(
                            response,
                            ctx,
                            &session_for_spawn,
                            &format_context_for_spawn,
                            &mut spawn_text_tx,
                            &mut spawn_control_tx,
                            true,
                            &mut repeat_for_spawn,
                        );
                    }

                    // A server-owned call is killed rather than detached when it
                    // ends, so the worker wait limit (which detaches) leaves it be.
                    let worker_cap_ms = crate::commands::bash_orchestrate::worker_wait_cap_ms(
                        worker_session && !server_completion,
                        settings.block_to_completion || settings.wait,
                        crate::commands::bash_orchestrate::worker_wait_max_ms(ctx),
                    );
                    let wait_window_ms = worker_cap_ms.unwrap_or_else(|| {
                        crate::commands::bash_orchestrate::select_foreground_wait_window_ms(
                            ctx.config().foreground_wait_window_ms,
                            settings.timeout,
                            settings.wait,
                        )
                    });
                    let wait_window_ms =
                        if settings.wait || settings.block_to_completion || server_completion {
                            wait_window_ms
                        } else {
                            wait_window_ms.min(20_000)
                        };
                    let deadline = received_at + Duration::from_millis(wait_window_ms);
                    let project_root = ctx.config().project_root.clone();
                    // Register the session as detachable exactly like the
                    // standalone path (bash_orchestrate) does: without this, a
                    // bash_wait_detach signal finds no active wait and wait:true
                    // blocks through user messages.
                    let detach_on_user_message = settings.wait && !server_completion;
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
                            // A capped worker wait detaches at its deadline.
                            block_to_completion: (settings.block_to_completion || settings.wait)
                                && worker_cap_ms.is_none(),
                            timeout: settings.timeout,
                            wait_window_ms,
                            detach_on_user_message,
                            worker_session,
                            worker_cap_ms,
                        });
                    }
                    response
                })
            }),
            submit_cancel,
        )
    });

    let executor = Arc::clone(executor);
    let completion_tx = completion_tx.clone();
    let poll_touch_tx = poll_touch_tx.clone();
    let task_metrics = Arc::clone(metrics);
    let root_for_task = root.clone();
    tokio::spawn(async move {
        let _response_task = ResponseTaskGuard::new(&task_metrics);
        let spawn_request_id = request_id.clone();
        let spawn_future = async move {
            match spawn_submission.await {
                Ok(rx) => await_executor_response(rx, spawn_request_id.clone()).await,
                Err(error) => Response::error(
                    &spawn_request_id,
                    "execution_failed",
                    format!("bash submission worker failed: {error}"),
                ),
            }
        };
        tokio::pin!(spawn_future);
        let mut answered_startup = false;
        let spawn_response = tokio::select! {
            biased;
            response = &mut spawn_future => response,
            _ = tokio::time::sleep_until(startup_deadline.into()) => {
                let task_id = receipt.expire();
                if task_id.is_none() { spawn_cancel.request_cancel(); }
                let response = match task_id {
                    Some(task_id) if !server_completion => deadline_handoff_response(&request_id, &task_id, startup_window, worker_session),
                    _ => Response::error(&request_id, "bash_start_deadline", startup_refusal_reason(&executor, &root_for_task, admitted.load(Ordering::Relaxed))),
                };
                log::warn!("bash startup reply deadline channel={} corr={corr} elapsed_ms={} admitted={} code={}", route.channel, received_at.elapsed().as_millis(), admitted.load(Ordering::Relaxed), response.data.get("code").and_then(Value::as_str).unwrap_or("promoted"));
                send_bash_deferred_completion(&completion_tx, &task_metrics, route, corr, flags, ver, root_for_task.clone(), request_id.clone(), Some(bash_result_from_response(response, &format_context)), false).await;
                answered_startup = true;
                (&mut spawn_future).await
            }
        };
        let spawn_control = spawn_control_rx.await;
        if answered_startup {
            // A slow startup may settle after its caller has already received
            // the task id. Promotion/persistence must not hold that reply open.
            if let Ok(BashSpawnControl::Foreground {
                task_id,
                session_id,
                detach_on_user_message,
                ..
            }) = spawn_control
            {
                detach_held_bash_in_background(drain::BashDetachTarget {
                    task_id,
                    session_id,
                    wait_mode: detach_on_user_message,
                    worker_session,
                    server_completion,
                    registry: spawn_ctx.bash_background().clone(),
                    request_id,
                    ver,
                    flags,
                    format_context,
                });
            }
            return;
        }
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
                worker_session,
                worker_cap_ms,
            }) => {
                let phase = if detach_on_user_message {
                    drain::BashHoldPhase::Wait
                } else if block_to_completion {
                    drain::BashHoldPhase::Block
                } else {
                    drain::BashHoldPhase::Foreground
                };
                task_metrics.held_bash_calls.set_phase(route, corr, phase);
                let _deferred_wait = DeferredBashWaitGuard::new(&task_metrics);
                let deadline_target = drain::BashDetachTarget {
                    task_id: task_id.clone(),
                    session_id: session_id.clone(),
                    wait_mode: detach_on_user_message,
                    worker_session,
                    server_completion,
                    registry: spawn_ctx.bash_background().clone(),
                    request_id: request_id.clone(),
                    ver,
                    flags,
                    format_context: format_context.clone(),
                };
                let wait_future = run_deferred_bash_wait(
                    executor,
                    completion_tx.clone(),
                    poll_touch_tx,
                    task_metrics.clone(),
                    route,
                    corr,
                    flags,
                    ver,
                    root_for_task.clone(),
                    request_id.clone(),
                    task_id,
                    session_id,
                    project_root,
                    storage_dir,
                    deadline,
                    block_to_completion,
                    timeout,
                    wait_window_ms,
                    detach_on_user_message,
                    worker_session,
                    worker_cap_ms,
                    format_context,
                    cancel,
                    claim.clone(),
                    repeat,
                    server_completion,
                );
                // A healthy executor normally finishes the deadline poll and
                // promotion, including repeat steering and the hard-kill note,
                // within one poll turn. Only bypass it when it fails to answer;
                // the bounded grace still leaves nearly five seconds under the
                // 25 s transport class, even at the largest plain wait window.
                let reply_deadline = deadline + PENDING_POLL_INTERVAL * 2;
                tokio::select! {
                    biased;
                    _ = wait_future => {}
                    _ = tokio::time::sleep_until(reply_deadline.into()), if !block_to_completion && !server_completion && worker_cap_ms.is_none() => {
                        if claim.claim_for_wait_task() {
                            let response = deadline_handoff_response(&request_id, &deadline_target.task_id, wait_window_ms, worker_session);
                            let result = bash_result_from_response(response, &deadline_target.format_context);
                            detach_held_bash_in_background(deadline_target);
                            send_bash_deferred_completion(&completion_tx, &task_metrics, route, corr, flags, ver, root_for_task, request_id, Some(result), false).await;
                        }
                    }
                }
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

fn deadline_handoff_response(
    request_id: &str,
    task_id: &str,
    wait_window_ms: u64,
    worker_session: bool,
) -> Response {
    Response::success(
        request_id,
        json!({
            "output": crate::commands::bash_orchestrate::format_promotion_message(task_id, None, wait_window_ms, worker_session),
            "task_id": task_id, "status": "running",
        }),
    )
}

fn startup_refusal_reason(executor: &Executor, root: &ProjectRootId, admitted: bool) -> String {
    if admitted {
        return "bash startup exceeded its reply budget during control-file creation or process startup; process creation is refused if it has not committed".into();
    }
    if let Some(writer) = executor
        .try_mutating_lane_snapshots()
        .and_then(|writers| writers.into_iter().find(|writer| &writer.root_id == root))
    {
        return format!("bash startup deadline: queued behind root Mutating job {} ({}); command was not started", writer.request_id, writer.command);
    }
    if executor
        .try_dispatch_liveness_snapshot()
        .is_some_and(|snapshot| {
            snapshot.running.interactive + snapshot.running.maintenance >= executor.pool_size()
        })
    {
        return "bash startup deadline: executor workers saturated; command was not started".into();
    }
    "bash startup deadline: queued for executor admission or scheduler lock; command was not started".into()
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
    worker_session: bool,
    worker_cap_ms: Option<u64>,
    format_context: crate::subc_format::FormatContext,
    cancel: BashWaitCancel,
    claim: Arc<drain::BashCallClaim>,
    repeat: Option<crate::run_tool_call::RepeatObservation>,
    server_completion: bool,
) {
    let Some(wait_ctx) = executor.actor_context(&root) else {
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
        return;
    };
    let registry = wait_ctx.bash_background().clone();
    // From here the module loop can answer this call and detach its command
    // without this task, should the executor polls below stall.
    metrics.held_bash_calls.set_detach_target(
        route,
        corr,
        drain::BashDetachTarget {
            task_id: task_id.clone(),
            session_id: session_id.clone(),
            wait_mode: detach_on_user_message,
            worker_session,
            server_completion,
            registry: registry.clone(),
            request_id: request_id.clone(),
            ver,
            flags,
            format_context: format_context.clone(),
        },
    );
    loop {
        tokio::select! {
            _ = claim.module_loop_claimed() => {
                // The module loop sent the terminal frame and owns the task's
                // registration; this completion only settles the accounting.
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
            _ = cancel.cancelled() => {
                if claim.claim_for_wait_task() {
                    // Registry locks stay off the frame loop's thread.
                    let registry = registry.clone();
                    let session_id = session_id.clone();
                    let task_id = task_id.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        if server_completion && !registry.is_remote_task(&task_id,&session_id) { let _ = registry.kill(&task_id, &session_id); }
                        else if registry.is_remote_task(&task_id,&session_id) { let _=registry.promote(&task_id,&session_id); }
                        release_wait_registration(
                            &registry,
                            &session_id,
                            &task_id,
                            detach_on_user_message,
                        );
                    })
                    .await;
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
            observation = async {
                tokio::select! {
                    _ = registry.terminal_transition_notified() => {}
                    _ = tokio::time::sleep(PENDING_POLL_INTERVAL) => {}
                }
                observe_deferred_bash_wait(
                    registry.clone(),
                    task_id.clone(),
                    session_id.clone(),
                    detach_on_user_message,
                    worker_cap_ms.map(Duration::from_millis),
                )
                .await
            } => {
                let DeferredWaitObservation { target_finished, detach_pending } = observation;
                let promotion_due = !block_to_completion && Instant::now() >= deadline;
                // While the module drains, every foreground wait (wait:true,
                // block_to_completion, or a plain wait window) is detached into
                // a background task so its request can answer before the drain
                // deadline. The command keeps running and its completion is
                // delivered like any promoted task's.
                let drain_detach_due = cancel.drain.is_active() && !server_completion;
                if !target_finished && !detach_pending && !promotion_due && !drain_detach_due {
                    continue;
                }
                let (poll_control_tx, poll_control_rx) = oneshot::channel::<BashPollControl>();
                let (poll_text_tx, poll_text_rx) = oneshot::channel::<String>();
                let root_for_poll = root.clone();
                let request_id_for_poll = request_id.clone();
                let task_id_for_poll = task_id.clone();
                let session_for_poll = session_id.clone();
                let storage_for_poll = storage_dir.clone();
                let project_root_for_poll = project_root.clone();
                let format_context_for_poll = format_context.clone();
                let claim_for_poll = Arc::clone(&claim);
                let mut repeat_for_poll = repeat.clone();
                let poll_rx = executor.submit_async(
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
                                if !claim_for_poll.claim_for_wait_task() {
                                    return abandon_bash_poll(request_id_for_poll, &mut poll_control_tx);
                                }
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
                                    true,
                                    &mut repeat_for_poll,
                                );
                            };

                            if drain_detach_due && !snapshot.info.status.is_terminal() {
                                if !claim_for_poll.claim_for_wait_task() {
                                    return abandon_bash_poll(request_id_for_poll, &mut poll_control_tx);
                                }
                                let response =
                                    crate::commands::bash_orchestrate::detach_bash_for_module_drain(
                                        ctx,
                                        &task_id_for_poll,
                                        &session_for_poll,
                                        &request_id_for_poll,
                                        worker_session,
                                        format_context_for_poll
                                            .bash_watch_available
                                            .unwrap_or(worker_session),
                                    );
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
                                    response,
                                    ctx,
                                    &session_for_poll,
                                    &format_context_for_poll,
                                    &mut poll_text_tx,
                                    &mut poll_control_tx,
                                    false,
                                    &mut repeat_for_poll,
                                );
                            }
                            if detach_on_user_message
                                && !snapshot.info.status.is_terminal()
                                && ctx
                                    .bash_background()
                                    .take_wait_mode_detach(&session_for_poll)
                            {
                                if !claim_for_poll.claim_for_wait_task() {
                                    return abandon_bash_poll(request_id_for_poll, &mut poll_control_tx);
                                }
                                let response = crate::commands::bash_orchestrate::detach_wait_mode_bash(
                                    ctx,
                                    &task_id_for_poll,
                                    &session_for_poll,
                                    &request_id_for_poll,
                                    worker_session,
                                    format_context_for_poll
                                        .bash_watch_available
                                        .unwrap_or(worker_session),
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
                                    false,
                                    &mut repeat_for_poll,
                                );
                            }
                            match crate::commands::bash_orchestrate::decide_bash_step(
                                snapshot,
                                deadline,
                                block_to_completion,
                                worker_cap_ms.is_some()
                                    && crate::commands::bash_orchestrate::worker_kill_deadline_within_handoff_margin(
                                        ctx.bash_background().hard_kill_remaining(
                                            &task_id_for_poll,
                                            &session_for_poll,
                                        ),
                                    ),
                                Instant::now(),
                                &request_id_for_poll,
                            ) {
                                crate::commands::bash_orchestrate::BashStep::Done(response) => {
                                    if !claim_for_poll.claim_for_wait_task() {
                                        return abandon_bash_poll(request_id_for_poll, &mut poll_control_tx);
                                    }
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
                                        true,
                                        &mut repeat_for_poll,
                                    )
                                }
                                crate::commands::bash_orchestrate::BashStep::Promote => {
                                    if !claim_for_poll.claim_for_wait_task() {
                                        return abandon_bash_poll(request_id_for_poll, &mut poll_control_tx);
                                    }
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
                );
                // The poll may sit queued behind other work on the root's actor;
                // if the module loop answers the call meanwhile, stop waiting.
                let poll_response = tokio::select! {
                    response = await_executor_response(poll_rx, request_id.clone()) => response,
                    _ = claim.module_loop_claimed() => {
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
                };
                let _ = send_counted_channel(
                    &poll_touch_tx,
                    &metrics.bash_poll_touch_queued,
                    root.clone(),
                )
                .await;
                match poll_control_rx.await.unwrap_or(BashPollControl::Done) {
                    BashPollControl::Done => {
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
                        // A poll job that never ran (its actor went away) claimed
                        // nothing; the module loop may have answered meanwhile.
                        let result = claim.claim_for_wait_task().then_some(result);
                        send_bash_deferred_completion(
                            &completion_tx,
                            &metrics,
                            route,
                            corr,
                            flags,
                            ver,
                            root,
                            request_id,
                            result,
                            fatal,
                        )
                        .await;
                        break;
                    }
                    BashPollControl::Abandoned => {
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
                    BashPollControl::Promote => {
                        let result = submit_bash_promote(
                            &executor,
                            root.clone(),
                            request_id.clone(),
                            task_id.clone(),
                            session_id.clone(),
                            timeout,
                            wait_window_ms,
                            worker_session,
                            worker_cap_ms.is_some(),
                            format_context.clone(),
                            repeat.clone(),
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
    worker_session: bool,
    capped_worker_wait: bool,
    format_context: crate::subc_format::FormatContext,
    repeat: Option<crate::run_tool_call::RepeatObservation>,
) -> ToolCallResult {
    let (text_tx, text_rx) = oneshot::channel::<String>();
    let request_id_for_promote = request_id.clone();
    let task_id_for_promote = task_id.clone();
    let session_for_promote = session_id.clone();
    let format_context_for_promote = format_context.clone();
    let promote_rx = executor.submit_async(
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
                        worker_session,
                        format_context_for_promote
                            .bash_watch_available
                            .unwrap_or(worker_session),
                        capped_worker_wait,
                    )
                };
                let result = finalized_bash_result(
                    response,
                    ctx,
                    &session_for_promote,
                    &format_context_for_promote,
                    false,
                    repeat,
                );
                let ToolCallResult { text, response } = result;
                let _ = text_tx.send(text);
                response
            })
        }),
    );
    let response = await_executor_response(promote_rx, request_id).await;
    let text = text_rx.await.unwrap_or_else(|_| {
        crate::subc_format::format_response_with_context("bash", &response, &format_context)
    });
    ToolCallResult { text, response }
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
    metrics.held_bash_calls.remove(done.route, done.corr);
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

/// Terminal for a bash call answered early because the module is draining. The
/// command never ran, so the retryable error is safe to retry.
#[allow(clippy::too_many_arguments)]
pub(super) fn bash_module_draining_completion(
    route: RouteChannel,
    corr: u64,
    flags: Flags,
    ver: u8,
    root: ProjectRootId,
    request_id: String,
    format_context: crate::subc_format::FormatContext,
) -> BashDeferredCompletion {
    let response = drain::module_draining_response(&request_id, "bash");
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
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use serde_json::json;
    use tokio::sync::mpsc;

    use super::*;

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
            .find("if !server_completion && (is_pty || settings.background)")
            .expect("background/PTY branch");
        let foreground_wait = submit
            .find("select_foreground_wait_window_ms")
            .expect("foreground branch");
        assert!(dispatch < mode_branch && mode_branch < foreground_wait);
        assert!(submit.contains("with_authenticated_principal"));
    }

    fn running_bash_stub(req: RawRequest, ctx: &AppContext) -> Response {
        let raw_params = req
            .params
            .get("params")
            .cloned()
            .unwrap_or_else(|| req.params.clone());
        let command = raw_params
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("sleep 1");
        crate::bash_background::spawn(
            &req.id,
            req.session(),
            command,
            crate::bash_background::BashShell::Bash,
            std::path::PathBuf::from("/bin/bash"),
            None,
            None,
            crate::bash_background::HardKill::from_timeout_ms(
                raw_params.get("timeout").and_then(Value::as_u64),
            ),
            ctx,
            false,
            false,
            false,
            false,
            24,
            80,
            Vec::new(),
            None,
        )
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn v1_server_completion_never_returns_a_promoted_or_background_launch() {
        for (background, pty) in [(false, false), (true, false), (false, true)] {
            let (_dir, root) = super::super::test_support::test_root("v1-shell-completion");
            let ctx = super::super::test_support::test_ctx();
            ctx.update_config(|config| config.foreground_wait_window_ms = 10);
            let executor = Arc::new(Executor::new());
            executor.register_actor(root.clone(), ctx);
            let metrics = Arc::new(DispatchPathMetrics::new());
            let (completion_tx, mut completion_rx) = mpsc::channel(8);
            let (touch_tx, _touch_rx) = mpsc::channel(8);
            let started = Instant::now();
            submit_deferred_bash(
                &executor,
                &completion_tx,
                &touch_tx,
                &metrics,
                running_bash_stub,
                root.clone(),
                root.as_path().into(),
                "v1-session".into(),
                "v1-shell".into(),
                RouteChannel {
                    channel: 1,
                    epoch: 1,
                },
                1,
                Flags::new(false, Priority::Interactive, false),
                PROTOCOL_VERSION,
                json!({"command":"sleep 0.2", "timeout":5000,"background":background,"pty":pty,"foreground_orchestrate":true,"block_to_completion":true}),
                crate::subc_format::FormatContext::default(),
                BashWaitCancel {
                    connection: PersistentCancelSignal::new(),
                    route: PersistentCancelSignal::new(),
                    drain: drain::ModuleDrainWindow::default(),
                },
                BindTrust::FirstParty,
                crate::sandbox_spawn::AuthenticatedPrincipal::FirstParty,
                None,
                None,
                None,
                None,
                false,
                true,
                None,
                Instant::now(),
            );
            let completion = tokio::time::timeout(Duration::from_secs(5), completion_rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(
                started.elapsed() >= Duration::from_millis(100),
                "returned before command completion"
            );
            let response = completion.response_for_test();
            assert_ne!(
                response.data.get("status").and_then(Value::as_str),
                Some("running")
            );
            assert!(
                tokio::time::timeout(Duration::from_millis(100), completion_rx.recv())
                    .await
                    .is_err()
            );
        }
    }

    struct DeferredWaitReleaseGuard(Vec<std::path::PathBuf>);

    fn deadline_test_call(
        executor: &Arc<Executor>,
        root: &ProjectRootId,
        dispatch: DispatchFn,
        arguments: Value,
    ) -> mpsc::Receiver<BashDeferredCompletion> {
        deadline_test_call_received(executor, root, dispatch, arguments, Instant::now())
    }

    fn deadline_test_call_received(
        executor: &Arc<Executor>,
        root: &ProjectRootId,
        dispatch: DispatchFn,
        arguments: Value,
        received_at: Instant,
    ) -> mpsc::Receiver<BashDeferredCompletion> {
        let metrics = Arc::new(DispatchPathMetrics::new());
        let (tx, rx) = mpsc::channel(8);
        let (touch_tx, _) = mpsc::channel(8);
        submit_deferred_bash(
            executor,
            &tx,
            &touch_tx,
            &metrics,
            dispatch,
            root.clone(),
            root.as_path().into(),
            "deadline-session".into(),
            "deadline-call".into(),
            RouteChannel {
                channel: 1,
                epoch: 1,
            },
            1,
            Flags::new(false, Priority::Interactive, false),
            PROTOCOL_VERSION,
            arguments,
            crate::subc_format::FormatContext::default(),
            BashWaitCancel {
                connection: PersistentCancelSignal::new(),
                route: PersistentCancelSignal::new(),
                drain: drain::ModuleDrainWindow::default(),
            },
            BindTrust::FirstParty,
            crate::sandbox_spawn::AuthenticatedPrincipal::FirstParty,
            None,
            None,
            None,
            None,
            false,
            false,
            received_at,
        );
        rx
    }

    #[tokio::test]
    async fn bash_deadline_counts_reader_queue_time_from_receipt() {
        let (_dir, root) = super::super::test_support::test_root("bash-receipt-clock");
        let ctx = super::super::test_support::test_ctx();
        ctx.update_config(|config| config.foreground_wait_window_ms = 2000);
        let executor = Arc::new(Executor::new());
        executor.register_actor(root.clone(), ctx);
        let mut rx = deadline_test_call_received(
            &executor,
            &root,
            slow_start_stub,
            json!({"command":"sleep 3", "timeout":60000}),
            Instant::now() - Duration::from_secs(3),
        );
        let done = tokio::time::timeout(Duration::from_millis(150), rx.recv())
            .await
            .expect("time spent in the reader queue is part of startup's reply budget")
            .unwrap();
        assert_eq!(done.response_for_test().data["code"], "bash_start_deadline");
    }

    #[tokio::test]
    async fn bash_deadline_refuses_contended_admission_lock() {
        let (_dir, root) = super::super::test_support::test_root("bash-admission-lock");
        let executor = Arc::new(Executor::new());
        executor.register_actor(root.clone(), super::super::test_support::test_ctx());
        let holder_executor = executor.clone();
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            holder_executor.hold_state_lock_for_test(|| {
                held_tx.send(()).unwrap();
                std::thread::sleep(Duration::from_millis(350));
            })
        });
        held_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let started = Instant::now();
        let mut rx = deadline_test_call(
            &executor,
            &root,
            running_bash_stub,
            json!({"command":"sleep 3"}),
        );
        let done = tokio::time::timeout(Duration::from_millis(150), rx.recv()).await;
        let elapsed = started.elapsed();
        holder.join().unwrap();
        assert!(
            elapsed < Duration::from_millis(200),
            "admission must never park the frame runtime: {elapsed:?}"
        );
        assert_eq!(
            done.unwrap().unwrap().response_for_test().data["code"],
            "executor_busy"
        );
    }

    async fn deadline_queue_case(saturation: bool) {
        let (_dir, root) = super::super::test_support::test_root("bash-deadline");
        let ctx = super::super::test_support::test_ctx();
        ctx.update_config(|config| {
            config.foreground_wait_window_ms = 80;
            config.project_root = Some(root.as_path().into());
        });
        let executor = Arc::new(Executor::with_config(crate::executor::ExecutorConfig {
            pool_size: 1,
            read_cap: 1,
            actor_cap: 1,
            heavy_permits: 1,
            drr_quantum: 1,
        }));
        executor.register_actor(root.clone(), ctx);
        let mut holders = Vec::new();
        // Use the effective pool size: the executor clamps a requested one-
        // worker pool to two. Bind-only reserve workers cannot run bash jobs.
        for index in 0..if saturation { executor.pool_size() } else { 1 } {
            let holder_root = if saturation {
                let path = root.as_path().join(format!("other-{index}"));
                std::fs::create_dir(&path).unwrap();
                let other = ProjectRootId::from_path(path).unwrap();
                executor.register_actor(other.clone(), super::super::test_support::test_ctx());
                other
            } else {
                root.clone()
            };
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let holder = executor.submit_async(
                holder_root,
                Lane::Mutating,
                "long-writer".into(),
                Box::new(move |_| {
                    started_tx.send(()).unwrap();
                    let _ = release_rx.recv_timeout(Duration::from_secs(2));
                    Response::success("long-writer", json!({}))
                }),
            );
            started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            holders.push((release_tx, holder));
        }
        let mut rx = deadline_test_call(
            &executor,
            &root,
            running_bash_stub,
            json!({"command":"printf ran > late-command", "timeout":60000}),
        );
        let done = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await;
        for (release_tx, holder) in holders {
            release_tx.send(()).unwrap();
            holder.await.unwrap();
        }
        let response = done
            .expect("bash must refuse before the transport deadline")
            .unwrap();
        assert_eq!(
            response.response_for_test().data["code"],
            "bash_start_deadline"
        );
        let message = response.response_for_test().data["message"]
            .as_str()
            .unwrap();
        assert!(
            message.contains(if saturation {
                "workers saturated"
            } else {
                "root Mutating job long-writer"
            }),
            "{message}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !root.as_path().join("late-command").exists(),
            "a refused bash must never start later"
        );
    }

    #[tokio::test]
    async fn bash_deadline_refuses_behind_long_root_writer() {
        deadline_queue_case(false).await;
    }

    #[tokio::test]
    async fn bash_deadline_refuses_executor_saturation() {
        deadline_queue_case(true).await;
    }

    fn slow_start_stub(req: RawRequest, ctx: &AppContext) -> Response {
        std::thread::sleep(Duration::from_millis(300));
        running_bash_stub(req, ctx)
    }

    fn disk_full_stub(req: RawRequest, ctx: &AppContext) -> Response {
        use crate::bash_background::persistence::{with_task_io_fault, TaskIoFault};
        with_task_io_fault(TaskIoFault::LayoutEnospc, || running_bash_stub(req, ctx))
    }

    fn running_disk_full_stub(req: RawRequest, ctx: &AppContext) -> Response {
        use crate::bash_background::persistence::{with_task_io_fault, TaskIoFault};
        with_task_io_fault(TaskIoFault::RunningEnospc, || running_bash_stub(req, ctx))
    }

    fn slow_running_write_stub(req: RawRequest, ctx: &AppContext) -> Response {
        use crate::bash_background::persistence::{with_task_io_fault, TaskIoFault};
        with_task_io_fault(
            TaskIoFault::RunningDelay(Duration::from_millis(2500)),
            || running_bash_stub(req, ctx),
        )
    }

    async fn startup_case(
        dispatch: DispatchFn,
        window: u64,
    ) -> (tempfile::TempDir, Arc<AppContext>, BashDeferredCompletion) {
        let (dir, root) = super::super::test_support::test_root("bash-startup");
        let ctx = super::super::test_support::test_ctx();
        ctx.update_config(|config| {
            config.foreground_wait_window_ms = window;
            config.project_root = Some(root.as_path().into());
            config.storage_dir = Some(dir.path().join("storage"));
            config.sandbox.enabled = false;
        });
        let executor = Arc::new(Executor::new());
        executor.register_actor(root.clone(), ctx.clone());
        let mut rx = deadline_test_call(
            &executor,
            &root,
            dispatch,
            json!({"command":"printf ran > started-command; sleep 3", "timeout":60000}),
        );
        let done = tokio::time::timeout(Duration::from_millis(window + 400), rx.recv())
            .await
            .expect("bash must answer within the receipt-based reply budget")
            .unwrap();
        (dir, ctx, done)
    }

    #[tokio::test]
    async fn bash_deadline_fences_slow_startup_before_process_creation() {
        let (dir, _ctx, done) = startup_case(slow_start_stub, 80).await;
        assert_eq!(done.response_for_test().data["code"], "bash_start_deadline");
        tokio::time::sleep(Duration::from_millis(450)).await;
        assert!(
            !dir.path().join("started-command").exists(),
            "a startup refusal must fence late process creation"
        );
    }

    #[tokio::test]
    async fn bash_deadline_names_enospc_during_task_directory_creation() {
        let (dir, _ctx, done) = startup_case(disk_full_stub, 1500).await;
        let response = done.response_for_test();
        assert!(!response.success);
        assert!(
            response.data["message"]
                .as_str()
                .unwrap()
                .contains("No space left on device"),
            "{response:?}"
        );
        assert!(!dir.path().join("started-command").exists());
    }

    #[tokio::test]
    async fn bash_deadline_retains_task_when_running_metadata_hits_enospc() {
        let (_dir, ctx, done) = startup_case(running_disk_full_stub, 1000).await;
        let response = done.response_for_test();
        assert!(response.success, "{response:?}");
        let task_id = response.data["task_id"].as_str().expect("live task id");
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            ctx.bash_background().task_for_test(task_id).is_some(),
            "running process must be registered despite ENOSPC"
        );
    }

    #[tokio::test]
    async fn bash_deadline_hands_off_before_running_metadata_write_settles() {
        let (_dir, ctx, done) = startup_case(slow_running_write_stub, 1000).await;
        let response = done.response_for_test();
        assert!(response.success, "{response:?}");
        let task_id = response.data["task_id"]
            .as_str()
            .expect("committed task id");
        tokio::time::sleep(Duration::from_millis(3000)).await;
        assert!(
            ctx.bash_background().task_for_test(task_id).is_some(),
            "receipt must name the actual late-registered task"
        );
    }

    #[tokio::test]
    async fn bash_deadline_promotion_bypasses_root_writer_and_task_lock() {
        let (dir, root) = super::super::test_support::test_root("bash-poll-deadline");
        let ctx = super::super::test_support::test_ctx();
        ctx.update_config(|config| {
            config.foreground_wait_window_ms = 2000;
            config.project_root = Some(root.as_path().into());
            config.storage_dir = Some(dir.path().join("storage"));
            config.sandbox.enabled = false;
        });
        let executor = Arc::new(Executor::new());
        executor.register_actor(root.clone(), ctx.clone());
        let mut rx = deadline_test_call(
            &executor,
            &root,
            running_bash_stub,
            json!({"command":"sleep 4", "timeout":60000}),
        );
        let started_by = Instant::now() + Duration::from_millis(1500);
        let task = loop {
            if let Some(snapshot) = ctx.bash_background().list(0).first() {
                break ctx
                    .bash_background()
                    .task_for_test(&snapshot.info.task_id)
                    .unwrap();
            }
            assert!(
                Instant::now() < started_by,
                "task startup must fit the test budget"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder = executor.submit_async(
            root,
            Lane::Mutating,
            "writer-after-spawn".into(),
            Box::new(move |_| {
                let _state = task.state.lock().unwrap();
                held_tx.send(()).unwrap();
                let _ = release_rx.recv_timeout(Duration::from_secs(5));
                Response::success("writer-after-spawn", json!({}))
            }),
        );
        held_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let done = tokio::time::timeout(Duration::from_millis(2300), rx.recv()).await;
        release_tx.send(()).unwrap();
        holder.await.unwrap();
        let done = done
            .expect("promotion must bypass both executor and registry locks")
            .unwrap();
        assert!(done.response_for_test().success);
        assert!(done.response_for_test().data["task_id"].as_str().is_some());
        assert!(
            !matches!(
                tokio::time::timeout(Duration::from_millis(100), rx.recv()).await,
                Ok(Some(_))
            ),
            "only one terminal reply"
        );
    }

    impl Drop for DeferredWaitReleaseGuard {
        fn drop(&mut self) {
            for path in &self.0 {
                let _ = std::fs::write(path, b"release");
            }
        }
    }

    #[tokio::test]
    async fn cap_many_deferred_bash_waits_leave_maintenance_admission_available() {
        let executor = Arc::new(Executor::with_config(crate::executor::ExecutorConfig {
            pool_size: 6,
            read_cap: 1,
            actor_cap: 1,
            heavy_permits: 1,
            drr_quantum: 1,
        }));
        let metrics = Arc::new(DispatchPathMetrics::new());
        let (completion_tx, mut completion_rx) = mpsc::channel(16);
        let (poll_touch_tx, mut poll_touch_rx) = mpsc::channel(16);
        let mut roots = Vec::new();

        for index in 0..5 {
            let (dir, root) = super::super::test_support::test_root(&format!("bash-wait-{index}"));
            executor.register_actor(root.clone(), super::super::test_support::test_ctx());
            roots.push((dir, root));
        }

        let releases = DeferredWaitReleaseGuard(
            roots
                .iter()
                .take(4)
                .map(|(_, root)| root.as_path().join("release"))
                .collect(),
        );
        for (index, (_, root)) in roots.iter().take(4).enumerate() {
            // Each command polls for its release file. Windows runs bash commands
            // through PowerShell, so the loop is written in its syntax there.
            #[cfg(windows)]
            let command = format!(
                "while (-not (Test-Path -LiteralPath '{}')) {{ Start-Sleep -Milliseconds 10 }}",
                releases.0[index].display()
            );
            #[cfg(not(windows))]
            let command = format!(
                "while [ ! -f '{}' ]; do sleep 0.01; done",
                releases.0[index].display()
            );
            let connection = PersistentCancelSignal::new();
            let route = PersistentCancelSignal::new();
            submit_deferred_bash(
                &executor,
                &completion_tx,
                &poll_touch_tx,
                &metrics,
                running_bash_stub,
                root.clone(),
                root.as_path().to_path_buf(),
                format!("session-{index}"),
                format!("wait-{index}"),
                RouteChannel {
                    channel: index as u16 + 1,
                    epoch: 1,
                },
                index as u64 + 1,
                Flags::new(false, Priority::Passive, false),
                PROTOCOL_VERSION,
                json!({
                    "command": command,
                    "wait": true,
                    "timeout": 60_000,
                }),
                crate::subc_format::FormatContext::default(),
                BashWaitCancel {
                    connection,
                    route,
                    drain: drain::ModuleDrainWindow::default(),
                },
                BindTrust::FirstParty,
                crate::sandbox_spawn::AuthenticatedPrincipal::FirstParty,
                None,
                None,
                None,
                None,
                false,
                false,
                None,
                Instant::now(),
            );
        }

        let waits_started_by = Instant::now() + Duration::from_secs(60);
        while metrics
            .deferred_bash_waits_in_flight
            .load(Ordering::Relaxed)
            < 4
        {
            assert!(
                Instant::now() < waits_started_by,
                "all deferred bash waits should park off executor workers"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // Every caller keeps its tool call open until the command actually exits.
        assert!(completion_rx.try_recv().is_err());
        assert_eq!(
            executor
                .try_dispatch_liveness_snapshot()
                .expect("dispatch liveness")
                .running
                .maintenance,
            0,
            "parked waits must not hold maintenance workers"
        );

        let admitted = executor.submit_maintenance_async(
            roots[4].1.clone(),
            Lane::MaintenanceCommit,
            "fresh-configure-tail".to_string(),
            Box::new(|_| {
                std::thread::sleep(Duration::from_millis(150));
                Response::success("fresh-configure-tail", json!({ "drained": true }))
            }),
        );
        let response = tokio::time::timeout(Duration::from_secs(60), admitted)
            .await
            .expect("maintenance admission must not wait for deferred bash")
            .expect("executor maintenance response");
        assert!(response.success);
        let maintenance_released_by = Instant::now() + Duration::from_secs(60);
        loop {
            let running = executor
                .try_dispatch_liveness_snapshot()
                .expect("dispatch liveness")
                .running
                .maintenance;
            if running == 0 {
                break;
            }
            assert!(
                Instant::now() < maintenance_released_by,
                "configure tail must release its maintenance slot promptly"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }

        // Maintenance must complete while the commands are still held, not
        // merely finish faster than a wall-clock threshold on an idle machine.
        assert!(completion_rx.try_recv().is_err());
        assert_eq!(
            metrics
                .deferred_bash_waits_in_flight
                .load(Ordering::Relaxed),
            4
        );
        drop(releases);
        // Require each long command's ordinary terminal response and exact formatted text.
        for _ in 0..4 {
            let completion = tokio::time::timeout(Duration::from_secs(60), completion_rx.recv())
                .await
                .expect("deferred completion deadline")
                .expect("deferred completion");
            let result = completion.result.expect("terminal bash result");
            assert!(result.response.success);
            assert_eq!(result.text, "");
            tokio::time::timeout(Duration::from_secs(60), poll_touch_rx.recv())
                .await
                .expect("poll touch deadline")
                .expect("poll touch");
        }
        assert_eq!(
            metrics
                .deferred_bash_waits_in_flight
                .load(Ordering::Relaxed),
            0
        );
    }

    /// A deferred bash wait runs on the frame loop's single-threaded runtime.
    /// While another thread holds its task's state mutex (the bash watchdog
    /// does, while it persists the task to disk and aft.db), the wait's
    /// periodic task check must not park that thread: every other route's
    /// frames are read and written by it. The timer ticks below stand in for
    /// those routes, because they only run when the runtime thread is free.
    #[tokio::test]
    async fn deferred_bash_wait_does_not_block_the_frame_loop_on_task_state_lock() {
        let executor = Arc::new(Executor::with_config(crate::executor::ExecutorConfig {
            pool_size: 2,
            read_cap: 1,
            actor_cap: 1,
            heavy_permits: 1,
            drr_quantum: 1,
        }));
        let metrics = Arc::new(DispatchPathMetrics::new());
        let (completion_tx, mut completion_rx) = mpsc::channel(4);
        let (poll_touch_tx, _poll_touch_rx) = mpsc::channel(16);
        let (_dir, root) = super::super::test_support::test_root("bash-wait-lock");
        executor.register_actor(root.clone(), super::super::test_support::test_ctx());

        submit_deferred_bash(
            &executor,
            &completion_tx,
            &poll_touch_tx,
            &metrics,
            running_bash_stub,
            root.clone(),
            root.as_path().to_path_buf(),
            "session-lock".to_string(),
            "wait-lock".to_string(),
            RouteChannel {
                channel: 1,
                epoch: 1,
            },
            1,
            Flags::new(false, Priority::Passive, false),
            PROTOCOL_VERSION,
            json!({
                "command": "sleep 2",
                "wait": true,
                "timeout": 10_000,
            }),
            crate::subc_format::FormatContext::default(),
            BashWaitCancel {
                connection: PersistentCancelSignal::new(),
                route: PersistentCancelSignal::new(),
                drain: drain::ModuleDrainWindow::default(),
            },
            BindTrust::FirstParty,
            crate::sandbox_spawn::AuthenticatedPrincipal::FirstParty,
            None,
            None,
            None,
            None,
            false,
            false,
            None,
            Instant::now(),
        );

        let started_by = Instant::now() + Duration::from_secs(3);
        while metrics
            .deferred_bash_waits_in_flight
            .load(Ordering::Relaxed)
            < 1
        {
            assert!(
                Instant::now() < started_by,
                "deferred bash wait never started"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let registry = executor
            .actor_context(&root)
            .expect("actor context")
            .bash_background()
            .clone();
        let task_id = registry
            .list(0)
            .into_iter()
            .next()
            .expect("running bash task")
            .info
            .task_id;
        let task = registry.task_for_test(&task_id).expect("registered task");

        let hold = Duration::from_millis(1_200);
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _state = task.state.lock().expect("task state");
            held_tx.send(()).expect("held signal");
            std::thread::sleep(hold);
        });
        held_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("holder took the task state lock");

        // Tick the runtime through several of the wait's poll intervals while
        // the lock is held. An inline snapshot parks the thread for the whole
        // hold, which shows up as one tick taking ~1.2 s.
        let ticking_until = Instant::now() + Duration::from_millis(800);
        let mut longest_tick = Duration::ZERO;
        while Instant::now() < ticking_until {
            let tick = Instant::now();
            tokio::time::sleep(Duration::from_millis(10)).await;
            longest_tick = longest_tick.max(tick.elapsed());
        }
        assert!(
            longest_tick < Duration::from_millis(300),
            "the frame loop stalled for {longest_tick:?} behind a held task state lock"
        );
        holder.join().expect("holder thread");

        // The call still answers normally once the command exits.
        let completion = tokio::time::timeout(Duration::from_secs(8), completion_rx.recv())
            .await
            .expect("deferred completion deadline")
            .expect("deferred completion");
        assert!(
            completion
                .result
                .expect("terminal bash result")
                .response
                .success
        );
    }
}
