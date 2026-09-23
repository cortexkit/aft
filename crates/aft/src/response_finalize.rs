#[path = "alert_render.rs"]
pub mod alert_render;
#[path = "repeat_breaker.rs"]
pub mod repeat_breaker;

use std::path::Path;

use serde_json::Value;

use crate::context::AppContext;
use crate::protocol::Response;

pub fn append_repeat_breaker_reminder(
    text: &mut String,
    session_id: &str,
    intervention: &repeat_breaker::RepeatIntervention,
) {
    let count = intervention.count;
    let span_seconds = intervention.span.as_secs();
    let ordinal = ordinal(count);
    // The output signal controls only the diagnosis: stable output says the
    // call returns nothing new, while changing output names common timestamp
    // drift. Both variants point legitimate waiting toward a background watch
    // instead of treating output churn as progress.
    let instruction = if repeat_breaker::escalation_starts_at(count) {
        "The turn must end now with no further tool call. If you are waiting on a task or CI run, use a background task with a watch (or the background handle you already hold) and end the turn. If you are already watching, let the watch return before calling again."
    } else {
        "If you are waiting on a task or CI run, use a background task with a watch (or the background handle you already hold) and end the turn. If you are already watching, let the watch return before calling again."
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
    if allow_bg_completions {
        attach_bg_completions(response, ctx, session_id, attach_command);
    }
    let publish = publish_fleet_status(response, ctx, session_id);
    if !response.data.get("text").is_some_and(Value::is_string) {
        return;
    }
    let Some(line) = status_bar_line(ctx, publish, attach_command) else {
        return;
    };
    if let Some(Value::String(text)) = response.data.get_mut("text") {
        append_trailing_line(text, &line);
    }
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
    if allow_bg_completions {
        attach_bg_completions(response, ctx, session_id, attach_command);
    }
    let publish = publish_fleet_status(response, ctx, session_id);
    if let Some(line) = status_bar_line(ctx, publish, attach_command) {
        append_trailing_line(text, &line);
    }
}

fn append_trailing_line(text: &mut String, line: &str) {
    if !text.is_empty() {
        text.push_str(if text.ends_with('\n') { "\n" } else { "\n\n" });
    }
    text.push_str(line);
}

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
    if allow_bg_completions {
        attach_bg_completions(response, ctx, session_id, attach_command);
    }
    let _ = publish_fleet_status(response, ctx, session_id);
    attach_alert_block(response, alerts, session_id, dispatch_root, attach_command);
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
    pub fn register(&mut self, pending: PendingResponse) {
        self.entries.retain(|entry| {
            let keep = entry.request_id != pending.request_id;
            if !keep {
                if let Some(cancellation) = &entry.cancellation {
                    cancellation.request_cancel();
                }
            }
            keep
        });
        self.entries.push(pending);
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

    pub fn drain_on_shutdown(&mut self) {
        for pending in self.entries.drain(..) {
            if let Some(cancellation) = &pending.cancellation {
                cancellation.request_cancel();
            }
        }
    }

    /// Resolve shutdown-aware entries before removing them from the registry.
    /// Entries without a shutdown terminal retain the legacy drop behavior.
    pub fn drain_on_shutdown_with(&mut self, ctx: &AppContext) -> Vec<ResolvedPending> {
        self.entries
            .drain(..)
            .filter_map(|mut pending| {
                if let Some(cancellation) = &pending.cancellation {
                    cancellation.request_cancel();
                }
                let response = (pending.on_shutdown.as_mut()?)(ctx);
                Some(ResolvedPending {
                    response,
                    session_id: pending.session_id,
                    attach_command: pending.attach_command,
                })
            })
            .collect()
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
    let stale_mark = if counts.tier2_stale { "~" } else { "" };
    // Self-labeled per the fleet status-line format ruling (2026-08-17): the
    // holder composes segments label-free and joins module boundaries with a
    // bullet, so each publisher's text must carry its own leading label.
    format!(
        "AFT E{} W{} | {}D{} U{} C{} | T{}",
        counts.errors,
        counts.warnings,
        stale_mark,
        counts.dead_code,
        counts.unused_exports,
        counts.duplicates,
        counts.todos
    )
}

/// Renders the agent-facing bar from the omission-preserving values. A category with no
/// trustworthy value yet shows `?` (as the OpenCode footer does) rather than a clean `0`.
fn agent_status_bar(values: &crate::context::StatusBarCountValues) -> String {
    fn count(value: Option<usize>) -> String {
        value.map_or_else(|| "?".to_string(), |value| value.to_string())
    }
    let stale_mark = if values.tier2_stale { "~" } else { "" };
    format!(
        "[AFT E{} W{} | {}D{} U{} C{} | T{}]",
        count(values.errors),
        count(values.warnings),
        stale_mark,
        count(values.dead_code),
        count(values.unused_exports),
        count(values.duplicates),
        count(values.todos)
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
    let aft_text = ctx
        .status_bar_counts()
        .as_ref()
        .map(aft_status_segment)
        .unwrap_or_default();
    client.publish(project_root, &harness_label, session_id, &aft_text);
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
    let values = ctx.status_bar_count_values();
    let nothing_known = [
        values.errors,
        values.warnings,
        values.dead_code,
        values.unused_exports,
        values.duplicates,
        values.todos,
    ]
    .iter()
    .all(Option::is_none);
    if nothing_known || !ctx.should_emit_status_bar(&values) {
        return None;
    }
    Some(agent_status_bar(&values))
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
            warnings: None,
            dead_code: Some(21),
            unused_exports: Some(0),
            duplicates: Some(13),
            todos: None,
            tier2_stale: true,
        };
        assert_eq!(agent_status_bar(&values), "[AFT E? W? | ~D21 U0 C13 | T?]");
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
        ctx.update_status_bar_tier2(Some(1), Some(2), Some(3), Some(4), false);

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
        assert_eq!(first, "file contents\n\n[AFT E? W? | D1 U2 C3 | T4]");

        let mut unchanged = String::from("file contents");
        finalize_tool_response(&mut response, &mut unchanged, &ctx, "s", "read", false);
        assert_eq!(unchanged, "file contents");

        let mut excluded = String::from("drained");
        ctx.update_status_bar_tier2(Some(9), Some(2), Some(3), Some(4), false);
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
        assert_eq!(
            standalone.data["text"],
            "listing\n\n[AFT E? W? | D9 U2 C3 | T4]"
        );
    }

    #[test]
    fn cross_root_marker_suppresses_the_bar_and_is_stripped() {
        let ctx = standalone_ctx();
        ctx.update_status_bar_tier2(Some(1), Some(2), Some(3), Some(4), false);
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
        ctx.update_status_bar_tier2(Some(21), Some(12), Some(13), Some(14), false);
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
            dead_code: 331,
            unused_exports: 221,
            duplicates: 1159,
            todos: 8,
            tier2_stale: false,
        };
        assert_eq!(
            format!("[{}]", aft_status_segment(&counts)),
            "[AFT E2 W5 | D331 U221 C1159 | T8]"
        );
    }

    #[test]
    fn shutdown_delivery_emits_terminal_before_removing_entry() {
        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        let mut pending = PendingResponses::default();
        pending.register(PendingResponse {
            request_id: "inspect-shutdown".to_string(),
            session_id: String::new(),
            attach_command: String::new(),
            poll: Box::new(|_| None),
            cancellation: None,
            on_shutdown: Some(Box::new(|_| {
                Response::error("inspect-shutdown", "daemon_shutdown", "shutdown")
            })),
        });

        let resolved = pending.drain_on_shutdown_with(&ctx);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].response.id, "inspect-shutdown");
        assert!(pending.is_empty());
    }

    #[test]
    fn published_segment_stale_marker_bytes_are_self_labeled() {
        let counts = StatusBarCounts {
            dead_code: 10,
            tier2_stale: true,
            ..StatusBarCounts::default()
        };
        assert_eq!(
            format!("[{}]", aft_status_segment(&counts)),
            "[AFT E0 W0 | ~D10 U0 C0 | T0]"
        );
    }
}
