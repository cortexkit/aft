#[path = "alert_render.rs"]
pub mod alert_render;
#[path = "repeat_breaker.rs"]
pub mod repeat_breaker;

use std::path::Path;

use serde_json::Value;

use crate::context::AppContext;
use crate::protocol::Response;

fn cap_response_text(response: &mut Response, ctx: &AppContext, session: &str, tool: &str) {
    if response.data.get("text").is_some_and(Value::is_string) {
        if let Value::String(mut text) = response.data["text"].take() {
            crate::tool_output::cap(ctx, session, tool, response, &mut text);
            response.data["text"] = Value::String(text);
        }
    }
}

pub fn append_repeat_breaker_reminder(
    text: &mut String,
    session_id: &str,
    intervention: &repeat_breaker::RepeatIntervention,
    worker_session: bool,
    bash_watch_available: bool,
) {
    let count = intervention.count;
    let span_seconds = intervention.span.as_secs();
    let ordinal = ordinal(count);
    // The output signal controls only the diagnosis: stable output says the
    // call returns nothing new, while changing output names common timestamp
    // drift. Both variants point legitimate waiting toward a background watch
    // instead of treating output churn as progress.
    //
    // A delegated worker (subagent) is never told to end its turn: a worker
    // that ends its turn has delivered its result and cannot be woken when the
    // awaited task finishes. It is told to wait on the task instead.
    let escalated = repeat_breaker::escalation_starts_at(count);
    let instruction = match (worker_session, bash_watch_available, escalated) {
        (false, _, true) => "The turn must end now with no further tool call. If you are waiting on a task or CI run, use a background task with a watch (or the background handle you already hold) and end the turn. If you are already watching, let the watch return before calling again.",
        (false, _, false) => "If you are waiting on a task or CI run, use a background task with a watch (or the background handle you already hold) and end the turn. If you are already watching, let the watch return before calling again.",
        (true, true, true) => "Stop now: make no further call with these arguments. If you are waiting on a task or CI run, wait with a watch on its background task (for example bash_watch on the task ID you already hold) instead of repeating this call. If you are already watching, let the watch return before calling again.",
        (true, true, false) => "If you are waiting on a task or CI run, wait with a watch on its background task (for example bash_watch on the task ID you already hold) instead of repeating this call. If you are already watching, let the watch return before calling again.",
        (true, false, true) => "Stop now: make no further call with these arguments. If you are waiting on a task or CI run, use bash_status on the task ID you already hold to inspect it, or bash_kill to stop it, instead of repeating this call.",
        (true, false, false) => "If you are waiting on a task or CI run, use bash_status on the task ID you already hold to inspect it, or bash_kill to stop it, instead of repeating this call.",
    };
    let observation = if intervention.outputs_identical {
        format!(
            "This is the {ordinal} identical call (same command, same output) in {span_seconds}s. This call is not returning anything new."
        )
    } else {
        format!(
            "This is the {ordinal} call with the same arguments in {span_seconds}s. The call is being repeated with the same arguments while its output is drifting (for example, because it includes a timestamp)."
        )
    };
    let reminder = format!("<system-reminder>\n{observation} {instruction}\n</system-reminder>");
    if text.is_empty() {
        *text = reminder;
    } else {
        text.push_str("\n\n");
        text.push_str(&reminder);
    }
    log::info!(
        "repeat_breaker fired session={} tool={} count={} span_ms={}",
        session_id,
        intervention.tool,
        count,
        intervention.span.as_millis()
    );
}

fn ordinal(count: u64) -> String {
    let suffix = if (11..=13).contains(&(count % 100)) {
        "th"
    } else {
        match count % 10 {
            1 => "st",
            2 => "nd",
            3 => "rd",
            _ => "th",
        }
    };
    format!("{count}{suffix}")
}

/// Finalize a direct protocol response that has no dispatch-root provenance. Agent-visible
/// finalization must use [`finalize_response_for_dispatch_root`] so alert delivery never infers
/// a root from the session context.
pub fn finalize_response(
    response: &mut Response,
    ctx: &AppContext,
    session_id: &str,
    attach_command: &str,
) {
    finalize_response_with_bg_completions(response, ctx, session_id, attach_command, true);
}

/// Finalization for direct protocol responses (the standalone NDJSON path). A standalone
/// `tool_call` response carries its agent-visible text in `data.text`, so the status bar is
/// appended there; a response without text has nothing the agent reads and gets no bar.
pub fn finalize_response_with_bg_completions(
    response: &mut Response,
    ctx: &AppContext,
    session_id: &str,
    attach_command: &str,
    allow_bg_completions: bool,
) {
    let (publish, counts) = finalize_prefix(
        response,
        ctx,
        session_id,
        attach_command,
        allow_bg_completions,
    );
    if response.data.get("text").is_some_and(Value::is_string) {
        if let Some(line) = status_bar_line(ctx, publish, attach_command, session_id, counts) {
            if let Some(Value::String(text)) = response.data.get_mut("text") {
                append_trailing_line(text, &line);
            }
        }
    }
    cap_response_text(response, ctx, session_id, attach_command);
}

/// Finalization for a tool result whose agent-visible text is held apart from the response
/// (the subc daemon path). Appends the status bar to `text`, the same line the standalone
/// path appends to `data.text`.
pub fn finalize_tool_response(
    response: &mut Response,
    text: &mut String,
    ctx: &AppContext,
    session_id: &str,
    attach_command: &str,
    allow_bg_completions: bool,
) {
    finalize_tool_response_with_visibility(
        response,
        text,
        ctx,
        session_id,
        attach_command,
        allow_bg_completions,
        true,
    );
}

/// Native plugin calls share tool dispatch with agent calls, but their replies are not
/// shown to the agent. They must leave cancelled-call notices pending for a visible reply.
#[allow(clippy::too_many_arguments)]
pub(crate) fn finalize_tool_response_with_visibility(
    response: &mut Response,
    text: &mut String,
    ctx: &AppContext,
    session_id: &str,
    attach_command: &str,
    allow_bg_completions: bool,
    agent_visible: bool,
) {
    finalize_tool_response_text(
        response,
        text,
        ctx,
        session_id,
        attach_command,
        allow_bg_completions,
        agent_visible,
        true,
    );
}

/// Shell delivery can render the body again after checking whether the runner
/// received the command.
/// Leave it whole until the writer applies the budget to that final rendering.
pub(crate) fn finalize_redisplayed_bash_response(
    response: &mut Response,
    text: &mut String,
    ctx: &AppContext,
    session_id: &str,
    allow_bg_completions: bool,
) {
    finalize_tool_response_text(
        response,
        text,
        ctx,
        session_id,
        "bash",
        allow_bg_completions,
        true,
        false,
    );
}

#[allow(clippy::too_many_arguments)]
fn finalize_tool_response_text(
    response: &mut Response,
    text: &mut String,
    ctx: &AppContext,
    session_id: &str,
    attach_command: &str,
    allow_bg_completions: bool,
    agent_visible: bool,
    cap_text: bool,
) {
    let (publish, counts) = finalize_prefix(
        response,
        ctx,
        session_id,
        attach_command,
        allow_bg_completions,
    );
    if agent_visible {
        append_cancelled_call_notices(response, text, ctx, session_id);
    }
    if let Some(line) = status_bar_line(ctx, publish, attach_command, session_id, counts) {
        append_trailing_line(text, &line);
    }
    if cap_text {
        crate::tool_output::cap(ctx, session_id, attach_command, response, text);
    }
}

/// Foreground and watch replies consume terminal results for their originating
/// session. `bash_status` is a read-only snapshot, even when terminal: polling
/// it must leave the completion available for a later notification or drain.
fn acknowledge_terminal_bash_reply(
    response: &Response,
    ctx: &AppContext,
    session_id: &str,
    command: &str,
) {
    if !response.success || !matches!(command, "bash" | "bash_watch") {
        return;
    }
    // A fast background child can exit before its launch reply is built. That
    // reply has a terminal status but only launch guidance in `output`; only a
    // foreground result includes the command's `output_preview`. Acknowledging
    // launch guidance would discard a queued completion or suppress its later
    // publication, even though the caller has not received the result.
    if command == "bash"
        && !response
            .data
            .get("output_preview")
            .is_some_and(Value::is_string)
    {
        return;
    }
    if !response
        .data
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|status| {
            matches!(
                status,
                "completed" | "failed" | "killed" | "timed_out" | "fate_unknown"
            )
        })
    {
        return;
    }
    if let Some(task_id) = response.data.get("task_id").and_then(Value::as_str) {
        ctx.bash_background()
            .ack_terminal_result_for_session(task_id, session_id);
    }
}

/// An agent-visible reply names tasks the session has not heard about: commands
/// still running after their waiting call was cancelled, and commands stopped
/// by an owner cancellation. The registry supplies the appropriate status for each.
pub(crate) fn append_cancelled_call_notices(
    response: &Response,
    text: &mut String,
    ctx: &AppContext,
    session_id: &str,
) {
    // An internal access ask is held for consent and replaced by a later tool reply.
    if response.data.get("code").and_then(Value::as_str)
        == Some(crate::access_policy::ACCESS_ASK_REQUIRED)
    {
        return;
    }
    for line in ctx
        .bash_background()
        .take_cancelled_call_notices(session_id)
    {
        append_trailing_line(text, &line);
    }
}

use crate::shell_format::append_trailing_line;

/// Finalize an agent-visible response using the root selected by dispatch. The finalizer owns
/// the alert transition and never reads `ctx.config().project_root` for alert state.
pub fn finalize_response_for_dispatch_root(
    response: &mut Response,
    ctx: &AppContext,
    alerts: &mut alert_render::AlertEngine,
    session_id: &str,
    dispatch_root: &Path,
    attach_command: &str,
    allow_bg_completions: bool,
) {
    let _ = finalize_prefix(
        response,
        ctx,
        session_id,
        attach_command,
        allow_bg_completions,
    );
    attach_alert_block(response, alerts, session_id, dispatch_root, attach_command);
    cap_response_text(response, ctx, session_id, attach_command);
}

fn attach_alert_block(
    response: &mut Response,
    alerts: &mut alert_render::AlertEngine,
    session_id: &str,
    dispatch_root: &Path,
    command: &str,
) {
    let Some(text) = response
        .data
        .as_object_mut()
        .and_then(|data| data.get_mut("text"))
        .and_then(|value| value.as_str())
        .map(str::to_string)
    else {
        return;
    };

    // A response can pass through a structured transport as well as its terminal adapter.
    // Refuse a second server reminder rather than consuming an alert behind a duplicate block.
    if text.contains("<system-reminder>") {
        return;
    }
    let Some(alert) = alerts.finalize(session_id, dispatch_root, command) else {
        return;
    };
    let joined = if text.is_empty() {
        alert.text
    } else {
        format!("{text}\n\n{}", alert.text)
    };
    if let Some(data) = response.data.as_object_mut() {
        data.insert("text".to_string(), serde_json::Value::String(joined));
    }
}

pub enum DispatchOutcome {
    Immediate(Response),
    Deferred(PendingResponse),
}

pub type PendingResponsePoll = Box<dyn FnMut(&AppContext) -> Option<Response> + Send>;
pub type PendingResponseShutdown = Box<dyn FnMut(&AppContext) -> Response + Send>;

pub(crate) use crate::pending_response::DeferredResponseWake;
pub use crate::pending_response::{
    pending_response_channel, PendingResponseReceiver, PendingResponseSender,
};

/// Deferred responses must choose a readiness mechanism: a wake-bound channel,
/// or periodic polling for state/deadline-based producers. A raw struct literal
/// cannot silently opt out of both mechanisms.
///
/// ```compile_fail
/// use aft::response_finalize::PendingResponse;
/// let _pending = PendingResponse {
///     request_id: "unwoken".into(),
///     session_id: "session".into(),
///     attach_command: "read".into(),
///     poll: Box::new(|_| None),
///     cancellation: None,
///     on_shutdown: None,
///     poll_interval: None,
/// };
/// ```
pub struct PendingResponse {
    pub request_id: String,
    pub session_id: String,
    pub attach_command: String,
    pub poll: PendingResponsePoll,
    /// Cancellation shared with work that continued after its executor setup
    /// job returned. Registry replacement and transport shutdown signal it
    /// before removing the pending entry.
    pub cancellation: Option<crate::executor::JobCancellation>,
    /// Optional terminal response emitted before this entry is removed during
    /// shutdown. Long-running inspect uses this to avoid silently dropping its
    /// only agent-visible terminal frame.
    pub on_shutdown: Option<PendingResponseShutdown>,
    poll_interval: Option<std::time::Duration>,
    admitted_config: Option<std::sync::Arc<crate::config::Config>>,
}

impl PendingResponse {
    /// For state-based waits that can resolve on a deadline without any producer
    /// sending a value. The transport must retain this timer even when idle.
    pub fn polling(
        request_id: String,
        session_id: String,
        attach_command: String,
        poll: PendingResponsePoll,
    ) -> Self {
        Self {
            request_id,
            session_id,
            attach_command,
            poll,
            cancellation: None,
            on_shutdown: None,
            poll_interval: Some(std::time::Duration::from_millis(100)),
            admitted_config: None,
        }
    }

    /// Channel-backed producers cannot forget the completion wake: only the
    /// receiver paired with a wake-bound sender can select completion-only polling.
    pub fn from_receiver<T: Send + 'static>(
        request_id: String,
        session_id: String,
        attach_command: String,
        receiver: PendingResponseReceiver<T>,
        mut poll: impl FnMut(&AppContext, Result<T, std::sync::mpsc::TryRecvError>) -> Option<Response>
            + Send
            + 'static,
    ) -> Self {
        // Standalone callers and callers without an admitted connection scope
        // cannot publish this wake. Fail safe with a timer rather than trusting
        // a channel whose sender has no transport to notify.
        let poll_interval =
            (!receiver.wake_installed).then_some(std::time::Duration::from_millis(100));
        Self {
            request_id,
            session_id,
            attach_command,
            poll: Box::new(move |ctx| poll(ctx, receiver.try_recv())),
            cancellation: None,
            on_shutdown: None,
            poll_interval,
            admitted_config: None,
        }
    }

    pub fn with_cancellation(mut self, cancellation: crate::executor::JobCancellation) -> Self {
        self.cancellation = Some(cancellation);
        self
    }

    /// Retain the admission snapshot for response polling and finalization.
    pub fn with_admitted_config(mut self, config: std::sync::Arc<crate::config::Config>) -> Self {
        self.admitted_config = Some(config);
        self
    }

    pub fn with_shutdown(mut self, on_shutdown: PendingResponseShutdown) -> Self {
        self.on_shutdown = Some(on_shutdown);
        self
    }

    /// A channel-backed request may also need an independent overall deadline.
    pub fn with_poll_interval(mut self, interval: std::time::Duration) -> Self {
        assert!(
            !interval.is_zero(),
            "pending response poll interval must be positive"
        );
        self.poll_interval = Some(interval);
        self
    }

    pub fn poll_interval(&self) -> Option<std::time::Duration> {
        self.poll_interval
    }
}

pub struct ResolvedPending {
    pub response: Response,
    pub session_id: String,
    pub attach_command: String,
}

#[derive(Default)]
pub struct PendingResponses {
    entries: Vec<PendingResponse>,
}

impl PendingResponses {
    /// IDs are unique across sessions while pending on one standalone connection.
    /// Rejected work stays owned by the caller; the original slot is untouched.
    pub fn register(&mut self, pending: PendingResponse) -> Result<(), Box<PendingResponse>> {
        if self.contains(&pending.request_id) {
            return Err(Box::new(pending));
        }
        self.entries.push(pending);
        Ok(())
    }

    /// Signal cooperative cancellation without removing the response slot.
    /// The worker owns the terminal response and resolves it through `poll_ready`.
    pub fn cancel_request(&mut self, request_id: &str) -> bool {
        let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.request_id == request_id)
        else {
            return false;
        };
        let Some(cancellation) = &entry.cancellation else {
            return false;
        };
        cancellation.request_cancel();
        true
    }

    pub fn poll_ready(&mut self, ctx: &AppContext) -> Vec<ResolvedPending> {
        let mut ready = Vec::new();
        let mut waiting = Vec::with_capacity(self.entries.len());

        for mut pending in self.entries.drain(..) {
            if let Some(response) = (pending.poll)(ctx) {
                ready.push(ResolvedPending {
                    response,
                    session_id: pending.session_id,
                    attach_command: pending.attach_command,
                });
            } else {
                waiting.push(pending);
            }
        }

        self.entries = waiting;
        ready
    }

    /// Resolve each ready response within its admission pin. Unvisited entries
    /// stay registered if delivery fails, rather than being lost with a batch.
    pub fn poll_ready_with<E>(
        &mut self,
        ctx: &AppContext,
        mut deliver: impl FnMut(ResolvedPending) -> Result<(), E>,
    ) -> Result<usize, E> {
        let mut delivered = 0;
        let mut index = 0;
        while index < self.entries.len() {
            let entry = &mut self.entries[index];
            let _pin = entry
                .admitted_config
                .as_ref()
                .map(|config| ctx.pin_config_to(std::sync::Arc::clone(config)));
            if let Some(response) = (entry.poll)(ctx) {
                let pending = self.entries.remove(index);
                deliver(ResolvedPending {
                    response,
                    session_id: pending.session_id,
                    attach_command: pending.attach_command,
                })?;
                delivered += 1;
            } else {
                index += 1;
            }
        }
        Ok(delivered)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// True while a response for `request_id` is still registered (not yet
    /// resolved by `poll_ready` or drained at shutdown).
    pub fn contains(&self, request_id: &str) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.request_id == request_id)
    }

    /// Cancellation-only cleanup after the transport is no longer writable.
    /// Use `drain_on_shutdown_delivering` to account for terminal replies.
    pub fn drain_on_shutdown(&mut self) {
        for pending in self.entries.drain(..) {
            if let Some(cancellation) = &pending.cancellation {
                cancellation.request_cancel();
            }
        }
    }

    /// Resolve every shutdown entry, using a fallback cancellation reply
    /// when a producer has not supplied its own shutdown terminal.
    pub fn drain_on_shutdown_with(&mut self, ctx: &AppContext) -> Vec<ResolvedPending> {
        let mut resolved = Vec::new();
        self.drain_on_shutdown_delivering(ctx, |response| resolved.push(response));
        resolved
    }

    /// Deliver shutdown terminals while the request's admission snapshot is pinned.
    pub fn drain_on_shutdown_delivering(
        &mut self,
        ctx: &AppContext,
        mut deliver: impl FnMut(ResolvedPending),
    ) {
        for mut pending in self.entries.drain(..) {
            let _pin = pending
                .admitted_config
                .as_ref()
                .map(|config| ctx.pin_config_to(std::sync::Arc::clone(config)));
            if let Some(cancellation) = &pending.cancellation {
                cancellation.request_cancel();
            }
            let response = if let Some(shutdown) = pending.on_shutdown.as_mut() {
                shutdown(ctx)
            } else {
                Response::error(
                    &pending.request_id,
                    "request_cancelled",
                    "request cancelled because the connection is shutting down",
                )
            };
            deliver(ResolvedPending {
                response,
                session_id: pending.session_id,
                attach_command: pending.attach_command,
            });
        }
    }
}

pub fn attach_bg_completions(
    response: &mut Response,
    ctx: &AppContext,
    session_id: &str,
    command: &str,
) {
    if matches!(
        command,
        "configure"
            | "bash_abort_inflight"
            | "bash_status"
            | "bash_write"
            | "bash_promote"
            | "bash_wait_detach"
            | "bash_regex_match"
            | "bash_artifact_owned"
            | "bash_owned_tasks"
            | "bash_drain_completions"
            | "bash_notify"
            | "bash_unnotify"
            | "bash_ack_completions"
    ) {
        return;
    }
    if !ctx
        .bash_background()
        .has_completions_for_session(Some(session_id))
    {
        return;
    }
    let completions = ctx
        .bash_background()
        .drain_completions_for_session(Some(session_id));
    if completions.is_empty() {
        return;
    }
    let value = serde_json::json!(completions);
    match response.data.as_object_mut() {
        Some(data) => {
            data.insert("bg_completions".to_string(), value);
        }
        None => {
            response.data = serde_json::json!({ "bg_completions": value });
        }
    }
}

fn aft_status_segment(counts: &crate::context::StatusBarCounts) -> String {
    // Self-labeled per the fleet status-line format ruling (2026-08-17): the
    // holder composes segments label-free and joins module boundaries with a
    // bullet, so each publisher's text must carry its own leading label.
    format!(
        "AFT E{} W{} | T{}",
        counts.errors, counts.warnings, counts.todos
    )
}

/// Renders the agent-facing bar from the omission-preserving values. A category with no
/// trustworthy value yet shows `?` (as the OpenCode footer does) rather than a clean `0`.
/// The bar has no dead-code, unused-export or duplicate counts: those analyses run only
/// when an agent calls `aft_inspect`, so a background count would be stale after any edit.
fn agent_status_bar(values: &crate::context::StatusBarCountValues) -> String {
    let count = |value: Option<usize>, category: &str| -> String {
        if values.disabled_categories.contains(&category) {
            "○".to_string()
        } else {
            value.map_or_else(|| "?".to_string(), |value| value.to_string())
        }
    };
    format!(
        "[AFT E{} W{} | T{}]",
        count(values.errors, "diagnostics"),
        count(values.warnings, "diagnostics"),
        count(values.todos, "todos")
    )
}

/// Outcome of offering this response's status to the fleet holder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FleetStatusPublish {
    /// The response opted out of status (cross-root indexed search), so no bar either.
    Suppressed,
    /// `reader_renders` is true only when a host plugin is actually reading this project's
    /// fleet line for this session's harness, so the composed line replaces AFT's own bar.
    Offered { reader_renders: bool },
}

/// Whether the holder's composed line reaches this session's agent. The only renderer of
/// that line is Prefrontal's status stamper, an OpenCode plugin; a Pi, runner, or MCP
/// session never sees it even when an OpenCode session reads the same project scope.
fn fleet_reader_renders_bar(
    harness: Option<&crate::harness::Harness>,
    reader_present: bool,
) -> bool {
    matches!(harness, Some(crate::harness::Harness::Opencode)) && reader_present
}

/// Publish the fleet status segment and report whether a fleet reader replaces AFT's bar.
fn publish_fleet_status(
    response: &mut Response,
    ctx: &AppContext,
    session_id: &str,
    counts: &mut Option<crate::context::StatusBarCountValues>,
) -> FleetStatusPublish {
    // Cross-root indexed searches suppress status. Remove the private marker so it cannot
    // appear in a response envelope.
    if response
        .data
        .as_object_mut()
        .and_then(|data| data.remove("_aft_suppress_status_bar"))
        .is_some()
    {
        return FleetStatusPublish::Suppressed;
    }

    let Some(client) = ctx.fleet_status_client() else {
        return FleetStatusPublish::Offered {
            reader_renders: false,
        };
    };
    let config = ctx.config();
    let Some(project_root) = config.project_root.as_deref() else {
        return FleetStatusPublish::Offered {
            reader_renders: false,
        };
    };
    let harness = ctx.harness_opt();
    let harness_label = harness
        .as_ref()
        .map(crate::harness::Harness::wire_label)
        .unwrap_or_else(|| "unknown".to_string());
    // The holder composes complete segments only: a partial set is published as quiet text.
    let Some(values) = ctx.try_status_bar_count_values() else {
        return FleetStatusPublish::Suppressed;
    };
    let aft_text = values
        .legacy_projection()
        .as_ref()
        .map(aft_status_segment)
        .unwrap_or_default();
    client.publish(project_root, &harness_label, session_id, &aft_text);
    *counts = Some(values);
    FleetStatusPublish::Offered {
        reader_renders: fleet_reader_renders_bar(
            harness.as_ref(),
            client.reader_present(project_root),
        ),
    }
}

/// The status-bar line to append to agent-visible text, if any. Emitted only when the values
/// changed since the last bar the agent was shown, so an unchanged bar costs no tokens and
/// keeps prompt caches stable. While a fleet reader renders the bar the change gate is left
/// untouched, so the bar reappears as soon as that reader goes away.
fn status_bar_line(
    ctx: &AppContext,
    publish: FleetStatusPublish,
    attach_command: &str,
    session_id: &str,
    counts: Option<crate::context::StatusBarCountValues>,
) -> Option<String> {
    if alert_render::is_excluded_finalization_command(attach_command) {
        return None;
    }
    match publish {
        FleetStatusPublish::Suppressed
        | FleetStatusPublish::Offered {
            reader_renders: true,
        } => return None,
        FleetStatusPublish::Offered {
            reader_renders: false,
        } => {}
    }
    let values = counts.or_else(|| ctx.try_status_bar_count_values())?;
    let nothing_known = [values.errors, values.warnings, values.todos]
        .iter()
        .all(Option::is_none);
    if nothing_known || !ctx.should_emit_status_bar_for_session(session_id, &values) {
        return None;
    }
    Some(agent_status_bar(&values))
}

fn finalize_prefix(
    response: &mut Response,
    ctx: &AppContext,
    session_id: &str,
    attach_command: &str,
    allow_bg_completions: bool,
) -> (
    FleetStatusPublish,
    Option<crate::context::StatusBarCountValues>,
) {
    acknowledge_terminal_bash_reply(response, ctx, session_id, attach_command);
    // Dispatch attaches these before tool text is rendered. Direct protocol
    // callers also need them; attachment is idempotent for the pinned outcome.
    if allow_bg_completions {
        attach_bg_completions(response, ctx, session_id, attach_command);
    }
    let mut counts = None;
    let publish = publish_fleet_status(response, ctx, session_id, &mut counts);
    (publish, counts)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{
        aft_status_segment, agent_status_bar, finalize_response_with_bg_completions,
        finalize_tool_response, fleet_reader_renders_bar, PendingResponse, PendingResponses,
    };
    use crate::config::Config;
    use crate::context::{AppContext, StatusBarCountValues, StatusBarCounts};
    use crate::fleet_status::FleetStatusClient;
    use crate::harness::Harness;
    use crate::parser::TreeSitterProvider;
    use crate::protocol::Response;

    #[test]
    fn duplicate_registration_preserves_original_response_and_cancellation() {
        let ctx = standalone_ctx();
        let original_cancel = crate::executor::JobCancellation::new();
        let mut pending = PendingResponses::default();
        pending
            .register(
                PendingResponse::polling(
                    "same".into(),
                    "first".into(),
                    "read".into(),
                    Box::new(|_| {
                        Some(Response::success(
                            "same",
                            serde_json::json!({"owner":"original"}),
                        ))
                    }),
                )
                .with_cancellation(original_cancel.clone()),
            )
            .unwrap_or_else(|_| panic!("pending request id must be unique"));
        assert!(pending
            .register(PendingResponse::polling(
                "same".into(),
                "second".into(),
                "read".into(),
                Box::new(|_| Some(Response::success(
                    "same",
                    serde_json::json!({"owner":"replacement"})
                )))
            ))
            .is_err());
        assert!(
            !original_cancel.cancel_requested_before_commit(),
            "duplicate must not cancel the original work"
        );
        let ready = pending.poll_ready(&ctx);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].response.data["owner"], "original");
    }

    #[test]
    fn shutdown_without_callback_has_one_correlated_terminal_and_cancels_first() {
        let ctx = standalone_ctx();
        let cancellation = crate::executor::JobCancellation::new();
        let mut pending = PendingResponses::default();
        pending
            .register(
                PendingResponse::polling(
                    "fallback".into(),
                    "session".into(),
                    "read".into(),
                    Box::new(|_| None),
                )
                .with_cancellation(cancellation.clone())
                .with_admitted_config(ctx.config()),
            )
            .unwrap_or_else(|_| panic!("pending request id must be unique"));
        ctx.update_config(|config| config.restrict_to_project_root = true);
        let mut delivered = Vec::new();
        pending.drain_on_shutdown_delivering(&ctx, |resolved| {
            assert!(cancellation.cancel_requested_before_commit());
            assert!(
                !ctx.config().restrict_to_project_root,
                "shutdown finalization must retain admission config"
            );
            delivered.push(resolved);
        });
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].response.id, "fallback");
        assert_eq!(delivered[0].response.data["code"], "request_cancelled");
        assert!(pending.is_empty());
        pending.drain_on_shutdown_delivering(&ctx, |_| panic!("terminal delivered twice"));
    }

    #[test]
    fn pending_response_channel_without_connection_scope_retains_timer() {
        let (_sender, receiver) = super::pending_response_channel::<Response>();
        let pending = PendingResponse::from_receiver(
            "unscoped".into(),
            "session".into(),
            "read".into(),
            receiver,
            |_, completion| completion.ok(),
        );
        assert_eq!(
            pending.poll_interval(),
            Some(std::time::Duration::from_millis(100))
        );
        let wake = super::DeferredResponseWake::default();
        let (_sender, receiver) = {
            let _scope = wake.install();
            super::pending_response_channel::<Response>()
        };
        let pending = PendingResponse::from_receiver(
            "scoped".into(),
            "session".into(),
            "read".into(),
            receiver,
            |_, completion| completion.ok(),
        );
        assert!(
            pending.poll_interval().is_none(),
            "admitted channel producers stay completion-only"
        );
    }

    #[tokio::test]
    async fn pending_response_channel_wakes_after_producer_disconnect() {
        let wake = super::DeferredResponseWake::default();
        let (sender, receiver) = {
            let _scope = wake.install();
            super::pending_response_channel::<Response>()
        };
        std::thread::spawn(move || drop(sender));
        tokio::time::timeout(std::time::Duration::from_secs(5), wake.notified())
            .await
            .expect("disconnected producer wake");
        assert_eq!(wake.generation(), 1);
        assert!(matches!(
            receiver.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn only_an_opencode_session_can_have_its_bar_rendered_by_a_fleet_reader() {
        assert!(fleet_reader_renders_bar(Some(&Harness::Opencode), true));
        assert!(!fleet_reader_renders_bar(Some(&Harness::Opencode), false));
        for harness in [Harness::Pi, Harness::Runner] {
            assert!(!fleet_reader_renders_bar(Some(&harness), true));
        }
        assert!(!fleet_reader_renders_bar(None, true));
    }

    #[test]
    fn agent_bar_marks_missing_categories_instead_of_zeroing_them() {
        let values = StatusBarCountValues {
            errors: None,
            warnings: Some(0),
            todos: None,
            disabled_categories: Vec::new(),
            ..Default::default()
        };
        assert_eq!(agent_status_bar(&values), "[AFT E? W0 | T?]");
    }

    /// The whole-repository analyses run only inside `aft_inspect`, so the bar
    /// carries no dead-code, unused-export or duplicate segment and no `~`
    /// staleness marker for them, even when a category is switched off.
    #[test]
    fn agent_bar_has_no_whole_repository_segment() {
        let values = StatusBarCountValues {
            errors: Some(1),
            warnings: Some(2),
            todos: Some(3),
            disabled_categories: vec!["dead_code", "duplicates"],
            ..Default::default()
        };
        assert_eq!(agent_status_bar(&values), "[AFT E1 W2 | T3]");
    }

    fn standalone_ctx() -> AppContext {
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(PathBuf::from("/tmp/project")),
                ..Config::default()
            },
        );
        ctx.set_harness(Harness::Opencode);
        ctx
    }

    #[test]
    fn bar_is_appended_to_text_only_on_change_and_never_to_textless_responses() {
        let ctx = standalone_ctx();
        ctx.update_status_bar_todos(Some(4));

        let mut textless = Response::success("plumbing", serde_json::json!({}));
        finalize_response_with_bg_completions(&mut textless, &ctx, "s", "read", false);
        assert_eq!(
            textless.data,
            serde_json::json!({}),
            "no text, no bar, no sidecar"
        );

        let mut first = String::from("file contents");
        let mut response = Response::success("first", serde_json::json!({}));
        finalize_tool_response(&mut response, &mut first, &ctx, "s", "read", false);
        assert_eq!(first, "file contents\n\n[AFT E? W? | T4]");

        let mut unchanged = String::from("file contents");
        finalize_tool_response(&mut response, &mut unchanged, &ctx, "s", "read", false);
        assert_eq!(unchanged, "file contents");

        let mut excluded = String::from("drained");
        ctx.update_status_bar_todos(Some(9));
        finalize_tool_response(
            &mut response,
            &mut excluded,
            &ctx,
            "s",
            "bash_drain_completions",
            false,
        );
        assert_eq!(excluded, "drained", "plumbing never consumes the change");

        let mut standalone =
            Response::success("standalone", serde_json::json!({ "text": "listing\n" }));
        finalize_response_with_bg_completions(&mut standalone, &ctx, "s", "glob", false);
        assert_eq!(standalone.data["text"], "listing\n\n[AFT E? W? | T9]");
    }

    #[test]
    fn status_bar_deduplicates_per_session() {
        let ctx = standalone_ctx();
        ctx.update_status_bar_todos(Some(4));
        for session in ["first", "second"] {
            let mut response = Response::success("bar", serde_json::json!({}));
            let mut text = "contents".to_string();
            finalize_tool_response(&mut response, &mut text, &ctx, session, "read", false);
            assert!(text.contains("[AFT"), "first bar for {session}");
            let mut repeat = "contents".to_string();
            finalize_tool_response(&mut response, &mut repeat, &ctx, session, "read", false);
            assert_eq!(repeat, "contents");
        }
    }

    #[test]
    fn fleet_and_agent_bar_share_one_count_sample() {
        let ctx = standalone_ctx();
        ctx.update_status_bar_todos(Some(4));
        let (client, mut wire_rx) = FleetStatusClient::dial_channel(1);
        ctx.install_fleet_status_client(Some(client));
        crate::context::STATUS_COUNT_PROBES.with(|count| count.set(0));
        let mut response = Response::success("counts", serde_json::json!({}));
        let mut text = "contents".to_string();
        finalize_tool_response(&mut response, &mut text, &ctx, "counts", "read", false);
        assert!(text.contains("[AFT"));
        wire_rx
            .try_recv()
            .expect("fleet publication")
            .complete_unavailable();
        assert_eq!(
            crate::context::STATUS_COUNT_PROBES.with(|count| count.get()),
            1
        );
    }

    #[test]
    fn cross_root_marker_suppresses_the_bar_and_is_stripped() {
        let ctx = standalone_ctx();
        ctx.update_status_bar_todos(Some(4));
        let mut response = Response::success(
            "search",
            serde_json::json!({ "text": "hits", "_aft_suppress_status_bar": true }),
        );
        finalize_response_with_bg_completions(&mut response, &ctx, "s", "aft_search", false);
        assert_eq!(response.data, serde_json::json!({ "text": "hits" }));
    }

    #[test]
    fn pre_discovery_publish_does_not_trip_the_holder_ownership_gate() {
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(PathBuf::from("/tmp/project")),
                ..Config::default()
            },
        );
        ctx.set_harness(Harness::Opencode);
        ctx.update_status_bar_todos(Some(14));
        let (client, mut wire_rx) = FleetStatusClient::dial_channel(1);
        ctx.install_fleet_status_client(Some(client));
        let mut response = Response::success("status", serde_json::json!({}));

        finalize_response_with_bg_completions(&mut response, &ctx, "session-1", "echo", false);

        assert!(response.data.get("status_bar").is_none());
        let publish = wire_rx.try_recv().expect("single discovery publish");
        assert_eq!(
            publish.body()["text"],
            "",
            "missing diagnostics stay absent instead of being published as E0 W0"
        );
        assert!(
            wire_rx.try_recv().is_err(),
            "response published more than once"
        );
        publish.complete_unavailable();
    }

    #[test]
    fn published_segment_bytes_are_self_labeled() {
        let counts = StatusBarCounts {
            errors: 2,
            warnings: 5,
            todos: 8,
        };
        assert_eq!(
            format!("[{}]", aft_status_segment(&counts)),
            "[AFT E2 W5 | T8]"
        );
    }

    #[test]
    fn shutdown_delivery_emits_terminal_before_removing_entry() {
        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        let mut pending = PendingResponses::default();
        pending
            .register(
                PendingResponse::polling(
                    "inspect-shutdown".to_string(),
                    String::new(),
                    String::new(),
                    Box::new(|_| None),
                )
                .with_shutdown(Box::new(|_| {
                    Response::error("inspect-shutdown", "daemon_shutdown", "shutdown")
                })),
            )
            .unwrap_or_else(|_| panic!("pending request id must be unique"));

        let resolved = pending.drain_on_shutdown_with(&ctx);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].response.id, "inspect-shutdown");
        assert!(pending.is_empty());
    }
}

pub const WATCHER_PENDING_NOTICE: &str = "Watcher changes pending: indexed or cached results may omit unapplied file changes. Retry after maintenance catches up; a missing result is not evidence of absence.";

/// Capture before execution: finishing maintenance cannot make an already
/// computed index-backed answer fresh retroactively. Live disk reads are exempt,
/// and so are `outline` and `zoom`: they parse the file on disk, and the symbol
/// cache they consult is checked against each file's current mtime and size.
pub fn watcher_query_pending(ctx: &AppContext, command: &str) -> bool {
    is_watcher_query_command(command) && ctx.watcher_query_has_pending_changes()
}

/// Commands whose answers come from indexed or cached producers that the
/// watcher keeps current.
fn is_watcher_query_command(command: &str) -> bool {
    matches!(
        command,
        "grep"
            | "glob"
            | "search"
            | "semantic_search"
            | "inspect"
            | "callgraph"
            | "callers"
            | "call_tree"
            | "impact"
            | "trace_to"
            | "trace_to_symbol"
            | "trace_data"
    )
}

/// Commands answered from the call graph.
fn is_callgraph_command(command: &str) -> bool {
    matches!(
        command,
        "callgraph"
            | "callers"
            | "call_tree"
            | "impact"
            | "trace_to"
            | "trace_to_symbol"
            | "trace_data"
    )
}

/// Two values captured together before a command runs, because a refresh that
/// lands while the answer is computed does not make that answer current:
/// whether unsorted watcher events exist (as [`watcher_query_pending`]
/// reports), and the files the call graph has not applied yet (`None` for
/// commands not answered from the call graph and when nothing is pending).
///
/// One pass reads each stage a changed file travels (the watcher thread's
/// flush window, then the dispatch channel and drain, then the refresh
/// worker) before the stage it moves to next; see
/// `AppContext::pending_changes_for_query`. Computing the two values
/// separately would read the dispatch channel first and the flush window
/// second. A flush between those reads moves the file out of the flush
/// window into the channel after the channel was read, so neither read would
/// see it.
pub fn query_pending(
    ctx: &AppContext,
    command: &str,
) -> (bool, Option<crate::context::CallgraphUnappliedChanges>) {
    let watcher_command = is_watcher_query_command(command);
    let callgraph_command = is_callgraph_command(command);
    if !watcher_command && !callgraph_command {
        return (false, None);
    }
    let pending = ctx.pending_changes_for_query(callgraph_command);
    (
        watcher_command && pending.watcher,
        Some(pending.callgraph).filter(|changes| callgraph_command && !changes.is_empty()),
    )
}

/// Above this many files of one kind, a call-graph answer reports how many
/// files are pending in a single gap instead of one gap per file, so a branch
/// switch that touches thousands of files does not bury the answer.
const NAMED_PENDING_FILE_LIMIT: usize = 20;

const VIEW_PENDING_REASON: &str = "callgraph changes not yet installed";
const STORE_PENDING_REASON: &str = "file changes not yet applied to the call graph";

/// Mark a successful call-graph answer as incomplete while edits it may not
/// reflect are still being applied, naming the files. Views report through
/// the same `view_pending` gap the per-checkout query wait uses.
pub fn attach_callgraph_pending_gaps(
    response: &mut Response,
    pending: Option<&crate::context::CallgraphUnappliedChanges>,
) {
    let Some(pending) = pending else {
        return;
    };
    if !response.success {
        return;
    }
    let pending = pending.clone();
    if pending.is_empty() {
        return;
    }
    let Some(data) = response.data.as_object_mut() else {
        return;
    };
    data.insert("complete".into(), Value::Bool(false));
    let mut added = Vec::new();
    for (kind, reason, paths) in [
        ("view_pending", VIEW_PENDING_REASON, &pending.view_paths),
        (
            "callgraph_pending",
            STORE_PENDING_REASON,
            &pending.store_paths,
        ),
    ] {
        if paths.len() > NAMED_PENDING_FILE_LIMIT {
            added.push(serde_json::json!({"kind": kind, "files": paths.len(), "reason": reason}));
        } else {
            added.extend(
                paths
                    .iter()
                    .map(|path| serde_json::json!({"kind": kind, "path": path, "reason": reason})),
            );
        }
    }
    if pending.whole_checkout {
        added.push(serde_json::json!({
            "kind": "view_pending",
            "reason": "checkout changes not yet installed in the call graph",
        }));
    }
    if pending.whole_store {
        added.push(serde_json::json!({
            "kind": "callgraph_pending",
            "reason": STORE_PENDING_REASON,
        }));
    }
    let gaps = data
        .entry("gaps")
        .or_insert_with(|| Value::Array(Vec::new()));
    if let Some(gaps) = gaps.as_array_mut() {
        for gap in added {
            if !gaps.contains(&gap) {
                gaps.push(gap);
            }
        }
    }
}

/// The line a rendered call-graph answer carries while edits are pending,
/// built from the gaps [`attach_callgraph_pending_gaps`] and the per-checkout
/// query wait attach.
pub(crate) fn append_callgraph_pending_notice(text: &mut String, response: &Response) {
    if let Some(notice) = callgraph_pending_notice(response) {
        if !text.contains(&notice) {
            append_trailing_line(text, &notice);
        }
    }
}

fn callgraph_pending_notice(response: &Response) -> Option<String> {
    let gaps = response.data.get("gaps")?.as_array()?;
    let mut named = Vec::new();
    let mut counted = 0u64;
    let mut whole_checkout = false;
    for gap in gaps {
        if !matches!(
            gap.get("kind").and_then(Value::as_str),
            Some("view_pending" | "callgraph_pending")
        ) {
            continue;
        }
        if let Some(path) = gap.get("path").and_then(Value::as_str) {
            if !named.contains(&path) {
                named.push(path);
            }
        } else if let Some(files) = gap.get("files").and_then(Value::as_u64) {
            counted += files;
        } else {
            whole_checkout = true;
        }
    }
    let subject = if whole_checkout {
        "this checkout".to_string()
    } else if counted > 0 || named.len() > 3 {
        format!("{} files", counted + named.len() as u64)
    } else if named.is_empty() {
        return None;
    } else {
        named.join(", ")
    };
    Some(format!(
        "Call graph update pending for {subject}: this answer may show calls from before those edits. Retry shortly for the updated graph."
    ))
}

pub fn attach_watcher_query_gap(response: &mut Response, pending: bool) {
    if !pending || !response.success {
        return;
    }
    if let Some(data) = response.data.as_object_mut() {
        data.insert("complete".into(), Value::Bool(false));
        let gap = serde_json::json!({"kind":"watcher_pending", "reason":"file changes not yet applied to indexed or cached producers"});
        let gaps = data
            .entry("gaps")
            .or_insert_with(|| Value::Array(Vec::new()));
        if let Some(gaps) = gaps.as_array_mut() {
            if !gaps.contains(&gap) {
                gaps.push(gap);
            }
        }
        if let Some(Value::String(text)) = data.get_mut("text") {
            if !text.contains(WATCHER_PENDING_NOTICE) {
                append_trailing_line(text, WATCHER_PENDING_NOTICE);
            }
        }
    }
}

pub(crate) fn append_watcher_query_notice(text: &mut String, response: &Response) {
    if response
        .data
        .get("gaps")
        .and_then(Value::as_array)
        .is_some_and(|gaps| {
            gaps.iter()
                .any(|gap| gap.get("kind").and_then(Value::as_str) == Some("watcher_pending"))
        })
        && !text.contains(WATCHER_PENDING_NOTICE)
    {
        append_trailing_line(text, WATCHER_PENDING_NOTICE);
    }
}

#[cfg(test)]
mod callgraph_pending_tests {
    use super::*;
    use crate::context::CallgraphUnappliedChanges;

    fn pending(view: &[&str], store: &[&str], whole_checkout: bool) -> CallgraphUnappliedChanges {
        CallgraphUnappliedChanges {
            store_paths: store.iter().map(|path| path.to_string()).collect(),
            view_paths: view.iter().map(|path| path.to_string()).collect(),
            whole_checkout,
            whole_store: false,
        }
    }

    fn rendered(response: &Response) -> String {
        let mut text = String::from("tree");
        append_callgraph_pending_notice(&mut text, response);
        text
    }

    #[test]
    fn pending_files_are_named_and_mark_the_answer_incomplete() {
        let mut response = Response::success("cg", serde_json::json!({"children": []}));
        attach_callgraph_pending_gaps(
            &mut response,
            Some(&pending(&["src/view.rs"], &["src/store.rs"], false)),
        );
        assert_eq!(response.data["complete"], false);
        assert_eq!(
            response.data["gaps"],
            serde_json::json!([
                {"kind": "view_pending", "path": "src/view.rs", "reason": VIEW_PENDING_REASON},
                {"kind": "callgraph_pending", "path": "src/store.rs", "reason": STORE_PENDING_REASON},
            ])
        );
        assert!(rendered(&response).ends_with(
            "Call graph update pending for src/view.rs, src/store.rs: this answer may show calls from before those edits. Retry shortly for the updated graph."
        ));
    }

    #[test]
    fn many_pending_files_are_counted_in_one_gap() {
        let paths = (0..=NAMED_PENDING_FILE_LIMIT)
            .map(|index| format!("src/file{index}.rs"))
            .collect::<Vec<_>>();
        let paths = paths.iter().map(String::as_str).collect::<Vec<_>>();
        let mut response = Response::success("cg", serde_json::json!({}));
        attach_callgraph_pending_gaps(&mut response, Some(&pending(&paths, &[], false)));
        assert_eq!(
            response.data["gaps"],
            serde_json::json!([{
                "kind": "view_pending",
                "files": NAMED_PENDING_FILE_LIMIT + 1,
                "reason": VIEW_PENDING_REASON,
            }])
        );
        assert!(rendered(&response).contains(&format!(
            "pending for {} files:",
            NAMED_PENDING_FILE_LIMIT + 1
        )));
    }

    #[test]
    fn a_whole_checkout_publication_is_reported_without_a_path() {
        let mut response = Response::success("cg", serde_json::json!({}));
        attach_callgraph_pending_gaps(&mut response, Some(&pending(&[], &[], true)));
        assert_eq!(response.data["complete"], false);
        assert!(response.data["gaps"][0].get("path").is_none());
        assert!(rendered(&response).contains("pending for this checkout:"));
    }

    #[test]
    fn nothing_pending_or_a_failed_answer_is_left_alone() {
        let mut response = Response::success("cg", serde_json::json!({"children": []}));
        attach_callgraph_pending_gaps(&mut response, None);
        assert_eq!(response.data, serde_json::json!({"children": []}));
        assert_eq!(rendered(&response), "tree");

        let mut failed = Response::error("cg", "symbol_not_found", "missing");
        let before = failed.data.clone();
        attach_callgraph_pending_gaps(&mut failed, Some(&pending(&["a.rs"], &[], false)));
        assert_eq!(failed.data, before);
    }
}
