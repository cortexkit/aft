//! `bash_watch`, served by the module itself.
//!
//! The OpenCode and Pi plugins implement `bash_watch` in their own process:
//! they poll `bash_status` and scan the task's output. A consumer that builds
//! its tool surface from AFT's catalog (Broca) has no such loop, so the
//! catalog's `worker` preset serves this module-side tool instead, with the
//! same arguments and the same semantics the plugin tool has.
//!
//! The wait never holds an executor worker: the executor job only validates
//! the call and returns a deferred response. A dedicated thread then waits on
//! the task's registry, which is shared and thread-safe, and the transport
//! loop picks up its result. A cancelled call or a module drain cancels that
//! thread through the job's cancellation.

use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::bash_background::persistence::{BgMode, TaskArtifact};
use crate::bash_background::registry::BgTaskSnapshot;
use crate::bash_background::{BgTaskRegistry, BgTaskStatus};
use crate::commands::bash_orchestrate::{
    format_wait_limit, kill_deadline_sentence, worker_kill_deadline_within_handoff_margin,
};
use crate::commands::bash_status::{format_erased_task_message, format_unknown_task_message};
use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};
use crate::response_finalize::{DispatchOutcome, PendingResponse};

/// The wait a primary session's watch gets when it passes no timeout. Mirrors
/// `DEFAULT_PRIMARY_WATCH_TIMEOUT_MS` in `packages/aft-bridge/src/bash-hints.ts`.
pub(crate) const DEFAULT_PRIMARY_WATCH_TIMEOUT_MS: u64 = 30_000;
/// The largest timeout a worker may pass, as in the plugins' tool schema.
pub(crate) const MAX_WATCH_TIMEOUT_MS: u64 = 1_800_000;
/// How much recent output a regex watch keeps for matching across reads.
const REGEX_SCAN_WINDOW_BYTES: usize = 64 * 1024;
/// Most lines of a still-running command's output shown to a worker.
const WORKER_OUTPUT_TAIL_LINES: usize = 20;
/// Output preview a watch result carries for a finished task.
const PREVIEW_BYTES: usize = crate::bash_background::output::RUNNING_OUTPUT_PREVIEW_BYTES;

#[derive(Debug, Default, Deserialize)]
struct BashWatchParams {
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default)]
    pattern: Option<Value>,
    #[serde(default)]
    background: Option<Value>,
    #[serde(default)]
    timeout_ms: Option<Value>,
    /// Accepted for the plugin tool's shape; only an async watch reads it,
    /// and the module always waits synchronously (see [`handle_deferred`]).
    #[serde(default)]
    #[allow(dead_code)]
    once: Option<Value>,
}

/// What the watch waits for besides the task's exit.
enum WaitPattern {
    Substring(regex::bytes::Regex, usize),
    Regex(regex::bytes::Regex),
}

impl WaitPattern {
    fn regex(&self) -> &regex::bytes::Regex {
        match self {
            Self::Substring(regex, _) | Self::Regex(regex) => regex,
        }
    }

    /// Where the scan buffer may be cut so a match spanning two reads is
    /// still found: a substring keeps one byte less than its length, a regex
    /// keeps a 64 KB window.
    fn keep_from(&self, buffer_len: usize) -> usize {
        let keep = match self {
            Self::Substring(_, len) => len.saturating_sub(1),
            Self::Regex(_) => REGEX_SCAN_WINDOW_BYTES,
        };
        buffer_len.saturating_sub(keep)
    }
}

/// Why a watch ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitReason {
    Matched,
    Exited,
    Timeout,
}

struct Waited {
    reason: WaitReason,
    elapsed_ms: u64,
    limit_ms: u64,
    matched: Option<(String, u64, Option<&'static str>)>,
}

fn wait_limit_outcome(
    limit_reached: bool,
    terminal: bool,
    worker: bool,
    kill_remaining: Option<Duration>,
) -> Option<WaitReason> {
    if terminal {
        Some(WaitReason::Exited)
    } else if limit_reached
        && !(worker && worker_kill_deadline_within_handoff_margin(kill_remaining))
    {
        Some(WaitReason::Timeout)
    } else {
        None
    }
}

/// One stream's scan state: bytes not yet ruled out, and where they start in
/// the stream.
#[derive(Default)]
struct StreamScan {
    buffer: Vec<u8>,
    base: u64,
    next: u64,
}

fn parse_params(req: &RawRequest) -> Result<BashWatchParams, String> {
    let raw = req
        .params
        .get("params")
        .cloned()
        .unwrap_or_else(|| req.params.clone());
    serde_json::from_value(raw).map_err(|error| format!("bash_watch: invalid params: {error}"))
}

fn parse_pattern(value: Option<&Value>) -> Result<Option<WaitPattern>, Response> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => {
            let regex = regex::bytes::Regex::new(&regex::escape(text))
                .expect("an escaped literal is a valid regex");
            Ok(Some(WaitPattern::Substring(regex, text.len())))
        }
        Some(Value::Object(object)) => match object.get("regex").and_then(Value::as_str) {
            Some(source) => regex::bytes::RegexBuilder::new(source)
                .multi_line(true)
                .build()
                .map(|regex| Some(WaitPattern::Regex(regex)))
                .map_err(|error| {
                    Response::error("", "invalid_request", format!("invalid_regex: {error}"))
                }),
            None => Err(Response::error(
                "",
                "invalid_request",
                "bash_watch: pattern must be a string or { regex: string }",
            )),
        },
        Some(_) => Err(Response::error(
            "",
            "invalid_request",
            "bash_watch: pattern must be a string or { regex: string }",
        )),
    }
}

fn with_id(mut response: Response, id: &str) -> Response {
    response.id = id.to_string();
    response
}

/// The wait this call gets, in milliseconds, or the refusal of its timeout.
///
/// A delegated worker cannot be woken once its turn ends, so without a timeout
/// it waits up to the worker wait limit (`bash.worker_wait_max_ms`), and an
/// explicit timeout cannot extend that wait beyond the configured limit. A primary
/// keeps the short default and the `bash.watch_sync_max_ms` cap. A worker's
/// `background: true` keeps the async request's meaning ("tell me when it's
/// done") as a wait up to the worker limit, exactly like the plugins when
/// they turn a worker's async watch into a sync one.
fn effective_wait_ms(
    params: &BashWatchParams,
    worker: bool,
    worker_limit_ms: u64,
    primary_cap_ms: u64,
) -> Result<u64, String> {
    let max = if worker {
        MAX_WATCH_TIMEOUT_MS
    } else {
        primary_cap_ms
    };
    let requested = match &params.timeout_ms {
        None | Some(Value::Null) => None,
        Some(value) => {
            let parsed = value
                .as_u64()
                .or_else(|| {
                    value
                        .as_str()
                        .and_then(|raw| raw.trim().parse::<u64>().ok())
                })
                .filter(|ms| (1..=max).contains(ms));
            match parsed {
                Some(ms) => Some(ms),
                None if worker => {
                    return Err(format!("timeoutMs must be an integer from 1 to {max}"))
                }
                None => {
                    return Err(format!(
                        "timeoutMs must be an integer from 1 to {max} (bash.watch_sync_max_ms)"
                    ))
                }
            }
        }
    };
    let background = params
        .background
        .as_ref()
        .and_then(crate::subc_translate::model_boolean)
        .unwrap_or(false);
    Ok(if worker && background {
        worker_limit_ms
    } else if worker {
        requested.unwrap_or(worker_limit_ms).min(worker_limit_ms)
    } else {
        requested
            .unwrap_or(DEFAULT_PRIMARY_WATCH_TIMEOUT_MS)
            .min(primary_cap_ms)
    })
}

/// Delay before the next look at the task, never past the deadline. The first
/// seconds look often so a short command or a quick pattern is seen at once;
/// later the interval grows. Mirrors `watchPollDelayMs` in the plugins.
fn poll_delay(elapsed: Duration) -> Duration {
    Duration::from_millis(match elapsed.as_millis() {
        0..=4_999 => 100,
        5_000..=29_999 => 250,
        30_000..=119_999 => 500,
        _ => 1_000,
    })
}

/// Validates a `bash_watch` call and defers its wait to a dedicated thread.
///
/// The module always waits synchronously: a catalog consumer has no channel
/// that would deliver an async watch's notification, so `background` only
/// changes how long a worker waits (see [`effective_wait_ms`]).
pub fn handle_deferred(req: &RawRequest, ctx: Arc<AppContext>) -> DispatchOutcome {
    let id = req.id.clone();
    let params = match parse_params(req) {
        Ok(params) => params,
        Err(message) => {
            return DispatchOutcome::Immediate(Response::error(&id, "invalid_request", message))
        }
    };
    let Some(task_id) = params.task_id.clone().filter(|task| !task.is_empty()) else {
        return DispatchOutcome::Immediate(Response::error(
            &id,
            "invalid_request",
            "bash_watch: missing taskId",
        ));
    };
    let pattern = match parse_pattern(params.pattern.as_ref()) {
        Ok(pattern) => pattern,
        Err(response) => return DispatchOutcome::Immediate(with_id(response, &id)),
    };
    let worker = req.worker_session();
    let worker_limit_ms = crate::commands::bash_orchestrate::worker_wait_max_ms(&ctx);
    let primary_cap_ms = ctx.config().bash.watch_sync_max_ms;
    let limit_ms = match effective_wait_ms(&params, worker, worker_limit_ms, primary_cap_ms) {
        Ok(limit) => limit,
        Err(message) => {
            return DispatchOutcome::Immediate(Response::error(&id, "invalid_request", message))
        }
    };
    let session = req.session().to_string();
    let registry = ctx.bash_background().clone();
    if registry.has_erased_watch_reference(&task_id) {
        return DispatchOutcome::Immediate(Response::error(
            &id,
            "task_erased",
            format_erased_task_message(&task_id),
        ));
    }
    let storage_dir = crate::bash_background::task_storage_dir(&ctx);
    let project_root = ctx.config().project_root.clone();
    let principal = crate::sandbox_spawn::current_authenticated_principal();
    let harness = crate::bash_background::route_harness()
        .or_else(|| ctx.config().harness.clone())
        .map(|harness| harness.storage_segment());
    let cancellation = crate::executor::current_job_cancellation()
        .unwrap_or_else(crate::executor::JobCancellation::new);
    let worker_cancellation = cancellation.clone();
    let (tx, rx) = crate::response_finalize::pending_response_channel();
    let thread_id = id.clone();
    std::thread::spawn(move || {
        crate::sandbox_spawn::with_authenticated_principal(principal, || {
            // Configure acknowledges the route before its replay tail finishes.
            // Resolve the durable record before starting the wait, off executor
            // slots, and without replaying unrelated tasks or relaxing the session.
            if registry.observed_status(&task_id, &session, 0).is_none() {
                registry.set_worker_wait_window(Duration::from_millis(worker_limit_ms));
                if let Err(error) = registry.recover_watch_task(
                    &storage_dir,
                    &session,
                    &task_id,
                    project_root.as_deref(),
                    harness.as_deref(),
                ) {
                    let _ = tx.send(Response::error(&thread_id, "task_recovery_failed", error));
                    return;
                }
            }
            if registry.observed_status(&task_id, &session, 0).is_none() {
                let _ = tx.send(Response::error(
                    &thread_id,
                    "task_not_found",
                    format_unknown_task_message(&task_id),
                ));
                return;
            }
            let watch = WatchJob {
                registry,
                task_id,
                session,
                pattern,
                limit_ms,
                worker,
                worker_limit_ms,
                primary_cap_ms,
            };
            let Some(waited) = watch.wait(&worker_cancellation) else {
                // Cancelled: the transport already answered the call.
                return;
            };
            // The task was already resolved under this route's identity. Do not
            // re-adopt another session's task through status's relaxed fallback if
            // the record disappears while the watch is waiting.
            let snapshot =
                watch
                    .registry
                    .observed_status(&watch.task_id, &watch.session, PREVIEW_BYTES);
            let response = match snapshot {
                Some(snapshot) => watch.reply(&thread_id, snapshot, &waited),
                None => Response::error(
                    &thread_id,
                    "task_not_found",
                    format_unknown_task_message(&watch.task_id),
                ),
            };
            let _ = tx.send(response);
        })
    });

    let mut settled = false;
    let disconnect_id = id.clone();
    DispatchOutcome::Deferred(PendingResponse::from_receiver(
        id,
        req.session().to_string(),
        "bash_watch".to_string(),
        rx,
        move |_, completion| {
            if settled {
                return None;
            }
            match completion {
                Ok(response) => {
                    settled = true;
                    Some(response)
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    settled = true;
                    Some(Response::error(
                        &disconnect_id,
                        "watch_interrupted",
                        "bash_watch: the watch ended without a result; the task keeps running. Call bash_watch again to keep waiting.",
                    ))
                }
                Err(mpsc::TryRecvError::Empty) => None,
            }
        },
    ).with_cancellation(cancellation))
}

struct WatchJob {
    registry: BgTaskRegistry,
    task_id: String,
    session: String,
    pattern: Option<WaitPattern>,
    limit_ms: u64,
    worker: bool,
    worker_limit_ms: u64,
    primary_cap_ms: u64,
}

impl WatchJob {
    /// Waits until the task exits, the pattern matches, or the limit passes.
    /// `None` when the call was cancelled first.
    fn wait(&self, cancellation: &crate::executor::JobCancellation) -> Option<Waited> {
        let started = Instant::now();
        let deadline = started + Duration::from_millis(self.limit_ms);
        let mut stdout = StreamScan::default();
        let mut stderr = StreamScan::default();
        let waited = |reason, matched| Waited {
            reason,
            elapsed_ms: started.elapsed().as_millis() as u64,
            limit_ms: self.limit_ms,
            matched,
        };
        loop {
            if cancellation.cancel_already_requested() {
                return None;
            }
            // A worker waiting on its task keeps the task's default hard kill
            // at least one worker wait limit away, as every other worker wait
            // does (see `BgTaskRegistry::renew_hard_kill`).
            if self.worker {
                self.registry.renew_hard_kill(
                    &self.task_id,
                    &self.session,
                    Duration::from_millis(self.worker_limit_ms),
                );
            }
            let snapshot = self
                .registry
                .observed_status(&self.task_id, &self.session, 0);
            let terminal = snapshot
                .as_ref()
                .is_none_or(|snapshot| snapshot.info.status.is_terminal());
            if let (Some(pattern), Some(snapshot)) = (&self.pattern, &snapshot) {
                if let Some(found) = self.scan(pattern, snapshot, &mut stdout, &mut stderr) {
                    return Some(waited(WaitReason::Matched, Some(found)));
                }
            }
            let now = Instant::now();
            let kill_remaining = if self.worker {
                self.registry
                    .hard_kill_remaining(&self.task_id, &self.session)
            } else {
                None
            };
            if let Some(reason) =
                wait_limit_outcome(now >= deadline, terminal, self.worker, kill_remaining)
            {
                return Some(waited(reason, None));
            }
            let pause = if now >= deadline {
                // The command is about to be killed; keep polling until the
                // terminal timeout result is visible instead of handing it off.
                poll_delay(started.elapsed())
            } else {
                poll_delay(started.elapsed()).min(deadline - now)
            };
            if cancellation.wait_for_cancellation(pause) {
                return None;
            }
        }
    }

    /// Reads the task's new output and looks for the pattern in it. The
    /// streams are scanned apart, so a stdout tail and a stderr head never
    /// form a match together; stdout is checked first.
    fn scan(
        &self,
        pattern: &WaitPattern,
        snapshot: &BgTaskSnapshot,
        stdout: &mut StreamScan,
        stderr: &mut StreamScan,
    ) -> Option<(String, u64, Option<&'static str>)> {
        let pty = snapshot.info.mode == BgMode::Pty;
        let streams: [(&mut StreamScan, TaskArtifact, Option<&'static str>); 2] = if pty {
            [
                (stdout, TaskArtifact::Pty, None),
                (stderr, TaskArtifact::Stderr, None),
            ]
        } else {
            [
                (stdout, TaskArtifact::Stdout, Some("stdout")),
                (stderr, TaskArtifact::Stderr, Some("stderr")),
            ]
        };
        for (index, (scan, artifact, label)) in streams.into_iter().enumerate() {
            if pty && index == 1 {
                break;
            }
            let Ok((bytes, next)) = self.registry.read_artifact_range(
                &self.task_id,
                &self.session,
                artifact,
                scan.next,
            ) else {
                continue;
            };
            if bytes.is_empty() {
                continue;
            }
            if scan.buffer.is_empty() {
                scan.base = scan.next;
            }
            scan.next = next.max(scan.next + bytes.len() as u64);
            scan.buffer.extend_from_slice(&bytes);
            if let Some(found) = pattern.regex().find(&scan.buffer) {
                return Some((
                    String::from_utf8_lossy(found.as_bytes()).into_owned(),
                    scan.base + found.start() as u64,
                    label,
                ));
            }
            let cut = pattern.keep_from(scan.buffer.len());
            if cut > 0 {
                scan.buffer.drain(..cut);
                scan.base += cut as u64;
            }
        }
        None
    }

    /// The watch result, rendered the way the plugins render it for the same
    /// role, plus the task's own kill deadline.
    fn reply(&self, id: &str, snapshot: BgTaskSnapshot, waited: &Waited) -> Response {
        let text = self.render(&snapshot, waited);
        let mut data = json!(snapshot);
        data["waited"] = json!({
            "reason": match waited.reason {
                WaitReason::Matched => "matched",
                WaitReason::Exited => "exited",
                WaitReason::Timeout => "timeout",
            },
            "elapsed_ms": waited.elapsed_ms,
            "limit_ms": waited.limit_ms,
        });
        if let Some((text, offset, stream)) = &waited.matched {
            data["waited"]["match"] = json!(text);
            data["waited"]["match_offset"] = json!(offset);
            if let Some(stream) = stream {
                data["waited"]["match_stream"] = json!(stream);
            }
        }
        data["output"] = json!(text);
        Response::success(id, data)
    }

    fn render(&self, snapshot: &BgTaskSnapshot, waited: &Waited) -> String {
        let status = status_name(&snapshot.info.status);
        let running = !snapshot.info.status.is_terminal();
        let exit = snapshot
            .exit_code
            .map(|code| format!(" (exit {code})"))
            .unwrap_or_default();
        let duration = snapshot
            .info
            .duration_ms
            .map(|ms| format!(" {}s", (ms as f64 / 1000.0).round() as u64))
            .unwrap_or_default();
        let mut text = format!("Task {}: {status}{exit}{duration}", self.task_id);
        // Only a primary's watch is bounded by the configured cap.
        let cap_note = if !self.worker && waited.limit_ms >= self.primary_cap_ms {
            ", the bash.watch_sync_max_ms cap"
        } else {
            ""
        };
        let waited_text = format!(
            "Waited {}ms (limit {}ms{cap_note})",
            waited.elapsed_ms, waited.limit_ms
        );
        match waited.reason {
            WaitReason::Matched => {
                let (matched, offset, stream) = waited
                    .matched
                    .as_ref()
                    .expect("a matched watch carries its match");
                let stream = stream.map(|s| format!(" in {s}")).unwrap_or_default();
                text.push_str(&format!(
                    "\n{waited_text}; matched {}{stream} at offset {offset}.",
                    Value::String(matched.clone())
                ));
            }
            WaitReason::Timeout if self.worker => {
                text.push_str(&format!(
                    "\n{waited_text}; timeout reached without match. {}",
                    self.worker_still_running(snapshot, waited.elapsed_ms)
                ));
            }
            WaitReason::Timeout => {
                text.push_str(&format!(
                    "\n{waited_text}; timeout reached without match. The command is still running; this is not a failure. Watch again, do other work, or end your turn: the completion reminder wakes you."
                ));
            }
            WaitReason::Exited => {
                let exit = snapshot
                    .exit_code
                    .map(|code| format!(", exit {code}"))
                    .unwrap_or_default();
                text.push_str(&format!("\n{waited_text}; task exited ({status}{exit})."));
            }
        }
        if !running && !snapshot.output_preview.is_empty() {
            text.push('\n');
            text.push_str(&snapshot.output_preview);
        }
        let deadline = self.kill_deadline_text(snapshot);
        if !deadline.is_empty() {
            text.push('\n');
            text.push_str(&deadline);
        }
        text
    }

    /// Mirrors `workerWatchStillRunning` in the plugins: the deadline that
    /// passed belongs to the watch, not the command, so the worker is told the
    /// command is still running, how long it has run, what it printed last,
    /// and how to wait again or stop it.
    fn worker_still_running(&self, snapshot: &BgTaskSnapshot, waited_ms: u64) -> String {
        let task_id = &self.task_id;
        let ran = snapshot
            .info
            .duration_ms
            .map(|ms| format!(" It has run for {}.", format_wait_limit(ms)))
            .unwrap_or_default();
        let tail = output_tail(&snapshot.output_preview);
        let output = if tail.is_empty() {
            "No output yet.".to_string()
        } else {
            format!("Recent output:\n{tail}")
        };
        format!(
            "The command is still running after {} of watching; this is not a failure.{ran} Call bash_watch({{ taskId: \"{task_id}\" }}) again to keep waiting (without timeoutMs a watch waits up to the worker wait limit, then reports it is still running), or bash_kill({{ taskId: \"{task_id}\" }}) if it should have finished by now. Don't report a result until it finishes.\n{output}",
            format_wait_limit(waited_ms)
        )
    }

    /// Mirrors `taskKillDeadlineText` in the plugins: how the task was killed
    /// when a hard limit fired, or its own kill deadline while it runs.
    fn kill_deadline_text(&self, snapshot: &BgTaskSnapshot) -> String {
        if snapshot.info.status == BgTaskStatus::TimedOut {
            return match snapshot.info.status_reason.as_deref() {
                Some(reason) if !reason.is_empty() => format!("The task was {reason}."),
                _ => "The task was killed by its time limit (exit 124).".to_string(),
            };
        }
        if snapshot.info.status.is_terminal() {
            return String::new();
        }
        kill_deadline_sentence(
            self.registry
                .hard_kill_deadline(&self.task_id, &self.session),
            snapshot.info.started_at,
            self.worker,
        )
    }
}

fn status_name(status: &BgTaskStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}

/// The last lines of `output`, so a worker can judge whether a command is stuck.
fn output_tail(output: &str) -> String {
    let trimmed = output.trim_end();
    let lines: Vec<&str> = trimmed.split('\n').collect();
    let start = lines.len().saturating_sub(WORKER_OUTPUT_TAIL_LINES);
    lines[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bash_background::persistence::{create_task_layout, write_task_at, PersistedTask};
    use crate::config::Config;
    use crate::harness::Harness;
    use crate::sandbox_spawn::{
        with_authenticated_principal, AuthenticatedPrincipal, PrincipalTrust,
    };

    const SESSION: &str = "alfonso:watch-restart-worker";
    const TASK: &str = "bash-0123456789abcdef";

    fn context(project: &std::path::Path, storage: &std::path::Path) -> Arc<AppContext> {
        Arc::new(AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            Config {
                project_root: Some(project.into()),
                storage_dir: Some(storage.into()),
                // Another route configured this shared root most recently.
                harness: Some(Harness::Opencode),
                experimental_bash_background: true,
                sandbox: crate::config::SandboxConfig {
                    enabled: false,
                    ..Default::default()
                },
                ..Config::default()
            },
        ))
    }

    fn principal(project: &std::path::Path) -> AuthenticatedPrincipal {
        AuthenticatedPrincipal::RouteBind {
            trust: PrincipalTrust::FirstParty,
            route_channel: 41,
            route_epoch: 1,
            project_root: project.into(),
            harness: "runner".into(),
            session_id: SESSION.into(),
            principal_id: Some("reserved:broca".into()),
        }
    }

    fn request(task: &str, session: &str) -> RawRequest {
        serde_json::from_value(json!({
            "id": "watch-restart",
            "command": "bash_watch",
            "session_id": session,
            "worker_session": true,
            "task_id": task,
            "timeout_ms": MAX_WATCH_TIMEOUT_MS,
        }))
        .unwrap()
    }

    async fn watch_with_deadline(ctx: Arc<AppContext>, request: RawRequest) -> Response {
        let worker = principal(ctx.config().project_root.as_deref().unwrap());
        let wake = crate::response_finalize::DeferredResponseWake::default();
        let producer_wake = wake.clone();
        let producer_ctx = Arc::clone(&ctx);
        tokio::time::timeout(Duration::from_secs(3), async move {
            let outcome = tokio::task::spawn_blocking(move || {
                let _wake = producer_wake.install();
                with_authenticated_principal(worker, || handle_deferred(&request, producer_ctx))
            })
            .await
            .unwrap();
            match outcome {
                DispatchOutcome::Immediate(response) => response,
                DispatchOutcome::Deferred(mut pending) => {
                    // The completion signal, not sleeps or repeated polling,
                    // proves the watch answered well before its 30-minute window.
                    wake.notified().await;
                    (pending.poll)(&ctx).expect("response queued before completion wake")
                }
            }
        })
        .await
        .expect("worker watch must answer within three seconds, not its 30-minute window")
    }

    fn persist_failed_task(ctx: &AppContext) {
        let worker = principal(ctx.config().project_root.as_deref().unwrap());
        with_authenticated_principal(worker, || {
            let project = ctx.config().project_root.clone().unwrap();
            let storage = crate::bash_background::task_storage_dir(ctx);
            let resolved = create_task_layout(&storage, SESSION, TASK).unwrap();
            let mut metadata = PersistedTask::starting(
                TASK.into(),
                SESSION.into(),
                "exit 1".into(),
                project.clone(),
                Some(project),
                None,
                false,
                false,
            );
            metadata.mark_terminal(BgTaskStatus::Failed, Some(1), None);
            write_task_at(&resolved, &metadata).unwrap();
            std::fs::write(&resolved.paths.stdout, "durable worker output\n").unwrap();
            std::fs::write(&resolved.paths.stderr, "").unwrap();
            std::fs::write(&resolved.paths.exit, "1\n").unwrap();
        });
    }

    #[tokio::test]
    async fn worker_watch_unknown_task_does_not_wait_for_window() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let response = watch_with_deadline(
            context(project.path(), storage.path()),
            request(TASK, SESSION),
        )
        .await;
        assert!(!response.success, "{response:?}");
        assert_eq!(response.data["code"], "task_not_found");
    }

    #[tokio::test]
    async fn worker_watch_recovers_terminal_task_before_configure_replay() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let old = context(project.path(), storage.path());
        persist_failed_task(&old);
        old.bash_background().detach();
        let restarted = context(project.path(), storage.path());
        assert!(restarted
            .bash_background()
            .observed_status(TASK, SESSION, 0)
            .is_none());
        let response = watch_with_deadline(restarted, request(TASK, SESSION)).await;
        assert!(
            response.success,
            "durable task must be recovered: {response:?}"
        );
        assert_eq!(response.data["status"], "failed");
        assert_eq!(response.data["exit_code"], 1);
        assert!(response.data["output"]
            .as_str()
            .unwrap()
            .contains("durable worker output"));
        assert_eq!(response.data["waited"]["reason"], "exited");
    }

    #[tokio::test]
    async fn worker_watch_recovery_does_not_adopt_another_session() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let ctx = context(project.path(), storage.path());
        persist_failed_task(&ctx);
        let response = watch_with_deadline(Arc::clone(&ctx), request(TASK, "other-worker")).await;
        assert!(!response.success, "{response:?}");
        assert_eq!(response.data["code"], "task_not_found");
        assert!(ctx
            .bash_background()
            .observed_status(TASK, SESSION, 0)
            .is_none());
    }

    fn params(value: Value) -> BashWatchParams {
        serde_json::from_value(value).unwrap()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn worker_watch_explicit_timeout_is_clamped_to_configured_cap() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let ctx = context(project.path(), storage.path());
        ctx.update_config(|config| config.bash.worker_wait_max_ms = 500);
        let spawn: RawRequest = serde_json::from_value(json!({
            "id":"start-watch-cap", "command":"bash", "session_id":SESSION,
            "worker_session":true,
            "params":{"command":"sleep 10", "background":true, "timeout":30_000},
        }))
        .unwrap();
        let launched = with_authenticated_principal(principal(project.path()), || {
            crate::commands::bash::handle(&spawn, &ctx)
        });
        assert!(launched.success, "{launched:?}");
        let task = launched.data["task_id"].as_str().unwrap();
        struct StopTask(crate::bash_background::BgTaskRegistry, String);
        impl Drop for StopTask {
            fn drop(&mut self) {
                let _ = self.0.kill(&self.1, SESSION);
            }
        }
        let _stop = StopTask(ctx.bash_background().clone(), task.into());
        let started = Instant::now();
        let response = watch_with_deadline(Arc::clone(&ctx), request(task, SESSION)).await;
        let elapsed = started.elapsed();
        assert!(response.success, "{response:?}");
        assert_eq!(response.data["status"], "running", "{response:?}");
        assert_eq!(response.data["waited"]["reason"], "timeout", "{response:?}");
        assert!(
            elapsed >= Duration::from_millis(400) && elapsed < Duration::from_secs(2),
            "{elapsed:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn worker_watch_recovers_running_task_started_before_restart() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let old = context(project.path(), storage.path());
        let spawn: RawRequest = serde_json::from_value(json!({
            "id": "start-before-restart", "command": "bash", "session_id": SESSION,
            "worker_session": true,
            "params": {"command": "printf restart-ready; sleep 30", "background": true, "timeout": 10000, "compressed": false},
        })).unwrap();
        let launched = with_authenticated_principal(principal(project.path()), || {
            crate::commands::bash::handle(&spawn, &old)
        });
        assert!(launched.success, "{launched:?}");
        let task_id = launched.data["task_id"].as_str().unwrap().to_string();
        let paths = old
            .bash_background()
            .task_json_path(&task_id, SESSION)
            .unwrap();
        let metadata = crate::bash_background::persistence::read_task(&paths).unwrap();
        // Always stop the real fixture child, including a baseline/mutation
        // failure that leaves the restarted registry unable to address it.
        struct StopChild(i32);
        impl Drop for StopChild {
            fn drop(&mut self) {
                let _ = crate::bash_background::process::terminate_pgid(self.0, None);
            }
        }
        let _stop = StopChild(metadata.pgid.unwrap());
        old.bash_background().detach();
        let restarted = context(project.path(), storage.path());
        let mut watch = request(&task_id, SESSION);
        watch.params["pattern"] = json!("restart-ready");
        let response = watch_with_deadline(Arc::clone(&restarted), watch).await;
        assert!(
            response.success,
            "live task must be recovered: {response:?}"
        );
        assert_eq!(response.data["status"], "running");
        assert_eq!(response.data["waited"]["reason"], "matched");
        let killed = restarted.bash_background().kill(&task_id, SESSION).unwrap();
        assert_eq!(killed.info.status, BgTaskStatus::Killed);
        let finished = watch_with_deadline(restarted, request(&task_id, SESSION)).await;
        assert!(finished.success, "{finished:?}");
        assert_eq!(finished.data["status"], "killed");
        assert_eq!(finished.data["waited"]["reason"], "exited");
    }

    #[test]
    fn wait_limits_follow_the_role_like_the_plugins() {
        let limit = |value, worker| effective_wait_ms(&params(value), worker, 1_800_000, 120_000);
        assert_eq!(limit(json!({}), true), Ok(1_800_000));
        assert_eq!(limit(json!({"timeout_ms": 5_000}), true), Ok(5_000));
        assert_eq!(
            limit(json!({"timeout_ms": 1_000, "background": true}), true),
            Ok(1_800_000)
        );
        assert_eq!(limit(json!({}), false), Ok(30_000));
        assert_eq!(limit(json!({"timeout_ms": 90_000}), false), Ok(90_000));
        assert!(limit(json!({"timeout_ms": 200_000}), false)
            .unwrap_err()
            .contains("bash.watch_sync_max_ms"));
        assert!(limit(json!({"timeout_ms": 0}), true).is_err());
        assert!(limit(json!({"timeout_ms": 1_800_001}), true).is_err());
    }

    #[test]
    fn worker_watch_at_cap_waits_for_a_nearby_kill_terminal() {
        assert_eq!(
            wait_limit_outcome(true, false, true, Some(Duration::from_secs(4)),),
            None,
            "a worker watch should keep waiting inside the handoff margin"
        );
        assert_eq!(
            wait_limit_outcome(true, true, true, Some(Duration::ZERO)),
            Some(WaitReason::Exited),
            "the timed-out task's terminal result ends the watch"
        );
    }

    #[test]
    fn substring_and_regex_buffers_keep_only_what_a_split_match_needs() {
        let substring = parse_pattern(Some(&json!("DONE"))).unwrap().unwrap();
        assert_eq!(substring.keep_from(10), 7);
        let regex = parse_pattern(Some(&json!({"regex": "a+"})))
            .unwrap()
            .unwrap();
        assert_eq!(regex.keep_from(10), 0);
        assert_eq!(
            regex.keep_from(REGEX_SCAN_WINDOW_BYTES + 5),
            5,
            "a regex keeps a 64 KB window"
        );
        assert!(parse_pattern(Some(&json!({"regex": "("}))).is_err());
        assert!(parse_pattern(Some(&json!(3))).is_err());
        assert!(parse_pattern(None).unwrap().is_none());
    }

    #[test]
    fn output_tail_keeps_the_last_twenty_lines() {
        let output: String = (1..=30).map(|n| format!("line {n}\n")).collect();
        let tail = output_tail(&output);
        assert!(tail.starts_with("line 11\n"));
        assert!(tail.ends_with("line 30"));
        assert_eq!(output_tail("  \n"), "");
    }
}
