//! Remote jobs use the ordinary task registry and its raw output artifacts.
#[cfg(unix)]
use super::*;
use crate::exec_remote::types::*;
#[cfg(unix)]
use crate::exec_remote::{
    self as exec, ExecRemoteClient, OutputSink, ResumePoint, StreamProgress, Verdict,
};
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::ffi::OsStr;
#[cfg(unix)]
use std::io::{self, Seek, SeekFrom, Write};
use std::path::PathBuf;

fn rendered_refusal_reason(reason: &str, detail: Option<&str>) -> String {
    let Some(detail) = detail else {
        return reason.to_owned();
    };
    // The runner detail is caller-visible text, so keep it bounded and single-line.
    let mut end = detail.len().min(1024);
    while !detail.is_char_boundary(end) {
        end -= 1;
    }
    let detail = detail[..end]
        .chars()
        .map(|character| {
            if character.is_control() || matches!(character, '\u{2028}' | '\u{2029}') {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    format!("{reason} ({detail})")
}

fn runon_refusal_message(reason: &str, detail: Option<&str>) -> String {
    let reason = rendered_refusal_reason(reason, detail);
    format!("runon refused: remote refused: {reason}; command was not run; retry, or omit runon to run locally")
}

fn retry_refusal_message(reason: &str, detail: Option<&str>, waited_ms: Option<u64>) -> String {
    let Some(waited_ms) = waited_ms else {
        return runon_refusal_message(reason, detail);
    };
    let reason = rendered_refusal_reason(reason, detail);
    let waited = format!(" after retrying for {}", retry_duration(waited_ms));
    format!("runon refused: remote refused: {reason}{waited}; command was not run; retry, or omit runon to run locally")
}

fn retry_duration(ms: u64) -> String {
    if ms < 1000 {
        return format!("{ms}ms");
    }
    let seconds = ms / 1000;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds % 60 == 0 {
        format!("{}m", seconds / 60)
    } else {
        format!("{}m{}s", seconds / 60, seconds % 60)
    }
}

fn retry_waited_ms(remote: &RemoteTask) -> Option<u64> {
    remote.retry.as_ref()?.waited_ms
}

/// The refusal text for a remote job. When the runner refused it for waiting
/// behind this session's own earlier job in the same workspace, say so and
/// name that job and its task instead of quoting the runner's queue report.
/// When no runner that would take the job can copy files back, say that
/// instead of suggesting a retry, which ck-motor refuses the same way.
fn remote_refusal_message(reason: &str, detail: Option<&str>, remote: &RemoteTask) -> String {
    let message = match &remote.workspace_holder {
        _ if return_paths_unsupported(reason, detail) => {
            RETURN_PATHS_UNSUPPORTED_MESSAGE.to_string()
        }
        Some(holder) if reason == "queue_wait_exceeded" => format!(
            "runon refused: your earlier job {} in this workspace is still running (task {}); this command waited behind it until the runner's queue wait limit passed and was not run; wait for task {} to end (bash_status shows it), then retry",
            holder.job_id, holder.task_id, holder.task_id
        ),
        _ => retry_refusal_message(reason, detail, retry_waited_ms(remote)),
    };
    match remote
        .job_id
        .or_else(|| remote.terminal.as_ref().map(|record| record.job_id))
    {
        Some(job_id) => format!("{message}; remote job {job_id}"),
        None => message,
    }
}

/// ck-motor's final refusal of a job with `return_paths` whose runner (any
/// Windows runner, or an older Linux one) does not copy files back: refused
/// before start as `request_rejected` with exactly this detail. It decides
/// this from the runner's Hello before sending the run, and an unchanged
/// retry gets the same answer.
const RETURN_PATHS_UNSUPPORTED_DETAIL: &str = "return_paths_unsupported";

const RETURN_PATHS_UNSUPPORTED_MESSAGE: &str = "runon refused: return_paths is not supported on the runner that would take this job; the command was not run; an unchanged retry is refused the same way";

fn return_paths_unsupported(reason: &str, detail: Option<&str>) -> bool {
    reason == "request_rejected" && detail == Some(RETURN_PATHS_UNSUPPORTED_DETAIL)
}

/// The job IDs a `queue_wait_exceeded` refusal detail names as running, when
/// the refused job was waiting on its workspace owner. ck-motor reports
/// `waiting_on=workspace owner; ...; holders=[job=<id> workspace=<key> ...; ...]; ...`;
/// the detail is quoted text, so anything else yields no IDs.
#[cfg(unix)]
fn workspace_owner_holders(detail: &str) -> Vec<Uuid> {
    let Some(rest) = detail.strip_prefix("waiting_on=workspace owner;") else {
        return Vec::new();
    };
    let Some(start) = rest.find("holders=[") else {
        return Vec::new();
    };
    let holders = &rest[start + "holders=[".len()..];
    let holders = holders.split(']').next().unwrap_or_default();
    holders
        .split(';')
        .filter_map(|entry| {
            entry
                .trim()
                .strip_prefix("job=")?
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        })
        .collect()
}

/// Derive the error from persisted remote proof, never from command output.
/// Keeping it in snapshots lets foreground and restarted/background readers
/// report the same named refusal without changing older task records.
pub(super) fn remote_refusal(metadata: &super::PersistedTask) -> Option<super::RemoteRefusal> {
    if metadata.status != super::BgTaskStatus::Failed {
        return None;
    }
    let remote = metadata.remote.as_ref().filter(|r| r.explicit_runon)?;
    if remote.terminal.is_none() && remote.job_id.is_none() {
        return Some(super::RemoteRefusal {
            code: "remote_unavailable",
            message: super::rendered_execution_note(metadata)?,
        });
    }
    let crate::exec_remote::Verdict::RunLocally {
        reason,
        refusal_detail,
    } = crate::exec_remote::grade(remote.terminal.as_ref()?)
    else {
        return None;
    };
    let reason = serde_json::to_value(reason).ok()?;
    Some(super::RemoteRefusal {
        code: "remote_unavailable",
        message: remote_refusal_message(reason.as_str()?, refusal_detail.as_deref(), remote),
    })
}

/// How long AFT keeps re-attaching after a job's own limits have passed, and
/// after the last record it received from the job. It covers the runner's
/// terminal delivery, ck-motor restarts and clock differences between machines.
#[cfg(unix)]
const REMOTE_REATTACH_GRACE: Duration = Duration::from_secs(5 * 60);

/// ck-motor's default queue wait limit, used for a request that sets no
/// `queue_wait_limit_s` (AFT never sets one).
/// Time a job waits only behind its own workspace's earlier job is not counted
/// against it, so this bounds the ordinary case, not every wait.
#[cfg(unix)]
const RUNNER_QUEUE_WAIT_LIMIT: Duration = Duration::from_secs(300);

/// The first pause before re-attaching after a lost stream; each failed attempt
/// without progress doubles it, up to [`REATTACH_BACKOFF_MAX`].
#[cfg(unix)]
const REATTACH_BACKOFF_FIRST: Duration = Duration::from_millis(100);

#[cfg(unix)]
const REATTACH_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// The pause before re-attach attempt `attempt` (0-based) since the last progress.
#[cfg(unix)]
fn reattach_backoff(attempt: u32) -> Duration {
    REATTACH_BACKOFF_FIRST
        .saturating_mul(1_u32 << attempt.min(16))
        .min(REATTACH_BACKOFF_MAX)
}

#[cfg(unix)]
const DRAINING_RETRY_BUDGET_MS: u64 = 10 * 60 * 1000;

#[cfg(unix)]
fn retries_draining(remote: &RemoteTask, verdict: &Verdict) -> bool {
    remote.explicit_runon
        && matches!(
            verdict,
            Verdict::RunLocally {
                reason: RefusalReason::RunnerDraining,
                ..
            }
        )
}

/// Refusals AFT resubmits by itself when the runner attaches a retry hint, as
/// (reason, refusal details). ck-motor hints `runner_authentication_failed`
/// only while its signer is briefly unavailable or answers badly; the details
/// that need an operator (a missing signer, a refused grant, bad credentials,
/// a stopped agent, a broken SSH config) come without a hint and are not
/// listed. Add a reason or detail here only when the runner guarantees that
/// such a refusal never ran the command and clears up by itself.
#[cfg(unix)]
const HINTED_RETRY_REFUSALS: &[(&str, &[&str])] = &[(
    "runner_authentication_failed",
    &["runner_sign_unavailable", "runner_sign_invalid_response"],
)];

/// The most times AFT resubmits one job after hinted refusals, however long
/// the runner keeps asking it to retry.
#[cfg(unix)]
const HINTED_RETRY_MAX_RESUBMITS: u32 = 5;

/// The runner's retry hint for a refusal AFT may resubmit by itself, or
/// `None` when the refusal must reach the caller as it is.
///
/// The record must say `ran: "none"`: that, with `refused_before_start`, is
/// the runner's proof that the command never started, so sending it again
/// cannot run it twice. A missing or unrecognised `ran` could hide a run.
#[cfg(unix)]
fn hinted_retry_after_ms(terminal: &TerminalRecord) -> Option<u64> {
    if terminal.ran != Some(Ran::None) {
        return None;
    }
    let Outcome::RefusedBeforeStart { reason } = &terminal.outcome else {
        return None;
    };
    let reason = String::from(reason.clone());
    let detail = terminal.refusal_detail()?;
    let listed = HINTED_RETRY_REFUSALS
        .iter()
        .any(|(listed, details)| *listed == reason && details.contains(&detail));
    if listed {
        terminal.retry_after_ms
    } else {
        None
    }
}

/// How long the runner gives a job to be admitted, counted from its first
/// submission: the request's own queue wait limit, or ck-motor's default when
/// it names none. Hinted resubmissions must fit inside it.
#[cfg(unix)]
fn hinted_retry_budget_ms(_root: &Path, request: &RunRequest) -> u64 {
    #[cfg(test)]
    if let Some(ms) = tests::hinted_retry_budget_ms(_root) {
        return ms;
    }
    request.queue_wait_limit_s.map_or_else(
        || duration_ms(RUNNER_QUEUE_WAIT_LIMIT),
        |seconds| seconds.saturating_mul(1000),
    )
}

/// What the worker that submitted a job keeps for resubmitting it after a
/// hinted refusal. Every fresh submission (the first one, or one sent after a
/// draining wait, also when a restarted daemon sends it) starts its own. It
/// lives only in that worker: a hinted refusal saved before a restart reaches
/// the caller unchanged.
#[cfg(unix)]
struct HintedRetry {
    request: RunRequest,
    /// When the runner's admission limit for the fresh submission passes, on
    /// the retry clock (Unix milliseconds outside tests).
    deadline_ms: u64,
    resubmits: u32,
}

#[cfg(unix)]
impl HintedRetry {
    /// The state for `request`, which is about to be submitted fresh: its
    /// admission time counts from now.
    fn start(root: &Path, request: RunRequest) -> Self {
        Self {
            deadline_ms: draining_now_ms(root)
                .saturating_add(hinted_retry_budget_ms(root, &request)),
            request,
            resubmits: 0,
        }
    }
}

#[cfg(unix)]
enum HintedRetryStep {
    /// Report the refusal as it is.
    Surface,
    /// The task already reached its terminal state (cancelled, or the
    /// resubmission could not be sent).
    Ended,
    Resubmitted(exec::RemoteStream),
}

#[cfg(unix)]
fn draining_delay_ms(attempt: u32, retry_after_ms: Option<u64>) -> u64 {
    retry_after_ms.unwrap_or_else(|| {
        5000_u64
            .saturating_mul(1_u64 << attempt.saturating_sub(1).min(4))
            .min(60000)
    })
}

#[cfg(unix)]
fn draining_budget_ms(_root: &Path) -> u64 {
    #[cfg(test)]
    if let Some(ms) = tests::draining_budget_ms(_root) {
        return ms;
    }
    DRAINING_RETRY_BUDGET_MS
}

#[cfg(unix)]
fn draining_now_ms(_root: &Path) -> u64 {
    #[cfg(test)]
    if let Some(clock) = tests::draining_clock(_root) {
        return clock.now();
    }
    unix_millis()
}

#[cfg(unix)]
async fn draining_retry_sleep(_root: &Path, _now: u64, delay: u64) {
    #[cfg(test)]
    if let Some(clock) = tests::draining_clock(_root) {
        clock.sleep_until(_now, _now.saturating_add(delay)).await;
        return;
    }
    tokio::time::sleep(Duration::from_millis(delay)).await;
}

/// The remote runner AFT dispatches to; named in every remote reply header.
#[cfg(unix)]
const RUNNER_ID: &str = "ck-motor";

#[cfg(unix)]
fn platform_acknowledged(remote: &RemoteTask) -> bool {
    remote.requested_platform != Some(Platform::Windows)
        || (remote.accepted && remote.accepted_platform == Some(Platform::Windows))
}

#[cfg(unix)]
fn remote_execution_note(remote: &RemoteTask) -> String {
    let job_id = remote
        .job_id
        .or_else(|| remote.terminal.as_ref().map(|record| record.job_id));
    let mut details = Vec::new();
    if let Some(count) = remote.requested_vcpus {
        details.push(if count == 1 {
            "1 vCPU requested".to_string()
        } else {
            format!("{count} vCPUs requested")
        });
    }
    if let Some(job_id) = job_id {
        details.push(format!("job {job_id}"));
    }
    if remote.requested_network {
        details.push("network".to_string());
    }
    let mut note = if details.is_empty() {
        format!("ran remotely on {RUNNER_ID}")
    } else {
        format!("ran remotely on {RUNNER_ID} ({})", details.join(", "))
    };
    if remote.requested_network && remote.accepted && !remote.granted_network {
        note.push_str("\nOFFLINE: the runner did not grant the requested network access");
    }
    note
}

/// Only an offline remote job is safe to repeat without checking outside effects.
/// Local fallback has its own warnings and never uses this advice.
///
/// The runner has no operation an agent can call to fetch a job AFT stopped
/// following, so the advice names the job and what to do instead: wait for it
/// to end by its own run timeout, then rerun.
#[cfg(unix)]
fn remote_unknown_outcome(
    job_id: Option<Uuid>,
    granted_network: bool,
    run_timeout_ms: Option<u64>,
) -> String {
    let mut text = "remote outcome unknown".to_owned();
    if let Some(job_id) = job_id {
        text.push_str(&format!(" (job {job_id})"));
    }
    if granted_network {
        text.push_str("; the job may have had outside effects through outbound network access, so check before rerunning");
    } else {
        text.push_str(
            "; the remote job could not affect this machine or the network, so rerunning is safe",
        );
    }
    if let Some(job_id) = job_id {
        let limit = run_timeout_ms.map_or_else(
            || "until it ends".to_owned(),
            |ms| format!("for up to its run timeout of {}", retry_duration(ms)),
        );
        text.push_str(&format!(
            "; job {job_id} may still be running on {RUNNER_ID} and holding this workspace {limit}, and AFT can no longer fetch its result: a rerun in this workspace waits behind it and can be refused with queue_wait_exceeded, so wait for it to end, then rerun"
        ));
    }
    text
}

/// This machine's operating system as a reader names it, for the header of a
/// run that fell back to it.
#[cfg(unix)]
fn local_os_name() -> &'static str {
    match std::env::consts::OS {
        "macos" => "macOS",
        "linux" => "Linux",
        "freebsd" => "FreeBSD",
        other => other,
    }
}

/// Bounds how long AFT keeps re-attaching to an accepted job whose stream it
/// lost. Silence on an open stream is never bounded; only recovery attempts
/// that bring no record are. While the job may still be queued or running on
/// the runner, AFT keeps trying: it stops only once the job's own limits (the
/// runner's queue wait limit plus the command's run timeout, counted from AFT's
/// latest submission of it) have passed, and never sooner than [`REMOTE_REATTACH_GRACE`]
/// after recovery began. Accepted jobs can disappear with a wiped runner state;
/// AFT must not resubmit them.
#[cfg(unix)]
struct ReattachBudget {
    grace: Duration,
    /// When the job's own limits have passed.
    job_end: tokio::time::Instant,
    /// Human-readable summary of those limits, for the give-up message.
    limit_note: String,
    deadline: Option<tokio::time::Instant>,
    /// Failed recovery attempts since the last record; selects the next pause.
    attempts: u32,
    /// A test shortened the budget: the job's limits stay treated as passed.
    #[cfg(test)]
    budget_override: bool,
    #[cfg(test)]
    clock: Option<Arc<tests::ReattachClock>>,
    #[cfg(test)]
    clock_job_end: Option<Duration>,
    #[cfg(test)]
    clock_deadline: Option<Duration>,
}

#[cfg(unix)]
impl ReattachBudget {
    fn new(_root: &Path, metadata: &PersistedTask) -> Self {
        let queue_wait = RUNNER_QUEUE_WAIT_LIMIT;
        let (until_job_end, recorded_timeout, run_timeout) = Self::until_job_end(metadata);
        let grace = REMOTE_REATTACH_GRACE;
        // A shortened test budget stands for the whole allowance: the job's
        // limits are treated as already passed.
        #[cfg(test)]
        let budget_override = tests::take_reattach_budget(_root);
        #[cfg(test)]
        let (grace, until_job_end) = match budget_override {
            Some(budget) => (budget, Duration::ZERO),
            None => (grace, until_job_end),
        };
        let limit_note = format!(
            "ck-motor's queue wait limit of {} and its {}run timeout of {} have passed since AFT submitted it, plus {} of grace",
            retry_duration(duration_ms(queue_wait)),
            if recorded_timeout.is_some() { "" } else { "assumed " },
            retry_duration(duration_ms(run_timeout)),
            retry_duration(duration_ms(grace)),
        );
        #[cfg(test)]
        let clock = tests::reattach_clock(_root);
        Self {
            grace,
            job_end: tokio::time::Instant::now() + until_job_end,
            limit_note,
            deadline: None,
            attempts: 0,
            #[cfg(test)]
            budget_override: budget_override.is_some(),
            #[cfg(test)]
            clock_job_end: clock.as_ref().map(|clock| clock.now() + until_job_end),
            #[cfg(test)]
            clock,
            #[cfg(test)]
            clock_deadline: None,
        }
    }

    /// How long until the runner's own limits for the task's latest
    /// submission have passed, with the recorded run timeout (if any) and the
    /// run timeout assumed from it.
    ///
    /// The limits count from the latest submission: a resubmission after a
    /// draining refusal starts a new queue wait and run on the runner, which
    /// can come many minutes after the task first started. A record without a
    /// resubmission time counts from the task's start.
    fn until_job_end(metadata: &PersistedTask) -> (Duration, Option<Duration>, Duration) {
        // A remote task records the run timeout AFT sent with the job: the
        // caller's `timeout`, or the 30-minute background default when the
        // call named none. A record without one predates that; assume the
        // default, the most AFT has sent for an untimed call.
        let recorded_timeout = metadata.timeout_ms.map(Duration::from_millis);
        let run_timeout = recorded_timeout.unwrap_or(DEFAULT_BG_TIMEOUT);
        let submitted_at = metadata
            .remote
            .as_ref()
            .and_then(|remote| remote.retry.as_ref())
            .and_then(|retry| retry.resubmitted_at_ms)
            .unwrap_or(metadata.started_at);
        let job_end_ms = submitted_at
            .saturating_add(duration_ms(RUNNER_QUEUE_WAIT_LIMIT))
            .saturating_add(duration_ms(run_timeout));
        (
            Duration::from_millis(job_end_ms.saturating_sub(unix_millis())),
            recorded_timeout,
            run_timeout,
        )
    }

    /// The job was submitted again after a draining refusal. Its limits now
    /// count from that submission, recorded in `metadata`, and a recovery
    /// deadline fixed for the earlier submission no longer applies.
    fn resubmitted(&mut self, metadata: &PersistedTask) {
        let (until_job_end, _, _) = Self::until_job_end(metadata);
        #[cfg(test)]
        let until_job_end = if self.budget_override {
            Duration::ZERO
        } else {
            until_job_end
        };
        self.job_end = tokio::time::Instant::now() + until_job_end;
        self.deadline = None;
        #[cfg(test)]
        {
            self.clock_job_end = self.clock.as_ref().map(|clock| clock.now() + until_job_end);
            self.clock_deadline = None;
        }
    }

    /// A record arrived: the job is alive, so recovery starts over.
    fn reset(&mut self) {
        self.deadline = None;
        self.attempts = 0;
        #[cfg(test)]
        if let Some(clock) = &self.clock {
            self.clock_deadline = None;
            clock.resets.send_modify(|resets| *resets += 1);
        }
    }

    fn expired(&self, job_id: Uuid) -> String {
        format!(
            "remote outcome unknown: job {job_id} could not be re-attached before its own time limit passed ({}); command not rerun",
            self.limit_note
        )
    }

    fn check(&self, job_id: Uuid) -> Result<(), String> {
        #[cfg(test)]
        if let Some(clock) = &self.clock {
            return if self
                .clock_deadline
                .is_some_and(|deadline| clock.now() >= deadline)
            {
                Err(self.expired(job_id))
            } else {
                Ok(())
            };
        }
        if self
            .deadline
            .is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
        {
            return Err(self.expired(job_id));
        }
        Ok(())
    }

    /// The deadline is fixed when recovery begins: the later of the job's own
    /// end and now, plus the grace period.
    fn deadline(&mut self) -> tokio::time::Instant {
        let (job_end, grace) = (self.job_end, self.grace);
        *self.deadline.get_or_insert_with(|| {
            let now = tokio::time::Instant::now();
            job_end.max(now) + grace
        })
    }

    async fn wait<T>(
        &mut self,
        job_id: Uuid,
        attempt: impl std::future::Future<Output = T>,
    ) -> Result<T, String> {
        self.check(job_id)?;
        #[cfg(test)]
        if let Some(clock) = self.clock.clone() {
            let (job_end, grace) = (self.clock_job_end, self.grace);
            let deadline = *self.clock_deadline.get_or_insert_with(|| {
                let now = clock.now();
                job_end.map_or(now, |end| end.max(now)) + grace
            });
            clock.waits.send_modify(|waits| *waits += 1);
            let mut ticks = clock.ticks.subscribe();
            return tokio::select! {
                value = attempt => Ok(value),
                _ = ticks.wait_for(|now| *now >= deadline) => Err(self.expired(job_id)),
            };
        }
        let deadline = self.deadline();
        tokio::time::timeout_at(deadline, attempt)
            .await
            .map_err(|_| self.expired(job_id))
    }

    /// Pause before the next recovery attempt, backing off while attempts keep
    /// failing. Fails once the deadline passes.
    async fn pause(&mut self, job_id: Uuid) -> Result<(), String> {
        let delay = reattach_backoff(self.attempts);
        self.attempts = self.attempts.saturating_add(1);
        #[cfg(test)]
        if self.clock.is_some() {
            // Paused-clock tests drive only the deadline; keep real pauses short
            // so a test can observe many attempts. `reattach_backoff` has its own test.
            let _ = delay;
            return self
                .wait(job_id, tokio::time::sleep(Duration::from_millis(5)))
                .await;
        }
        self.wait(job_id, tokio::time::sleep(delay)).await
    }
}

#[cfg(unix)]
fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct DrainingRetry {
    request_digest: String,
    attempts: u32,
    first_refusal_at_ms: Option<u64>,
    next_retry_at_ms: Option<u64>,
    waited_ms: Option<u64>,
    /// When AFT last submitted the job again after a draining refusal, in
    /// Unix milliseconds. The runner's queue wait and run timeout count from
    /// that submission, so re-attach limits do too. Records written before
    /// this field existed lack it and count from the task's start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resubmitted_at_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RemoteOutputStream {
    Stdout,
    Stderr,
    Unknown,
}

impl RemoteOutputStream {
    fn label(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
            Self::Unknown => "remote-unknown-output",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RemoteOutputShortStream {
    pub stream: RemoteOutputStream,
    pub actual_bytes: u64,
    pub checkpoint_bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RemoteOutputIncompleteReason {
    CheckpointShort,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RemoteOutputIncomplete {
    pub reason: RemoteOutputIncompleteReason,
    pub streams: Vec<RemoteOutputShortStream>,
}

impl RemoteOutputIncomplete {
    pub(super) fn warning_line(&self) -> String {
        let reason = match self.reason {
            RemoteOutputIncompleteReason::CheckpointShort => "remote_output_checkpoint_short",
        };
        let streams = self
            .streams
            .iter()
            .map(|loss| {
                format!(
                    "{} has {} of {} checkpointed bytes",
                    loss.stream.label(),
                    loss.actual_bytes,
                    loss.checkpoint_bytes
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        format!(
            "Output incomplete ({reason}): {streams}; missing bytes cannot be mapped to replay sequences."
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct RemoteTask {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_platform: Option<Platform>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_platform: Option<Platform>,
    pub connection_file: Option<PathBuf>,
    #[serde(default)]
    pub explicit_runon: bool,
    pub harness: String,
    pub session: String,
    #[serde(default)]
    pub requested_vcpus: Option<u32>,
    #[serde(default)]
    pub requested_network: bool,
    #[serde(default)]
    pub granted_network: bool,
    #[serde(default)]
    pub accepted: bool,
    #[serde(default)]
    pub started: Option<Started>,
    pub job_id: Option<Uuid>,
    pub last_seq: Option<u64>,
    #[serde(default)]
    pub gap_recovery: Option<crate::exec_remote::GapRecovery>,
    #[serde(default)]
    pub undelivered_output: Vec<(u64, u64)>,
    pub stdout_len: u64,
    pub stderr_len: u64,
    #[serde(default)]
    pub unknown_len: u64,
    pub cancel_requested: bool,
    pub terminal: Option<TerminalRecord>,
    #[serde(default)]
    pub fallback_digest: Option<String>,
    #[serde(default)]
    pub env_not_forwarded: Option<Vec<String>>,
    #[serde(default)]
    pub retry: Option<DrainingRetry>,
    /// The queue position the runner reported when it accepted the job. It is
    /// reported once and never updated, so it can only fall afterwards.
    #[serde(default)]
    pub queue_position: Option<u32>,
    /// Output proves execution started even when an older runner sends no
    /// `Started` record.
    #[serde(default)]
    pub output_received: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_incomplete: Option<RemoteOutputIncomplete>,
    /// Set when the runner refused this job with `queue_wait_exceeded` while it
    /// waited behind an earlier job of this same session in the same workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_holder: Option<WorkspaceHolder>,
    /// Files the caller asked the runner to copy back, relative to the
    /// task's workdir (`return_paths`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub return_paths: Vec<String>,
    /// The outcome for each of `return_paths`, saved once the runner's
    /// manifest was handled or the job ended without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub returned_files: Option<Vec<super::return_files::ReturnedPath>>,
    /// The seq of the job's first returned-file chunk. A restart before the
    /// manifest attaches again from here, because the staged bytes did not
    /// survive the restart but the runner still replays them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub return_first_seq: Option<u64>,
    /// Staging files and directories created for the returned files and
    /// not yet placed or removed. A restart removes them from here.
    #[serde(
        default,
        skip_serializing_if = "super::return_files::StagingRecord::is_empty"
    )]
    pub return_staging: super::return_files::StagingRecord,
}

/// This session's earlier remote job that still held the workspace when the
/// runner refused a later job in it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WorkspaceHolder {
    pub job_id: Uuid,
    pub task_id: String,
}

/// Where a remote job is, as far as its own stream has shown. Used to tell a
/// caller whose blocking wait ended what the job is doing now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RemotePhase {
    /// Inputs are still being submitted; the runner has not accepted the job.
    Submitting,
    /// Accepted, but no queue position was retained across a restart.
    WaitingOnRunner,
    /// Accepted at this queue position, and nothing has shown it started.
    Queued { position: u32 },
    /// The command has started on the runner.
    Running,
}

impl RemotePhase {
    /// The one place that decides a remote job's phase.
    ///
    /// New runners explicitly report `Started`; output remains evidence of
    /// execution for older runners that do not send that record.
    pub(crate) fn of(remote: &RemoteTask) -> Self {
        if !remote.accepted {
            Self::Submitting
        } else if remote.started.is_some() || remote.output_received {
            Self::Running
        } else if let (Some(_), Some(position)) = (remote.job_id, remote.queue_position) {
            Self::Queued { position }
        } else {
            Self::WaitingOnRunner
        }
    }

    /// A stable machine-readable name for replies.
    pub(crate) fn tag(self) -> &'static str {
        match self {
            Self::Submitting => "submitting",
            Self::WaitingOnRunner => "waiting_on_runner",
            Self::Queued { .. } => "queued",
            Self::Running => "running",
        }
    }

    /// The phase in words, naming the runner.
    pub(crate) fn describe(self) -> String {
        match self {
            Self::Submitting => "submitting to ck-motor (capturing inputs; not accepted yet)".to_string(),
            Self::WaitingOnRunner => {
                "waiting on the runner (ck-motor has not reported a queue position or any output yet)"
                    .to_string()
            }
            Self::Queued { position } => format!(
                "queued at position {position} on ck-motor (the position when the job was accepted; no output yet)"
            ),
            Self::Running => "running on ck-motor (execution started)".to_string(),
        }
    }
}

/// A remote task's job ID, once the runner assigned one, and its phase.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoteProgress {
    pub job_id: Option<Uuid>,
    pub phase: RemotePhase,
}

impl RemoteProgress {
    pub(crate) fn of(remote: &RemoteTask) -> Self {
        Self {
            job_id: remote.job_id,
            phase: RemotePhase::of(remote),
        }
    }
}

/// Where a still-running remote task is, for bash_status and bash_watch:
/// `submitting to the runner (capturing inputs)` before acceptance, then
/// `queued on the runner (N s, position P)` until the runner reports that the
/// command started, then `the runner stops it N s after it started`, because
/// the runner enforces the timeout from start, not from queue entry.
///
/// It is read from the task's persisted record, so it reads the same after an
/// AFT restart. `None` for a local task, a finished task, or a final refusal;
/// a draining refusal awaiting retry is still submitting inputs.
pub(crate) fn remote_phase_note(metadata: &super::PersistedTask, now_ms: u64) -> Option<String> {
    if metadata.is_terminal() || metadata.child_pid.is_some() {
        return None;
    }
    let remote = metadata
        .remote
        .as_ref()
        .filter(|remote| remote.terminal.is_none() || !remote.accepted && remote.retry.is_some())?;
    let phase = RemotePhase::of(remote);
    if phase == RemotePhase::Submitting {
        return Some("submitting to the runner (capturing inputs; not accepted yet)".to_string());
    }
    if phase == RemotePhase::Running {
        // The request asks the runner for this many whole seconds (see
        // `spawn_remote`), so round up the same way.
        return Some(match metadata.timeout_ms {
            Some(ms) => format!(
                "the runner stops it {} s after it started",
                ms.div_ceil(1000)
            ),
            None => "running on the runner".to_string(),
        });
    }
    // The persisted task creation time includes submission as well as queueing.
    let queued_s = now_ms.saturating_sub(metadata.started_at) / 1000;
    Some(match phase {
        RemotePhase::Queued { position } => {
            format!("queued on the runner ({queued_s} s, position {position})")
        }
        _ => format!("queued on the runner ({queued_s} s)"),
    })
}

#[cfg(unix)]
impl RemoteTask {
    fn point(&self) -> Option<ResumePoint> {
        // Returned files are staged only in memory and private files that a
        // restart discards, so resume before their first chunk until the
        // manifest has been handled. This holds during gap recovery too: the
        // saved terminal proof stays with the resume point, and the records
        // buffered above the gap stay pending, while the chunks before the
        // gap are fetched again.
        let last_seq = match (self.return_first_seq, &self.returned_files) {
            (Some(first), None) => self.last_seq.min(first.checked_sub(1)),
            _ => self.last_seq,
        };
        Some(ResumePoint {
            job_id: self.job_id?,
            last_seq,
            gap_recovery: self.gap_recovery.clone(),
        })
    }
}

#[cfg(unix)]
#[derive(Default)]
struct DirtyOutput {
    stdout: bool,
    stderr: bool,
    unknown: bool,
}

#[cfg(unix)]
impl DirtyOutput {
    fn sync(
        &mut self,
        stdout: &fs::File,
        stderr: &fs::File,
        unknown: Option<&fs::File>,
    ) -> io::Result<()> {
        fn sync_dirty(file: &fs::File, dirty: &mut bool) -> io::Result<()> {
            if *dirty {
                super::super::persistence::sync_task_file(file)?;
                *dirty = false;
            }
            Ok(())
        }
        sync_dirty(stdout, &mut self.stdout)?;
        sync_dirty(stderr, &mut self.stderr)?;
        if let Some(file) = unknown {
            sync_dirty(file, &mut self.unknown)?;
        }
        Ok(())
    }
}

#[cfg(unix)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct OutputProgress {
    last_seq: Option<u64>,
    stdout_len: u64,
    stderr_len: u64,
    unknown_len: u64,
}

#[cfg(unix)]
impl OutputProgress {
    fn of(remote: &RemoteTask) -> Self {
        Self {
            last_seq: remote.last_seq,
            stdout_len: remote.stdout_len,
            stderr_len: remote.stderr_len,
            unknown_len: remote.unknown_len,
        }
    }

    fn store(self, remote: &mut RemoteTask) {
        remote.last_seq = self.last_seq;
        remote.stdout_len = self.stdout_len;
        remote.stderr_len = self.stderr_len;
        remote.unknown_len = self.unknown_len;
    }
}

#[cfg(unix)]
struct TaskSink {
    registry: BgTaskRegistry,
    task: Arc<BgTask>,
    stdout: fs::File,
    stderr: fs::File,
    unknown: Option<fs::File>,
    dirty: DirtyOutput,
    // Keep newly received byte counts and sequence numbers out of shared task
    // metadata until the files are synced. Cancellation also saves that metadata.
    output_progress: OutputProgress,
    last_progress_persist: Instant,
    #[cfg(test)]
    progress_clock: Option<Arc<dyn Fn() -> Instant + Send + Sync>>,
    /// Runs between the checkpoint snapshot and its file write, so a test can
    /// land another metadata write in that window.
    #[cfg(test)]
    before_checkpoint_write: Option<Box<dyn FnMut() + Send>>,
    /// Stages the job's returned-file chunks until the runner's manifest
    /// arrives, then places the files that verify.
    returns: Option<super::return_files::ReturnReceiver>,
    /// A handled manifest's report whose save failed, saved when the
    /// consumer delivers the manifest again.
    unsaved_return_report: Option<Vec<super::return_files::ReturnedPath>>,
}

/// Longest time the saved output cursor of a running remote task may lag the
/// output already received, unless a state change saves it sooner.
///
/// The saved cursor is read only when a daemon restart (or a host crash)
/// resumes the task: recovery truncates the output files to the saved byte
/// lengths and attaches from the saved sequence number, so the runner sends
/// again everything received after the last checkpoint. A longer interval
/// means fewer file syncs while output streams in, and a longer replay after
/// a restart. It assumes the runner keeps at least this much recent output
/// for reattach, which five seconds is far inside. If the runner has trimmed
/// some of it, the replay starts with a truncation notice and the missing
/// records are reported as lost output, never skipped silently. State
/// changes (acceptance, start, terminal proof, truncation, gap recovery) are
/// saved at once and do not wait for this interval.
#[cfg(unix)]
const PROGRESS_CHECKPOINT_INTERVAL: Duration = Duration::from_secs(5);

#[cfg(unix)]
impl TaskSink {
    fn new(registry: &BgTaskRegistry, task: Arc<BgTask>) -> io::Result<Self> {
        let (remote, workdir, project_root) = {
            let state = task
                .state
                .lock()
                .map_err(|_| io::Error::other("task lock poisoned"))?;
            (
                state
                    .metadata
                    .remote
                    .clone()
                    .ok_or_else(|| io::Error::other("remote record absent"))?,
                state.metadata.workdir.clone(),
                state.metadata.project_root.clone(),
            )
        };
        // Staged bytes do not survive a restart: remove the files an earlier
        // AFT process staged, from the places it recorded. A restart before
        // the manifest attaches again from the first chunk to stage afresh.
        super::return_files::remove_recorded_staging(&remote.return_staging, &task.task_id);
        let returns = if !remote.return_paths.is_empty() && remote.returned_files.is_none() {
            Some(super::return_files::ReturnReceiver::new(
                remote.return_paths.clone(),
                exec::return_max_bytes(),
                &workdir,
                project_root.as_deref().unwrap_or(&workdir),
                worktree_root_containing(&workdir).ok().flatten().as_deref(),
                // Staged bytes live in the task's private control folder,
                // which sandboxed commands cannot write.
                resolve_task_layout(&task.paths.session_dir, &task.task_id)?
                    .dirs
                    .control
                    .clone(),
                &task.task_id,
            )?)
        } else {
            None
        };
        let stdout = open_task_artifact_for_remote(&task, TaskArtifact::Stdout)?;
        let stderr = open_task_artifact_for_remote(&task, TaskArtifact::Stderr)?;
        // Discard bytes beyond the saved checkpoint, but never extend a short
        // file. The saved sequence number has no byte offsets for replaying its tail.
        fn restore_length(file: &fs::File, checkpoint: u64) -> io::Result<(u64, bool)> {
            let actual = file.metadata()?.len();
            if actual > checkpoint {
                file.set_len(checkpoint)?;
            }
            Ok((actual.min(checkpoint), actual > checkpoint))
        }
        let (stdout_len, stdout_dirty) = restore_length(&stdout, remote.stdout_len)?;
        let (stderr_len, stderr_dirty) = restore_length(&stderr, remote.stderr_len)?;
        let layout = resolve_task_layout(&task.paths.session_dir, &task.task_id)?;
        let unknown = match layout
            .dirs
            .io
            .open_file(OsStr::new("remote-unknown-output"), true)
        {
            Ok(file) => Some(file),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let (unknown_len, unknown_dirty) = unknown
            .as_ref()
            .map(|file| restore_length(file, remote.unknown_len))
            .transpose()?
            .unwrap_or((0, false));
        let mut sink = Self {
            registry: registry.clone(),
            task,
            stdout,
            stderr,
            unknown,
            dirty: DirtyOutput {
                stdout: stdout_dirty,
                stderr: stderr_dirty,
                unknown: unknown_dirty,
            },
            output_progress: OutputProgress {
                stdout_len,
                stderr_len,
                unknown_len,
                ..OutputProgress::of(&remote)
            },
            last_progress_persist: Instant::now(),
            #[cfg(test)]
            progress_clock: None,
            #[cfg(test)]
            before_checkpoint_write: None,
            returns,
            unsaved_return_report: None,
        };
        let losses: Vec<_> = [
            (RemoteOutputStream::Stdout, stdout_len, remote.stdout_len),
            (RemoteOutputStream::Stderr, stderr_len, remote.stderr_len),
            (RemoteOutputStream::Unknown, unknown_len, remote.unknown_len),
        ]
        .into_iter()
        .filter(|(_, actual, checkpoint)| actual < checkpoint)
        .map(
            |(stream, actual_bytes, checkpoint_bytes)| RemoteOutputShortStream {
                stream,
                actual_bytes,
                checkpoint_bytes,
            },
        )
        .collect();
        if !losses.is_empty() {
            sink.dirty
                .sync(&sink.stdout, &sink.stderr, sink.unknown.as_ref())?;
            let mut db = DeferredDbWrites::new(registry, &sink.task);
            let mut state = sink
                .task
                .state
                .lock()
                .map_err(|_| io::Error::other("task lock poisoned"))?;
            let previous = state.metadata.clone();
            let remote = state.metadata.remote.as_mut().unwrap();
            remote.stdout_len = stdout_len;
            remote.stderr_len = stderr_len;
            remote.unknown_len = unknown_len;
            let incomplete =
                remote
                    .output_incomplete
                    .get_or_insert_with(|| RemoteOutputIncomplete {
                        reason: RemoteOutputIncompleteReason::CheckpointShort,
                        streams: Vec::new(),
                    });
            for loss in losses {
                if !incomplete
                    .streams
                    .iter()
                    .any(|saved| saved.stream == loss.stream)
                {
                    incomplete.streams.push(loss);
                }
            }
            // Keep the saved sequence number and any known command outcome.
            // Replaying earlier records would duplicate bytes in intact streams;
            // no per-record byte offsets are saved to separate them from lost bytes.
            if let Err(error) = registry.persist_task_locked(&sink.task, &state.metadata, &mut db) {
                state.metadata = previous;
                return Err(error);
            }
        }
        if !remote.return_staging.is_empty() {
            sink.commit(|r| r.return_staging = Default::default())?;
        }
        Ok(sink)
    }

    #[cfg(test)]
    fn set_progress_clock_for_test(&mut self, clock: Arc<dyn Fn() -> Instant + Send + Sync>) {
        self.last_progress_persist = clock();
        self.progress_clock = Some(clock);
    }

    fn progress_now(&self) -> Instant {
        #[cfg(test)]
        if let Some(clock) = &self.progress_clock {
            return clock();
        }
        Instant::now()
    }

    fn progress_elapsed(&self) -> Duration {
        #[cfg(test)]
        if let Some(clock) = &self.progress_clock {
            return clock().saturating_duration_since(self.last_progress_persist);
        }
        self.last_progress_persist.elapsed()
    }

    fn commit(&mut self, update: impl FnOnce(&mut RemoteTask)) -> io::Result<()> {
        // Acceptance, start, and terminal records save output progress too, so
        // their metadata must not become durable before the output files.
        self.dirty
            .sync(&self.stdout, &self.stderr, self.unknown.as_ref())?;
        let mut db = DeferredDbWrites::new(&self.registry, &self.task);
        let mut state = self
            .task
            .state
            .lock()
            .map_err(|_| io::Error::other("task lock poisoned"))?;
        let previous = state.metadata.remote.clone();
        let remote = state
            .metadata
            .remote
            .as_mut()
            .ok_or_else(|| io::Error::other("remote record absent"))?;
        self.output_progress.store(remote);
        update(remote);
        let result = self
            .registry
            .persist_task_locked(&self.task, &state.metadata, &mut db);
        if result.is_err() {
            state.metadata.remote = previous;
        } else {
            self.output_progress = OutputProgress::of(state.metadata.remote.as_ref().unwrap());
            self.last_progress_persist = self.progress_now();
            self.task.publish_runon_launch_state(&state);
        }
        result
    }

    fn progress(&mut self, seq: u64, stream: Option<OutputStream>, len: u64) -> io::Result<()> {
        let previous = self.output_progress;
        match stream {
            Some(OutputStream::Stdout) => self.output_progress.stdout_len = len,
            Some(OutputStream::Stderr) => self.output_progress.stderr_len = len,
            None => self.output_progress.unknown_len = len,
            Some(_) => unreachable!("unknown streams use unknown_output"),
        }
        self.output_progress.last_seq = Some(seq);
        let became_running = {
            let mut state = self
                .task
                .state
                .lock()
                .map_err(|_| io::Error::other("task lock poisoned"))?;
            let remote = state.metadata.remote.as_mut().unwrap();
            let was_running = remote.started.is_some() || remote.output_received;
            if was_running {
                remote.output_received |= stream.is_some();
            }
            !was_running && stream.is_some()
        };
        let result = if became_running {
            // Older runners omit Started. Their first output is still a
            // running transition, saved durably at once like Started is.
            self.commit(|remote| remote.output_received = true)
        } else if self.progress_elapsed() < PROGRESS_CHECKPOINT_INTERVAL {
            return Ok(());
        } else {
            self.checkpoint_progress()
        };
        if result.is_err() {
            // The stream consumer has not advanced past this record and will
            // deliver it again, so its bytes are written again at the same offset.
            self.output_progress = previous;
        }
        result
    }

    /// Saves the output cursor alone, without holding the task's state lock
    /// across the file syncs and the metadata write.
    fn checkpoint_progress(&mut self) -> io::Result<()> {
        // A saved cursor must never cover output bytes that are not yet on
        // disk: after a restart the output files are cut to the saved lengths.
        self.dirty
            .sync(&self.stdout, &self.stderr, self.unknown.as_ref())?;
        let progress = self.output_progress;
        let (snapshot, json_writes_seen, previous) = {
            let mut state = self
                .task
                .state
                .lock()
                .map_err(|_| io::Error::other("task lock poisoned"))?;
            let remote = state
                .metadata
                .remote
                .as_mut()
                .ok_or_else(|| io::Error::other("remote record absent"))?;
            let previous = OutputProgress::of(remote);
            // The output is synced, so any other writer (cancel, completion
            // delivery) may now save this cursor too.
            progress.store(remote);
            (
                state.metadata.clone(),
                BgTaskRegistry::json_writes_seen(&self.task),
                previous,
            )
        };
        #[cfg(test)]
        if let Some(hook) = self.before_checkpoint_write.as_mut() {
            hook();
        }
        let result = self
            .registry
            .persist_task_checkpoint(&self.task, &snapshot, json_writes_seen);
        match result {
            Ok(_) => {
                self.last_progress_persist = self.progress_now();
                Ok(())
            }
            Err(error) => {
                if let Ok(mut state) = self.task.state.lock() {
                    if let Some(remote) = state.metadata.remote.as_mut() {
                        if OutputProgress::of(remote) == progress {
                            previous.store(remote);
                        }
                    }
                }
                Err(error)
            }
        }
    }
}

#[cfg(unix)]
fn open_task_artifact_for_remote(task: &BgTask, artifact: TaskArtifact) -> io::Result<fs::File> {
    if let Some(handles) = &task
        .state
        .lock()
        .map_err(|_| io::Error::other("task lock poisoned"))?
        .io_handles
    {
        return handles.clone_file(artifact);
    }
    // Replay uses the same pinned-directory validation as local artifact reads.
    let resolved = resolve_task_layout(&task.paths.session_dir, &task.task_id)?;
    resolved
        .dirs
        .io
        .open_file(OsStr::new(artifact.file_name()), true)
}

#[cfg(unix)]
fn terminal_outcome(
    verdict: Verdict,
    error: Option<String>,
) -> (BgTaskStatus, Option<i64>, Option<String>) {
    match verdict {
        Verdict::Exited { code } => (
            if code == 0 {
                BgTaskStatus::Completed
            } else {
                BgTaskStatus::Failed
            },
            Some(code),
            None,
        ),
        Verdict::Signalled { signal } => (
            BgTaskStatus::Failed,
            128_i32.checked_add(signal).map(i64::from),
            Some(format!("remote signal {signal}")),
        ),
        Verdict::DeadlineKilled => (
            BgTaskStatus::TimedOut,
            Some(124),
            Some("remote deadline kill".into()),
        ),
        Verdict::Cancelled | Verdict::CancelKilled => {
            (BgTaskStatus::Killed, None, Some("remote cancel".into()))
        }
        Verdict::HistoryExpired => (
            BgTaskStatus::FateUnknown,
            None,
            Some("remote history_expired; prior outcome unavailable; command not rerun".into()),
        ),
        Verdict::RunLocally { .. } => (BgTaskStatus::Failed, None, error),
        _ => (
            BgTaskStatus::FateUnknown,
            None,
            error.or_else(|| {
                Some("remote terminal proof reported outcome unknown; command not rerun".into())
            }),
        ),
    }
}

#[cfg(unix)]
pub(super) fn terminal_metadata_from_proof(metadata: &mut PersistedTask) -> Option<()> {
    let record = metadata.remote.as_ref()?.terminal.as_ref()?;
    let verdict = exec::grade(record);
    // A refusal to start can still trigger a draining retry or local fallback;
    // it is not a completed run that the read-only disk fallback may settle.
    if matches!(verdict, Verdict::RunLocally { .. }) {
        return None;
    }
    let (status, code, reason) = terminal_outcome(verdict, None);
    metadata.mark_terminal(status, code, reason);
    Some(())
}

#[cfg(unix)]
pub(super) fn persist_terminal_artifacts(paths: &TaskPaths, metadata: &mut PersistedTask) {
    use super::super::persistence::{persist_terminal_exit, record_terminal_artifact};
    persist_terminal_exit(paths, metadata);
    let statuses = metadata
        .remote
        .as_ref()
        .and_then(|remote| remote.terminal.as_ref())
        .and_then(|terminal| terminal.pipestatus.as_ref())
        .filter(|statuses| !statuses.is_empty());
    let content = statuses.map(|statuses| {
        format!(
            "{}\n",
            statuses
                .iter()
                .map(i32::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
    });
    metadata.pipeline_status_unavailable = match record_terminal_artifact(
        paths,
        TaskArtifact::PipelineStatus,
        content.as_deref().map(str::as_bytes),
    ) {
        Ok(()) if content.is_some() => None,
        Ok(()) => Some(
            if metadata
                .remote
                .as_ref()
                .is_some_and(|remote| remote.terminal.is_some())
            {
                "runner omitted per-segment statuses from its terminal proof".into()
            } else {
                "remote terminal proof unavailable; per-segment outcomes are unknown".into()
            },
        ),
        Err(error) => Some(format!(
            "remote pipeline status persistence failed: {error}"
        )),
    };
}

#[cfg(unix)]
impl OutputSink for TaskSink {
    fn accepted(&mut self, accepted: &Accepted) -> io::Result<()> {
        // A resubmitted job returns its files afresh.
        if let Some(returns) = self.returns.as_mut() {
            returns.reset();
        }
        self.commit(|r| {
            r.return_first_seq = None;
            r.return_staging = Default::default();
            r.job_id = Some(accepted.job_id);
            r.queue_position = Some(accepted.queue_position);
            r.accepted = true;
            r.accepted_platform = accepted.platform.clone();
            r.granted_network = accepted.network == Some(Network::Outbound);
            if accepted.env_not_forwarded.is_some() {
                r.env_not_forwarded = accepted.env_not_forwarded.clone();
            }
        })?;
        let mut db = DeferredDbWrites::new(&self.registry, &self.task);
        let mut state = self
            .task
            .state
            .lock()
            .map_err(|_| io::Error::other("task lock poisoned"))?;
        append_executor_environment_disclosure(&mut state.metadata);
        if let Some(remote) = &state.metadata.remote {
            let note = remote_execution_note(remote);
            let existing = state
                .metadata
                .execution_note
                .get_or_insert_with(String::new);
            // Preserve environment disclosures already attached to the request header.
            let suffix = existing
                .find('\n')
                .map(|index| existing[index..].to_owned())
                .unwrap_or_default();
            *existing = note + &suffix;
        }
        crate::slog_info!(
            "runon: remote task accepted task_id={} session={} job_id={}",
            self.task.task_id,
            state.metadata.session_id,
            accepted.job_id,
        );
        self.registry
            .persist_task_locked(&self.task, &state.metadata, &mut db)?;
        // The remote executor's Accepted record confirms inputs were captured.
        // Refusals skip Accepted; those callers wait for a settled terminal error.
        self.task.remote_launch_ready.store(true, Ordering::Release);
        Ok(())
    }
    fn started(&mut self, started: &Started) -> io::Result<()> {
        // Runner wall-clock time is display data, not a local watchdog deadline.
        self.commit(|r| {
            r.started = Some(started.clone());
            r.last_seq = Some(started.seq);
        })?;
        let _ = self.registry.inner.wake_tx.try_send(());
        Ok(())
    }
    fn output(&mut self, seq: u64, stream: OutputStream, bytes: &[u8]) -> io::Result<()> {
        let stdout = stream == OutputStream::Stdout;
        let offset = if stdout {
            self.output_progress.stdout_len
        } else {
            self.output_progress.stderr_len
        };
        let file = if stdout {
            &mut self.stdout
        } else {
            &mut self.stderr
        };
        file.seek(SeekFrom::Start(offset))?;
        if stdout {
            self.dirty.stdout = true;
        } else {
            self.dirty.stderr = true;
        }
        file.write_all(bytes)?;
        file.set_len(offset + bytes.len() as u64)?;
        self.progress(seq, Some(stream), offset + bytes.len() as u64)?;
        let _ = self.registry.inner.wake_tx.try_send(());
        Ok(())
    }
    fn truncated(&mut self, before_seq: u64) -> io::Result<()> {
        self.truncate_output(before_seq, false)
    }
    fn undelivered(&mut self, before_seq: u64) -> io::Result<()> {
        self.truncate_output(before_seq, true)
    }
    fn gap_recovery(&mut self, recovery: &exec::GapRecovery) -> io::Result<()> {
        self.commit(|r| r.gap_recovery = Some(recovery.clone()))
    }
    fn unknown_output(&mut self, seq: u64, bytes: &[u8]) -> io::Result<()> {
        let offset = self.output_progress.unknown_len;
        if !bytes.is_empty() {
            if self.unknown.is_none() {
                let layout = resolve_task_layout(&self.task.paths.session_dir, &self.task.task_id)?;
                let name = OsStr::new("remote-unknown-output");
                let file = match layout.dirs.io.open_new_file(name) {
                    Ok(file) => file,
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        layout.dirs.io.open_file(name, true)?
                    }
                    Err(error) => return Err(error),
                };
                self.unknown = Some(file);
            }
            let file = self.unknown.as_mut().unwrap();
            file.seek(SeekFrom::Start(offset))?;
            self.dirty.unknown = true;
            file.write_all(bytes)?;
            file.set_len(offset + bytes.len() as u64)?;
        }
        self.progress(seq, None, offset + bytes.len() as u64)
    }
    fn return_chunk(&mut self, chunk: &ReturnChunk) -> io::Result<()> {
        let Some(mut returns) = self.returns.take() else {
            return self.unknown_output(chunk.seq, &[]);
        };
        if !returns.received_chunk() {
            // Saved before the cursor can pass the chunk, so a restart knows
            // where to attach to receive the files again.
            let seq = chunk.seq;
            if let Err(error) = self.commit(|r| r.return_first_seq = Some(seq)) {
                self.returns = Some(returns);
                return Err(error);
            }
        }
        returns.chunk(chunk, &mut |staging| self.record_staging(staging));
        self.returns = Some(returns);
        let unknown_len = self.output_progress.unknown_len;
        self.progress(chunk.seq, None, unknown_len)
    }
    fn return_manifest(&mut self, manifest: &ReturnManifest) -> io::Result<()> {
        let report = match self.unsaved_return_report.take() {
            Some(report) => report,
            None => match self.returns.take() {
                Some(mut returns) => {
                    returns.manifest(manifest, &mut |staging| self.record_staging(staging))
                }
                None => return self.unknown_output(manifest.seq, &[]),
            },
        };
        let seq = manifest.seq;
        let saved = report.clone();
        let result = self.commit(|r| {
            r.returned_files = Some(report);
            // Every staging file was placed or removed with the report.
            r.return_staging = Default::default();
            r.last_seq = Some(seq);
        });
        if result.is_err() {
            // The files are already placed. The consumer delivers this
            // manifest again, and the same report is saved then.
            self.unsaved_return_report = Some(saved);
        }
        result
    }
    fn terminal(&mut self, record: &TerminalRecord, verdict: &Verdict) -> io::Result<()> {
        #[cfg(test)]
        let clock = tests::task_draining_clock(&self.task);
        // A job that ran but sent no manifest returned nothing. A refusal
        // before start may be retried, so it leaves the request open.
        let unreported = (!matches!(verdict, Verdict::RunLocally { .. }))
            .then(|| {
                self.returns
                    .take()
                    .map(|mut returns| returns.without_manifest())
            })
            .flatten();
        self.commit(|r| {
            if let Some(report) = unreported {
                r.returned_files = Some(report);
                r.return_staging = Default::default();
            }
            r.job_id = Some(record.job_id);
            r.terminal = Some(record.clone());
            r.gap_recovery = None;
            if retries_draining(r, verdict) {
                if let Some(retry) = r.retry.as_mut() {
                    let now = unix_millis();
                    #[cfg(test)]
                    let now = clock.as_ref().map_or(now, |clock| clock.now());
                    retry.first_refusal_at_ms.get_or_insert(now);
                    retry.next_retry_at_ms =
                        Some(now.saturating_add(draining_delay_ms(
                            retry.attempts,
                            record.retry_after_ms,
                        )));
                }
            }
        })
    }
}

#[cfg(unix)]
impl TaskSink {
    /// Save staging files and directories in the task record before the
    /// receiver creates them.
    fn record_staging(&mut self, staging: &super::return_files::StagingRecord) -> io::Result<()> {
        let staging = staging.clone();
        self.commit(|r| {
            r.return_staging.files.extend(staging.files);
            r.return_staging.dirs.extend(staging.dirs);
        })
    }

    fn truncate_output(&mut self, before_seq: u64, undelivered: bool) -> io::Result<()> {
        self.dirty
            .sync(&self.stdout, &self.stderr, self.unknown.as_ref())?;
        let mut db = DeferredDbWrites::new(&self.registry, &self.task);
        let mut state = self
            .task
            .state
            .lock()
            .map_err(|_| io::Error::other("task lock poisoned"))?;
        let previous = state.metadata.clone();
        let remote = state
            .metadata
            .remote
            .as_mut()
            .ok_or_else(|| io::Error::other("remote record absent"))?;
        self.output_progress.store(remote);
        let first = remote.last_seq.map_or(0, |seq| seq.saturating_add(1));
        remote.last_seq = before_seq.checked_sub(1);
        if first < before_seq {
            if undelivered {
                remote.undelivered_output.push((first, before_seq - 1));
            }
            state
                .metadata
                .incomplete_output
                .push((first, before_seq - 1));
        }
        append_output_loss(&mut state.metadata);
        let result = self
            .registry
            .persist_task_locked(&self.task, &state.metadata, &mut db);
        if result.is_err() {
            state.metadata = previous;
        } else {
            self.output_progress = OutputProgress::of(state.metadata.remote.as_ref().unwrap());
            self.last_progress_persist = self.progress_now();
        }
        result
    }
}

#[cfg(unix)]
fn save_retry_request(task: &BgTask, request: &RunRequest) -> Result<String, String> {
    let layout =
        resolve_task_layout(&task.paths.session_dir, &task.task_id).map_err(|e| e.to_string())?;
    let bytes = serde_json::to_vec(request).map_err(|e| e.to_string())?;
    // Environment values stay in a private control file, not mirrored task metadata.
    let file = crate::bash_background::persistence::create_control_file(
        &layout.dirs,
        "remote-retry-request",
        &bytes,
    )
    .map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    Ok(retry_request_digest(task, &bytes))
}

#[cfg(unix)]
fn retry_request_digest(task: &BgTask, bytes: &[u8]) -> String {
    let mut hash = blake3::Hasher::new();
    hash.update(task.task_id.as_bytes());
    hash.update(bytes);
    hash.finalize().to_hex().to_string()
}

#[cfg(unix)]
fn restore_retry_request(task: &BgTask, expected: &str) -> Result<RunRequest, String> {
    use std::io::Read;
    let layout =
        resolve_task_layout(&task.paths.session_dir, &task.task_id).map_err(|e| e.to_string())?;
    let mut file =
        crate::bash_background::persistence::open_control_file(&layout, "remote-retry-request")
            .map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    if retry_request_digest(task, &bytes) != expected {
        return Err("remote retry request digest mismatch; command not resubmitted".into());
    }
    serde_json::from_slice(&bytes).map_err(|e| e.to_string())
}

#[derive(Clone)]
#[cfg(unix)]
struct LocalFallback {
    plan: SpawnPlan,
    shell_path: PathBuf,
    env: HashMap<String, String>,
    capture_pipeline: bool,
    linux_scope: bool,
}

#[cfg(unix)]
impl BgTaskRegistry {
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spawn_remote(
        &self,
        launch: super::super::RemoteLaunch,
        plan: SpawnPlan,
        command: &str,
        shell_path: PathBuf,
        session_id: String,
        workdir: PathBuf,
        env: HashMap<String, String>,
        hard_kill: super::super::HardKill,
        storage_dir: PathBuf,
        slot: super::super::TaskSlot,
        notify: bool,
        compressed: bool,
        root: Option<PathBuf>,
    ) -> Result<String, String> {
        self.spawn_remote_typed(
            launch,
            plan,
            command,
            shell_path,
            session_id,
            workdir,
            env,
            hard_kill,
            storage_dir,
            slot,
            notify,
            compressed,
            root,
        )
        .map_err(|error| error.to_string())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spawn_remote_typed(
        &self,
        launch: super::super::RemoteLaunch,
        mut plan: SpawnPlan,
        command: &str,
        shell_path: PathBuf,
        session_id: String,
        workdir: PathBuf,
        mut env: HashMap<String, String>,
        hard_kill: super::super::HardKill,
        storage_dir: PathBuf,
        slot: super::super::TaskSlot,
        notify: bool,
        compressed: bool,
        root: Option<PathBuf>,
    ) -> Result<String, RegistryError> {
        let _admission = self.background_admission_guard(slot);
        self.check_background_slot_typed(slot, &session_id)?;
        let layout = match plan.prepared_task() {
            Some(p) => p.resolved_task(),
            None => allocate_task_layout(&storage_dir, &session_id).map_err(|e| e.to_string())?,
        };
        let layout_cleanup = UncommittedLayout::new(&layout);
        if plan.prepared_task().is_none() {
            let environment =
                crate::sandbox_spawn::approved_payload_environment(&env, &std::env::temp_dir());
            let prepared = crate::sandbox_spawn::prepare_task_payload(
                &layout,
                command.as_bytes(),
                root.as_deref().unwrap_or(&workdir),
                &workdir,
                &crate::sandbox_spawn::current_authenticated_principal(),
                &shell_path,
                &environment,
            )?;
            plan = plan.with_prepared_task(prepared);
        }
        let task_id = layout.paths.task_id.clone();
        let requested_vcpus = launch.requested_vcpus;
        let mut metadata = PersistedTask::starting(
            task_id.clone(),
            session_id,
            command.into(),
            workdir.clone(),
            root.clone(),
            Some(hard_kill.limit().as_millis() as u64),
            notify,
            compressed,
        );
        metadata.harness = metadata.harness.or_else(|| self.fallback_db_harness());
        metadata.default_hard_kill = hard_kill.renewable();
        metadata.status = BgTaskStatus::Running;
        metadata.execution_note = Some(format!("remote execution requested on {RUNNER_ID}"));
        metadata.remote = Some(RemoteTask {
            requested_platform: launch.requested_platform.clone(),
            accepted_platform: None,
            connection_file: launch.connection_file,
            explicit_runon: launch.explicit_runon,
            harness: launch.harness,
            session: launch.session,
            requested_vcpus,
            requested_network: launch.requested_network,
            granted_network: false,
            accepted: false,
            started: None,
            job_id: None,
            last_seq: None,
            gap_recovery: None,
            undelivered_output: Vec::new(),
            stdout_len: 0,
            stderr_len: 0,
            unknown_len: 0,
            cancel_requested: false,
            terminal: None,
            fallback_digest: None,
            env_not_forwarded: None,
            queue_position: None,
            output_received: false,
            retry: None,
            output_incomplete: None,
            workspace_holder: None,
            return_paths: launch.return_paths.clone(),
            returned_files: None,
            return_first_seq: None,
            return_staging: Default::default(),
        });
        metadata.pipeline_segments = single_top_level_pipeline(command)
            .map(|p| p.segments.into_iter().map(|s| s.label).collect())
            .unwrap_or_default();
        let capture_pipeline = should_capture_pipeline_status(
            &plan,
            !metadata.pipeline_segments.is_empty(),
            &shell_path,
        );
        attach_sandbox_metadata(&mut metadata, &plan);
        let digest = crate::sandbox_spawn::save_local_launch(
            &plan,
            root.as_deref().unwrap_or(&workdir),
            &workdir,
            &crate::sandbox_spawn::current_authenticated_principal(),
            &shell_path,
            env.get("AFT_INTERNAL_LINUX_SCOPE")
                .is_some_and(|v| v == "1"),
        )?;
        metadata.remote.as_mut().unwrap().fallback_digest = Some(digest);
        let handles =
            TaskIoHandles::create(&layout, BgMode::Pipes, true).map_err(|e| e.to_string())?;
        write_task_at(&layout, &metadata).map_err(|e| e.to_string())?;
        // Commit point for a remote task: its record exists, and the remote
        // worker (which can run the command remotely or fall back to a local
        // process) starts below. Without this, a startup the reply deadline
        // had already refused as "not started" could still run here.
        if let Err(error) = super::super::commit_spawn_receipt(&task_id) {
            let _ = delete_resolved_task(&layout);
            return Err(RegistryError::StartupDeadline(error));
        }
        layout_cleanup.disarm();
        self.dual_write_task(&layout.paths, &metadata);
        self.insert_rehydrated_task(metadata, layout.paths, false)?;
        let task = self.task(&task_id).unwrap();
        task.holds_background_slot
            .store(slot.holds_slot(), Ordering::SeqCst);
        task.state
            .lock()
            .map_err(|_| "task lock poisoned")?
            .io_handles = Some(handles);
        let request_root = root.as_deref().unwrap_or(&workdir);
        let linux_scope = env
            .remove("AFT_INTERNAL_LINUX_SCOPE")
            .is_some_and(|v| v == "1");
        let shell_environment: crate::sandbox_spawn::ChildEnvironment =
            if !plan.is_native_launcher() && plan.host_shell_path().is_none() {
                std::env::vars_os()
                    .chain(env.clone().into_iter().map(|(k, v)| (k.into(), v.into())))
                    .collect()
            } else {
                crate::sandbox_spawn::approved_environment_for_plan(&plan, &env)
            };
        let mut stripped = Vec::new();
        let environment = shell_environment
            .into_iter()
            .filter(|(key, _)| {
                if let Some(key) = key
                    .to_str()
                    .filter(|name| exec::denied_environment_name(name))
                {
                    stripped.push(key.to_owned());
                    false
                } else {
                    true
                }
            })
            .map(|(key, value)| {
                Ok((
                    key.into_string().map_err(|_| {
                        exec::Error::Protocol("shell environment contains a non-Unicode key".into())
                    })?,
                    value.into_string().map_err(|_| {
                        exec::Error::Protocol(
                            "shell environment contains a non-Unicode value".into(),
                        )
                    })?,
                ))
            })
            .collect::<Result<BTreeMap<_, _>, exec::Error>>();
        {
            let mut db = DeferredDbWrites::new(self, &task);
            let mut state = task.state.lock().map_err(|_| "task lock poisoned")?;
            state.metadata.stripped_env_names = stripped;
            append_environment_disclosure(&mut state.metadata);
            self.persist_task_locked(&task, &state.metadata, &mut db)
                .map_err(|e| e.to_string())?;
        }
        let request = environment.and_then(|environment| {
            *task
                .completion_secrets
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some(crate::log_redact::SecretValues::from_environment(
                    environment.iter().map(|(key, value)| {
                        (std::ffi::OsStr::new(key), std::ffi::OsStr::new(value))
                    }),
                ));
            let limit = hard_kill.limit();
            let timeout_seconds = limit
                .as_secs()
                .saturating_add(u64::from(limit.subsec_nanos() > 0));
            build_remote_request(
                request_root,
                &workdir,
                command,
                environment,
                timeout_seconds,
                &exec::PresetParams {
                    platform: launch.requested_platform,
                    siblings: launch.params.siblings,
                    weight_hint: requested_vcpus,
                    network: launch.requested_network.then_some(Network::Outbound),
                    return_paths: launch.return_paths,
                    ..Default::default()
                },
            )
        });
        let fallback = LocalFallback {
            plan,
            shell_path,
            linux_scope,
            env,
            capture_pipeline,
        };
        self.start_remote_worker(task, Some((request, fallback)))?;
        self.start_watchdog();
        Ok(task_id)
    }

    pub(super) fn resume_remote_task(&self, task_id: &str) -> Result<(), String> {
        self.start_remote_worker(self.task(task_id).ok_or("remote task missing")?, None)
    }

    fn start_remote_worker(
        &self,
        task: Arc<BgTask>,
        initial: Option<(Result<RunRequest, exec::Error>, LocalFallback)>,
    ) -> Result<(), String> {
        let registry = self.clone();
        std::thread::Builder::new()
            .name(format!("aft-remote-{}", task.task_id))
            .spawn(move || {
                let result = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| e.to_string())
                    .and_then(|rt| rt.block_on(registry.run_remote_task(task.clone(), initial)));
                if let Err(error) = result {
                    let point = task
                        .state
                        .lock()
                        .ok()
                        .and_then(|state| state.metadata.remote.as_ref()?.point())
                        .filter(|point| point.gap_recovery.is_some());
                    let verdict = point
                        .map(|point| {
                            // Exhausted attach/connect recovery cannot invalidate a
                            // terminal already received. Preserve buffered output
                            // and mark only its missing sequences as lost.
                            let known = exec::grade(&point.gap_recovery.as_ref().unwrap().terminal);
                            let mut consumer = exec::StreamConsumer::resume(point.clone());
                            match TaskSink::new(&registry, task.clone()) {
                                Ok(mut sink) => {
                                    consumer.finish_lost_output(&mut sink).unwrap_or(known)
                                }
                                Err(_) => known,
                            }
                        })
                        .unwrap_or(Verdict::OutcomeUnknown);
                    registry.remote_terminal(&task, verdict, Some(error));
                }
                // Refusals must be rendered from their settled terminal state,
                // not raced against the worker that is still recording them.
                task.remote_launch_ready.store(true, Ordering::Release);
            })
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn run_remote_task(
        &self,
        task: Arc<BgTask>,
        mut initial: Option<(Result<RunRequest, exec::Error>, LocalFallback)>,
    ) -> Result<(), String> {
        let remote = task
            .state
            .lock()
            .map_err(|_| "task lock poisoned")?
            .metadata
            .remote
            .clone()
            .unwrap();
        let root = task
            .state
            .lock()
            .map_err(|_| "task lock poisoned")?
            .metadata
            .project_root
            .clone()
            .unwrap_or_else(|| PathBuf::from("/"));
        let mut sink = TaskSink::new(self, task.clone()).map_err(|e| e.to_string())?;
        let mut recovery = ReattachBudget::new(
            &root,
            &task
                .state
                .lock()
                .map_err(|_| "task lock poisoned")?
                .metadata,
        );
        let mut retry_request = None;
        if let Some(record) = remote.terminal.as_ref() {
            let verdict = exec::grade(record);
            if retries_draining(&remote, &verdict) && remote.retry.is_some() {
                retry_request = self.wait_draining_retry(&task, &root).await?;
                if retry_request.is_none() {
                    return Ok(());
                }
            } else {
                if let Verdict::RunLocally {
                    reason,
                    refusal_detail,
                } = verdict
                {
                    let reason = serde_json::to_value(reason)
                        .unwrap()
                        .as_str()
                        .unwrap_or("unknown")
                        .to_owned();
                    return self.restored_remote_fallback(
                        &task,
                        &remote,
                        &reason,
                        refusal_detail.as_deref(),
                    );
                }
                self.remote_terminal(&task, exec::grade(record), None);
                return Ok(());
            }
        }
        let connection = remote
            .connection_file
            .as_ref()
            .ok_or_else(|| "daemon connection file unavailable".to_string());
        let mut client = loop {
            let connect = async {
                match &connection {
                    Ok(path) => ExecRemoteClient::connect(
                        path,
                        subc_protocol::BindIdentity::new(
                            root.display().to_string(),
                            remote.harness.clone(),
                            remote.session.clone(),
                        ),
                    )
                    .await
                    .map_err(|e| e.to_string()),
                    Err(error) => Err(error.clone()),
                }
            };
            let result = if initial.is_none() && retry_request.is_none() {
                if let Some(point) = remote.point() {
                    recovery.wait(point.job_id, connect).await?
                } else {
                    connect.await
                }
            } else {
                connect.await
            };
            match result {
                Ok(c) => break c,
                Err(error) => {
                    if retry_request.is_some() {
                        return self.refuse_remote_before_dispatch(&task, &error);
                    }
                    if let Some((_, fallback)) = initial.take() {
                        return if remote.explicit_runon {
                            self.refuse_remote_before_dispatch(&task, &error)
                        } else {
                            self.remote_fallback(&task, fallback, &error, None)
                        };
                    }
                    if let Some(point) = remote.point().filter(|_| remote.connection_file.is_some())
                    {
                        recovery.pause(point.job_id).await?;
                        continue;
                    }
                    return Err(format!("remote outcome unknown; attach required: {error}"));
                }
            }
        };
        let mut fallback = None;
        let mut hinted: Option<HintedRetry> = None;
        if remote.accepted && !platform_acknowledged(&remote) {
            if let Some(job_id) = remote.job_id {
                let _ = client.cancel_job(job_id).await;
            }
            return Err("remote outcome unknown: Windows platform was not acknowledged; cancel requested; result not trusted".into());
        }
        let mut stream = if retry_request.is_some() {
            let Some((stream, retry)) = self.submit_draining_retry(&client, &task, &root).await?
            else {
                return Ok(());
            };
            hinted = Some(retry);
            recovery.resubmitted(
                &task
                    .state
                    .lock()
                    .map_err(|_| "task lock poisoned")?
                    .metadata,
            );
            stream
        } else if let Some((request, local)) = initial.take() {
            let request = match request {
                Ok(r) => r,
                Err(error) => {
                    return if remote.explicit_runon {
                        self.refuse_remote_before_dispatch(&task, &error.to_string())
                    } else {
                        self.remote_fallback(&task, local, &error.to_string(), None)
                    }
                }
            };
            fallback = Some(local);
            if remote.explicit_runon {
                let request_digest = save_retry_request(&task, &request)?;
                sink.commit(|r| {
                    r.retry = Some(DrainingRetry {
                        request_digest,
                        attempts: 1,
                        first_refusal_at_ms: None,
                        next_retry_at_ms: None,
                        waited_ms: None,
                        resubmitted_at_ms: None,
                    })
                })
                .map_err(|e| e.to_string())?;
                hinted = Some(HintedRetry::start(&root, request.clone()));
            }
            match client.run(&request).await {
                Ok(stream) => stream,
                Err(error) if proves_no_start(&error) => {
                    return if remote.explicit_runon {
                        self.refuse_remote_before_dispatch(&task, &error.to_string())
                    } else {
                        self.remote_fallback(
                            &task,
                            fallback.take().unwrap(),
                            &error.to_string(),
                            None,
                        )
                    }
                }
                Err(error) => {
                    return Err(format!("remote outcome unknown; not resubmitted: {error}"))
                }
            }
        } else {
            let point = remote.point().ok_or("remote outcome unknown: acceptance job ID was not persisted; command was not resubmitted")?;
            self.attach_remote(&mut client, point, &remote, &root, &task, &mut recovery)
                .await?
        };
        let mut cancelled = false;
        loop {
            // Poll intent even when every stream read is immediately ready.
            // A new sleep per read cannot provide fairness under steady output.
            let cancel = task
                .state
                .lock()
                .map_err(|_| "task lock poisoned")?
                .metadata
                .remote
                .as_ref()
                .is_some_and(|r| r.cancel_requested);
            if cancel && !cancelled {
                if let Some(point) = stream.resume_point() {
                    let _ = client.cancel_job(point.job_id).await;
                    stream = self
                        .attach_remote(&mut client, point, &remote, &root, &task, &mut recovery)
                        .await?;
                    cancelled = true;
                }
            }
            let result = tokio::select! {
                result=stream.next(&mut sink)=>result,
                _=tokio::time::sleep(Duration::from_millis(50))=>{
                    continue;
                }
            };
            // Old runners may ignore the platform field even when discovery
            // advertises it. Require the acceptance to acknowledge Windows.
            let platform_ok = if remote.requested_platform == Some(Platform::Windows) {
                let (accepted, acknowledged, job_id) = {
                    let state = task.state.lock().map_err(|_| "task lock poisoned")?;
                    let current = state.metadata.remote.as_ref().unwrap();
                    (
                        current.accepted,
                        platform_acknowledged(current),
                        current.job_id,
                    )
                };
                if accepted && !acknowledged {
                    if let Some(job_id) = job_id {
                        let _ = client.cancel_job(job_id).await;
                    }
                    return Err("remote outcome unknown: Windows platform was not acknowledged; cancel requested; result not trusted".into());
                }
                acknowledged
            } else {
                true
            };
            match result {
                Ok(StreamProgress::Record) => recovery.reset(),
                Ok(StreamProgress::Complete(Verdict::RunLocally {
                    reason,
                    refusal_detail,
                })) => {
                    let verdict = Verdict::RunLocally {
                        reason: reason.clone(),
                        refusal_detail: refusal_detail.clone(),
                    };
                    if retries_draining(&remote, &verdict) {
                        if self.wait_draining_retry(&task, &root).await?.is_some() {
                            let Some((next, retry)) =
                                self.submit_draining_retry(&client, &task, &root).await?
                            else {
                                return Ok(());
                            };
                            stream = next;
                            hinted = Some(retry);
                            recovery.resubmitted(
                                &task
                                    .state
                                    .lock()
                                    .map_err(|_| "task lock poisoned")?
                                    .metadata,
                            );
                            recovery.reset();
                            continue;
                        }
                        return Ok(());
                    }
                    if let Some(retry) = hinted.as_mut() {
                        match self.hinted_retry(&client, &task, &root, retry).await? {
                            HintedRetryStep::Resubmitted(next) => {
                                stream = next;
                                recovery.resubmitted(
                                    &task
                                        .state
                                        .lock()
                                        .map_err(|_| "task lock poisoned")?
                                        .metadata,
                                );
                                recovery.reset();
                                continue;
                            }
                            HintedRetryStep::Ended => return Ok(()),
                            HintedRetryStep::Surface => {}
                        }
                    }
                    let reason = serde_json::to_value(reason)
                        .unwrap()
                        .as_str()
                        .unwrap_or("unknown")
                        .to_owned();
                    if let Some(fallback) = fallback.take() {
                        // A live refusal uses the launch plan captured before
                        // dispatch. The private disk snapshot is for restarts.
                        return self.remote_fallback(
                            &task,
                            fallback,
                            &reason,
                            refusal_detail.as_deref(),
                        );
                    }
                    return self.restored_remote_fallback(
                        &task,
                        &remote,
                        &reason,
                        refusal_detail.as_deref(),
                    );
                }
                Ok(StreamProgress::Complete(verdict)) => {
                    if !platform_ok {
                        return Err("remote outcome unknown: Windows platform was not acknowledged; result not trusted".into());
                    }
                    self.remote_terminal(&task, verdict, None);
                    return Ok(());
                }
                Err(error) => {
                    let Some(point) = stream.resume_point() else {
                        return Err(format!("remote outcome unknown; not resubmitted: {error}"));
                    };
                    // Even an undecodable frame proves this was not an empty
                    // attach. A quiet but open stream has no output deadline.
                    if stream.received_frame() {
                        recovery.reset();
                    }
                    // A known accepted job is recovered only by attach. Never
                    // convert a lost stream into another run or local fallback.
                    recovery.pause(point.job_id).await?;
                    stream = self
                        .attach_remote(&mut client, point, &remote, &root, &task, &mut recovery)
                        .await?;
                }
            }
        }
    }

    async fn wait_draining_retry(
        &self,
        task: &Arc<BgTask>,
        root: &Path,
    ) -> Result<Option<RunRequest>, String> {
        let budget = draining_budget_ms(root);
        loop {
            let (remote, now) = {
                let mut db = DeferredDbWrites::new(self, task);
                let mut state = task.state.lock().map_err(|_| "task lock poisoned")?;
                let remote = state
                    .metadata
                    .remote
                    .clone()
                    .ok_or("remote record absent")?;
                let retry = remote
                    .retry
                    .as_ref()
                    .ok_or("draining retry request absent; command not resubmitted")?;
                let first = retry
                    .first_refusal_at_ms
                    .ok_or("draining retry start absent; command not resubmitted")?;
                let now = draining_now_ms(root);
                state.metadata.execution_note = Some(format!(
                    "build server is draining for maintenance; retrying (attempt {}, {} of {} used)",
                    retry.attempts.saturating_add(1), retry_duration(now.saturating_sub(first).min(budget)), retry_duration(budget)
                ));
                self.persist_task_locked(task, &state.metadata, &mut db)
                    .map_err(|e| e.to_string())?;
                (remote, now)
            };
            if remote.cancel_requested {
                self.remote_terminal(task, Verdict::Cancelled, None);
                return Ok(None);
            }
            #[cfg(test)]
            if tests::stop_draining_worker(root) {
                return Ok(None);
            }
            let retry = remote.retry.as_ref().unwrap();
            let first = retry.first_refusal_at_ms.unwrap();
            let elapsed = now.saturating_sub(first);
            if elapsed >= budget {
                let mut sink = TaskSink::new(self, task.clone()).map_err(|e| e.to_string())?;
                sink.commit(|r| r.retry.as_mut().unwrap().waited_ms = Some(elapsed))
                    .map_err(|e| e.to_string())?;
                let Verdict::RunLocally { refusal_detail, .. } =
                    exec::grade(remote.terminal.as_ref().ok_or("draining refusal absent")?)
                else {
                    return Err("draining refusal absent".into());
                };
                return self
                    .refuse_remote_executor(task, "runner_draining", refusal_detail.as_deref())
                    .map(|_| None);
            }
            if now >= retry.next_retry_at_ms.unwrap_or(now) {
                return restore_retry_request(task, &retry.request_digest).map(Some);
            }
            // A durable cancellation intent plus Notify covers both live kills
            // and kills recorded before the worker/restarted daemon began waiting.
            let delay = retry
                .next_retry_at_ms
                .unwrap()
                .saturating_sub(now)
                .min(budget - elapsed)
                .min(1000);
            tokio::select! {
                _ = task.remote_cancel_notify.notified() => {},
                _ = draining_retry_sleep(root, now, delay) => {},
            }
        }
    }

    async fn submit_draining_retry(
        &self,
        client: &ExecRemoteClient,
        task: &Arc<BgTask>,
        root: &Path,
    ) -> Result<Option<(exec::RemoteStream, HintedRetry)>, String> {
        // Recheck after connecting: reconnect time also consumes the retry budget.
        let Some(request) = self.wait_draining_retry(task, root).await? else {
            return Ok(None);
        };
        {
            let mut db = DeferredDbWrites::new(self, task);
            let mut state = task.state.lock().map_err(|_| "task lock poisoned")?;
            let remote = state
                .metadata
                .remote
                .as_mut()
                .ok_or("remote record absent")?;
            if remote.cancel_requested {
                drop(state);
                self.remote_terminal(task, Verdict::Cancelled, None);
                return Ok(None);
            }
            // Clear the no-start proof before dispatch. A crash after this write
            // is ambiguous and must recover by attach, never another exec.run.
            remote.terminal = None;
            remote.job_id = None;
            remote.last_seq = None;
            let retry = remote
                .retry
                .as_mut()
                .ok_or("draining retry request absent")?;
            retry.attempts = retry.attempts.saturating_add(1);
            retry.next_retry_at_ms = None;
            // Written with the dispatch, so a restart that re-attaches to this
            // submission counts its limits from it, not from the first one.
            retry.resubmitted_at_ms = Some(unix_millis());
            state.metadata.execution_note = Some(format!(
                "remote execution requested on {RUNNER_ID} after maintenance wait"
            ));
            self.persist_task_locked(task, &state.metadata, &mut db)
                .map_err(|e| e.to_string())?;
        }
        // A fresh submission gets its own signer-retry allowance, counted from now.
        let hinted = HintedRetry::start(root, request);
        match client.run(&hinted.request).await {
            Ok(stream) => Ok(Some((stream, hinted))),
            Err(error) if proves_no_start(&error) => {
                self.refuse_remote_before_dispatch(task, &error.to_string())?;
                Ok(None)
            }
            Err(error) => Err(format!("remote outcome unknown; not resubmitted: {error}")),
        }
    }

    /// Act on a refusal the runner marked transient with a retry hint: wait
    /// for the hint, then send the same request again. The refusal is reported
    /// unchanged when it carries no usable hint, the hint is longer than the
    /// job's admission time left, that time has run out by the moment of
    /// resending, or the job was already resubmitted
    /// [`HINTED_RETRY_MAX_RESUBMITS`] times. A kill during the wait ends the
    /// task at once.
    async fn hinted_retry(
        &self,
        client: &ExecRemoteClient,
        task: &Arc<BgTask>,
        root: &Path,
        retry: &mut HintedRetry,
    ) -> Result<HintedRetryStep, String> {
        let (terminal, started) = {
            let state = task.state.lock().map_err(|_| "task lock poisoned")?;
            let remote = state
                .metadata
                .remote
                .as_ref()
                .ok_or("remote record absent")?;
            (
                remote.terminal.clone(),
                remote.started.is_some() || remote.output_received,
            )
        };
        // A `Started` record or any output would show the command did run,
        // whatever the terminal record says.
        let Some(terminal) = terminal.filter(|_| !started) else {
            return Ok(HintedRetryStep::Surface);
        };
        let Some(hint) = hinted_retry_after_ms(&terminal) else {
            return Ok(HintedRetryStep::Surface);
        };
        if retry.resubmits >= HINTED_RETRY_MAX_RESUBMITS {
            return Ok(HintedRetryStep::Surface);
        }
        let now = draining_now_ms(root);
        if hint > retry.deadline_ms.saturating_sub(now) {
            return Ok(HintedRetryStep::Surface);
        }
        let reason = match &terminal.outcome {
            Outcome::RefusedBeforeStart { reason } => String::from(reason.clone()),
            _ => String::new(),
        };
        crate::slog_info!(
            "runon: resubmitting job {} after its refusal before start (reason={reason} detail={} retry_after_ms={hint}); resubmission {} of at most {HINTED_RETRY_MAX_RESUBMITS}",
            terminal.job_id,
            terminal.refusal_detail().unwrap_or_default(),
            retry.resubmits + 1,
        );
        let until = now.saturating_add(hint);
        loop {
            let cancel = task
                .state
                .lock()
                .map_err(|_| "task lock poisoned")?
                .metadata
                .remote
                .as_ref()
                .is_some_and(|remote| remote.cancel_requested);
            if cancel {
                self.remote_terminal(task, Verdict::Cancelled, None);
                return Ok(HintedRetryStep::Ended);
            }
            let now = draining_now_ms(root);
            if now >= until {
                break;
            }
            // A kill records its intent before it notifies, and a notification
            // sent before this wait began is kept, so the check above and this
            // wait together never miss one.
            tokio::select! {
                _ = task.remote_cancel_notify.notified() => {},
                _ = draining_retry_sleep(root, now, until - now) => {},
            }
        }
        {
            let mut db = DeferredDbWrites::new(self, task);
            let mut state = task.state.lock().map_err(|_| "task lock poisoned")?;
            let remote = state
                .metadata
                .remote
                .as_mut()
                .ok_or("remote record absent")?;
            if remote.cancel_requested {
                drop(state);
                drop(db);
                self.remote_terminal(task, Verdict::Cancelled, None);
                return Ok(HintedRetryStep::Ended);
            }
            // The wait can end late, and a zero hint does not wait at all, so
            // check the admission time again right before sending: once it has
            // run out, report the saved refusal as it is.
            if draining_now_ms(root) >= retry.deadline_ms {
                return Ok(HintedRetryStep::Surface);
            }
            // Clear the refusal before dispatch, as a draining resubmission
            // does: a crash after this write is ambiguous and must recover by
            // attach, never by another exec.run.
            remote.terminal = None;
            remote.job_id = None;
            remote.last_seq = None;
            if let Some(saved) = remote.retry.as_mut() {
                // Re-attach limits after a restart count from this submission.
                saved.resubmitted_at_ms = Some(unix_millis());
            }
            self.persist_task_locked(task, &state.metadata, &mut db)
                .map_err(|e| e.to_string())?;
        }
        retry.resubmits += 1;
        match client.run(&retry.request).await {
            Ok(stream) => Ok(HintedRetryStep::Resubmitted(stream)),
            Err(error) if proves_no_start(&error) => {
                self.refuse_remote_before_dispatch(task, &error.to_string())?;
                Ok(HintedRetryStep::Ended)
            }
            Err(error) => Err(format!("remote outcome unknown; not resubmitted: {error}")),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn attach_remote(
        &self,
        client: &mut ExecRemoteClient,
        point: ResumePoint,
        remote: &RemoteTask,
        root: &Path,
        task: &Arc<BgTask>,
        recovery: &mut ReattachBudget,
    ) -> Result<exec::RemoteStream, String> {
        point.attach_request().map_err(|e| e.to_string())?;
        loop {
            let cancel = task
                .state
                .lock()
                .map_err(|_| "task lock poisoned")?
                .metadata
                .remote
                .as_ref()
                .is_some_and(|r| r.cancel_requested);
            let attempt = recovery
                .wait(point.job_id, async {
                    if cancel {
                        let _ = client.cancel_job(point.job_id).await;
                    }
                    client.attach(point.clone()).await
                })
                .await?;
            if let Ok(stream) = attempt {
                return Ok(stream);
            }
            // The route may have closed with the runner (a ck-motor restart).
            // Back off, then reconnect and attach again; never resubmit.
            recovery.pause(point.job_id).await?;
            if let Some(path) = &remote.connection_file {
                let reconnected = recovery
                    .wait(
                        point.job_id,
                        ExecRemoteClient::connect(
                            path,
                            subc_protocol::BindIdentity::new(
                                root.display().to_string(),
                                remote.harness.clone(),
                                remote.session.clone(),
                            ),
                        ),
                    )
                    .await?;
                if let Ok(reconnected) = reconnected {
                    *client = reconnected;
                }
            }
        }
    }

    fn remote_fallback(
        &self,
        task: &Arc<BgTask>,
        local: LocalFallback,
        reason: &str,
        refusal_detail: Option<&str>,
    ) -> Result<(), String> {
        let mut db = DeferredDbWrites::new(self, task);
        let mut state = task.state.lock().map_err(|_| "task lock poisoned")?;
        let metadata = state.metadata.clone();
        // Automatic prefix routing may fall back; an explicit remote demand
        // must fail before any local launch state or process is created. A
        // refusal of return_paths is final too: running the command here
        // would not copy back the files it was asked for.
        if metadata.remote.as_ref().is_some_and(|r| r.explicit_runon)
            || return_paths_unsupported(reason, refusal_detail)
        {
            drop(state);
            drop(db);
            return self.refuse_remote_executor(task, reason, refusal_detail);
        }
        let layout = resolve_task_layout(&task.paths.session_dir, &task.task_id)
            .map_err(|e| e.to_string())?;
        let durable = read_task_at(&layout).map_err(|e| e.to_string())?;
        if durable.remote.as_ref().is_some_and(|r| r.cancel_requested) {
            if let Some(remote) = state.metadata.remote.as_mut() {
                remote.cancel_requested = true;
            }
            drop(state);
            drop(db);
            self.remote_terminal(
                task,
                Verdict::RunLocally {
                    reason: RefusalReason::Unknown(reason.into()),
                    refusal_detail: refusal_detail.map(str::to_owned),
                },
                Some("remote refused before start; local fallback skipped because cancellation was requested".into()),
            );
            return Ok(());
        }
        // Commit the switch to local before spawning. A crash before the PID
        // write must report an uncertain local start, never submit it again.
        state.metadata.remote = None;
        state.metadata.local_fallback_started = true;
        state.metadata.started_at = unix_millis();
        let reason = rendered_refusal_reason(reason, refusal_detail).replace(['\n', '\r'], " ");
        state.metadata.execution_note = Some(format!(
            "ran locally on {}: remote refused: {reason}",
            local_os_name()
        ));
        if metadata
            .remote
            .as_ref()
            .is_some_and(|remote| !remote.return_paths.is_empty())
        {
            let note = state.metadata.execution_note.as_mut().unwrap();
            note.push('\n');
            note.push_str(super::return_files::LOCAL_RUN_NOTE);
        }
        if let Some(changes) = metadata
            .remote
            .as_ref()
            .and_then(|r| r.terminal.as_ref())
            .and_then(|t| t.workspace_changes.as_ref())
            .filter(|c| !c.is_empty())
        {
            state
                .metadata
                .execution_note
                .as_mut()
                .unwrap()
                .push_str(&format!(
                    "\nremote workspace_changes (not copied back): {}",
                    serde_json::to_string(changes).unwrap()
                ));
        }
        append_output_loss(&mut state.metadata);
        self.persist_task_locked(task, &state.metadata, &mut db)
            .map_err(|e| e.to_string())?;
        #[cfg(test)]
        tests::after_local_fallback_marker(&task.task_id);
        let child = spawn_detached_child(
            &local.plan,
            &metadata.command,
            super::super::BashShell::Bash,
            &local.shell_path,
            &task.paths,
            &metadata.workdir,
            &local.env,
            state.io_handles.as_mut().ok_or("task handles absent")?,
            local.capture_pipeline,
            local.linux_scope,
            // local_fallback_started was persisted above before process creation;
            // a later failed PID write reports unknown, never "did not run" or a
            // second execution. This caller has no ordinary-local "run" sender,
            // so its wrapper must start without that identity-checkpoint gate.
            LocalIdentityGate::CommittedFallback,
        )?;
        #[cfg(test)]
        if tests::simulate_crash_before_local_pid(&task.task_id) {
            return Ok(());
        }
        state.metadata.mark_running(child.id(), child.id() as i32);
        state.runtime = TaskRuntime::Piped(Some(child));
        state.detached = false;
        let persisted = (|| {
            #[cfg(test)]
            tests::fail_running_metadata_after_spawn(&task.task_id)?;
            self.persist_task_locked(task, &state.metadata, &mut db)
                .map_err(|e| e.to_string())
        })();
        drop(state);
        drop(db);
        if let Err(error) = persisted {
            // Spawn succeeded. This is not a no-start refusal, even when the
            // PID publication fails; both live and restarted callers end here.
            self.remote_terminal(
                task,
                Verdict::OutcomeUnknown,
                Some(format!(
                    "the command started locally; AFT lost track of it: {error}; never rerun"
                )),
            );
        }
        Ok(())
    }

    fn restored_remote_fallback(
        &self,
        task: &Arc<BgTask>,
        remote: &RemoteTask,
        reason: &str,
        refusal_detail: Option<&str>,
    ) -> Result<(), String> {
        // Decide before even restoring a local launch plan: this also covers
        // a refusal persisted just before the previous AFT process stopped.
        if remote.explicit_runon || return_paths_unsupported(reason, refusal_detail) {
            return self.refuse_remote_executor(task, reason, refusal_detail);
        }
        let restore = (|| {
            let layout = resolve_task_layout(&task.paths.session_dir, &task.task_id)
                .map_err(|e| e.to_string())?;
            let (plan, shell_path, env, linux_scope) = crate::sandbox_spawn::restore_local_launch(
                &layout,
                remote
                    .fallback_digest
                    .as_deref()
                    .ok_or("local snapshot digest absent")?,
            )?;
            let metadata = task
                .state
                .lock()
                .map_err(|_| "task lock poisoned")?
                .metadata
                .clone();
            let capture_pipeline = should_capture_pipeline_status(
                &plan,
                !metadata.pipeline_segments.is_empty(),
                &shell_path,
            );
            // Replay has validated handles, but no local runtime retained them.
            let handles = TaskIoHandles::reopen(&layout).map_err(|e| e.to_string())?;
            task.state
                .lock()
                .map_err(|_| "task lock poisoned")?
                .io_handles = Some(handles);
            self.remote_fallback(
                task,
                LocalFallback {
                    plan,
                    shell_path,
                    env,
                    capture_pipeline,
                    linux_scope,
                },
                reason,
                refusal_detail,
            )
        })();
        if let Err(error) = restore {
            self.remote_terminal(
                task,
                Verdict::RunLocally {
                    reason: RefusalReason::Unknown(reason.into()),
                    refusal_detail: refusal_detail.map(str::to_owned),
                },
                Some(format!("remote refused before start; the local fallback could not be restored after restart, so the command did not run: {error}")),
            );
        }
        Ok(())
    }

    fn refuse_remote_executor(
        &self,
        task: &Arc<BgTask>,
        reason: &str,
        refusal_detail: Option<&str>,
    ) -> Result<(), String> {
        let holder = if reason == "queue_wait_exceeded" {
            refusal_detail.and_then(|detail| self.own_workspace_holder(task, detail))
        } else {
            None
        };
        let message = {
            let mut state = task.state.lock().map_err(|_| "task lock poisoned")?;
            let remote = state
                .metadata
                .remote
                .as_mut()
                .ok_or("remote record absent")?;
            // Saved with the terminal below, so restarted readers render the same text.
            remote.workspace_holder = holder;
            remote_refusal_message(reason, refusal_detail, remote)
        };
        self.remote_terminal(
            task,
            Verdict::RunLocally {
                reason: RefusalReason::Unknown(reason.into()),
                refusal_detail: refusal_detail.map(str::to_owned),
            },
            Some(message),
        );
        Ok(())
    }

    /// This session's earlier remote task whose job the refusal detail names as
    /// running in the same checkout as `task`, if there is one.
    fn own_workspace_holder(&self, task: &Arc<BgTask>, detail: &str) -> Option<WorkspaceHolder> {
        let holders = workspace_owner_holders(detail);
        if holders.is_empty() {
            return None;
        }
        let workspace =
            |metadata: &PersistedTask| worktree_root_containing(&metadata.workdir).ok().flatten();
        let own_workspace = workspace(&task.state.lock().ok()?.metadata)?;
        let tasks: Vec<_> = self.inner.tasks.lock().ok()?.values().cloned().collect();
        tasks.into_iter().find_map(|other| {
            if other.task_id == task.task_id || other.session_id != task.session_id {
                return None;
            }
            let state = other.state.lock().ok()?;
            let job_id = state.metadata.remote.as_ref()?.job_id?;
            (holders.contains(&job_id) && workspace(&state.metadata)? == own_workspace).then(|| {
                WorkspaceHolder {
                    job_id,
                    task_id: other.task_id.clone(),
                }
            })
        })
    }

    fn refuse_remote_before_dispatch(&self, task: &Arc<BgTask>, error: &str) -> Result<(), String> {
        // Missing discovery providers and other proven pre-dispatch failures
        // cannot satisfy an explicit remote demand. Do not turn them into a
        // local run.
        self.remote_terminal(
            task,
            Verdict::RunLocally {
                reason: RefusalReason::Unreachable,
                refusal_detail: None,
            },
            Some(format!(
                "runon refused: remote execution is unavailable; command did not run: {error}"
            )),
        );
        Ok(())
    }

    fn remote_terminal(&self, task: &Arc<BgTask>, verdict: Verdict, error: Option<String>) {
        // Known terminal outcomes keep their status. An unknown outcome is reported
        // as uncertain, never automatically resubmitted.
        let refused = matches!(&verdict, Verdict::RunLocally { .. });
        let unknown = matches!(&verdict, Verdict::OutcomeUnknown);
        let (status, code, mut reason) = terminal_outcome(verdict, error);
        let (session_id, job_id, status_label) = {
            let mut db = DeferredDbWrites::new(self, task);
            let Ok(mut state) = task.state.lock() else {
                return;
            };
            if state.metadata.is_terminal() {
                return;
            }
            let remote_note = state
                .metadata
                .remote
                .as_ref()
                .map(remote_execution_note)
                .unwrap_or_else(|| format!("ran remotely on {RUNNER_ID}"));
            if unknown {
                if let Some(remote) = state.metadata.remote.as_ref() {
                    let job_id = remote
                        .job_id
                        .or_else(|| remote.terminal.as_ref().map(|record| record.job_id));
                    // A lost acceptance cannot prove that a requested network mode was denied.
                    let network_risk =
                        remote.granted_network || (remote.requested_network && !remote.accepted);
                    // Retain recovery diagnostics alongside advice based on the
                    // runner's network grant. Local fallback removes remote metadata.
                    if let Some(detail) = reason.as_deref() {
                        state
                            .metadata
                            .execution_note
                            .get_or_insert_with(|| remote_note.clone())
                            .push_str(&format!("\n{detail}"));
                    }
                    reason = Some(remote_unknown_outcome(
                        job_id,
                        network_risk,
                        state.metadata.timeout_ms,
                    ));
                } else if reason.is_none() {
                    reason = Some("remote outcome_unknown; never rerun".into());
                }
            }
            if refused {
                let accepted_refusal = state
                    .metadata
                    .remote
                    .as_ref()
                    .is_some_and(|remote| remote.terminal.is_some());
                let job_id = state.metadata.remote.as_ref().and_then(|remote| {
                    remote
                        .job_id
                        .or_else(|| remote.terminal.as_ref().map(|record| record.job_id))
                });
                state.metadata.execution_note = Some(if accepted_refusal {
                    job_id.map_or_else(
                        || "remote executor refused before start".into(),
                        |job_id| format!("remote executor refused before start (job {job_id})"),
                    )
                } else {
                    "runon refused before remote dispatch; command did not run".into()
                });
            } else if matches!(
                status,
                BgTaskStatus::Completed
                    | BgTaskStatus::Failed
                    | BgTaskStatus::Killed
                    | BgTaskStatus::TimedOut
            ) {
                state.metadata.execution_note = Some(remote_note.clone());
            }
            if status == BgTaskStatus::Killed
                && super::scope_end_label(state.metadata.status_reason.as_deref()).is_some()
            {
                reason = state.metadata.status_reason.clone();
            }
            let status_label = format!("{status:?}");
            state.metadata.mark_terminal(status, code, reason.clone());
            // Remote execution has no local shell wrapper to write markers.
            // Publish from durable runner proof, including after registry restart.
            persist_terminal_artifacts(&task.paths, &mut state.metadata);
            append_output_loss(&mut state.metadata);
            if let Some(reason) = reason {
                let note = state
                    .metadata
                    .execution_note
                    .get_or_insert_with(|| remote_note.clone());
                note.push_str(&format!("\n{reason}"));
            }
            let session_id = state.metadata.session_id.clone();
            let job_id = state.metadata.remote.as_ref().and_then(|remote| {
                remote
                    .job_id
                    .or_else(|| remote.terminal.as_ref().map(|record| record.job_id))
            });
            task.mark_terminal_now();
            let _ = self.persist_task_locked(task, &state.metadata, &mut db);
            task.publish_runon_launch_state(&state);
            task.kill_settled.notify_all();
            (session_id, job_id, status_label)
        };
        crate::slog_info!(
            "runon: remote task ended task_id={} session={} job_id={} status={}",
            task.task_id,
            session_id,
            job_id.map_or_else(|| "none".to_string(), |job_id| job_id.to_string()),
            status_label,
        );
        let _ = self.post_terminal_transition(task, true);
        self.inner.terminal_transition.notify_waiters();
        let _ = self.inner.wake_tx.try_send(());
    }

    fn record_remote_cancel(&self, task: &Arc<BgTask>) -> Result<bool, String> {
        {
            let mut db = DeferredDbWrites::new(self, task);
            let mut state = task.state.lock().map_err(|_| "task lock poisoned")?;
            if !state.metadata.is_terminal() {
                let Some(remote) = state.metadata.remote.as_mut() else {
                    return Ok(false);
                };
                remote.cancel_requested = true;
                self.persist_task_locked(task, &state.metadata, &mut db)
                    .map_err(|e| e.to_string())?;
            }
        }
        task.remote_cancel_notify.notify_one();
        Ok(true)
    }

    pub(super) fn kill_remote_task(&self, task: &Arc<BgTask>) -> Result<BgTaskSnapshot, String> {
        if !self.record_remote_cancel(task)? {
            return self.kill(&task.task_id, &task.session_id);
        }
        let bound = remote_kill_settle_bound();
        let state = task.state.lock().map_err(|_| "task lock poisoned")?;
        let (state, _) = task
            .kill_settled
            .wait_timeout_while(state, bound, |s| !s.metadata.is_terminal())
            .map_err(|_| "task lock poisoned")?;
        if !state.metadata.is_terminal() {
            return Err(
                "remote cancellation requested; terminal pending; inspect bash_status (never rerun)".into(),
            );
        }
        drop(state);
        Ok(self.snapshot_with_terminal_cache(task, 8 * 1024))
    }
}

/// Longest a kill waits, after recording a remote task's cancellation, for
/// the runner to report the task's terminal state. If no terminal state has
/// arrived by then, the kill returns "terminal pending"; the cancellation is
/// already recorded, and the terminal may still arrive later and is delivered
/// as the task's completion.
///
/// The cap is small because the wait blocks the calling thread. `bash_kill`
/// runs in the executor's mutating lane: it holds one of the executor's few
/// general worker threads, and the mutating lane admits one job per project at
/// a time, so the project's edits and other mutating calls queue behind the
/// wait too. The scope-end, abort and withdrawal kills also run as executor
/// jobs; the hard-kill path waits on the watchdog thread.
#[cfg(unix)]
const REMOTE_KILL_SETTLE_CAP: Duration = Duration::from_secs(5);

/// Part of the request's budget left over after the wait, for building the
/// reply and handing it back before the tool call's own deadline.
#[cfg(unix)]
const REMOTE_KILL_REPLY_MARGIN: Duration = Duration::from_secs(2);

/// How long [`BgTaskRegistry::kill_remote_task`] may wait for the terminal
/// state: what is left of the current request's budget minus the reply
/// margin, and never more than [`REMOTE_KILL_SETTLE_CAP`]. A kill made
/// outside any request (no budget installed) waits for the cap alone.
#[cfg(unix)]
fn remote_kill_settle_bound() -> Duration {
    crate::bounded_io::current().map_or(REMOTE_KILL_SETTLE_CAP, |budget| {
        budget
            .deadline
            .saturating_duration_since(Instant::now())
            .saturating_sub(REMOTE_KILL_REPLY_MARGIN)
            .min(REMOTE_KILL_SETTLE_CAP)
    })
}

#[cfg(unix)]
/// Every runon job gets AFT's hard-kill limit, rounded up to seconds so the
/// runner does not stop earlier than a caller's millisecond timeout.
fn build_remote_request(
    session_root: &Path,
    cwd: &Path,
    command: &str,
    environment: BTreeMap<String, String>,
    timeout_seconds: u64,
    preset: &exec::PresetParams,
) -> Result<RunRequest, exec::Error> {
    let session_root = session_root
        .canonicalize()
        .map_err(|error| exec::Error::Protocol(error.to_string()))?;
    let session_worktree = worktree_root_containing(&session_root)?.unwrap_or(session_root);
    let Some(worktree_root) = worktree_root_containing(cwd)? else {
        return Err(workdir_outside_session_repository(
            cwd,
            None,
            &session_worktree,
        ));
    };
    if repository_root(&worktree_root) != repository_root(&session_worktree) {
        return Err(workdir_outside_session_repository(
            cwd,
            Some(&worktree_root),
            &session_worktree,
        ));
    }

    // The runner snapshots this key, so it must name the checkout containing cwd.
    exec::build_request(
        &worktree_root,
        &repository_root(&worktree_root),
        cwd,
        command,
        environment,
        Some(timeout_seconds),
        preset,
    )
}

#[cfg(unix)]
fn worktree_root_containing(path: &Path) -> Result<Option<PathBuf>, exec::Error> {
    let path = path
        .canonicalize()
        .map_err(|error| exec::Error::Protocol(error.to_string()))?;
    for ancestor in path.ancestors() {
        let git_marker = ancestor.join(".git");
        if git_marker.is_file() || git_marker.is_dir() {
            return ancestor
                .canonicalize()
                .map(Some)
                .map_err(|error| exec::Error::Protocol(error.to_string()));
        }
    }
    Ok(None)
}

/// The refusal for a runon workdir the runner cannot snapshot. It names the
/// workdir and this session's checkout so the caller can see which path to
/// change. `workdir_checkout` is the checkout containing the workdir when it is
/// in one (it then belongs to a different repository than the session's).
#[cfg(unix)]
fn workdir_outside_session_repository(
    workdir: &Path,
    workdir_checkout: Option<&Path>,
    session_checkout: &Path,
) -> exec::Error {
    // Show the resolved path: a symlinked or relative workdir is otherwise
    // hard to compare with the session checkout.
    let workdir = workdir
        .canonicalize()
        .unwrap_or_else(|_| workdir.to_path_buf());
    let reason = match workdir_checkout {
        None => format!(
            "workdir {} is not inside a git checkout; this session's checkout is {}",
            workdir.display(),
            session_checkout.display()
        ),
        Some(checkout) => format!(
            "workdir {} is in the checkout {}, which belongs to a different repository than this session's checkout {}",
            workdir.display(),
            checkout.display(),
            session_checkout.display()
        ),
    };
    exec::Error::Protocol(format!("cwd resolves outside the workspace key: {reason}"))
}

#[cfg(unix)]
fn repository_root(root: &Path) -> PathBuf {
    let Some(gitdir) = fs::read_to_string(root.join(".git"))
        .ok()
        .and_then(|s| s.strip_prefix("gitdir: ").map(|s| root.join(s.trim())))
    else {
        return root.into();
    };
    fs::read_to_string(gitdir.join("commondir"))
        .ok()
        .and_then(|s| gitdir.join(s.trim()).canonicalize().ok())
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| root.into())
}

#[cfg(unix)]
fn append_output_loss(metadata: &mut PersistedTask) {
    append_environment_disclosure(metadata);
    append_executor_environment_disclosure(metadata);
    for (first, last) in &metadata.incomplete_output {
        let reason = if metadata
            .remote
            .as_ref()
            .is_some_and(|r| r.undelivered_output.contains(&(*first, *last)))
        {
            "never delivered it"
        } else {
            "no longer retained it"
        };
        let warning = format!("output lost between seq {first} and {last}: the executor {reason}");
        let note = metadata
            .execution_note
            .get_or_insert_with(|| format!("remote execution on {RUNNER_ID}"));
        if !note.contains(&warning) {
            note.push_str(&format!("\n{warning}"));
        }
    }
}

/// What the runner reported about the workspace after a remote run, printed
/// after the command's output. Writes on the server are never copied back,
/// so every change it reports is named. A kind of change the runner reported
/// as empty says nothing; a kind it did not report says so, on its own line,
/// instead of reading as "nothing changed". `None` when the command did not
/// run remotely or there is nothing to say.
#[cfg(unix)]
pub(crate) fn remote_report(
    metadata: &crate::bash_background::persistence::PersistedTask,
) -> Option<String> {
    let remote = metadata.remote.as_ref()?;
    let terminal = remote.terminal.as_ref()?;
    if matches!(terminal.outcome, Outcome::RefusedBeforeStart { .. })
        || terminal.ran == Some(Ran::None)
    {
        return None;
    }
    let returned_files = remote.returned_files.as_deref().unwrap_or_default();
    // A file that was copied back is not also listed as left on the server.
    // ck-motor reports `workspace_changes` relative to the checkout root with
    // `/` separators on every platform. On Linux the list comes from `git
    // status --porcelain -z --untracked-files=all` (renames as their
    // destination, untracked files included); on Windows it lists every
    // changed file inside the checkout, git-ignored ones too. Both name a
    // returned file by its checkout-relative path, so the same filter applies.
    let copied_back: Vec<&str> = returned_files
        .iter()
        .filter(|entry| {
            matches!(
                entry.outcome,
                super::return_files::ReturnOutcome::Returned { .. }
            )
        })
        .filter_map(|entry| entry.checkout_path.as_deref())
        .collect();
    let returned =
        (!returned_files.is_empty()).then(|| super::return_files::render_report(returned_files));
    match (returned, render_terminal_report(terminal, &copied_back)) {
        (Some(returned), Some(report)) => Some(format!("{returned}\n{report}")),
        (returned, report) => returned.or(report),
    }
}

/// [`remote_report`] for one terminal record. `copied_back` names files,
/// relative to the checkout as the runner reports them, that were copied back
/// to this machine; they are left out of the changed-files list. The
/// untracked-files list is printed as sent: ck-motor does not send it, and
/// the contract does not state its path base.
#[cfg(unix)]
fn render_terminal_report(terminal: &TerminalRecord, copied_back: &[&str]) -> Option<String> {
    // Paths come from another machine; escaping keeps one per line.
    fn path(path: &str) -> String {
        path.escape_default().to_string()
    }
    fn id(id: Option<&str>) -> String {
        id.map_or_else(|| "none".to_string(), |id| id.chars().take(12).collect())
    }
    let mut blocks: Vec<String> = Vec::new();
    let mut unreported: Vec<&str> = Vec::new();

    let left_on_server = |paths: &'_ [String]| -> Vec<String> {
        paths
            .iter()
            .filter(|path| !copied_back.contains(&path.as_str()))
            .cloned()
            .collect()
    };
    match terminal.workspace_changes.as_deref().map(left_on_server) {
        Some(changes) if changes.is_empty() => {}
        Some(changes) => {
            let mut block =
                String::from("These files changed on the server and were NOT copied back:");
            for changed in &changes {
                block.push_str(&format!("\n  {}", path(changed)));
            }
            blocks.push(block);
        }
        None => unreported.push("changed files"),
    }

    match &terminal.git_state_changed {
        Some(git) if git.changed() => {
            let mut block =
                String::from("Git state changed on the server and was NOT copied back:");
            if git.head_before != git.head_after {
                block.push_str(&format!(
                    "\n  HEAD: {} -> {}",
                    id(git.head_before.as_deref()),
                    id(git.head_after.as_deref())
                ));
            }
            if git.ref_before != git.ref_after {
                let name = |r: Option<&str>| r.map_or_else(|| "detached".to_string(), path);
                block.push_str(&format!(
                    "\n  ref: {} -> {}",
                    name(git.ref_before.as_deref()),
                    name(git.ref_after.as_deref())
                ));
            }
            if git.index_tree_before != git.index_tree_after {
                block.push_str("\n  index tree changed (staged changes differ)");
            }
            if git.stash_count_before != git.stash_count_after {
                let delta = i64::from(git.stash_count_after) - i64::from(git.stash_count_before);
                block.push_str(&format!(
                    "\n  stash count: {} -> {} ({delta:+})",
                    git.stash_count_before, git.stash_count_after
                ));
            }
            blocks.push(block);
        }
        Some(_) => {}
        None => unreported.push("git state"),
    }

    match &terminal.untracked_files {
        Some(untracked) if !untracked.paths.is_empty() || untracked.truncated => {
            let mut block = String::from(
                "These untracked files were created on the server and were NOT copied back:",
            );
            for created in &untracked.paths {
                block.push_str(&format!("\n  {}", path(created)));
            }
            if untracked.truncated {
                block.push_str("\n  (the runner listed only some of them; more were created)");
            }
            blocks.push(block);
        }
        Some(_) => {}
        None => unreported.push("untracked files"),
    }

    match &terminal.ignored_writes {
        Some(ignored) if ignored.count > 0 => {
            let mut block = format!(
                "{} write{} under ignored paths on the server {} NOT copied back",
                ignored.count,
                if ignored.count == 1 { "" } else { "s" },
                if ignored.count == 1 { "was" } else { "were" },
            );
            if ignored.sample_paths.is_empty() {
                block.push('.');
            } else {
                block.push_str(", for example:");
                for written in &ignored.sample_paths {
                    block.push_str(&format!("\n  {}", path(written)));
                }
            }
            blocks.push(block);
        }
        Some(_) => {}
        None => unreported.push("ignored writes"),
    }

    for field in unreported {
        blocks.push(format!("{field}: not reported by the runner"));
    }
    (!blocks.is_empty()).then(|| blocks.join("\n"))
}

#[cfg(not(unix))]
pub(crate) fn remote_report(
    _metadata: &crate::bash_background::persistence::PersistedTask,
) -> Option<String> {
    None
}

#[cfg(unix)]
fn append_environment_disclosure(metadata: &mut PersistedTask) {
    if metadata.stripped_env_names.is_empty() {
        return;
    }
    let disclosure = format!(
        "AFT stripped env names: {}",
        serde_json::to_string(&metadata.stripped_env_names).unwrap()
    );
    let note = metadata
        .execution_note
        .get_or_insert_with(|| format!("remote execution on {RUNNER_ID}"));
    // Each disclosure gets its own line, so the header stays the first line.
    if !note.contains(&disclosure) {
        note.push_str(&format!("\n{disclosure}"));
    }
}

#[cfg(unix)]
fn append_executor_environment_disclosure(metadata: &mut PersistedTask) {
    let Some(names) = metadata
        .remote
        .as_ref()
        .and_then(|r| r.env_not_forwarded.as_ref())
        .filter(|n| !n.is_empty())
    else {
        return;
    };
    let shown = names.len().min(10);
    let mut disclosure = format!(
        "the remote job does not receive: {}",
        names[..shown]
            .iter()
            .map(|name| name.escape_default().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    if names.len() > shown {
        disclosure.push_str(&format!("; +{} more", names.len() - shown));
    }
    let note = metadata
        .execution_note
        .get_or_insert_with(|| format!("remote execution on {RUNNER_ID}"));
    // Each disclosure gets its own line, so the header stays the first line.
    if !note.contains(&disclosure) {
        note.push_str(&format!("\n{disclosure}"));
    }
}

#[cfg(unix)]
fn proves_no_start(error: &exec::Error) -> bool {
    matches!(
        error,
        exec::Error::Transport(
            subc_client_rs::CallError::NotSent(_)
                | subc_client_rs::CallError::StaleRouteHandle(_)
                | subc_client_rs::CallError::CapabilityUnprovided { .. }
                | subc_client_rs::CallError::CapabilityAmbiguous { .. }
                | subc_client_rs::CallError::InvalidCapabilityIdentifier { .. }
        ) | exec::Error::Protocol(_)
            | exec::Error::MissingCapability { .. }
    )
}

#[cfg(all(test, unix))]
#[path = "remote_tests.rs"]
mod tests;
