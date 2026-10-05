use std::path::Path;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::bash_background::registry::{BgTaskSnapshot, HardKillDeadline, HardKillSource};
use crate::bash_background::BgTaskStatus;
use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};
use crate::response_finalize::{DispatchOutcome, PendingResponse, PendingResponsePoll};

const TEST_FOREGROUND_WAIT_ENV: &str = "AFT_TEST_FOREGROUND_WAIT_MS";
/// Test-only override for `bash.worker_wait_max_ms`, read once per call.
/// Config refuses values under a minute, which integration tests cannot wait
/// out; they start the engine process with this set instead. Never set
/// outside tests.
const TEST_WORKER_WAIT_ENV: &str = "AFT_TEST_WORKER_WAIT_MAX_MS";
const DEFAULT_FOREGROUND_WAIT_TIMEOUT_MS: u64 = 30 * 60 * 1000;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct BashOrchestrateParams {
    foreground_orchestrate: bool,
    block_to_completion: bool,
    wait: bool,
    background: bool,
    pty: bool,
    timeout: Option<u64>,
}

/// Port of `packages/aft-bridge/src/bash-format.ts` `formatForegroundResult` (lines 8-25).
pub fn format_foreground_result(snapshot: &BgTaskSnapshot) -> String {
    let mut rendered = snapshot.output_preview.clone();
    if snapshot.output_truncated {
        if let Some(output_path) = snapshot.output_path.as_deref() {
            rendered.push_str(&format!(
                "\n[output truncated; full output at {output_path}]"
            ));
        }
    }
    if snapshot.info.status == BgTaskStatus::TimedOut {
        rendered.push_str("\n[command timed out]");
        // Name the limit that killed it, so AFT's own default limit is not
        // mistaken for the command failing.
        if let Some(reason) = snapshot.info.status_reason.as_deref() {
            rendered.push(' ');
            rendered.push_str(reason);
        }
    }
    if let Some(exit) = snapshot.exit_code.filter(|exit| *exit != 0) {
        rendered.push_str(&format!("\n[exit code: {exit}]"));
    }
    rendered
}

/// Port of `packages/aft-bridge/src/bash-format.ts` `formatSeconds` (lines 3-6).
pub fn format_seconds(ms: u64) -> String {
    let mut seconds = format!("{:.1}", ms as f64 / 1000.0);
    if seconds.ends_with(".0") {
        seconds.truncate(seconds.len() - 2);
    }
    format!("{seconds}s")
}

/// What happens to a task once it runs in the background, worded for the
/// caller's role. A primary session is woken by a completion reminder when the
/// task finishes. A delegated worker (`worker_session`) never is: once its
/// turn ends it has delivered its result, so it is told to wait for the task
/// instead of being promised a reminder that would never reach it.
fn format_background_handoff_tail(task_id: &str, worker_session: bool) -> String {
    if worker_session {
        return format!(
            "{task_id}. It won't wake you when it finishes, so wait for it before you report a result; use bash_status({{ taskId: \"{task_id}\" }}) to inspect output or bash_kill({{ taskId: \"{task_id}\" }}) to terminate."
        );
    }
    format!(
        "{task_id}. A completion reminder will be delivered automatically; use bash_status({{ taskId: \"{task_id}\" }}) to inspect output or bash_kill({{ taskId: \"{task_id}\" }}) to terminate."
    )
}

/// A wait limit in words: whole minutes as minutes, anything else in seconds.
pub(crate) fn format_wait_limit(ms: u64) -> String {
    match ms {
        60_000 => "1 minute".to_string(),
        ms if ms > 0 && ms % 60_000 == 0 => format!("{} minutes", ms / 60_000),
        ms => format_seconds(ms),
    }
}

/// The sentence naming a task's own kill deadline, for every reply that hands
/// a task back. It is kept apart from any wait on the task: a worker whose
/// watch "had no limit" took the task's unseen 30-minute default kill for its
/// command failing and re-ran it twice. A worker is also told that its waits
/// move the default kill, so a task it keeps watching is not killed.
pub(crate) fn kill_deadline_sentence(
    deadline: Option<HardKillDeadline>,
    worker_session: bool,
) -> String {
    let Some(deadline) = deadline else {
        return "This task has no kill deadline.".to_string();
    };
    let limit = format_wait_limit(deadline.limit_ms);
    match deadline.source {
        HardKillSource::Timeout => {
            format!("AFT kills this task once it has run {limit} (the `timeout` you passed).")
        }
        HardKillSource::Default if worker_session => format!(
            "AFT kills this task once it has run {limit} (its default background limit), but each wait you make on it moves that kill to at least the worker wait limit (`bash.worker_wait_max_ms`) after the wait, so it is not killed while you keep waiting; pass a `timeout` to set your own limit."
        ),
        HardKillSource::Default => format!(
            "AFT kills this task once it has run {limit} (its default background limit) unless you pass a longer `timeout`."
        ),
    }
}

/// [`kill_deadline_sentence`] for a task in `registry`, on its own line.
pub(crate) fn kill_deadline_note(
    registry: &crate::bash_background::BgTaskRegistry,
    task_id: &str,
    session_id: &str,
    worker_session: bool,
) -> String {
    format!(
        "\n{}",
        kill_deadline_sentence(
            registry.hard_kill_deadline(task_id, session_id),
            worker_session
        )
    )
}

/// Reply to a delegated worker whose blocking call (`wait: true`, or
/// `block_to_completion`) reached the worker wait limit
/// (`bash.worker_wait_max_ms`). The command was moved to the background,
/// not killed: a worker must get control back so it can notice a stuck
/// command, but a long build it still wants must keep running. AFT cannot
/// assume the host has a `bash_watch` tool, so this says "wait again"; the
/// plugins that have one append how to call it.
///
/// `ran_ms` is how long the command has run and `tail` its most recent
/// output, so the worker can judge whether it is stuck.
pub fn format_worker_wait_limit_message(
    task_id: &str,
    limit_ms: u64,
    ran_ms: u64,
    tail: &str,
) -> String {
    let limit = format_wait_limit(limit_ms);
    let ran = format_seconds(ran_ms);
    let tail = tail.trim_end();
    let output = if tail.is_empty() {
        "No output yet.".to_string()
    } else {
        format!("Recent output:\n{tail}")
    };
    format!(
        "The command is still running after {limit}, the worker wait limit (bash.worker_wait_max_ms), so it now runs in the background as {task_id}; it was not killed. It has run for {ran}. It won't wake you when it finishes. Wait for it again to keep waiting (each wait lasts up to {limit}, and while you keep waiting it is not killed for running long), inspect it with bash_status({{ taskId: \"{task_id}\" }}), or stop it with bash_kill({{ taskId: \"{task_id}\" }}) if it should have finished by now. Don't report a result until it finishes.\n{output}"
    )
}

/// Port of OpenCode `packages/opencode-plugin/src/tools/bash.ts` `formatPromotionMessage` (lines 603-614).
pub fn format_promotion_message(
    task_id: &str,
    timeout: Option<u64>,
    wait_window_ms: u64,
    worker_session: bool,
) -> String {
    let waited = timeout
        .map(|timeout| timeout.min(wait_window_ms))
        .unwrap_or(wait_window_ms);
    format!(
        "Foreground bash didn't finish within {} and was promoted to background: {}",
        format_seconds(waited),
        format_background_handoff_tail(task_id, worker_session)
    )
}

pub fn format_wait_detach_message(task_id: &str, worker_session: bool) -> String {
    format!(
        "Foreground bash is running in background as {}\nDetached because a user message arrived.",
        format_background_handoff_tail(task_id, worker_session)
    )
}

pub fn format_module_drain_detach_message(task_id: &str, worker_session: bool) -> String {
    format!(
        "Foreground bash is running in background as {}\nDetached because AFT is restarting; the command keeps running.",
        format_background_handoff_tail(task_id, worker_session)
    )
}

/// Port of OpenCode `packages/opencode-plugin/src/tools/bash.ts` `formatBackgroundLaunch` (lines 593-601).
/// Worded per role like [`format_background_handoff_tail`].
pub fn format_background_launch(task_id: &str, pty: bool, worker_session: bool) -> String {
    if pty {
        let tail = if worker_session {
            "It won't wake you when it exits."
        } else {
            "A completion reminder fires automatically when the task exits."
        };
        return format!(
            "PTY task started: {task_id}. Use bash_status({{ taskId: \"{task_id}\", outputMode: \"screen\" }}) to see the visible terminal, bash_write({{ taskId: \"{task_id}\", input: ... }}) to send keystrokes. {tail}"
        );
    }
    if worker_session {
        return format!(
            "Background task started: {task_id}. It won't wake you when it finishes, so wait for it before you report a result."
        );
    }
    format!(
        "Background task started: {task_id}. A completion reminder will be delivered automatically; don't poll bash_status."
    )
}

pub fn foreground_orchestrate_enabled(req: &RawRequest) -> bool {
    parse_params(req)
        .map(|params| params.foreground_orchestrate)
        .unwrap_or(false)
}

/// The repeat breaker's capture of a `bash` or `powershell` request that a
/// plugin sent directly over the standalone protocol, as the Pi plugin and the
/// OpenCode plugin in standalone mode do for every model bash call.
///
/// Those plugins show the model the response's `output` field, so this is
/// where the breaker's reminder goes. Only the standalone request loop may
/// build one: the shared tool-call runner and subc reach the same bash handler
/// internally and already observe the call, so observing there too would count
/// every call twice.
pub struct RawBashRepeat(Option<crate::run_tool_call::RepeatObservation>);

/// Captures a top-level `bash`/`powershell` request before it runs. The plugin
/// nests the tool arguments under `params` because `command` would otherwise
/// collide with the request's own `command` field.
pub fn raw_bash_repeat(req: &RawRequest) -> RawBashRepeat {
    let arguments = req.params.get("params").unwrap_or(&req.params);
    RawBashRepeat(crate::run_tool_call::RepeatObservation::for_agent_call(
        req.session(),
        &req.command,
        arguments,
        false,
        req.worker_session(),
    ))
}

impl RawBashRepeat {
    /// Observes the call once, on whichever response answers it: at once, or
    /// from the deferred foreground wait when the command finishes or is
    /// promoted to the background. A deferred call that never answers (the
    /// connection closes or AFT shuts down) is not observed.
    pub fn observe_outcome(self, ctx: &AppContext, outcome: DispatchOutcome) -> DispatchOutcome {
        let Some(repeat) = self.0 else {
            return outcome;
        };
        match outcome {
            DispatchOutcome::Immediate(mut response) => {
                observe_raw_bash_response(repeat, ctx, &mut response);
                DispatchOutcome::Immediate(response)
            }
            DispatchOutcome::Deferred(mut pending) => {
                let mut inner = pending.poll;
                let mut repeat = Some(repeat);
                pending.poll = Box::new(move |ctx| {
                    let mut response = inner(ctx)?;
                    if let Some(repeat) = repeat.take() {
                        observe_raw_bash_response(repeat, ctx, &mut response);
                    }
                    Some(response)
                });
                DispatchOutcome::Deferred(pending)
            }
        }
    }
}

/// Hashes and extends the text the plugins render: `output` on a result, or
/// `message` on an error, which the plugins raise as the tool's error text.
///
/// A permission ask is not an answer. The plugin asks the user and resends the
/// same request with the grant, still for one model call, so only the resent
/// request's answer is observed.
fn observe_raw_bash_response(
    repeat: crate::run_tool_call::RepeatObservation,
    ctx: &AppContext,
    response: &mut Response,
) {
    let Some(data) = response.data.as_object_mut() else {
        return;
    };
    let field = if response.success {
        "output"
    } else if data.get("code").and_then(Value::as_str)
        == Some(crate::protocol::ERROR_PERMISSION_REQUIRED)
    {
        return;
    } else {
        "message"
    };
    let Some(mut text) = data.get(field).and_then(Value::as_str).map(str::to_string) else {
        return;
    };
    repeat.observe(ctx, &mut text);
    data.insert(field.to_string(), Value::String(text));
}

pub fn build_bash_outcome(
    req: &RawRequest,
    ctx: &AppContext,
    spawn_response: Response,
) -> DispatchOutcome {
    if !spawn_response.success {
        return DispatchOutcome::Immediate(spawn_response);
    }

    let params = parse_params(req).unwrap_or_default();
    let worker_session = req.worker_session();
    let Some(task_id) = spawn_response
        .data
        .get("task_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return DispatchOutcome::Immediate(spawn_response);
    };
    if spawn_response.data.get("status").and_then(Value::as_str) != Some("running") {
        return DispatchOutcome::Immediate(spawn_response);
    }

    let mode = spawn_response
        .data
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("pipes");
    let is_pty = mode == "pty" || params.pty;
    if is_pty || params.background {
        return DispatchOutcome::Immediate(background_launch_response(
            &req.id,
            &task_id,
            is_pty,
            worker_session,
            &kill_deadline_note(
                ctx.bash_background(),
                &task_id,
                req.session(),
                worker_session,
            ),
        ));
    }

    let request_id = req.id.clone();
    let session_id = req.session().to_string();
    let attach_command = "bash".to_string();
    let detach_on_user_message = params.wait;
    ctx.bash_background()
        .register_foreground_task(&session_id, &task_id);
    if detach_on_user_message {
        ctx.bash_background()
            .begin_wait_mode_session(&session_id, &task_id);
    }
    let worker_cap_ms = worker_wait_cap_ms(
        worker_session,
        params.block_to_completion || params.wait,
        worker_wait_max_ms(ctx),
    );
    let wait_window_ms = worker_cap_ms.unwrap_or_else(|| {
        select_foreground_wait_window_ms(
            ctx.config().foreground_wait_window_ms,
            params.timeout,
            params.wait,
        )
    });
    let deadline = Instant::now() + Duration::from_millis(wait_window_ms);
    // A capped worker wait detaches at its deadline instead of blocking on.
    let block_to_completion =
        (params.block_to_completion || params.wait) && worker_cap_ms.is_none();
    let timeout = params.timeout;
    let storage_dir = crate::bash_background::task_storage_dir(ctx);
    let project_root = ctx.config().project_root.clone();
    let task_id_for_poll = task_id.clone();
    let request_id_for_poll = request_id.clone();
    let session_id_for_poll = session_id.clone();
    let session_id_for_cleanup = session_id.clone();

    let mut poll: PendingResponsePoll = Box::new(move |ctx| {
        // A worker blocked on its command is waiting on it: keep its default
        // hard kill at least one wait limit away, so the kill cannot fire
        // before the cap hands control back.
        if let Some(cap_ms) = worker_cap_ms {
            ctx.bash_background().renew_hard_kill(
                &task_id_for_poll,
                &session_id_for_poll,
                Duration::from_millis(cap_ms),
            );
        }
        // Foreground polls only need task state. Terminal snapshots still
        // return cached output.
        let response = if let Some(snapshot) = poll_bash_status(
            ctx,
            &task_id_for_poll,
            &session_id_for_poll,
            project_root.as_deref(),
            &storage_dir,
            0,
        ) {
            if snapshot.info.status.is_terminal() {
                Some(foreground_result_response(&request_id_for_poll, snapshot))
            } else if detach_on_user_message
                && ctx
                    .bash_background()
                    .take_wait_mode_detach(&session_id_for_poll)
            {
                Some(detach_wait_mode_bash(
                    ctx,
                    &task_id_for_poll,
                    &session_id_for_poll,
                    &request_id_for_poll,
                    worker_session,
                ))
            } else {
                match decide_bash_step(
                    snapshot,
                    deadline,
                    block_to_completion,
                    Instant::now(),
                    &request_id_for_poll,
                ) {
                    BashStep::Done(response) => Some(response),
                    BashStep::Promote => Some(promote_bash(
                        ctx,
                        &task_id_for_poll,
                        &session_id_for_poll,
                        timeout,
                        wait_window_ms,
                        &request_id_for_poll,
                        worker_session,
                        worker_cap_ms.is_some(),
                    )),
                    BashStep::Wait => None,
                }
            }
        } else {
            Some(task_not_found_response(
                &request_id_for_poll,
                &task_id_for_poll,
            ))
        };

        if response.is_some() {
            if detach_on_user_message {
                ctx.bash_background()
                    .end_wait_mode_session(&session_id_for_cleanup, &task_id_for_poll);
            } else {
                ctx.bash_background()
                    .unregister_foreground_task(&session_id_for_cleanup, &task_id_for_poll);
            }
        }
        response
    });

    if let Some(response) = poll(ctx) {
        return DispatchOutcome::Immediate(response);
    }

    // This state-based producer resolves inside its poll, including hand-backs
    // before the task exits. A task-completion wake alone cannot drive deadlines.
    DispatchOutcome::Deferred(PendingResponse::polling(
        request_id,
        session_id,
        attach_command,
        poll,
    ))
}

pub(crate) fn poll_bash_status(
    ctx: &AppContext,
    task_id: &str,
    session_id: &str,
    project_root: Option<&Path>,
    storage_dir: &Path,
    preview_bytes: usize,
) -> Option<BgTaskSnapshot> {
    ctx.bash_background().status(
        task_id,
        session_id,
        project_root,
        Some(storage_dir),
        preview_bytes,
    )
}

pub(crate) enum BashStep {
    Done(Response),
    Promote,
    Wait,
}

pub(crate) fn task_not_found_response(request_id: &str, task_id: &str) -> Response {
    Response::error(
        request_id,
        "task_not_found",
        crate::commands::bash_status::format_unknown_task_message(task_id),
    )
}

pub(crate) fn decide_bash_step(
    snapshot: BgTaskSnapshot,
    deadline: Instant,
    block_to_completion: bool,
    now: Instant,
    request_id: &str,
) -> BashStep {
    if snapshot.info.status.is_terminal() {
        BashStep::Done(foreground_result_response(request_id, snapshot))
    } else if !block_to_completion && now >= deadline {
        BashStep::Promote
    } else {
        BashStep::Wait
    }
}

/// Moves a foreground command to the background at the end of its wait.
/// `capped_worker_wait` marks a delegated worker's blocking call that hit the
/// worker wait limit (`wait_window_ms` is then that limit): it gets its
/// own reply, and its hard kill is pushed one more limit away so the worker
/// has time to wait again.
#[allow(clippy::too_many_arguments)]
pub(crate) fn promote_bash(
    ctx: &AppContext,
    task_id: &str,
    session_id: &str,
    timeout: Option<u64>,
    wait_window_ms: u64,
    request_id: &str,
    worker_session: bool,
    capped_worker_wait: bool,
) -> Response {
    match ctx.bash_background().promote(task_id, session_id) {
        Ok(_) if capped_worker_wait => {
            ctx.bash_background().renew_hard_kill(
                task_id,
                session_id,
                Duration::from_millis(wait_window_ms),
            );
            let snapshot = ctx.bash_background().observed_status(
                task_id,
                session_id,
                crate::bash_background::output::RUNNING_OUTPUT_PREVIEW_BYTES,
            );
            let ran_ms = snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.elapsed_ms.or(snapshot.info.duration_ms))
                .unwrap_or(wait_window_ms);
            let deadline_note =
                kill_deadline_note(ctx.bash_background(), task_id, session_id, worker_session);
            let tail = snapshot
                .as_ref()
                .map(|snapshot| snapshot.output_preview.as_str())
                .unwrap_or_default();
            Response::success(
                request_id,
                json!({
                    "output": format!(
                        "{}{deadline_note}",
                        format_worker_wait_limit_message(task_id, wait_window_ms, ran_ms, tail)
                    ),
                    "duration_ms": ran_ms,
                    "task_id": task_id,
                    "status": "running",
                }),
            )
        }
        Ok(_) => promotion_response(
            request_id,
            task_id,
            timeout,
            wait_window_ms,
            worker_session,
            &kill_deadline_note(ctx.bash_background(), task_id, session_id, worker_session),
        ),
        Err(message) if message.contains("not found") => Response::error(
            request_id,
            "task_not_found",
            crate::commands::bash_status::format_unknown_task_message(task_id),
        ),
        Err(message) => Response::error(request_id, "execution_failed", message),
    }
}

pub(crate) fn detach_wait_mode_bash(
    ctx: &AppContext,
    task_id: &str,
    session_id: &str,
    request_id: &str,
    worker_session: bool,
) -> Response {
    match ctx.bash_background().promote(task_id, session_id) {
        Ok(_) => wait_detach_response(
            request_id,
            task_id,
            worker_session,
            &kill_deadline_note(ctx.bash_background(), task_id, session_id, worker_session),
        ),
        Err(message) if message.contains("not found") => Response::error(
            request_id,
            "task_not_found",
            crate::commands::bash_status::format_unknown_task_message(task_id),
        ),
        Err(message) => Response::error(request_id, "execution_failed", message),
    }
}

/// Hands a still-running foreground command to the background because the
/// module is about to restart. Same promotion as a user-message detach, so the
/// task keeps running, is persisted, and delivers its completion later.
pub(crate) fn detach_bash_for_module_drain(
    ctx: &AppContext,
    task_id: &str,
    session_id: &str,
    request_id: &str,
    worker_session: bool,
) -> Response {
    match ctx.bash_background().promote(task_id, session_id) {
        Ok(_) => {
            let snapshot = ctx
                .bash_background()
                .observed_status(task_id, session_id, 0);
            let output_path = snapshot
                .as_ref()
                .and_then(|task| task.output_path.as_deref());
            let stderr_path = snapshot
                .as_ref()
                .and_then(|task| task.stderr_path.as_deref());
            crate::slog_info!("bash drain detach persisted: task_id={task_id} output={output_path:?} stderr={stderr_path:?}");
            Response::success(
                request_id,
                json!({
                    "output": format!(
                        "{}{}\nOutput: {}",
                        format_module_drain_detach_message(task_id, worker_session),
                        kill_deadline_note(ctx.bash_background(), task_id, session_id, worker_session),
                        output_path.unwrap_or("unavailable")
                    ),
                    "task_id": task_id,
                    "status": "running",
                    "output_path": output_path,
                    "stderr_path": stderr_path,
                }),
            )
        }
        Err(message) if message.contains("not found") => Response::error(
            request_id,
            "task_not_found",
            crate::commands::bash_status::format_unknown_task_message(task_id),
        ),
        Err(message) => Response::error(request_id, "execution_failed", message),
    }
}

fn foreground_result_response(request_id: &str, snapshot: BgTaskSnapshot) -> Response {
    let (output, foreground_envelope) =
        if let Some(envelope) = snapshot.bash_output_list_envelope.as_ref() {
            let trailer = crate::list_surfaces::bash::envelope_trailer(envelope);
            let mut foreground_snapshot = snapshot.clone();
            foreground_snapshot.output_preview = foreground_snapshot
                .output_preview
                .strip_suffix(&trailer)
                .unwrap_or(&foreground_snapshot.output_preview)
                .trim_end_matches('\n')
                .to_string();
            // The envelope replaces the legacy output-path truncation clause. Exit and
            // timeout diagnostics still render before the final canonical trailer.
            foreground_snapshot.output_truncated = false;
            let mut output = format_foreground_result(&foreground_snapshot);
            // Formatting may add exit diagnostics, which are not command output lines.
            if !output.is_empty() && !output.ends_with('\n') {
                output.push('\n');
            }
            output.push_str(&trailer);
            (output, Some(envelope.clone()))
        } else {
            (format_foreground_result(&snapshot), None)
        };
    if snapshot.sandbox_native
        && snapshot.sandbox_unavailable
        && snapshot.exit_code == Some(crate::sandbox_spawn::SANDBOX_UNAVAILABLE_EXIT_CODE)
    {
        return Response::error_with_data(
            request_id,
            "sandbox_unavailable",
            "native sandbox failed before the command could run; set sandbox.enabled=false to disable native sandboxing",
            json!({
                "task_id": snapshot.info.task_id,
                "exit_code": snapshot.exit_code,
                "output": output,
            }),
        );
    }
    let timed_out = snapshot.info.status == BgTaskStatus::TimedOut;
    let mut data = json!({
        "output": output,
        "task_id": snapshot.info.task_id,
        "status": snapshot.info.status,
        "mode": snapshot.info.mode,
        "exit_code": snapshot.exit_code,
        "output_preview": snapshot.output_preview,
        "output_truncated": snapshot.output_truncated,
        "truncated": snapshot.output_truncated,
        "output_path": snapshot.output_path,
        "timed_out": timed_out,
        "duration_ms": snapshot.info.duration_ms,
    });
    crate::list_surfaces::bash::attach_bash_output_envelope(
        data.as_object_mut()
            .expect("foreground bash data is an object"),
        &foreground_envelope,
    );
    Response::success(request_id, data)
}

fn background_launch_response(
    request_id: &str,
    task_id: &str,
    is_pty: bool,
    worker_session: bool,
    deadline_note: &str,
) -> Response {
    Response::success(
        request_id,
        json!({
            "output": format!("{}{deadline_note}", format_background_launch(task_id, is_pty, worker_session)),
            "task_id": task_id,
            "status": "running",
            "mode": if is_pty { "pty" } else { "pipes" },
        }),
    )
}

fn promotion_response(
    request_id: &str,
    task_id: &str,
    timeout: Option<u64>,
    wait_window_ms: u64,
    worker_session: bool,
    deadline_note: &str,
) -> Response {
    Response::success(
        request_id,
        json!({
            "output": format!(
                "{}{deadline_note}",
                format_promotion_message(task_id, timeout, wait_window_ms, worker_session)
            ),
            "task_id": task_id,
            "status": "running",
        }),
    )
}

fn wait_detach_response(
    request_id: &str,
    task_id: &str,
    worker_session: bool,
    deadline_note: &str,
) -> Response {
    Response::success(
        request_id,
        json!({
            "output": format!("{}{deadline_note}", format_wait_detach_message(task_id, worker_session)),
            "task_id": task_id,
            "status": "running",
        }),
    )
}

fn parse_params(req: &RawRequest) -> Option<BashOrchestrateParams> {
    let raw_params = req
        .params
        .get("params")
        .cloned()
        .unwrap_or_else(|| req.params.clone());
    serde_json::from_value::<BashOrchestrateParams>(raw_params).ok()
}

pub(crate) fn resolve_foreground_wait_window_ms(configured: u64) -> u64 {
    std::env::var(TEST_FOREGROUND_WAIT_ENV)
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(configured)
}

/// The worker wait limit in force: `bash.worker_wait_max_ms`, or the test
/// override (see `TEST_WORKER_WAIT_ENV`).
pub(crate) fn worker_wait_max_ms(ctx: &AppContext) -> u64 {
    std::env::var(TEST_WORKER_WAIT_ENV)
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or_else(|| ctx.config().bash.worker_wait_max_ms)
}

/// How long a foreground bash call may block before it detaches, when the
/// worker wait limit bounds it: any delegated worker call that would block
/// until its command finishes, i.e. `wait: true` or `block_to_completion`
/// (which the plugins send for every worker foreground call when
/// `bash.subagent_background` is false, so those are never auto-promoted).
/// `None` for every other call: a primary session's blocking call still
/// blocks until the command finishes or its hard kill fires, and a
/// non-blocking foreground call is promoted after the much shorter foreground
/// wait window anyway. A worker's explicit `timeout` shorter than the limit
/// simply ends the call first; a longer one no longer holds the worker past
/// the limit, though the command keeps that timeout as its hard kill.
pub(crate) fn worker_wait_cap_ms(
    worker_session: bool,
    blocking: bool,
    limit_ms: u64,
) -> Option<u64> {
    (worker_session && blocking).then_some(limit_ms)
}

pub(crate) fn select_foreground_wait_window_ms(
    configured: u64,
    timeout: Option<u64>,
    wait: bool,
) -> u64 {
    if wait {
        timeout.unwrap_or(DEFAULT_FOREGROUND_WAIT_TIMEOUT_MS)
    } else {
        resolve_foreground_wait_window_ms(configured)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bash_background::persistence::BgMode;
    use crate::bash_background::registry::BgTaskSnapshot;
    use crate::bash_background::BgTaskInfo;

    fn snapshot(
        output_preview: &str,
        output_truncated: bool,
        output_path: Option<&str>,
        status: BgTaskStatus,
        exit_code: Option<i32>,
    ) -> BgTaskSnapshot {
        BgTaskSnapshot {
            info: BgTaskInfo {
                task_id: "bash-test".to_string(),
                status,
                command: "echo test".to_string(),
                mode: BgMode::Pipes,
                started_at: 0,
                duration_ms: Some(1),
                status_reason: None,
            },
            exit_code,
            child_pid: None,
            workdir: "/tmp".to_string(),
            output_preview: output_preview.to_string(),
            bash_output_list_envelope: None,
            output_truncated,
            output_path: output_path.map(str::to_string),
            stderr_path: None,
            pty_rows: None,
            pty_cols: None,
            pty_screen: None,
            scanner_report: Vec::new(),
            sandbox_native: false,
            sandbox_unavailable: false,
            live_descendants: Some(Vec::new()),
            live_descendants_omitted: 0,
            live_descendants_summary: None,
            kill_signaled: false,
            kill_reached: 0,
            hard_kill: None,
            elapsed_ms: None,
        }
    }

    #[test]
    fn native_launcher_exit_78_is_a_structured_sandbox_error() {
        let mut snapshot = snapshot(
            "sandbox_unavailable: backend failed",
            false,
            None,
            BgTaskStatus::Failed,
            Some(crate::sandbox_spawn::SANDBOX_UNAVAILABLE_EXIT_CODE),
        );
        snapshot.sandbox_native = true;
        snapshot.sandbox_unavailable = true;

        let response = foreground_result_response("sandbox-failed", snapshot);
        assert!(!response.success);
        assert_eq!(
            response
                .data
                .get("code")
                .and_then(serde_json::Value::as_str),
            Some("sandbox_unavailable")
        );
        assert!(response
            .data
            .get("message")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|message| message.contains("sandbox.enabled=false")));
    }

    #[test]
    fn native_command_exit_78_is_not_misreported_as_launcher_failure() {
        let mut snapshot = snapshot(
            "command selected exit 78",
            false,
            None,
            BgTaskStatus::Failed,
            Some(crate::sandbox_spawn::SANDBOX_UNAVAILABLE_EXIT_CODE),
        );
        snapshot.sandbox_native = true;

        let response = foreground_result_response("command-exit-78", snapshot);
        assert!(response.success);
        assert_eq!(
            response
                .data
                .get("exit_code")
                .and_then(serde_json::Value::as_i64),
            Some(i64::from(
                crate::sandbox_spawn::SANDBOX_UNAVAILABLE_EXIT_CODE
            ))
        );
    }

    #[test]
    fn decide_bash_step_returns_done_for_terminal_snapshot_even_at_deadline() {
        let snapshot = snapshot("done", false, None, BgTaskStatus::Completed, Some(0));
        let now = Instant::now();

        match decide_bash_step(snapshot, now, false, now, "req-terminal") {
            BashStep::Done(response) => {
                assert_eq!(response.id, "req-terminal");
                assert!(response.success);
                assert_eq!(response.data["status"], json!("completed"));
                assert_eq!(response.data["output"], json!("done"));
            }
            BashStep::Promote => panic!("terminal snapshot should not promote"),
            BashStep::Wait => panic!("terminal snapshot should not wait"),
        }
    }

    #[test]
    fn decide_bash_step_promotes_at_deadline_when_not_blocking() {
        let snapshot = snapshot("running", false, None, BgTaskStatus::Running, None);
        let now = Instant::now();

        match decide_bash_step(snapshot, now, false, now, "req-promote") {
            BashStep::Promote => {}
            BashStep::Done(_) => panic!("running snapshot should not finish"),
            BashStep::Wait => panic!("deadline should promote when not blocking"),
        }
    }

    #[test]
    fn decide_bash_step_waits_before_deadline() {
        let snapshot = snapshot("running", false, None, BgTaskStatus::Running, None);
        let now = Instant::now();

        match decide_bash_step(
            snapshot,
            now + Duration::from_millis(1),
            false,
            now,
            "req-wait",
        ) {
            BashStep::Wait => {}
            BashStep::Done(_) => panic!("running snapshot should not finish"),
            BashStep::Promote => panic!("snapshot should wait before the deadline"),
        }
    }

    #[test]
    fn decide_bash_step_never_promotes_when_blocking_to_completion() {
        let snapshot = snapshot("running", false, None, BgTaskStatus::Running, None);
        let now = Instant::now();

        match decide_bash_step(snapshot, now, true, now, "req-block") {
            BashStep::Wait => {}
            BashStep::Done(_) => panic!("running snapshot should not finish"),
            BashStep::Promote => panic!("block_to_completion should suppress promotion"),
        }
    }

    #[test]
    fn select_foreground_wait_window_uses_timeout_budget_for_wait_true() {
        assert_eq!(
            select_foreground_wait_window_ms(8_000, Some(250), true),
            250
        );
        assert_eq!(
            select_foreground_wait_window_ms(8_000, None, true),
            DEFAULT_FOREGROUND_WAIT_TIMEOUT_MS
        );
    }

    /// Every blocking call from a delegated worker is capped by the worker
    /// wait limit; a primary's blocking call and a worker's non-blocking call
    /// (promoted after the foreground window) are not.
    #[test]
    fn worker_wait_cap_applies_only_to_a_worker_blocking_call() {
        assert_eq!(worker_wait_cap_ms(true, true, 90_000), Some(90_000));
        assert_eq!(worker_wait_cap_ms(false, true, 90_000), None);
        assert_eq!(worker_wait_cap_ms(true, false, 90_000), None);
    }

    /// The reply names the configured limit, says the command was not killed,
    /// and offers the way to stop it.
    #[test]
    fn worker_wait_limit_message_names_the_configured_limit() {
        let text =
            format_worker_wait_limit_message("bash-cap", 5_400_000, 5_401_000, "line 1\nline 2\n");
        assert!(text.contains("It has run for 5401s"), "{text}");
        assert!(text.ends_with("Recent output:\nline 1\nline 2"), "{text}");
        assert!(
            format_worker_wait_limit_message("bash-cap", 60_000, 60_000, "")
                .ends_with("No output yet.")
        );
        assert!(text.contains("still running after 90 minutes"), "{text}");
        assert!(text.contains("bash.worker_wait_max_ms"), "{text}");
        assert!(text.contains("it was not killed"), "{text}");
        assert!(text.contains("won't wake you"), "{text}");
        assert!(
            text.contains("bash_kill({ taskId: \"bash-cap\" })"),
            "{text}"
        );
        assert!(!text.contains("completion reminder"), "{text}");
        assert_eq!(format_wait_limit(1_800_000), "30 minutes");
        assert_eq!(format_wait_limit(60_000), "1 minute");
        assert_eq!(format_wait_limit(1_500), "1.5s");
    }

    #[test]
    fn foreground_result_format_matches_typescript_order() {
        let snapshot = snapshot(
            "hello",
            true,
            Some("/tmp/aft-output.txt"),
            BgTaskStatus::TimedOut,
            Some(124),
        );

        assert_eq!(
            format_foreground_result(&snapshot),
            "hello\n[output truncated; full output at /tmp/aft-output.txt]\n[command timed out]\n[exit code: 124]"
        );
    }

    #[test]
    fn format_seconds_strips_integer_decimal_and_keeps_tenths() {
        assert_eq!(format_seconds(8_000), "8s");
        assert_eq!(format_seconds(5_500), "5.5s");
        assert_eq!(format_seconds(14_999), "15s");
    }

    #[test]
    fn promotion_message_matches_opencode_copy() {
        assert_eq!(
            format_promotion_message("bash-123", Some(5_500), 8_000, false),
            "Foreground bash didn't finish within 5.5s and was promoted to background: bash-123. A completion reminder will be delivered automatically; use bash_status({ taskId: \"bash-123\" }) to inspect output or bash_kill({ taskId: \"bash-123\" }) to terminate."
        );
    }

    #[test]
    fn wait_detach_message_mentions_user_message() {
        assert_eq!(
            format_wait_detach_message("bash-123", false),
            "Foreground bash is running in background as bash-123. A completion reminder will be delivered automatically; use bash_status({ taskId: \"bash-123\" }) to inspect output or bash_kill({ taskId: \"bash-123\" }) to terminate.\nDetached because a user message arrived."
        );
    }

    #[test]
    fn background_launch_messages_match_opencode_copy() {
        assert_eq!(
            format_background_launch("bash-bg", false, false),
            "Background task started: bash-bg. A completion reminder will be delivered automatically; don't poll bash_status."
        );
        assert_eq!(
            format_background_launch("bash-pty", true, false),
            "PTY task started: bash-pty. Use bash_status({ taskId: \"bash-pty\", outputMode: \"screen\" }) to see the visible terminal, bash_write({ taskId: \"bash-pty\", input: ... }) to send keystrokes. A completion reminder fires automatically when the task exits."
        );
    }

    /// A delegated worker is never woken by a completion reminder, so no
    /// hand-off text it can receive may promise one; a primary's text is
    /// unchanged.
    #[test]
    fn hand_off_texts_are_worded_per_role() {
        let worker = [
            format_background_launch("bash-bg", false, true),
            format_background_launch("bash-pty", true, true),
            format_promotion_message("bash-123", None, 8_000, true),
            format_wait_detach_message("bash-123", true),
            format_module_drain_detach_message("bash-123", true),
            format_worker_wait_limit_message("bash-123", 1_800_000, 1_800_000, ""),
        ];
        for text in &worker {
            assert!(!text.contains("completion reminder"), "{text}");
            assert!(!text.contains("end the turn"), "{text}");
            assert!(text.contains("won't wake you"), "{text}");
        }
        let primary = [
            format_background_launch("bash-bg", false, false),
            format_background_launch("bash-pty", true, false),
            format_promotion_message("bash-123", None, 8_000, false),
            format_wait_detach_message("bash-123", false),
            format_module_drain_detach_message("bash-123", false),
        ];
        for text in &primary {
            assert!(text.contains("completion reminder"), "{text}");
        }
    }
}
