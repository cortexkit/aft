//! Track tool replies from reader receipt to the writer's socket handoff.

use std::time::{SystemTime, UNIX_EPOCH};

use super::super::{route_key, DecodedFrame, FrameType};
use super::{Arc, DispatchPathMetrics, Duration, Frame, Instant, RouteChannel, Value};
use crate::blocking_wait::ReplyBudget;
use crate::commands::bash_watch::WatchReplyTimingSink;

const UNANSWERED_TOOL_DEADLINE: Duration = Duration::from_secs(25);
const BASH_WATCH_UNANSWERED_GRACE: Duration = Duration::from_secs(5);

pub(super) struct UnansweredTool {
    received_at: Instant,
    name: String,
    deadline: Option<Instant>,
    warned: bool,
    shell_reply_queued: bool,
    watch_timing: Option<WatchReplyTimingSink>,
}

impl UnansweredTool {
    fn is_shell(&self) -> bool {
        matches!(self.name.as_str(), "bash" | "powershell")
    }

    fn settlement_line(&self, route: RouteChannel, corr: u64, reason: &str) -> String {
        format!(
            "tool_call reply not written name={} channel={} epoch={} corr={corr} reason={reason}",
            self.name, route.channel, route.epoch
        )
    }
}

fn completes_request(frame: &Frame) -> bool {
    matches!(frame.header.ty, FrameType::Response | FrameType::Error)
}

pub(super) fn client_deadline(body: &Value) -> Option<Instant> {
    let deadline_ms = body.get("deadline_ms")?.as_u64()?;
    // Sample both clocks together so time queued before dispatch is not added
    // to the deadline sent by the caller.
    let now = Instant::now();
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64;
    now.checked_add(Duration::from_millis(deadline_ms.saturating_sub(now_ms)))
}

/// Estimate the synchronous watch window for warning purposes only. The
/// watch handler still chooses and enforces its own wait.
pub(in crate::subc) fn bash_watch_warning_wait_ms(
    arguments: &Value,
    worker: bool,
    worker_limit_ms: u64,
    primary_cap_ms: u64,
) -> Option<u64> {
    let arguments = arguments.get("params").unwrap_or(arguments);
    let requested = match arguments
        .get("timeout_ms")
        .or_else(|| arguments.get("timeoutMs"))
    {
        None | Some(Value::Null) => None,
        Some(value) => {
            let requested = value.as_u64().or_else(|| {
                value
                    .as_str()
                    .and_then(|raw| raw.trim().parse::<u64>().ok())
            })?;
            if requested == 0 {
                return None;
            }
            Some(requested)
        }
    };
    let background = arguments
        .get("background")
        .and_then(crate::subc_translate::model_boolean)
        .unwrap_or(false);
    Some(if worker && background {
        worker_limit_ms
    } else if worker {
        requested.unwrap_or(worker_limit_ms).min(worker_limit_ms)
    } else {
        requested
            .unwrap_or(crate::commands::bash_watch::DEFAULT_PRIMARY_WATCH_TIMEOUT_MS)
            .min(primary_cap_ms)
    })
}

impl DispatchPathMetrics {
    #[cfg(test)]
    pub(super) fn tool_received(&self, frame: &Frame, received_at: Instant) {
        let body = serde_json::from_slice::<Value>(&frame.body).ok();
        self.tool_received_value(frame, received_at, body.as_ref());
    }

    pub(in crate::subc) fn tool_received_decoded(&self, decoded: &DecodedFrame) {
        self.tool_received_value(
            &decoded.frame,
            decoded.phase_trace.received_at(),
            decoded.body().ok(),
        );
    }

    fn tool_received_value(&self, frame: &Frame, received_at: Instant, body: Option<&Value>) {
        // bg_events is a held-open subscription, not a unary call awaiting a reply.
        if body.and_then(|body| body.get("op")).and_then(Value::as_str) == Some("bg_events") {
            return;
        }
        let name = body
            .and_then(|body| body.get("name").or_else(|| body.get("op")))
            .and_then(Value::as_str)
            .unwrap_or("undecoded");
        let deadline = match name {
            "bash" => body.map(|body| ReplyBudget::from_envelope(received_at, body).deadline),
            // Dispatch installs the role-specific watch window once config is available.
            "bash_watch" => None,
            _ => received_at.checked_add(UNANSWERED_TOOL_DEADLINE),
        };
        if let Ok(mut tools) = self.unanswered_tools.lock() {
            tools.insert(
                (
                    route_key(frame.header.channel, frame.header.epoch),
                    frame.header.corr,
                ),
                UnansweredTool {
                    received_at,
                    name: name.into(),
                    deadline,
                    warned: false,
                    shell_reply_queued: false,
                    watch_timing: (name == "bash_watch").then(Arc::default),
                },
            );
        }
    }

    pub(in crate::subc) fn set_bash_unanswered_deadline(
        &self,
        route: RouteChannel,
        corr: u64,
        deadline: Instant,
    ) {
        if let Ok(mut tools) = self.unanswered_tools.lock() {
            if let Some(tool) = tools
                .get_mut(&(route, corr))
                .filter(|tool| tool.name == "bash")
            {
                tool.deadline = Some(deadline);
            }
        }
    }

    pub(in crate::subc) fn set_bash_watch_unanswered_wait(
        &self,
        route: RouteChannel,
        corr: u64,
        wait_ms: u64,
    ) {
        if let Ok(mut tools) = self.unanswered_tools.lock() {
            if let Some(tool) = tools
                .get_mut(&(route, corr))
                .filter(|tool| tool.name == "bash_watch")
            {
                let window =
                    Duration::from_millis(wait_ms).saturating_add(BASH_WATCH_UNANSWERED_GRACE);
                tool.deadline = tool.received_at.checked_add(window);
            }
        }
    }

    pub(in crate::subc) fn watch_reply_timing(
        &self,
        route: RouteChannel,
        corr: u64,
    ) -> Option<WatchReplyTimingSink> {
        self.unanswered_tools
            .lock()
            .ok()?
            .get(&(route, corr))?
            .watch_timing
            .clone()
    }

    /// The independent watchdog must still leave evidence if dispatch or writing stalls.
    pub(in crate::subc) fn warn_unanswered_tools(&self) {
        self.warn_unanswered_tools_with(Instant::now(), &|line| log::warn!("{line}"));
    }

    pub(super) fn warn_unanswered_tools_with(&self, now: Instant, log: &dyn Fn(&str)) {
        let Ok(mut tools) = self.unanswered_tools.try_lock() else {
            return;
        };
        for (&(route, corr), tool) in tools.iter_mut() {
            if let Some(deadline) = tool
                .deadline
                .filter(|deadline| !tool.warned && now >= *deadline)
            {
                tool.warned = true;
                log(&format!(
                    "tool_call unanswered past client deadline class name={} channel={} epoch={} corr={corr} elapsed_ms={} deadline_class_ms={}",
                    tool.name,
                    route.channel,
                    route.epoch,
                    now.saturating_duration_since(tool.received_at).as_millis(),
                    deadline.saturating_duration_since(tool.received_at).as_millis(),
                ));
            }
        }
    }

    pub(in crate::subc) fn tool_replied(&self, frame: &Frame, held: bool) {
        self.tool_replied_with(frame, held, Instant::now(), &|line| log::info!("{line}"));
    }

    pub(super) fn tool_replied_with(
        &self,
        frame: &Frame,
        held: bool,
        now: Instant,
        log: &dyn Fn(&str),
    ) {
        if !completes_request(frame) {
            return;
        }
        let route = route_key(frame.header.channel, frame.header.epoch);
        let corr = frame.header.corr;
        let Some(tool) = self
            .unanswered_tools
            .lock()
            .ok()
            .and_then(|mut tools| tools.remove(&(route, corr)))
        else {
            return;
        };
        if held && (tool.is_shell() || tool.name == "bash_watch") {
            // The carrier got a held frame, not the command's result.
            log(&tool.settlement_line(route, corr, "held"));
        } else if tool.is_shell() || tool.name == "bash_watch" {
            let mut line = format!(
                "tool_call reply written name={} channel={} epoch={} corr={corr} total_ms={} destination=subc_daemon",
                tool.name, route.channel, route.epoch,
                now.saturating_duration_since(tool.received_at).as_millis(),
            );
            if let Some(timing) = tool.watch_timing.as_ref().and_then(|sink| sink.get()) {
                timing.append_log_fields(&mut line);
            }
            log(&line);
        }
    }

    pub(in crate::subc) fn mark_shell_reply_queued(&self, frame: &Frame) {
        if completes_request(frame) {
            let key = (
                route_key(frame.header.channel, frame.header.epoch),
                frame.header.corr,
            );
            if let Ok(mut tools) = self.unanswered_tools.lock() {
                if let Some(tool) = tools.get_mut(&key).filter(|tool| tool.is_shell()) {
                    tool.shell_reply_queued = true;
                }
            }
        }
    }

    pub(in crate::subc) fn drop_shell_reply(&self, route: RouteChannel, corr: u64, reason: &str) {
        self.drop_shell_replies_with(
            reason,
            |key, tool| key == (route, corr) && !tool.shell_reply_queued,
            &|line| log::info!("{line}"),
        );
    }

    pub(in crate::subc) fn close_shell_reply_route(&self, route: RouteChannel) {
        // Results already queued may still be written before the route's Goodbye.
        self.drop_shell_replies_with(
            "route_closed",
            |key, tool| key.0 == route && !tool.shell_reply_queued,
            &|line| log::info!("{line}"),
        );
    }

    pub(in crate::subc) fn close_shell_reply_connection(&self, writer_failed: bool) {
        if writer_failed {
            self.drop_shell_replies_with("writer_failed", |_, _| true, &|line| {
                log::warn!("{line}")
            });
        } else {
            self.drop_shell_replies_with("connection_closed", |_, _| true, &|line| {
                log::info!("{line}")
            });
        }
    }

    fn drop_shell_replies_with(
        &self,
        reason: &str,
        matches: impl Fn((RouteChannel, u64), &UnansweredTool) -> bool,
        log: &dyn Fn(&str),
    ) {
        if let Ok(mut tools) = self.unanswered_tools.lock() {
            tools.retain(|&(route, corr), tool| {
                if !tool.is_shell() || !matches((route, corr), tool) {
                    return true;
                }
                log(&tool.settlement_line(route, corr, reason));
                false
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::bash_watch::WatchReplyTiming;
    use std::cell::RefCell;

    fn reply(name: &str, corr: u64, metrics: &DispatchPathMetrics, received: Instant) -> Frame {
        let mut frame = Frame::build_with_version(
            crate::subc::PROTOCOL_VERSION,
            FrameType::Request,
            crate::subc::control_flags(),
            15,
            2,
            corr,
            serde_json::to_vec(&serde_json::json!({"name":name})).unwrap(),
        )
        .unwrap();
        metrics.tool_received(&frame, received);
        frame.header.ty = FrameType::Response;
        frame
    }

    #[test]
    fn shell_reply_tracking_distinguishes_queued_written_held_and_dropped() {
        let metrics = DispatchPathMetrics::new();
        let now = Instant::now();
        let route = route_key(15, 2);
        let written = reply("bash", 1, &metrics, now);
        let held = reply("powershell", 2, &metrics, now);
        let failed = reply("bash", 3, &metrics, now);
        let cancelled = reply("powershell", 4, &metrics, now);
        let _unbound = reply("bash", 5, &metrics, now);
        let lines = RefCell::new(Vec::new());
        let sink = |line: &str| lines.borrow_mut().push(line.to_owned());

        metrics.mark_shell_reply_queued(&written);
        metrics.mark_shell_reply_queued(&failed);
        metrics.tool_replied_with(&held, true, now, &sink);
        metrics.drop_shell_replies_with(
            "cancelled",
            |key, _| key == (route, cancelled.header.corr),
            &sink,
        );
        metrics.drop_shell_replies_with(
            "route_closed",
            |key, tool| key.0 == route && !tool.shell_reply_queued,
            &sink,
        );
        metrics.tool_replied_with(&written, false, now, &sink);
        metrics.drop_shell_replies_with("writer_failed", |_, _| true, &sink);
        metrics.tool_replied_with(&failed, false, now, &sink);
        assert_eq!(*lines.borrow(), [
            "tool_call reply not written name=powershell channel=15 epoch=2 corr=2 reason=held",
            "tool_call reply not written name=powershell channel=15 epoch=2 corr=4 reason=cancelled",
            "tool_call reply not written name=bash channel=15 epoch=2 corr=5 reason=route_closed",
            "tool_call reply written name=bash channel=15 epoch=2 corr=1 total_ms=0 destination=subc_daemon",
            "tool_call reply not written name=bash channel=15 epoch=2 corr=3 reason=writer_failed",
        ]);
        assert!(metrics.unanswered_tools.lock().unwrap().is_empty());
    }

    #[test]
    fn shell_reply_tracking_logs_bash_watch_phase_times_only_on_reply_write() {
        let metrics = DispatchPathMetrics::new();
        let received = Instant::now();
        let frame = reply("bash_watch", 4, &metrics, received);
        let timing = metrics.watch_reply_timing(route_key(15, 2), 4).unwrap();
        assert!(timing.set(WatchReplyTiming::default()).is_ok());
        let lines = RefCell::new(Vec::new());
        let sink = |line: &str| lines.borrow_mut().push(line.to_owned());
        let now = received + Duration::from_millis(6_423);
        metrics.tool_replied_with(&frame, false, now, &sink);
        metrics.tool_replied_with(&frame, false, now, &sink);
        assert_eq!(*lines.borrow(), [
            "tool_call reply written name=bash_watch channel=15 epoch=2 corr=4 total_ms=6423 destination=subc_daemon recovery_ms=0 scan_ms=0 status_ms=0 preview_ms=0 format_ms=0",
        ]);
    }
}
