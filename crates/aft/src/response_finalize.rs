#[path = "alert_render.rs"]
pub mod alert_render;

use std::path::Path;

use crate::context::AppContext;
use crate::protocol::Response;

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

/// Compatibility finalization for direct protocol responses without explicit dispatch-root
/// provenance. Agent-visible responses use [`finalize_response_for_dispatch_root`].
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
    let plane_live = publish_fleet_status(response, ctx, session_id);

    // The pre-tool-call protocol has no dispatch-root provenance or agent-visible text. Keep its
    // legacy envelope seam isolated from terminal agent responses while older direct fixtures
    // migrate to explicit-root finalization.
    if response.data.get("text").is_none()
        && !alert_render::is_excluded_finalization_command(attach_command)
    {
        attach_status_bar_after_publish(response, ctx, plane_live);
    }
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

fn holder_owns_status_bar(plane_live: bool, harness: Option<&crate::harness::Harness>) -> bool {
    plane_live && matches!(harness, Some(crate::harness::Harness::Opencode))
}

/// Publish the retained fleet status segment. Agent-facing status-bar envelope insertion is
/// intentionally absent: reminder rendering below is the only agent response finalizer.
fn publish_fleet_status(
    response: &mut Response,
    ctx: &AppContext,
    session_id: &str,
) -> Option<bool> {
    // Cross-root indexed searches currently suppress fleet status. Remove the private marker
    // before publishing the response so it cannot appear in a response envelope.
    if response
        .data
        .as_object_mut()
        .and_then(|data| data.remove("_aft_suppress_status_bar"))
        .is_some()
    {
        return None;
    }

    let local_counts = ctx.status_bar_counts();
    let harness = ctx.harness_opt();
    let plane_live = ctx.fleet_status_client().is_some_and(|client| {
        let config = ctx.config();
        let Some(project_root) = config.project_root.as_deref() else {
            return false;
        };
        let harness_label = harness
            .as_ref()
            .map(crate::harness::Harness::wire_label)
            .unwrap_or_else(|| "unknown".to_string());
        let aft_text = local_counts
            .as_ref()
            .map(aft_status_segment)
            .unwrap_or_default();
        client.publish(project_root, &harness_label, session_id, &aft_text)
    });
    Some(plane_live)
}

/// Retired envelope helper retained for direct legacy test fixtures. Production finalization
/// calls `publish_fleet_status` and cannot emit this field.
pub fn attach_status_bar(
    response: &mut Response,
    ctx: &AppContext,
    session_id: &str,
    command: &str,
) {
    if alert_render::is_excluded_finalization_command(command) {
        return;
    }
    let plane_live = publish_fleet_status(response, ctx, session_id);
    attach_status_bar_after_publish(response, ctx, plane_live);
}

fn attach_status_bar_after_publish(
    response: &mut Response,
    ctx: &AppContext,
    plane_live: Option<bool>,
) {
    let Some(plane_live) = plane_live else {
        return;
    };
    let harness = ctx.harness_opt();
    if holder_owns_status_bar(plane_live, harness.as_ref()) {
        return;
    }
    let Some(counts) = ctx.status_bar_counts() else {
        return;
    };
    if !ctx.should_emit_status_bar(&counts) {
        return;
    }
    let value = serde_json::json!({
        "errors": counts.errors,
        "warnings": counts.warnings,
        "dead_code": counts.dead_code,
        "unused_exports": counts.unused_exports,
        "duplicates": counts.duplicates,
        "todos": counts.todos,
        "tier2_stale": counts.tier2_stale,
    });
    match response.data.as_object_mut() {
        Some(data) => {
            data.insert("status_bar".to_string(), value);
        }
        None => {
            response.data = serde_json::json!({ "status_bar": value });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{
        aft_status_segment, finalize_response_with_bg_completions, holder_owns_status_bar,
        PendingResponse, PendingResponses,
    };
    use crate::config::Config;
    use crate::context::{AppContext, StatusBarCounts};
    use crate::fleet_status::FleetStatusClient;
    use crate::harness::Harness;
    use crate::parser::TreeSitterProvider;
    use crate::protocol::Response;

    #[test]
    fn live_holder_retires_only_opencode_response_bars() {
        assert_eq!(
            (
                holder_owns_status_bar(true, Some(&Harness::Opencode)),
                holder_owns_status_bar(true, Some(&Harness::Runner)),
            ),
            (true, false)
        );
        assert!(!holder_owns_status_bar(true, Some(&Harness::Pi)));
        assert!(!holder_owns_status_bar(false, Some(&Harness::Opencode)));
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
