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
use crate::bash_background::watch_lines::EACH_LINE_NEEDS_BACKGROUND;
use crate::bash_background::{BgTaskRegistry, BgTaskStatus};
use crate::commands::bash_orchestrate::{
    format_wait_limit, kill_deadline_sentence, worker_kill_deadline_within_handoff_margin,
};
use crate::commands::bash_status::{
    format_erased_task_message, format_unknown_polled_task_message,
};
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
/// Bytes kept before a trimmed regex window as context only. `^`, `\b` and
/// `\B` look back one character, and a UTF-8 character is at most 4 bytes.
const REGEX_SCAN_CONTEXT_BYTES: usize = 4;
/// Added to an exited task's no-match result when a regex scan had to drop
/// older output from its window, so the non-match is not a complete answer.
/// Literal patterns never set it: their kept tail is exact.
const REGEX_WINDOW_SCAN_INCOMPLETE: &str = "No match found, but the scan is incomplete: a regex match longer than 64 KiB may have been missed. Literal patterns are always scanned in full.";
/// Most lines of a still-running command's output shown to a worker.
const WORKER_OUTPUT_TAIL_LINES: usize = 20;
/// Output preview a watch result carries for a finished task.
const PREVIEW_BYTES: usize = crate::bash_background::output::RUNNING_OUTPUT_PREVIEW_BYTES;

#[derive(Debug, Default, Deserialize)]
struct BashWatchParams {
    #[serde(default)]
    deliver: Option<Value>,
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
    /// The task exited without a match, but the regex window dropped bytes.
    scan_incomplete: bool,
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
    has_more: bool,
    read_timed_out: bool,
    /// A regex match ended exactly at the end of the buffer while more
    /// captured bytes were still to be read. It is checked again once the
    /// next page arrives or the stream is known to end there.
    deferred: bool,
    /// A regex scan cut older bytes out of its window, so a match longer
    /// than the window could have been missed.
    dropped: bool,
    /// Leading buffer bytes that are context from before a trimmed window.
    /// The search starts after them, so the trimmed start never looks like
    /// the start of the output to `^`, `\b` or `\B`.
    search_from: usize,
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
    let budget =
        crate::commands::blocking_wait::ReplyBudget::from_envelope(Instant::now(), &req.params);
    handle_deferred_with_budget(req, ctx, budget)
}

pub(crate) fn handle_deferred_with_budget(
    req: &RawRequest,
    ctx: Arc<AppContext>,
    budget: crate::commands::blocking_wait::ReplyBudget,
) -> DispatchOutcome {
    handle_deferred_with_observation_only(req, ctx, budget, false)
}

/// An observation-only transport may wait on output, but cannot renew, recover,
/// or otherwise change the task that produced it. The flag is not a tool argument.
pub(crate) fn handle_deferred_with_observation_only(
    req: &RawRequest,
    ctx: Arc<AppContext>,
    budget: crate::commands::blocking_wait::ReplyBudget,
    observation_only: bool,
) -> DispatchOutcome {
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
    if params.deliver.is_some() {
        return DispatchOutcome::Immediate(Response::error(
            &id,
            "invalid_request",
            EACH_LINE_NEEDS_BACKGROUND,
        ));
    }
    let pattern = match parse_pattern(params.pattern.as_ref()) {
        Ok(pattern) => pattern,
        Err(response) => return DispatchOutcome::Immediate(with_id(response, &id)),
    };
    let worker = req.worker_session();
    let worker_limit_ms = crate::commands::bash_orchestrate::worker_wait_max_ms(&ctx);
    let primary_cap_ms = ctx.config().bash.watch_sync_max_ms;
    let wait_cap_ms = if worker {
        crate::commands::blocking_wait::clamp_worker_wait_ms(worker_limit_ms)
    } else {
        worker_limit_ms
    };
    let limit_ms = match effective_wait_ms(&params, worker, wait_cap_ms, primary_cap_ms) {
        Ok(limit) => limit,
        Err(message) => {
            return DispatchOutcome::Immediate(Response::error(&id, "invalid_request", message))
        }
    };
    let session = req.session().to_string();
    let shell_tool = req.host_tool_names().shell().to_string();
    let registry = ctx.bash_background().clone();
    let erased = if observation_only {
        registry.observed_erased_watch_reference(&task_id)
    } else {
        registry.has_erased_watch_reference(&task_id)
    };
    if erased {
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
    let ask_route = crate::access_policy::capture_ask_route();
    let grant_scope = crate::access_policy::current_scope();
    if let Err(error) = crate::bash_background::worker_thread::spawn(
        crate::bash_background::worker_thread::Worker::Watch,
        move || {
            let _ask_route = ask_route.install();
            let _grant_scope = crate::access_policy::scope_guard(grant_scope);
            crate::sandbox_spawn::with_authenticated_principal(principal, || {
                // Configure acknowledges the route before its replay tail finishes.
                // Resolve the durable record before starting the wait, off executor
                // slots, and without replaying unrelated tasks or relaxing the session.
                if !observation_only && registry.observed_status(&task_id, &session, 0).is_none() {
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
                let watch = WatchJob {
                    registry,
                    task_id,
                    session,
                    pattern,
                    limit_ms,
                    worker,
                    worker_limit_ms,
                    primary_cap_ms,
                    budget,
                    observation_only,
                    storage_dir,
                    project_root,
                };
                if watch.observed_status(0).is_none() {
                    let _ = tx.send(Response::error(
                        &thread_id,
                        "task_not_found",
                        format_unknown_polled_task_message(&watch.task_id, &shell_tool),
                    ));
                    return;
                }
                let Some(waited) = watch.wait(&worker_cancellation) else {
                    // Cancelled: the transport already answered the call.
                    return;
                };
                // The task was already resolved under this route's identity. Do not
                // re-adopt another session's task through status's relaxed fallback if
                // the record disappears while the watch is waiting.
                let snapshot = watch.observed_status(PREVIEW_BYTES);
                let response = match snapshot {
                    Some(snapshot) => watch.reply(&thread_id, snapshot, &waited),
                    None => Response::error(
                        &thread_id,
                        "task_not_found",
                        format_unknown_polled_task_message(&watch.task_id, &shell_tool),
                    ),
                };
                let _ = tx.send(response);
            })
        },
    ) {
        return DispatchOutcome::Immediate(Response::error(
            &id,
            "internal_error",
            format!("failed to start bash watch worker: {error}"),
        ));
    }

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
    budget: crate::commands::blocking_wait::ReplyBudget,
    observation_only: bool,
    storage_dir: std::path::PathBuf,
    project_root: Option<std::path::PathBuf>,
}

impl WatchJob {
    fn observed_status(&self, preview_bytes: usize) -> Option<BgTaskSnapshot> {
        if self.observation_only {
            self.registry
                .observation_status(
                    &self.task_id,
                    &self.session,
                    self.project_root.as_deref(),
                    &self.storage_dir,
                    preview_bytes,
                )
                .map(|(snapshot, _)| snapshot)
        } else {
            self.registry
                .observed_status(&self.task_id, &self.session, preview_bytes)
        }
    }

    /// Waits until the task exits, the pattern matches, or the limit passes.
    /// `None` when the call was cancelled first.
    fn wait(&self, cancellation: &crate::executor::JobCancellation) -> Option<Waited> {
        let started = self.budget.received_at;
        let deadline = self
            .budget
            .cap(started + Duration::from_millis(self.limit_ms));
        let mut stdout = StreamScan::default();
        let mut stderr = StreamScan::default();
        let waited = |reason, matched| Waited {
            reason,
            elapsed_ms: started.elapsed().as_millis() as u64,
            limit_ms: deadline.saturating_duration_since(started).as_millis() as u64,
            matched,
            scan_incomplete: false,
        };
        loop {
            if cancellation.cancel_already_requested() {
                return None;
            }
            // A worker waiting on its task keeps the task's default hard kill
            // at least one worker wait limit away, as every other worker wait
            // does (see `BgTaskRegistry::renew_hard_kill`).
            if self.worker && !self.observation_only {
                self.registry.renew_hard_kill(
                    &self.task_id,
                    &self.session,
                    Duration::from_millis(self.worker_limit_ms),
                );
            }
            let snapshot = self.observed_status(0);
            let mut terminal = snapshot
                .as_ref()
                .is_none_or(|snapshot| snapshot.info.status.is_terminal());
            if let (Some(pattern), Some(snapshot)) = (&self.pattern, &snapshot) {
                if let Some(found) = self.scan(pattern, snapshot, &mut stdout, &mut stderr) {
                    return Some(waited(WaitReason::Matched, Some(found)));
                }
                // Task exit is not the end of a paged scan. Check unread
                // captured bytes before reporting a terminal non-match.
                terminal &= !stdout.has_more && !stderr.has_more;
            }
            // The filesystem access cap can expire before the reply deadline.
            // Do not spin retrying failed I/O or turn unread output into EOF.
            if stdout.read_timed_out || stderr.read_timed_out {
                return Some(waited(
                    if terminal {
                        WaitReason::Exited
                    } else {
                        WaitReason::Timeout
                    },
                    None,
                ));
            }
            let now = Instant::now();
            // A nearby process kill may extend an ordinary watch limit, never
            // the reply deadline: the caller must receive its task id in time.
            let window_dropped = stdout.dropped || stderr.dropped;
            if now >= self.budget.deadline {
                let mut result = waited(
                    if terminal {
                        WaitReason::Exited
                    } else {
                        WaitReason::Timeout
                    },
                    None,
                );
                result.scan_incomplete = terminal && window_dropped;
                return Some(result);
            }
            let kill_remaining = if self.worker {
                self.registry
                    .hard_kill_remaining(&self.task_id, &self.session)
            } else {
                None
            };
            if let Some(reason) =
                wait_limit_outcome(now >= deadline, terminal, self.worker, kill_remaining)
            {
                let mut result = waited(reason, None);
                result.scan_incomplete = reason == WaitReason::Exited && window_dropped;
                return Some(result);
            }
            if stdout.has_more || stderr.has_more {
                continue;
            }
            let pause = if now >= deadline {
                // The command is about to be killed; keep polling until the
                // terminal timeout result is visible instead of handing it off.
                poll_delay(started.elapsed())
            } else {
                poll_delay(started.elapsed()).min(deadline - now)
            }
            .min(self.budget.deadline - now);
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
            if Instant::now() >= self.budget.deadline {
                scan.has_more = true;
                continue;
            }
            let page = if self.observation_only {
                self.registry
                    .observation_status(
                        &self.task_id,
                        &self.session,
                        self.project_root.as_deref(),
                        &self.storage_dir,
                        0,
                    )
                    .ok_or_else(|| {
                        std::io::Error::new(std::io::ErrorKind::NotFound, "task_not_found")
                    })
                    .and_then(|(_, paths)| {
                        self.registry.observation_artifact_page_until(
                            paths,
                            artifact,
                            scan.next,
                            self.budget.deadline,
                        )
                    })
            } else {
                self.registry.read_artifact_page_until(
                    &self.task_id,
                    &self.session,
                    artifact,
                    scan.next,
                    self.budget.deadline,
                )
            };
            let page = match page {
                Ok(page) => page,
                Err(error) => {
                    // The helper's queue-wait or access cap can expire before the reply deadline.
                    // A timed-out read did not establish EOF, even for an exited task.
                    scan.read_timed_out = error.kind() == std::io::ErrorKind::TimedOut;
                    scan.has_more = scan.read_timed_out || Instant::now() >= self.budget.deadline;
                    continue;
                }
            };
            scan.has_more = page.has_more;
            let bytes = page.bytes;
            let next = page.next_offset;
            if bytes.is_empty() {
                // A deferred match whose stream turned out to end there is a
                // real match of the whole output; nothing new needs reading.
                if !(scan.deferred && !scan.has_more) {
                    continue;
                }
            } else {
                if scan.buffer.is_empty() {
                    scan.base = scan.next;
                }
                scan.next = next.max(scan.next + bytes.len() as u64);
                scan.buffer.extend_from_slice(&bytes);
            }
            scan.deferred = false;
            if let Some(found) = pattern.regex().find_at(&scan.buffer, scan.search_from) {
                // A page ends where the read stopped, not where the output
                // ends. A regex match that reaches the end of the buffer may
                // depend on what follows ($, \z, \b, a greedy repeat), so while
                // more captured bytes follow it is not yet a match of the
                // whole output. A literal is complete once all its bytes are in.
                let end_dependent = matches!(pattern, WaitPattern::Regex(_))
                    && scan.has_more
                    && found.end() == scan.buffer.len();
                if !end_dependent {
                    return Some((
                        String::from_utf8_lossy(found.as_bytes()).into_owned(),
                        scan.base + found.start() as u64,
                        label,
                    ));
                }
                scan.deferred = true;
            }
            let cut = pattern.keep_from(scan.buffer.len());
            if cut > 0 {
                let regex = matches!(pattern, WaitPattern::Regex(_));
                let context = if regex {
                    cut.min(REGEX_SCAN_CONTEXT_BYTES)
                } else {
                    0
                };
                scan.buffer.drain(..cut - context);
                scan.base += (cut - context) as u64;
                scan.search_from = context;
                scan.dropped |= regex;
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
        if waited.scan_incomplete {
            data["waited"]["scan_incomplete"] = json!(true);
        }
        if let Some((text, offset, stream)) = &waited.matched {
            data["waited"]["match"] = json!(text);
            data["waited"]["match_offset"] = json!(offset);
            if let Some(stream) = stream {
                data["waited"]["match_stream"] = json!(stream);
            }
        }
        data["output"] = json!(text);
        if !self.observation_only {
            self.registry.attach_terminal_view(&mut data, &self.session);
        }
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
            WaitReason::Timeout if snapshot.phase == Some("submitting") => {
                text.push_str(&format!(
                    "\n{waited_text}; timeout reached without match. The task is still submitting; the runner has not confirmed input capture. Call bash_watch again to keep waiting."
                ));
            }
            // A terminal task can still time out here when the watch ran out of
            // time while scanning output pages captured before the task exited.
            WaitReason::Timeout if !running => {
                text.push_str(&format!("\n{waited_text}; watch limit reached while scanning captured output. The task already exited; the pattern scan is incomplete. Use a longer watch or paged raw output to inspect the remaining bytes."));
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
                if waited.scan_incomplete {
                    text.push(' ');
                    text.push_str(REGEX_WINDOW_SCAN_INCOMPLETE);
                }
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
        // A remote task has no AFT kill clock: say where it is on the build
        // server instead (capturing inputs, queued, or when the runner stops it).
        if let Some(note) = &snapshot.remote_phase_note {
            return remote_phase_sentence(note);
        }
        kill_deadline_sentence(
            self.registry
                .hard_kill_deadline(&self.task_id, &self.session),
            snapshot.info.started_at,
            self.worker,
        )
    }
}

/// Turns `note` (from `remote_phase_note`) into a sentence: first character
/// upper-cased, a period added if missing. An empty note gives an empty string.
fn remote_phase_sentence(note: &str) -> String {
    let mut chars = note.chars();
    match chars.next() {
        Some(first) => format!(
            "{}{}{}",
            first.to_uppercase(),
            chars.as_str(),
            if note.ends_with('.') { "" } else { "." }
        ),
        None => String::new(),
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
    #[test]
    fn remote_progress_sentences_keep_the_approved_final_period() {
        assert_eq!(
            super::remote_phase_sentence("queued on the runner (12 s)"),
            "Queued on the runner (12 s)."
        );
        for note in [
            "The remote job is uploading its files (14s so far).",
            "The remote job is waiting for a free build host (2m 10s so far): room available.",
            "The remote job is connecting to its build host (3s so far).",
        ] {
            assert_eq!(super::remote_phase_sentence(note), note);
        }
    }
    use super::*;
    #[test]
    fn each_line_sync_watch_refuses_delivery() {
        let project = tempfile::tempdir().unwrap();
        let ctx = context(project.path(), project.path());
        let req = serde_json::from_value(json!({"id":"sync-delivery", "command":"bash_watch", "task_id":"task", "deliver":"each_line"})).unwrap();
        let DispatchOutcome::Immediate(response) = handle_deferred(&req, ctx) else {
            panic!("sync delivery must refuse immediately")
        };
        assert!(!response.success);
        assert_eq!(
            serde_json::to_value(&response).unwrap()["message"],
            r#"bash_watch: deliver: "each_line" needs background: true."#
        );
    }
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

    /// A terminal pinned task with its row and view record in aft.db, as a
    /// restart recovery plus terminal upsert would leave it.
    fn view_watch_fixture() -> (
        tempfile::TempDir,
        WatchJob,
        BgTaskSnapshot,
        Waited,
        serde_json::Value,
    ) {
        let manifest = crate::db::rich_ui::test_view_manifest_for_test();
        let pin = manifest.pins[0].clone();
        let _writing = crate::db::rich_ui::enable_test_view_writing_for_test(manifest.clone());
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path(), dir.path());
        let db = std::sync::Arc::new(std::sync::Mutex::new(
            crate::db::open(&dir.path().join("aft.db")).unwrap(),
        ));
        ctx.set_db(std::sync::Arc::clone(&db));
        ctx.bash_background()
            .set_db_pool(std::sync::Arc::clone(&db));
        let bound = principal(dir.path());
        let (snapshot, stored) = with_authenticated_principal(bound, || {
            let storage = crate::bash_background::task_storage_dir(&ctx);
            let resolved = create_task_layout(&storage, SESSION, TASK).unwrap();
            let mut metadata = crate::db::rich_ui::terminal_task_metadata_for_test(Some(&pin));
            metadata.session_id = SESSION.into();
            metadata.task_id = TASK.into();
            metadata.harness = Some("runner".into());
            metadata.workdir = dir.path().to_path_buf();
            metadata.project_root = Some(dir.path().to_path_buf());
            metadata.status = crate::bash_background::BgTaskStatus::Failed;
            write_task_at(&resolved, &metadata).unwrap();
            std::fs::write(&resolved.paths.stdout, "durable worker output\n").unwrap();
            std::fs::write(&resolved.paths.stderr, "").unwrap();
            std::fs::write(&resolved.paths.exit, "1\n").unwrap();
            let row =
                crate::db::rich_ui::task_row_for_test(&metadata, "runner", "failed", Some(1), true);
            let conn = db.lock().unwrap();
            crate::db::rich_ui::upsert_task_row_with_view(&conn, &row, &metadata).unwrap();
            let stored = crate::db::rich_ui::view_record_json(&conn, "runner", SESSION, TASK)
                .unwrap()
                .expect("record must be stored");
            drop(conn);
            let storage = crate::bash_background::task_storage_dir(&ctx);
            ctx.bash_background()
                .recover_watch_task(&storage, SESSION, TASK, Some(dir.path()), Some("runner"))
                .unwrap();
            let snapshot = ctx
                .bash_background()
                .observed_status(TASK, SESSION, 0)
                .expect("recovered terminal snapshot");
            (snapshot, stored)
        });
        assert!(snapshot.info.status.is_terminal());
        let storage = crate::bash_background::task_storage_dir(&ctx);
        let job = WatchJob {
            registry: ctx.bash_background().clone(),
            task_id: TASK.into(),
            session: SESSION.into(),
            pattern: None,
            limit_ms: 30_000,
            worker: false,
            worker_limit_ms: 1_800_000,
            primary_cap_ms: 120_000,
            budget: Instant::now().into(),
            observation_only: false,
            storage_dir: storage,
            project_root: Some(dir.path().into()),
        };
        // The writing guard drops here: seeding is complete, and delivery
        // reads only the stored record plus a thread-local result-body flag.
        let waited = Waited {
            reason: WaitReason::Exited,
            elapsed_ms: 1,
            limit_ms: 30_000,
            matched: None,
            scan_incomplete: false,
        };
        (dir, job, snapshot, waited, stored)
    }

    /// A terminal `bash_watch` on a tool-result/v1 route carries the stored
    /// record; legacy routes, carrier (observation-only) reads and running
    /// snapshots never do.
    #[test]
    fn terminal_bash_watch_carries_the_stored_record_on_tool_result_routes() {
        let (_dir, mut job, snapshot, waited, stored) = view_watch_fixture();
        assert!(stored.is_object());
        // A legacy-envelope route never receives the record.
        let legacy = job.reply("legacy", snapshot.clone(), &waited);
        assert!(legacy.success);
        assert!(legacy.data.get("view").is_none());
        let _v1 = crate::subc::result_body::enter(true);
        // A terminal reply on a versioned route carries the stored record.
        let response = job.reply("terminal", snapshot.clone(), &waited);
        assert!(response.success);
        assert_eq!(response.data["view"], stored);
        assert_eq!(response.data["view"]["task_id"], TASK);
        // The extra member leaves the rendered output bytes unchanged.
        assert_eq!(
            response.data["output"], legacy.data["output"],
            "view must not change content"
        );
        // A carried (observation-only) read keeps its ordinary result shape.
        job.observation_only = true;
        let carried = job.reply("carried", snapshot.clone(), &waited);
        assert!(carried.success);
        assert!(carried.data.get("view").is_none());
        // A running snapshot carries no record either.
        job.observation_only = false;
        let mut running = snapshot;
        running.info.status = crate::bash_background::BgTaskStatus::Running;
        let timeout = job.reply(
            "timeout",
            running,
            &Waited {
                reason: WaitReason::Timeout,
                elapsed_ms: 1,
                limit_ms: 1,
                matched: None,
                scan_incomplete: false,
            },
        );
        assert!(timeout.success);
        assert!(timeout.data.get("view").is_none());
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

    #[test]
    fn approved_watch_worker_admission_failure_is_immediate_internal_error() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let ctx = context(project.path(), storage.path());
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::bash_background::worker_thread::with_refusal(
                crate::bash_background::worker_thread::Worker::Watch,
                || handle_deferred(&request(TASK, SESSION), ctx),
            )
        }));
        assert!(
            result.is_ok(),
            "worker admission panicked instead of answering the call"
        );
        let DispatchOutcome::Immediate(response) = result.unwrap() else {
            panic!("a refused worker must not leave a pending response");
        };
        assert!(!response.success);
        let wire = serde_json::to_value(response).unwrap();
        assert_eq!(wire["code"], "internal_error");
        assert!(wire["message"]
            .as_str()
            .unwrap()
            .contains("injected worker admission failure"));
    }

    fn approved_terminal_watch_fixture() -> (tempfile::TempDir, WatchJob, BgTaskSnapshot) {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path(), dir.path());
        let bound = principal(dir.path());
        with_authenticated_principal(bound, || {
            persist_failed_task(&ctx);
            let storage = crate::bash_background::task_storage_dir(&ctx);
            let session = crate::bash_background::persistence::session_tasks_dir(&storage, SESSION);
            let layout =
                crate::bash_background::persistence::resolve_task_layout(&session, TASK).unwrap();
            let mut output = vec![b'x'; 3 * 64 * 1024];
            output[65534..65545].copy_from_slice(b"FINAL-MATCH");
            std::fs::write(&layout.paths.stdout, output).unwrap();
            std::fs::write(&layout.paths.stderr, "").unwrap();
            ctx.bash_background()
                .recover_watch_task(&storage, SESSION, TASK, Some(dir.path()), Some("runner"))
                .unwrap();
            let snapshot = ctx
                .bash_background()
                .observed_status(TASK, SESSION, 0)
                .unwrap();
            let job = WatchJob {
                registry: ctx.bash_background().clone(),
                task_id: TASK.into(),
                session: SESSION.into(),
                pattern: Some(WaitPattern::Substring(
                    regex::bytes::Regex::new("FINAL-MATCH").unwrap(),
                    11,
                )),
                limit_ms: 30_000,
                worker: false,
                worker_limit_ms: 1_800_000,
                primary_cap_ms: 120_000,
                budget: Instant::now().into(),
                observation_only: false,
                storage_dir: storage,
                project_root: Some(dir.path().into()),
            };
            (dir, job, snapshot)
        })
    }

    #[test]
    fn approved_watch_scan_buffer_is_bounded_across_pages() {
        let (_dir, job, snapshot) = approved_terminal_watch_fixture();
        let mut stdout = StreamScan::default();
        let mut stderr = StreamScan::default();
        let mut matched = None;
        for _ in 0..8 {
            matched = job.scan(
                job.pattern.as_ref().unwrap(),
                &snapshot,
                &mut stdout,
                &mut stderr,
            );
            assert!(
                stdout.buffer.len() <= 64 * 1024 + 11,
                "scan allocated an unbounded output suffix: {}",
                stdout.buffer.len()
            );
            if matched.is_some() {
                break;
            }
        }
        assert_eq!(matched, Some(("FINAL-MATCH".into(), 65534, Some("stdout"))));
    }

    #[test]
    fn approved_terminal_watch_drains_pages_before_reporting_exit() {
        let (_dir, job, _snapshot) = approved_terminal_watch_fixture();
        let result = job.wait(&crate::executor::JobCancellation::new()).unwrap();
        assert!(matches!(result.reason, WaitReason::Matched));
        assert_eq!(
            result.matched,
            Some(("FINAL-MATCH".into(), 65534, Some("stdout")))
        );
    }

    #[test]
    fn approved_paged_watch_obeys_cancellation_and_its_original_deadline() {
        let (_dir, mut job, _snapshot) = approved_terminal_watch_fixture();
        job.pattern = Some(WaitPattern::Substring(
            regex::bytes::Regex::new("NEVER").unwrap(),
            5,
        ));
        let cancelled = crate::executor::JobCancellation::new();
        cancelled.request_cancel();
        assert!(job.wait(&cancelled).is_none());
        job.limit_ms = 0;
        let result = job.wait(&crate::executor::JobCancellation::new()).unwrap();
        assert!(matches!(result.reason, WaitReason::Timeout));
        assert!(result.matched.is_none());
    }

    #[test]
    fn approved_terminal_paging_timeout_does_not_claim_task_is_running() {
        let (_dir, mut job, snapshot) = approved_terminal_watch_fixture();
        job.pattern = Some(WaitPattern::Substring(
            regex::bytes::Regex::new("NEVER").unwrap(),
            5,
        ));
        job.limit_ms = 0;
        let result = job.wait(&crate::executor::JobCancellation::new()).unwrap();
        let text = job.render(&snapshot, &result);
        assert!(
            !text.contains("still running"),
            "terminal paging produced a false liveness claim: {text}"
        );
        assert!(text.contains("captured output"));
    }

    const PAGE: usize = 64 * 1024;
    const BUDGET_PAGES: usize = 6;
    /// The match sits inside the last of `BUDGET_PAGES` captured pages.
    const LAST_PAGE_MATCH: u64 = ((BUDGET_PAGES - 1) * PAGE + 100) as u64;
    /// Each page read (stdout and stderr per scan pass) sleeps this long, so a
    /// full drain takes about `BUDGET_PAGES * 2 * PAGE_DELAY`.
    const PAGE_DELAY: Duration = Duration::from_millis(150);

    /// An exited task with `BUDGET_PAGES` pages of stdout, watched by a primary
    /// call with a long `limit_ms` and a reply budget parsed from `deadline_ms`
    /// the way the transport parses it.
    fn budgeted_terminal_watch(client_window: Duration) -> (tempfile::TempDir, WatchJob) {
        let mut output = vec![b'x'; BUDGET_PAGES * PAGE];
        let at = LAST_PAGE_MATCH as usize;
        output[at..at + 11].copy_from_slice(b"FINAL-MATCH");
        let (dir, mut job) = terminal_watch_with_stdout(&output);
        let wall_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        job.budget = crate::commands::blocking_wait::ReplyBudget::from_envelope(
            Instant::now(),
            &json!({ "deadline_ms": wall_ms + client_window.as_millis() as u64 }),
        );
        job.limit_ms = 30_000;
        (dir, job)
    }

    /// The exited-task fixture with its stdout replaced by `output`.
    fn terminal_watch_with_stdout(output: &[u8]) -> (tempfile::TempDir, WatchJob) {
        let (dir, job, _snapshot) = approved_terminal_watch_fixture();
        let ctx = context(dir.path(), dir.path());
        let layout = with_authenticated_principal(principal(dir.path()), || {
            let storage = crate::bash_background::task_storage_dir(&ctx);
            let session = crate::bash_background::persistence::session_tasks_dir(&storage, SESSION);
            crate::bash_background::persistence::resolve_task_layout(&session, TASK).unwrap()
        });
        std::fs::write(&layout.paths.stdout, output).unwrap();
        (dir, job)
    }

    fn regex_pattern(source: &str) -> WaitPattern {
        match parse_pattern(Some(&json!({ "regex": source }))) {
            Ok(Some(pattern)) => pattern,
            _ => panic!("test regex {source:?} must parse"),
        }
    }

    /// Runs a full terminal watch of `output` for a regex pattern.
    fn watch_exited_output(output: &[u8], source: &str) -> Waited {
        let (_dir, mut job) = terminal_watch_with_stdout(output);
        job.pattern = Some(regex_pattern(source));
        job.wait(&crate::executor::JobCancellation::new()).unwrap()
    }

    /// `head` ends exactly at the first page boundary and `tail` follows it.
    fn across_first_page(head: &[u8], tail: &[u8]) -> Vec<u8> {
        let mut output = vec![b'.'; PAGE - head.len()];
        output.extend_from_slice(head);
        output.extend_from_slice(tail);
        output
    }

    #[test]
    fn approved_end_anchor_at_page_boundary_is_not_a_match_when_output_continues() {
        let output = across_first_page(b"READY", b"-NOT\n");
        let waited = watch_exited_output(&output, "READY$");
        assert!(
            matches!(waited.reason, WaitReason::Exited),
            "READY$ falsely matched at a page edge: {:?}",
            waited.matched
        );
        assert!(waited.matched.is_none());
    }

    #[test]
    fn approved_text_end_anchor_at_page_boundary_is_not_a_match_when_output_continues() {
        let output = across_first_page(b"READY", b"-NOT\n");
        let waited = watch_exited_output(&output, r"READY\z");
        assert!(waited.matched.is_none(), "{:?}", waited.matched);
        // The same anchor still matches at the true end of the output.
        let waited = watch_exited_output(&across_first_page(b"READY", b""), r"READY\z");
        assert_eq!(
            waited.matched,
            Some(("READY".into(), (PAGE - 5) as u64, Some("stdout")))
        );
    }

    #[test]
    fn approved_word_boundary_at_page_boundary_is_not_a_match_when_the_word_continues() {
        let output = across_first_page(b" READY", b"NESS\n");
        let waited = watch_exited_output(&output, r"\bREADY\b");
        assert!(waited.matched.is_none(), "{:?}", waited.matched);
        // A real word boundary just past the page edge is still found.
        let output = across_first_page(b" READY", b" now\n");
        let waited = watch_exited_output(&output, r"\bREADY\b");
        assert_eq!(
            waited.matched,
            Some(("READY".into(), (PAGE - 5) as u64, Some("stdout")))
        );
    }

    /// Three pages: `before` ends the first 64 KiB page, the second page
    /// starts with `head` and is padded to a full page, and `tail` is on the
    /// third page. After the second page is scanned the window is trimmed to
    /// start exactly at `head`.
    fn trimmed_start_output(before: u8, head: &[u8], tail: &[u8]) -> Vec<u8> {
        let mut output = vec![b'x'; PAGE - 1];
        output.push(before);
        output.extend_from_slice(head);
        output.extend(std::iter::repeat_n(b'y', PAGE - head.len()));
        output.extend_from_slice(tail);
        output
    }

    #[test]
    fn approved_line_anchor_at_a_trimmed_window_start_sees_the_real_preceding_byte() {
        let pattern = r"^READY[\s\S]*tail";
        let output = trimmed_start_output(b'x', b"READY", b"tail\n");
        let waited = watch_exited_output(&output, pattern);
        assert!(
            waited.matched.is_none(),
            "^ matched at a trimmed start that follows 'x': {:?}",
            waited.matched
        );
        let output = trimmed_start_output(b'\n', b"READY", b"tail\n");
        let waited = watch_exited_output(&output, pattern);
        assert_eq!(waited.matched.map(|found| found.1), Some(PAGE as u64));
    }

    #[test]
    fn approved_word_boundary_at_a_trimmed_window_start_sees_the_real_preceding_byte() {
        let pattern = r"\bREADY[\s\S]*tail";
        let output = trimmed_start_output(b'x', b"READY", b"tail\n");
        let waited = watch_exited_output(&output, pattern);
        assert!(waited.matched.is_none(), "{:?}", waited.matched);
        let output = trimmed_start_output(b' ', b"READY", b"tail\n");
        let waited = watch_exited_output(&output, pattern);
        assert_eq!(waited.matched.map(|found| found.1), Some(PAGE as u64));
    }

    #[test]
    fn approved_end_anchor_held_at_a_page_edge_is_found_when_the_next_page_starts_a_line() {
        let mut output = across_first_page(b"READY", b"\n");
        output.extend(std::iter::repeat_n(b'z', PAGE - 1));
        let waited = watch_exited_output(&output, "READY$");
        assert_eq!(
            waited.matched,
            Some(("READY".into(), (PAGE - 5) as u64, Some("stdout")))
        );
    }

    #[test]
    fn approved_open_ended_held_match_does_not_pin_the_engine_scan_buffer() {
        // BEGIN[\s\S]* reaches the end of every page, so it is held on each
        // one. The cut after each page depends only on the buffer length,
        // never on the held match, so the buffer stays bounded.
        let mut output = b"BEGIN".to_vec();
        output.extend(std::iter::repeat_n(b'x', 8 * PAGE));
        let (_dir, mut job) = terminal_watch_with_stdout(&output);
        job.pattern = Some(regex_pattern(r"BEGIN[\s\S]*"));
        let snapshot = job
            .registry
            .observed_status(&job.task_id, &job.session, 0)
            .unwrap();
        let mut stdout = StreamScan::default();
        let mut stderr = StreamScan::default();
        for _ in 0..9 {
            let found = job.scan(
                job.pattern.as_ref().unwrap(),
                &snapshot,
                &mut stdout,
                &mut stderr,
            );
            assert!(found.is_none(), "{found:?}");
            assert!(
                stdout.buffer.len() <= PAGE + REGEX_SCAN_CONTEXT_BYTES,
                "scan buffer grew to {} bytes",
                stdout.buffer.len()
            );
        }
        let (waited, text) = exited_watch_text(&output, regex_pattern(r"BEGIN[\s\S]*"));
        assert!(waited.matched.is_none());
        assert!(waited.scan_incomplete, "{text}");
        assert!(text.contains(REGEX_WINDOW_SCAN_INCOMPLETE), "{text}");
    }

    #[test]
    fn approved_regex_match_straddling_two_pages_is_found_once_at_its_offset() {
        let output = across_first_page(b"BOUND", b"ARY-7\nmore\nBOUNDARY-8\n");
        let waited = watch_exited_output(&output, r"BOUNDARY-\d");
        assert_eq!(
            waited.matched,
            Some(("BOUNDARY-7".into(), (PAGE - 5) as u64, Some("stdout")))
        );
    }

    /// A full terminal watch of `output` for `pattern`, with its rendered text.
    fn exited_watch_text(output: &[u8], pattern: WaitPattern) -> (Waited, String) {
        let (_dir, mut job) = terminal_watch_with_stdout(output);
        job.pattern = Some(pattern);
        let waited = job.wait(&crate::executor::JobCancellation::new()).unwrap();
        let snapshot = job
            .registry
            .observed_status(&job.task_id, &job.session, 0)
            .unwrap();
        let text = job.render(&snapshot, &waited);
        (waited, text)
    }

    fn literal_pattern(text: &str) -> WaitPattern {
        match parse_pattern(Some(&json!(text))) {
            Ok(Some(pattern)) => pattern,
            _ => panic!("literal {text:?} must parse"),
        }
    }

    #[test]
    fn approved_regex_match_longer_than_window_reports_incomplete_not_no_match() {
        let mut output = b"BEGIN\n".to_vec();
        output.extend(std::iter::repeat_n(b'x', 200 * 1024));
        output.extend_from_slice(b"\nEND\n");
        let (waited, text) = exited_watch_text(&output, regex_pattern(r"BEGIN[\s\S]*END"));
        assert!(matches!(waited.reason, WaitReason::Exited));
        assert!(waited.matched.is_none());
        assert!(
            waited.scan_incomplete,
            "a 200 KiB match was ruled out by a 64 KiB window"
        );
        assert!(text.contains(REGEX_WINDOW_SCAN_INCOMPLETE), "{text}");
    }

    #[test]
    fn approved_absent_regex_is_incomplete_only_when_the_window_dropped_bytes() {
        let large = vec![b'x'; 200 * 1024];
        let (waited, text) = exited_watch_text(&large, regex_pattern("NEVER"));
        assert!(waited.matched.is_none());
        assert!(waited.scan_incomplete, "{text}");
        assert!(text.contains(REGEX_WINDOW_SCAN_INCOMPLETE), "{text}");

        let small = vec![b'x'; 1024];
        let (waited, text) = exited_watch_text(&small, regex_pattern("NEVER"));
        assert!(matches!(waited.reason, WaitReason::Exited));
        assert!(!waited.scan_incomplete, "{text}");
        assert!(!text.contains("incomplete"), "{text}");

        // A literal keeps an exact tail, so dropping bytes loses nothing.
        let (waited, text) = exited_watch_text(&large, literal_pattern("NEVER"));
        assert!(!waited.scan_incomplete, "{text}");
        assert!(!text.contains("incomplete"), "{text}");
    }

    /// Runs the wait on its own thread with slowed page reads. The hard
    /// deadline keeps a wait that ignores its budget from hanging the suite.
    fn wait_with_slow_pages(job: WatchJob) -> (WatchJob, Waited, Duration) {
        wait_with_page_delay(job, PAGE_DELAY)
    }

    fn wait_with_page_delay(job: WatchJob, page_delay: Duration) -> (WatchJob, Waited, Duration) {
        wait_with_page_delay_and_access_budget(job, page_delay, None)
    }

    fn wait_with_page_delay_and_access_budget(
        job: WatchJob,
        page_delay: Duration,
        access_budget: Option<Duration>,
    ) -> (WatchJob, Waited, Duration) {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _io = crate::bounded_io::enter(access_budget.map(|duration| {
                crate::bounded_io::Budget::after(Duration::from_secs(30))
                    .with_access_budget(duration)
            }));
            crate::bash_background::registry::ARTIFACT_PAGE_DELAY
                .with(|delay| delay.set(Some(page_delay)));
            let started = Instant::now();
            let waited = job.wait(&crate::executor::JobCancellation::new());
            let _ = tx.send((job, waited, started.elapsed()));
        });
        // The hard deadline must outlast the longest reply budget a test gives
        // its watch (20 s), or a slow runner trips this guard while the watch
        // is still inside its own budget.
        let (job, waited, elapsed) = rx
            .recv_timeout(Duration::from_secs(60))
            .expect("watch did not answer within the test's hard deadline");
        (job, waited.expect("watch was not cancelled"), elapsed)
    }

    #[test]
    fn approved_terminal_drain_answers_within_short_reply_budget() {
        // A 1s client window leaves a 500ms reply deadline. Even one 1200ms
        // artifact read must not consume the time reserved for reply delivery.
        let client_window = Duration::from_millis(1_000);
        let (_dir, job) = budgeted_terminal_watch(client_window);
        let (job, waited, elapsed) = wait_with_page_delay(job, Duration::from_millis(1_200));
        assert!(
            elapsed + Duration::from_millis(100) < client_window,
            "reply took {elapsed:?}, too close to the {client_window:?} client deadline"
        );
        assert!(
            matches!(waited.reason, WaitReason::Timeout),
            "the backlog could not be drained inside the budget, so the scan is incomplete"
        );
        assert!(waited.matched.is_none());
        let snapshot = job
            .registry
            .observed_status(&job.task_id, &job.session, 0)
            .unwrap();
        let text = job.render(&snapshot, &waited);
        assert!(
            text.contains("task already exited; the pattern scan is incomplete"),
            "{text}"
        );
        assert!(!text.contains("still running"), "{text}");
        // An unfinished scan must not read as a completed scan that found nothing.
        assert!(!text.contains("without match"), "{text}");
        assert!(!text.contains("task exited ("), "{text}");
    }

    #[test]
    fn approved_terminal_drain_finds_last_page_match_within_long_reply_budget() {
        let (_dir, job) = budgeted_terminal_watch(Duration::from_secs(20));
        let (_job, waited, _elapsed) = wait_with_slow_pages(job);
        assert!(matches!(waited.reason, WaitReason::Matched));
        assert_eq!(
            waited.matched,
            Some(("FINAL-MATCH".into(), LAST_PAGE_MATCH, Some("stdout")))
        );
    }

    #[test]
    fn approved_terminal_drain_access_timeout_keeps_scan_incomplete() {
        let (_dir, job) = budgeted_terminal_watch(Duration::from_secs(20));
        let (job, waited, _elapsed) = wait_with_page_delay_and_access_budget(
            job,
            Duration::from_millis(150),
            Some(Duration::from_millis(50)),
        );
        assert!(
            Instant::now() < job.budget.deadline,
            "the filesystem access cap, not the reply deadline, must end the read"
        );
        assert!(matches!(waited.reason, WaitReason::Timeout));
        assert!(waited.matched.is_none());
        let snapshot = job
            .registry
            .observed_status(&job.task_id, &job.session, 0)
            .unwrap();
        let text = job.render(&snapshot, &waited);
        assert!(
            text.contains("task already exited; the pattern scan is incomplete"),
            "{text}"
        );
        assert!(!text.contains("without match"), "{text}");
        assert!(!text.contains("task exited ("), "{text}");
    }

    async fn watch_with_deadline(ctx: Arc<AppContext>, request: RawRequest) -> Response {
        watch_with_wait_deadline(ctx, request, Duration::from_secs(3)).await
    }

    async fn watch_with_wait_deadline(
        ctx: Arc<AppContext>,
        request: RawRequest,
        deadline: Duration,
    ) -> Response {
        let worker = principal(ctx.config().project_root.as_deref().unwrap());
        let wake = crate::response_finalize::DeferredResponseWake::default();
        let producer_wake = wake.clone();
        let producer_ctx = Arc::clone(&ctx);
        tokio::time::timeout(deadline, async move {
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
        .expect("worker watch must answer before the test deadline, not its 30-minute window")
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
    async fn bash_watch_unknown_id_reply_is_one_line_naming_bash() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let response = watch_with_deadline(
            context(project.path(), storage.path()),
            request(TASK, SESSION),
        )
        .await;
        assert_eq!(response.data["code"], "task_not_found", "{response:?}");
        let message = response.data["message"].as_str().unwrap();
        assert_eq!(
            message,
            format!("background task not found: {TASK}. This session has no record of it (a record can be lost across an AFT restart). To run a command, use `bash`.")
        );
        assert!(!message.contains('\n'), "{message:?}");
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
    async fn worker_watch_replies_before_short_client_deadline() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let ctx = context(project.path(), storage.path());
        let spawn: RawRequest = serde_json::from_value(json!({
            "id":"start-watch-deadline", "command":"bash", "session_id":SESSION,
            "worker_session":true,
            "params":{"command":"sleep 60", "background":true,"timeout":120_000},
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
        let mut watch = request(task, SESSION);
        watch.params["deadline_ms"] = json!(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64
                + 800
        );
        let started = Instant::now();
        let response = watch_with_wait_deadline(ctx, watch, Duration::from_secs(2)).await;
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(700),
            "reply arrived after safety cutoff: elapsed={elapsed:?}, client deadline=800ms"
        );
        assert!(response.success, "{response:?}");
        assert_eq!(response.data["status"], "running", "{response:?}");
        assert_eq!(response.data["waited"]["reason"], "timeout", "{response:?}");
        assert!(response.data["output"]
            .as_str()
            .unwrap()
            .contains("still running"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn worker_watch_explicit_timeout_is_clamped_to_configured_cap() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let ctx = context(project.path(), storage.path());
        ctx.update_config(|config| config.bash.worker_wait_max_ms = 1_000);
        let spawn: RawRequest = serde_json::from_value(json!({
            "id":"start-watch-cap", "command":"bash", "session_id":SESSION,
            "worker_session":true,
            "params":{"command":"sleep 60", "background":true, "timeout":120_000},
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
        let response = watch_with_wait_deadline(
            Arc::clone(&ctx),
            request(task, SESSION),
            Duration::from_secs(20),
        )
        .await;
        let elapsed = started.elapsed();
        assert!(response.success, "{response:?}");
        assert_eq!(response.data["status"], "running", "{response:?}");
        assert_eq!(response.data["waited"]["reason"], "timeout", "{response:?}");
        assert!(
            elapsed >= Duration::from_millis(800) && elapsed < Duration::from_secs(20),
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
    fn head_watch_defaults_and_background_requests_stay_within_sync_cap() {
        for cap in [1_000, 120_000, 1_800_000] {
            for background in [false, true] {
                let defaults = params(json!({"background":background}));
                assert_eq!(
                    effective_wait_ms(&defaults, false, 3_600_000, cap),
                    Ok(30_000.min(cap))
                );
                let explicit = params(json!({"background":background,"timeout_ms":cap}));
                assert_eq!(effective_wait_ms(&explicit, false, 3_600_000, cap), Ok(cap));
                let too_long = params(json!({"background":background,"timeout_ms":cap+1}));
                assert!(effective_wait_ms(&too_long, false, 3_600_000, cap)
                    .unwrap_err()
                    .contains("bash.watch_sync_max_ms"));
            }
        }
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
