use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::bash_background::registry::remote::{RemotePhase, RemoteProgress};
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
// Leave room for polling and the kill worker to publish its terminal status.
const WORKER_KILL_HANDOFF_MARGIN: Duration = Duration::from_secs(5);

pub(crate) fn worker_kill_deadline_within_handoff_margin(remaining: Option<Duration>) -> bool {
    remaining.is_some_and(|remaining| remaining <= WORKER_KILL_HANDOFF_MARGIN)
}

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
/// caller's role and available tools. A primary session is woken by a completion
/// reminder. A delegated worker (`worker_session`) never is: it is directed to
/// `bash_watch` only when its catalog includes that tool.
fn format_background_handoff_tail(
    task_id: &str,
    worker_session: bool,
    bash_watch_available: bool,
) -> String {
    if worker_session {
        let waiting = if bash_watch_available {
            "wait for it before you report a result"
        } else {
            "check whether it has finished before you report a result"
        };
        return format!(
            "{task_id}. It won't wake you when it finishes, so {waiting}; use bash_status({{ taskId: \"{task_id}\" }}) to inspect output or bash_kill({{ taskId: \"{task_id}\" }}) to terminate."
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
    started_at_ms: u64,
    worker_session: bool,
) -> String {
    kill_deadline_sentence_at(deadline, started_at_ms, unix_millis_now(), worker_session)
}

fn kill_deadline_sentence_at(
    deadline: Option<HardKillDeadline>,
    started_at_ms: u64,
    now_ms: u64,
    worker_session: bool,
) -> String {
    let Some(deadline) = deadline else {
        return "This task has no kill deadline.".to_string();
    };
    let limit = format_wait_limit(deadline.limit_ms);
    let deadline_at = started_at_ms.saturating_add(deadline.limit_ms);
    let when = format!(
        "at {}, when it has run {limit}",
        crate::subc_format::format_unix_millis_utc(i64::try_from(deadline_at).unwrap_or(i64::MAX))
    );
    let source = match deadline.source {
        HardKillSource::Timeout => "(the `timeout` you passed)".to_string(),
        HardKillSource::Default if worker_session => "(its default background limit), but each wait you make on it moves that kill to at least the worker wait limit (`bash.worker_wait_max_ms`) after the wait, so it is not killed while you keep waiting; pass a `timeout` to set your own limit".to_string(),
        HardKillSource::Default => "(its default background limit) unless you pass a longer `timeout`".to_string(),
    };
    let remaining = if deadline_at <= now_ms {
        "; the kill deadline has passed".to_string()
    } else {
        format!(
            "; about {} remain",
            format_approximate_remaining(deadline_at - now_ms)
        )
    };
    format!("AFT kills this task {when} {source}{remaining}.")
}

fn format_approximate_remaining(ms: u64) -> String {
    if ms >= 60_000 {
        let minutes = ms.saturating_add(30_000) / 60_000;
        format!(
            "{} minute{}",
            minutes.max(1),
            if minutes == 1 { "" } else { "s" }
        )
    } else {
        format_wait_limit(ms)
    }
}

fn unix_millis_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

/// [`kill_deadline_sentence`] for a task in `registry`, on its own line.
pub(crate) fn kill_deadline_note(
    registry: &crate::bash_background::BgTaskRegistry,
    task_id: &str,
    session_id: &str,
    worker_session: bool,
) -> String {
    if let Some(note) = registry.execution_note(task_id, session_id) {
        // Remote timeouts are enforced from run start, not queue entry; AFT
        // cannot infer their remaining wall-clock time or renew the runner.
        return format!("\n{note}");
    }
    let Some((deadline, started_at_ms)) =
        registry.hard_kill_deadline_with_start(task_id, session_id)
    else {
        return "\nThis task has no kill deadline.".to_string();
    };
    format!(
        "\n{}",
        kill_deadline_sentence(Some(deadline), started_at_ms, worker_session)
    )
}

/// Reply to a caller whose blocking call (`wait: true`, or
/// `block_to_completion`) reached the configured wait limit
/// (`bash.worker_wait_max_ms`). The command was moved to the background,
/// not killed: the caller must get control back so it can notice a stuck
/// command, but a long build it still wants must keep running. This names
/// `bash_watch` only when the caller's catalog includes it; otherwise it points
/// the caller at `bash_status` to inspect the task. The host plugins append any
/// additional guidance they provide.
///
/// `ran_ms` is how long the command has run and `tail` its most recent
/// output, so the caller can judge whether it is stuck.
pub fn format_worker_wait_limit_message(
    task_id: &str,
    limit_ms: u64,
    ran_ms: u64,
    tail: &str,
    bash_watch_available: bool,
) -> String {
    let limit = format_wait_limit(limit_ms);
    let ran = format_seconds(ran_ms);
    let tail = tail.trim_end();
    let output = if tail.is_empty() {
        "No output yet.".to_string()
    } else {
        format!("Recent output:\n{tail}")
    };
    let next_step = if bash_watch_available {
        format!(
            "Wait for it again to keep waiting (each wait lasts up to {limit}, and while you keep waiting it is not killed for running long), inspect it with bash_status({{ taskId: \"{task_id}\" }})"
        )
    } else {
        format!(
            "Use bash_status({{ taskId: \"{task_id}\" }}) to check whether it has finished or inspect output"
        )
    };
    format!(
        "The command is still running after {limit}, the worker wait limit (bash.worker_wait_max_ms), so it now runs in the background as {task_id}; it was not killed. It has run for {ran}. It won't wake you when it finishes. {next_step}, or stop it with bash_kill({{ taskId: \"{task_id}\" }}) if it should have finished by now. Don't report a result until it finishes.\n{output}"
    )
}

/// Port of OpenCode `packages/opencode-plugin/src/tools/bash.ts` `formatPromotionMessage` (lines 603-614).
pub fn format_promotion_message(
    task_id: &str,
    timeout: Option<u64>,
    wait_window_ms: u64,
    worker_session: bool,
    bash_watch_available: bool,
) -> String {
    let waited = timeout
        .map(|timeout| timeout.min(wait_window_ms))
        .unwrap_or(wait_window_ms);
    format!(
        "Foreground bash didn't finish within {} and was promoted to background: {}",
        format_seconds(waited),
        format_background_handoff_tail(task_id, worker_session, bash_watch_available)
    )
}

pub fn format_wait_detach_message(
    task_id: &str,
    worker_session: bool,
    bash_watch_available: bool,
) -> String {
    format!(
        "Foreground bash is running in background as {}\nDetached because a user message arrived.",
        format_background_handoff_tail(task_id, worker_session, bash_watch_available)
    )
}

pub fn format_module_drain_detach_message(
    task_id: &str,
    worker_session: bool,
    bash_watch_available: bool,
) -> String {
    format!(
        "Foreground bash is running in background as {}\nDetached because AFT is restarting; the command keeps running.",
        format_background_handoff_tail(task_id, worker_session, bash_watch_available)
    )
}

/// Port of OpenCode `packages/opencode-plugin/src/tools/bash.ts` `formatBackgroundLaunch` (lines 593-601).
/// Worded per role like [`format_background_handoff_tail`].
pub fn format_background_launch(
    task_id: &str,
    pty: bool,
    worker_session: bool,
    bash_watch_available: bool,
) -> String {
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
        if bash_watch_available {
            return format!(
                "Background task started: {task_id}. It won't wake you when it finishes, so wait for it before you report a result."
            );
        }
        return format!(
            "Background task started: {task_id}. It won't wake you when it finishes, so use bash_status({{ taskId: \"{task_id}\" }}) to check whether it has finished before you report a result, or bash_kill({{ taskId: \"{task_id}\" }}) to stop it."
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
    // Direct OpenCode/Pi worker requests use the host plugins' own
    // `bash_watch` tool. Catalog consumers override this on their separate
    // deferred path with the tool present in their resolved preset.
    let bash_watch_available = worker_session;
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
            bash_watch_available,
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
    let worker_cap_ms = blocking_wait_cap_ms(
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
    // A blocking wait on a remote task ends here even while its job waits in
    // the runner's queue; see `remote_block_handback_ms`.
    let remote_handback = (params.block_to_completion || params.wait).then(|| {
        let waited_ms = remote_block_handback_ms(params.timeout, worker_cap_ms);
        (Instant::now() + Duration::from_millis(waited_ms), waited_ms)
    });
    // A capped blocking wait detaches at its deadline instead of blocking on.
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
        // A caller blocked on its command is waiting on it: keep its default
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
                    bash_watch_available,
                ))
            } else if let Some(waited_ms) = remote_handback
                .filter(|(at, _)| Instant::now() >= *at)
                .map(|(_, waited_ms)| waited_ms)
                .filter(|_| {
                    ctx.bash_background()
                        .is_remote_task(&task_id_for_poll, &session_id_for_poll)
                })
            {
                Some(handback_remote_bash(
                    ctx,
                    &task_id_for_poll,
                    &session_id_for_poll,
                    &request_id_for_poll,
                    waited_ms,
                    worker_session,
                    bash_watch_available,
                ))
            } else {
                match decide_bash_step(
                    snapshot,
                    deadline,
                    block_to_completion,
                    worker_cap_ms.is_some()
                        && worker_kill_deadline_within_handoff_margin(
                            ctx.bash_background()
                                .hard_kill_remaining(&task_id_for_poll, &session_id_for_poll),
                        ),
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
                        bash_watch_available,
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
    wait_for_kill_terminal: bool,
    now: Instant,
    request_id: &str,
) -> BashStep {
    if snapshot.info.status.is_terminal() {
        BashStep::Done(foreground_result_response(request_id, snapshot))
    } else if !block_to_completion && !wait_for_kill_terminal && now >= deadline {
        BashStep::Promote
    } else {
        BashStep::Wait
    }
}

/// Moves a foreground command to the background at the end of its wait.
/// `capped_worker_wait` marks a blocking call that hit the configured
/// wait limit (`wait_window_ms` is then that limit): it gets its
/// own reply, and its hard kill is pushed one more limit away so the caller
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
    bash_watch_available: bool,
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
                        format_worker_wait_limit_message(
                            task_id,
                            wait_window_ms,
                            ran_ms,
                            tail,
                            bash_watch_available,
                        )
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
            bash_watch_available,
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
    bash_watch_available: bool,
) -> Response {
    match ctx.bash_background().promote(task_id, session_id) {
        Ok(_) => wait_detach_response(
            request_id,
            task_id,
            worker_session,
            bash_watch_available,
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

/// How long a blocking call (`wait: true` or `block_to_completion`) on a
/// remote task waits before it hands the task back.
///
/// A local command's `timeout` starts when it spawns, so its hard kill ends a
/// blocking wait in time. A remote command's `timeout` starts only when the
/// runner starts running it, and AFT keeps no kill clock of its own for it.
/// While the job waits in the runner's queue nothing would end the wait, and
/// the plugin would give up on the call with no reply at all.
///
/// The OpenCode and Pi plugins size their transport deadline
/// (`orchestratedTransportTimeoutMs`) as this same budget plus a margin: the
/// `timeout`, or 30 minutes without one, capped by the worker wait limit for
/// a delegated worker. This never exceeds the 30-minute wait cap either.
pub(crate) fn remote_block_handback_ms(timeout: Option<u64>, worker_cap_ms: Option<u64>) -> u64 {
    let budget = timeout
        .unwrap_or(DEFAULT_FOREGROUND_WAIT_TIMEOUT_MS)
        .min(DEFAULT_FOREGROUND_WAIT_TIMEOUT_MS);
    worker_cap_ms.map_or(budget, |cap| budget.min(cap))
}

/// The reply to a blocking call whose remote command did not finish within
/// [`remote_block_handback_ms`]: the task id, the runner's job id and where
/// the job is. The job keeps going; only the call ends.
pub(crate) fn format_remote_handback_message(
    task_id: &str,
    waited_ms: u64,
    progress: Option<&RemoteProgress>,
    worker_session: bool,
    bash_watch_available: bool,
) -> String {
    let job = progress
        .and_then(|progress| progress.job_id)
        .map_or_else(|| "not assigned yet".to_string(), |id| id.to_string());
    let phase = progress.map_or(RemotePhase::WaitingOnRunner, |progress| progress.phase);
    format!(
        "The remote command did not finish within {}, the longest this call waits, so it now runs in the background as {}\nIt was not killed. Remote job: {job}. Phase: {}. Its `timeout` counts from when it starts running on ck-motor; a job that waits too long in ck-motor's queue is refused without running.",
        format_wait_limit(waited_ms),
        format_background_handoff_tail(task_id, worker_session, bash_watch_available),
        phase.describe(),
    )
}

/// Moves a blocking call's remote task to the background at the end of its
/// wait (see [`remote_block_handback_ms`]) and replies with its job and phase.
pub(crate) fn handback_remote_bash(
    ctx: &AppContext,
    task_id: &str,
    session_id: &str,
    request_id: &str,
    waited_ms: u64,
    worker_session: bool,
    bash_watch_available: bool,
) -> Response {
    match ctx.bash_background().promote(task_id, session_id) {
        Ok(_) => {
            let progress = ctx.bash_background().remote_progress(task_id, session_id);
            let output = format!(
                "{}{}",
                format_remote_handback_message(
                    task_id,
                    waited_ms,
                    progress.as_ref(),
                    worker_session,
                    bash_watch_available,
                ),
                kill_deadline_note(ctx.bash_background(), task_id, session_id, worker_session),
            );
            let phase = progress
                .as_ref()
                .map_or(RemotePhase::WaitingOnRunner, |progress| progress.phase);
            let queue_position = match phase {
                RemotePhase::Queued { position } => Some(position),
                _ => None,
            };
            Response::success(
                request_id,
                json!({
                    "output": output,
                    "task_id": task_id,
                    "status": "running",
                    "remote_job_id": progress
                        .as_ref()
                        .and_then(|progress| progress.job_id)
                        .map(|id| id.to_string()),
                    "remote_phase": phase.tag(),
                    "queue_position": queue_position,
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

/// Hands a still-running foreground command to the background because the
/// module is about to restart. Same promotion as a user-message detach, so the
/// task keeps running, is persisted, and delivers its completion later.
pub(crate) fn detach_bash_for_module_drain(
    ctx: &AppContext,
    task_id: &str,
    session_id: &str,
    request_id: &str,
    worker_session: bool,
    bash_watch_available: bool,
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
                        format_module_drain_detach_message(
                            task_id,
                            worker_session,
                            bash_watch_available,
                        ),
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
    if let Some(refusal) = &snapshot.remote_refusal {
        return Response::error_with_data(
            request_id,
            refusal.code,
            &refusal.message,
            json!({
                "task_id": snapshot.info.task_id,
                "status": snapshot.info.status,
                "exit_code": snapshot.exit_code,
                "output": output,
            }),
        );
    }
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
    bash_watch_available: bool,
    deadline_note: &str,
) -> Response {
    Response::success(
        request_id,
        json!({
            "output": format!("{}{deadline_note}", format_background_launch(task_id, is_pty, worker_session, bash_watch_available)),
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
    bash_watch_available: bool,
    deadline_note: &str,
) -> Response {
    Response::success(
        request_id,
        json!({
            "output": format!(
                "{}{deadline_note}",
                format_promotion_message(
                    task_id,
                    timeout,
                    wait_window_ms,
                    worker_session,
                    bash_watch_available,
                )
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
    bash_watch_available: bool,
    deadline_note: &str,
) -> Response {
    Response::success(
        request_id,
        json!({
            "output": format!("{}{deadline_note}", format_wait_detach_message(task_id, worker_session, bash_watch_available)),
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

/// Every session's blocking bash call (`wait: true` or `block_to_completion`)
/// is bounded by `bash.worker_wait_max_ms`. Ordinary foreground calls use the
/// shorter foreground window. The cap returns control without changing an
/// explicit command timeout, which still governs the process's lifetime.
pub(crate) fn blocking_wait_cap_ms(blocking: bool, limit_ms: u64) -> Option<u64> {
    blocking.then_some(limit_ms)
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
            output_incomplete: false,
            output_path: output_path.map(str::to_string),
            stderr_path: None,
            pty_rows: None,
            pty_cols: None,
            pty_screen: None,
            scanner_report: Vec::new(),
            sandbox_native: false,
            sandbox_unavailable: false,
            remote_refusal: None,
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
    fn runon_foreground_refusal_is_a_structured_error_not_a_command_failure() {
        let mut refused = snapshot("", false, None, BgTaskStatus::Failed, None);
        refused.remote_refusal = Some(crate::bash_background::registry::RemoteRefusal {
            code: "remote_unavailable",
            message: "runon refused: remote refused: unreachable; command was not run; retry, or omit runon to run locally".into(),
        });
        let response = foreground_result_response("refused", refused);
        assert!(!response.success, "{response:?}");
        assert_eq!(response.data["code"], "remote_unavailable");
        assert_eq!(response.data["task_id"], "bash-test");
        assert!(response.data["message"]
            .as_str()
            .unwrap()
            .contains("remote refused: unreachable"));

        let failed = snapshot("command failed", false, None, BgTaskStatus::Failed, Some(1));
        let response = foreground_result_response("failed", failed);
        assert!(
            response.success,
            "ordinary command exit is not a remote refusal: {response:?}"
        );
        assert_eq!(response.data["exit_code"], 1);
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

        match decide_bash_step(snapshot, now, false, false, now, "req-terminal") {
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

        match decide_bash_step(snapshot, now, false, false, now, "req-promote") {
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

        match decide_bash_step(snapshot, now, true, false, now, "req-block") {
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

    /// Blocking calls are capped independently of role; ordinary foreground
    /// calls still use the shorter promotion window.
    #[test]
    fn blocking_wait_cap_applies_to_every_blocking_call() {
        assert_eq!(blocking_wait_cap_ms(true, 90_000), Some(90_000));
        assert_eq!(blocking_wait_cap_ms(false, 90_000), None);
    }

    #[test]
    fn worker_wait_cap_waits_for_timeout_when_kill_deadline_is_at_the_cap() {
        assert!(worker_kill_deadline_within_handoff_margin(Some(
            Duration::ZERO
        )));
    }

    #[test]
    fn worker_wait_cap_hands_off_when_timeout_is_beyond_the_margin() {
        let remaining = Some(WORKER_KILL_HANDOFF_MARGIN + Duration::from_millis(1));
        assert!(!worker_kill_deadline_within_handoff_margin(remaining));
        let running = snapshot("still running", false, None, BgTaskStatus::Running, None);
        let now = Instant::now();
        match decide_bash_step(
            running,
            now,
            false,
            worker_kill_deadline_within_handoff_margin(remaining),
            now,
            "req-handoff",
        ) {
            BashStep::Promote => {}
            BashStep::Done(_) => panic!("running task should not finish"),
            BashStep::Wait => panic!("deadline beyond the margin should hand off"),
        }
    }

    #[test]
    fn worker_wait_cap_returns_timeout_terminal_instead_of_handoff() {
        let now = Instant::now();
        let running = snapshot("still running", false, None, BgTaskStatus::Running, None);
        match decide_bash_step(
            running,
            now,
            false,
            worker_kill_deadline_within_handoff_margin(Some(Duration::ZERO)),
            now,
            "req-timeout-terminal",
        ) {
            BashStep::Wait => {}
            BashStep::Done(_) => panic!("running task must wait for its timeout terminal"),
            BashStep::Promote => panic!("nearby kill deadline must not hand off"),
        }

        let timed_out = snapshot("timed out", false, None, BgTaskStatus::TimedOut, Some(124));
        match decide_bash_step(
            timed_out,
            now,
            false,
            worker_kill_deadline_within_handoff_margin(Some(Duration::ZERO)),
            now,
            "req-timeout-terminal",
        ) {
            BashStep::Done(response) => {
                let output = response.data["output"].as_str().unwrap();
                assert_eq!(response.data["status"], json!("timed_out"));
                assert_eq!(response.data["exit_code"], json!(124));
                assert!(output.contains("timed out"), "{output}");
                assert!(!output.contains("worker wait limit"), "{output}");
            }
            BashStep::Promote => panic!("timed-out task must not hand off"),
            BashStep::Wait => panic!("terminal snapshot must return immediately"),
        }
    }

    /// The reply names the configured limit, says the command was not killed,
    /// and offers the way to stop it.
    #[test]
    fn worker_wait_limit_message_names_the_configured_limit() {
        let text = format_worker_wait_limit_message(
            "bash-cap",
            5_400_000,
            5_401_000,
            "line 1\nline 2\n",
            true,
        );
        assert!(text.contains("It has run for 5401s"), "{text}");
        assert!(text.ends_with("Recent output:\nline 1\nline 2"), "{text}");
        assert!(
            format_worker_wait_limit_message("bash-cap", 60_000, 60_000, "", true)
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
        assert!(text.contains("Wait for it again to keep waiting"), "{text}");
        assert!(!text.contains("completion reminder"), "{text}");
        assert_eq!(format_wait_limit(1_800_000), "30 minutes");
        assert_eq!(format_wait_limit(60_000), "1 minute");
        assert_eq!(format_wait_limit(1_500), "1.5s");
    }

    #[test]
    fn kill_deadline_message_uses_command_start_and_names_its_source() {
        let started_at_ms = 1_700_000_000_000;
        let now_ms = started_at_ms + 18 * 60_000;
        let deadline_at =
            crate::subc_format::format_unix_millis_utc((started_at_ms + 30 * 60_000) as i64);
        let default_text = kill_deadline_sentence_at(
            Some(HardKillDeadline {
                limit_ms: 30 * 60_000,
                source: HardKillSource::Default,
            }),
            started_at_ms,
            now_ms,
            false,
        );
        assert!(
            default_text.contains(&format!("at {deadline_at}, when it has run 30 minutes")),
            "{default_text}"
        );
        assert!(
            default_text.contains("its default background limit"),
            "{default_text}"
        );
        assert!(
            default_text.contains("about 12 minutes remain"),
            "{default_text}"
        );

        let explicit_text = kill_deadline_sentence_at(
            Some(HardKillDeadline {
                limit_ms: 30 * 60_000,
                source: HardKillSource::Timeout,
            }),
            started_at_ms,
            now_ms,
            false,
        );
        assert!(
            explicit_text.contains("(the `timeout` you passed)"),
            "{explicit_text}"
        );
        assert!(
            !explicit_text.contains("default background limit"),
            "{explicit_text}"
        );
        assert!(
            explicit_text.contains("about 12 minutes remain"),
            "{explicit_text}"
        );
    }

    #[test]
    fn kill_deadline_sentence_matches_the_typescript_shared_fixture() {
        #[derive(Deserialize)]
        struct Fixture {
            cases: Vec<Case>,
        }
        #[derive(Deserialize)]
        struct Case {
            name: String,
            started_at_ms: u64,
            now_ms: u64,
            limit_ms: u64,
            source: String,
            role: String,
            expected: String,
        }

        let fixture: Fixture = serde_json::from_str(include_str!(
            "../../../../spec/fixtures/bash-kill-deadline-parity.json"
        ))
        .expect("shared kill-deadline fixture parses");
        for case in fixture.cases {
            let source = match case.source.as_str() {
                "default" => HardKillSource::Default,
                "timeout" => HardKillSource::Timeout,
                other => panic!("unknown hard-kill source: {other}"),
            };
            let actual = kill_deadline_sentence_at(
                Some(HardKillDeadline {
                    limit_ms: case.limit_ms,
                    source,
                }),
                case.started_at_ms,
                case.now_ms,
                case.role == "worker",
            );
            assert_eq!(actual, case.expected, "{}", case.name);
        }
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
            format_promotion_message("bash-123", Some(5_500), 8_000, false, false),
            "Foreground bash didn't finish within 5.5s and was promoted to background: bash-123. A completion reminder will be delivered automatically; use bash_status({ taskId: \"bash-123\" }) to inspect output or bash_kill({ taskId: \"bash-123\" }) to terminate."
        );
    }

    #[test]
    fn wait_detach_message_mentions_user_message() {
        assert_eq!(
            format_wait_detach_message("bash-123", false, false),
            "Foreground bash is running in background as bash-123. A completion reminder will be delivered automatically; use bash_status({ taskId: \"bash-123\" }) to inspect output or bash_kill({ taskId: \"bash-123\" }) to terminate.\nDetached because a user message arrived."
        );
    }

    #[test]
    fn background_launch_messages_match_opencode_copy() {
        assert_eq!(
            format_background_launch("bash-bg", false, false, false),
            "Background task started: bash-bg. A completion reminder will be delivered automatically; don't poll bash_status."
        );
        assert_eq!(
            format_background_launch("bash-pty", true, false, false),
            "PTY task started: bash-pty. Use bash_status({ taskId: \"bash-pty\", outputMode: \"screen\" }) to see the visible terminal, bash_write({ taskId: \"bash-pty\", input: ... }) to send keystrokes. A completion reminder fires automatically when the task exits."
        );
    }

    /// A delegated worker is never woken by a completion reminder, so no
    /// hand-off text it can receive may promise one; a primary's text is
    /// unchanged.
    #[test]
    fn hand_off_texts_are_worded_per_role() {
        let worker = [
            format_background_launch("bash-bg", false, true, true),
            format_background_launch("bash-pty", true, true, true),
            format_promotion_message("bash-123", None, 8_000, true, true),
            format_wait_detach_message("bash-123", true, true),
            format_module_drain_detach_message("bash-123", true, true),
            format_worker_wait_limit_message("bash-123", 1_800_000, 1_800_000, "", true),
        ];
        for text in &worker {
            assert!(!text.contains("completion reminder"), "{text}");
            assert!(!text.contains("end the turn"), "{text}");
            assert!(text.contains("won't wake you"), "{text}");
        }
        let primary = [
            format_background_launch("bash-bg", false, false, false),
            format_background_launch("bash-pty", true, false, false),
            format_promotion_message("bash-123", None, 8_000, false, false),
            format_wait_detach_message("bash-123", false, false),
            format_module_drain_detach_message("bash-123", false, false),
        ];
        for text in &primary {
            assert!(text.contains("completion reminder"), "{text}");
        }
    }

    #[test]
    fn worker_handoffs_name_bash_watch_only_when_the_catalog_serves_it() {
        let without_watch = [
            format_background_launch("bash-1", false, true, false),
            format_promotion_message("bash-1", None, 15_000, true, false),
            format_wait_detach_message("bash-1", true, false),
            format_module_drain_detach_message("bash-1", true, false),
            format_worker_wait_limit_message("bash-1", 1_800_000, 1_800_000, "", false),
        ];
        for text in &without_watch {
            assert!(!text.contains("bash_watch"), "{text}");
            assert!(
                text.contains("bash_status({ taskId: \"bash-1\" })"),
                "{text}"
            );
            assert!(text.contains("bash_kill({ taskId: \"bash-1\" })"), "{text}");
        }

        assert!(
            without_watch[0].contains("use bash_status"),
            "{}",
            without_watch[0]
        );
        for text in &without_watch[1..4] {
            assert!(
                text.contains("check whether it has finished before you report a result"),
                "{text}"
            );
        }
        assert!(
            without_watch[4].contains("Use bash_status"),
            "{}",
            without_watch[4]
        );

        let with_watch = [
            format_background_launch("bash-1", false, true, true),
            format_promotion_message("bash-1", None, 15_000, true, true),
            format_wait_detach_message("bash-1", true, true),
            format_module_drain_detach_message("bash-1", true, true),
            format_worker_wait_limit_message("bash-1", 1_800_000, 1_800_000, "", true),
        ];
        for text in &with_watch {
            assert!(!text.contains("completion reminder"), "{text}");
        }
        assert_eq!(
            with_watch[0],
            "Background task started: bash-1. It won't wake you when it finishes, so wait for it before you report a result."
        );
        assert!(
            with_watch[2].starts_with(
                "Foreground bash is running in background as bash-1. It won't wake you when it finishes, so wait for it before you report a result; use bash_status"
            ),
            "{}",
            with_watch[2]
        );
        assert!(
            with_watch[2].ends_with("Detached because a user message arrived."),
            "{}",
            with_watch[2]
        );
        assert!(
            with_watch[4].contains("Wait for it again to keep waiting"),
            "worker wait-cap wording changed: {}",
            with_watch[4]
        );
    }
}
