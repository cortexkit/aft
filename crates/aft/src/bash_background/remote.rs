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
    if let Some(refusal) = remote.pre_start_refusal.as_ref() {
        return Some(refusal.as_response());
    }
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

/// ck-motor's queue wait for jobs eligible for Boat (at most eight vCPUs,
/// without outbound networking). Hosted-only jobs may wait longer. AFT leaves
/// `queue_wait_limit_s` unset so the runner chooses its capacity-aware default.
/// This baseline also caps retries after a proven terminal refusal.
#[cfg(unix)]
const RUNNER_QUEUE_WAIT_LIMIT: Duration = Duration::from_secs(300);

/// Bound queue time without duplicating ck-motor's routing decisions. Its
/// default admission policy allows 300 seconds for Boat-eligible jobs, or
/// min(1800 seconds, the run timeout) for hosted-only jobs. The larger of 300
/// seconds and the run timeout therefore covers either queue before execution
/// consumes its separate runtime budget. Both re-attach and submission-key
/// recovery must allow that time before declaring a job's outcome unknown.
#[cfg(unix)]
fn recovery_queue_wait_allowance(run_timeout: Duration) -> Duration {
    RUNNER_QUEUE_WAIT_LIMIT.max(run_timeout)
}

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
fn draining_delay_ms(
    attempt: u32,
    retry_after_ms: Option<u64>,
    jitter: impl FnOnce(u64) -> u64,
) -> u64 {
    let delay = retry_after_ms
        .map(|hint| hint.max(5000))
        .unwrap_or_else(|| {
            5000_u64
                .saturating_mul(1_u64 << attempt.saturating_sub(1).min(4))
                .min(60000)
        });
    delay.saturating_add(jitter(delay / 5))
}

#[cfg(unix)]
fn retry_jitter_for_task(_task: &BgTask) -> impl FnOnce(u64) -> u64 {
    #[cfg(test)]
    let clock = tests::task_draining_clock(_task);
    move |max| {
        #[cfg(test)]
        if let Some(clock) = clock {
            return clock.retry_jitter_ms(max);
        }
        // Reject the incomplete remainder of the u64 range so every millisecond
        // in the inclusive jitter range has the same probability.
        let range = max + 1;
        let reject_below = range.wrapping_neg() % range;
        loop {
            let mut bytes = [0; 8];
            if let Err(error) = getrandom::fill(&mut bytes) {
                crate::slog_warn!("runon: retry jitter entropy unavailable: {error}");
                // An entropy failure must not turn backoff into an immediate retry.
                return max;
            }
            let sample = u64::from_ne_bytes(bytes);
            if sample >= reject_below {
                return sample % range;
            }
        }
    }
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

/// Shown for a remote task cancelled after it was submitted but before the
/// build server accepted it: with no job id, nothing can confirm whether the
/// command ran.
#[cfg(unix)]
pub(crate) const PRE_ACCEPTANCE_CANCELLED: &str =
    "cancelled before the build server accepted the job; whether it ran is unknown";

/// Prefix shown, followed by `: <error>`, when cancelling such a task
/// failed.
#[cfg(unix)]
pub(crate) const PRE_ACCEPTANCE_CANCEL_FAILED: &str =
    "could not cancel before the build server accepted the job; whether it ran is unknown";

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
/// following, so the advice names the job and how to stop it or wait for its
/// run timeout before rerunning.
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
            "; job {job_id} may still be running on {RUNNER_ID} and holding this workspace {limit}, and AFT can no longer fetch its result: a rerun in this workspace waits behind it and can be refused with queue_wait_exceeded, so wait for it to end or cancel it with bash_kill, then rerun"
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
/// the runner, AFT keeps trying: it stops only once a conservative queue
/// allowance plus the command's run timeout (counted from AFT's latest
/// submission of it) have passed, and never sooner than [`REMOTE_REATTACH_GRACE`]
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
        let (until_job_end, recorded_timeout, run_timeout) = Self::until_job_end(metadata);
        let queue_wait = recovery_queue_wait_allowance(run_timeout);
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
            "a queue wait allowance of {} and its {}run timeout of {} have passed since AFT submitted it, plus {} of grace",
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
            .saturating_add(duration_ms(recovery_queue_wait_allowance(run_timeout)))
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

    fn observe(&mut self, progress: &StreamProgress) {
        if matches!(progress, StreamProgress::Record) {
            self.reset();
        }
    }

    /// An execution record arrived: the job is alive, so recovery starts over.
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
#[serde(tag = "code")]
pub(crate) enum RemotePreStartRefusal {
    #[serde(rename = "runon_cd_outside_checkout")]
    CdOutsideCheckout { message: String, ran: Ran },
}

impl RemotePreStartRefusal {
    fn as_response(&self) -> super::RemoteRefusal {
        match self {
            Self::CdOutsideCheckout { message, .. } => super::RemoteRefusal {
                code: "runon_cd_outside_checkout",
                message: message.clone(),
            },
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct RemoteTask {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submission: Option<KeyedSubmission>,
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
    /// A cancellation the runner still has to be sent (or to answer): set,
    /// with `cancel_requested`, before an owner pause marks the task
    /// finished, and cleared once the runner acknowledges the cancel or
    /// reports a terminal for the job, or no job remains that it could reach.
    /// A task saved finished with this still set gets a worker on replay. If
    /// a runner job id is saved, that worker sends the cancel, so a restart
    /// cannot drop it; without one there is nothing it can reach, and the
    /// worker records the cancellation as failed and clears this.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cancel_pending: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_pause_cancel: Option<String>,
    pub terminal: Option<TerminalRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_start_refusal: Option<RemotePreStartRefusal>,
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
    /// Seq ranges missing before an expired replay's manifest that carried
    /// no chunk counts, where the derived count did not match and recovery
    /// could not fill them: lost output or differently sized chunks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expired_gap_unverified: Vec<(u64, u64)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct KeyedSubmission {
    key: String,
    created_at_ms: u64,
    request_digest: String,
    attempts: u8,
    fresh_key_used: bool,
    resuming: bool,
    reattached: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refusal: Option<TerminalRecord>,
}

#[cfg(unix)]
impl KeyedSubmission {
    fn expires_at(&self, metadata: &PersistedTask) -> u64 {
        let run_timeout = metadata
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_BG_TIMEOUT);
        let submitted_at = metadata
            .remote
            .as_ref()
            .and_then(|r| r.retry.as_ref())
            .and_then(|r| r.resubmitted_at_ms)
            .unwrap_or(metadata.started_at);
        // Admission can wait before execution starts; that wait does not
        // consume the runner's execution timeout.
        let job_end_ms = submitted_at
            .saturating_add(duration_ms(recovery_queue_wait_allowance(run_timeout)))
            .saturating_add(duration_ms(run_timeout));
        self.created_at_ms
            .saturating_add(SUBMISSION_RETENTION_MS)
            .min(job_end_ms)
    }
}

#[cfg(unix)]
const SUBMISSION_RETENTION_MS: u64 = 24 * 60 * 60 * 1000;
#[cfg(unix)]
const SUBMISSION_RECOVERY_ATTEMPTS: u8 = 3;

#[cfg(unix)]
fn new_submission_key(task_id: &str) -> Result<String, String> {
    let mut nonce = [0; 16];
    getrandom::fill(&mut nonce).map_err(|e| e.to_string())?;
    Ok(format!("aft:{task_id}:{}", Uuid::from_bytes(nonce)))
}

// Reattachment is recorded in metadata without adding agent-visible wording.
#[cfg(all(test, unix))]
const REATTACHED_RESULT_SENTENCE: Option<&str> = None;

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

#[cfg(unix)]
fn admission_progress_detail(detail: &str) -> String {
    const MAX_DETAIL_CHARS: usize = 120;
    let single_line: String = detail
        .chars()
        .map(|character| {
            if character.is_control() || matches!(character, '\u{2028}' | '\u{2029}') {
                ' '
            } else {
                character
            }
        })
        .collect();
    let normalized = single_line.split_whitespace().collect::<Vec<_>>().join(" ");
    // Mask complete credentials before a display cut could split their shape.
    let redacted = crate::log_redact::aft_redactor(&normalized);
    if redacted.chars().count() <= MAX_DETAIL_CHARS {
        return redacted.into_owned();
    }
    let (boundary, _) = redacted.char_indices().nth(MAX_DETAIL_CHARS - 1).unwrap();
    format!("{}…", &redacted[..boundary])
}

#[cfg(unix)]
fn admission_progress_line(progress: &exec::types::Progress) -> Option<String> {
    use exec::types::ProgressPhase;
    let step = match progress.phase() {
        ProgressPhase::Upload => "uploading its files".to_owned(),
        ProgressPhase::PlacementWait => "waiting for a free build host".to_owned(),
        ProgressPhase::Connect => "connecting to its build host".to_owned(),
        _ => return None,
    };
    let elapsed =
        crate::lsp::manager::format_retry_delay(Duration::from_millis(progress.phase_elapsed_ms()));
    let detail = progress
        .detail()
        .map(admission_progress_detail)
        .filter(|detail| !detail.is_empty())
        .map(|detail| format!(": {detail}"))
        .unwrap_or_default();
    Some(format!(
        "The remote job is {step} ({elapsed} so far){detail}."
    ))
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
    fn resubmit_for_recovery(&self) -> bool {
        // A confirmed terminal needs only output recovery. Attach preserves the
        // saved terminal and buffered records if executor history has expired.
        self.submission.is_some() && self.gap_recovery.is_none()
    }

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
            last_output_seq: self.last_output_seq(),
        })
    }

    /// The seq of the last output record received, which an expired
    /// replay's missing chunks are counted from. It is not saved apart from
    /// the stream cursor: chunks follow every output record, so while the
    /// file returns are unfinished it is the record just before the first
    /// chunk, and otherwise the cursor itself. Once the manifest has been
    /// handled no expired-replay gap is checked again, so the cursor serves.
    fn last_output_seq(&self) -> Option<u64> {
        match (self.return_first_seq, &self.returned_files) {
            (Some(first), None) => self.last_seq.min(first.checked_sub(1)),
            _ => self.last_seq,
        }
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
    sent_at: Option<Instant>,
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
                worktree_root_containing(&workdir, exec::DirectoryKind::WorkingDirectory)
                    .ok()
                    .flatten()
                    .as_deref(),
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
            sent_at: None,
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
                sink.task
                    .drain_reply
                    .publish_metadata(&state.metadata, sink.task.hard_kill_renewable);
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
            self.task
                .drain_reply
                .publish_metadata(&state.metadata, self.task.hard_kill_renewable);
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
    fn admission_progress(&mut self, progress: &exec::types::Progress) {
        let Some(note) = admission_progress_line(progress) else {
            return;
        };
        let Ok(mut state) = self.task.state.lock() else {
            return;
        };
        if !state.metadata.is_terminal()
            && state.metadata.remote.as_ref().is_some_and(|r| !r.accepted)
        {
            // Replace, never append: pre-acceptance status is ephemeral and is
            // not output, a resume checkpoint, or execution evidence.
            state.remote_admission_note = Some(note);
            self.task.publish_runon_launch_state(&state);
        }
    }
    fn accepted(&mut self, accepted: &Accepted) -> io::Result<()> {
        // Measure receipt before local persistence adds its own latency. An
        // attach after restart has no in-process exec.run send timestamp.
        let capture_ms = self
            .sent_at
            .map(|sent_at| sent_at.elapsed().as_millis().to_string())
            .unwrap_or_else(|| "unknown".into());
        // A resubmitted job returns its files afresh.
        if let Some(returns) = self.returns.as_mut() {
            returns.reset();
        }
        self.commit(|r| {
            if let Some(submission) = r.submission.as_mut() {
                submission.reattached |= accepted.attached;
            }
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
        state.remote_admission_note = None;
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
            "runon: remote task accepted task_id={} session={} job_id={} capture_ms={}",
            self.task.task_id,
            state.metadata.session_id,
            accepted.job_id,
            capture_ms,
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
        self.truncate_output(before_seq, MissingRecords::NoLongerRetained)
    }
    fn undelivered(&mut self, before_seq: u64) -> io::Result<()> {
        self.truncate_output(before_seq, MissingRecords::NeverDelivered)
    }
    fn expired_gap_unverified(&mut self, before_seq: u64) -> io::Result<()> {
        self.truncate_output(before_seq, MissingRecords::ExpiredGapUnverified)
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
    fn return_manifest(
        &mut self,
        manifest: &ReturnManifest,
        expired_replay: Option<u64>,
    ) -> io::Result<()> {
        let report = match self.unsaved_return_report.take() {
            Some(report) => report,
            None => match self.returns.take() {
                Some(mut returns) => returns.manifest(manifest, expired_replay, &mut |staging| {
                    self.record_staging(staging)
                }),
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
        let jitter = retry_jitter_for_task(&self.task);
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
            let resubmit_refusal = r.submission.as_ref().is_some_and(|s| s.resuming)
                && matches!(verdict, Verdict::RunLocally { .. });
            // A keyed refusal belongs to this call, not to the existing job.
            // Keep the existing ID available for cancellation and recovery.
            if resubmit_refusal {
                r.submission.as_mut().unwrap().refusal = Some(record.clone());
                return;
            }
            if let Some(report) = unreported {
                r.returned_files = Some(report);
                r.return_staging = Default::default();
            }
            r.job_id = Some(record.job_id);
            r.terminal = Some(record.clone());
            if r.submission.is_none()
                || !matches!(verdict, Verdict::OutcomeUnknown | Verdict::HistoryExpired)
            {
                r.cancel_pending = false;
            }
            r.gap_recovery = None;
            if retries_draining(r, verdict) {
                if let Some(retry) = r.retry.as_mut() {
                    let now = unix_millis();
                    #[cfg(test)]
                    let now = clock.as_ref().map_or(now, |clock| clock.now());
                    retry.first_refusal_at_ms.get_or_insert(now);
                    retry.next_retry_at_ms = Some(now.saturating_add(draining_delay_ms(
                        retry.attempts,
                        record.retry_after_ms,
                        jitter,
                    )));
                }
            }
        })?;
        if self.task.state.lock().is_ok_and(|state| {
            state
                .metadata
                .remote
                .as_ref()
                .is_some_and(|r| !r.cancel_pending)
        }) {
            remove_owed_cancel_marker(&self.task.paths);
        }
        Ok(())
    }
}

#[cfg(unix)]
impl TaskSink {
    fn settle_never_accepted(&mut self) -> io::Result<()> {
        let mut proven_never_accepted = false;
        self.commit(|r| {
            // Dispatch clears terminal and submission.refusal for the next key.
            // Only a refusal still belonging to the current key permits fallback.
            let current_refusal = r
                .submission
                .as_ref()
                .and_then(|s| s.refusal.as_ref())
                .or(r.terminal.as_ref());
            let current_proof = current_refusal.is_some_and(|record| {
                matches!(
                    exec::grade(record),
                    Verdict::RunLocally {
                        reason: RefusalReason::SubmissionNeverAccepted,
                        ..
                    }
                )
            });
            if !current_proof {
                return;
            }
            proven_never_accepted = true;
            if let Some(submission) = r.submission.as_mut() {
                submission.resuming = false;
                if let Some(refusal) = submission.refusal.take() {
                    r.job_id = Some(refusal.job_id);
                    r.terminal = Some(refusal);
                }
            }
            r.cancel_pending = false;
        })?;
        if !proven_never_accepted {
            return Err(io::Error::other(
                "current keyed submission outcome unknown; local fallback withheld",
            ));
        }
        Ok(())
    }

    /// Save staging files and directories in the task record before the
    /// receiver creates them.
    fn record_staging(&mut self, staging: &super::return_files::StagingRecord) -> io::Result<()> {
        let staging = staging.clone();
        self.commit(|r| {
            r.return_staging.files.extend(staging.files);
            r.return_staging.dirs.extend(staging.dirs);
        })
    }

    fn truncate_output(&mut self, before_seq: u64, kind: MissingRecords) -> io::Result<()> {
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
            let range = (first, before_seq - 1);
            match kind {
                MissingRecords::NoLongerRetained => state.metadata.incomplete_output.push(range),
                MissingRecords::NeverDelivered => {
                    remote.undelivered_output.push(range);
                    state.metadata.incomplete_output.push(range);
                }
                // Not known to be lost output, so not listed as such.
                MissingRecords::ExpiredGapUnverified => remote.expired_gap_unverified.push(range),
            }
        }
        append_output_loss(&mut state.metadata);
        let result = self
            .registry
            .persist_task_locked(&self.task, &state.metadata, &mut db);
        if result.is_err() {
            state.metadata = previous;
            self.task
                .drain_reply
                .publish_metadata(&state.metadata, self.task.hard_kill_renewable);
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
    // Keep the submitting request's local ceiling even if configuration reloads.
    memory_limit_mib: Option<u64>,
    linux_scope: bool,
    linux_scope_max_runtime_secs: u64,
}

/// How `attach_remote` ended when it did not fail.
#[cfg(unix)]
enum Attached {
    /// Re-attached to the job; its records continue on this stream.
    Stream(exec::RemoteStream),
    /// The scope's owner paused it through `bash.kill_scope`, which stopped
    /// this task locally and was waiting for this worker to send the runner's
    /// cancellation. That cancel result is now recorded, so the worker must end
    /// here: another reconnect would only send the cancel again, and giving up
    /// later would overwrite the recorded result with a recovery failure.
    OwnerPauseSettled,
}

#[cfg(unix)]
enum RemoteRequestError {
    Build(exec::Error),
    CdOutsideCheckout(String),
}

#[cfg(unix)]
impl std::fmt::Display for RemoteRequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Build(error) => error.fmt(f),
            Self::CdOutsideCheckout(message) => f.write_str(message),
        }
    }
}

#[cfg(unix)]
const RUNON_CD_OUTSIDE_CHECKOUT_MESSAGE: &str = "runon refused: the command changes into <path>, which is outside the checkout the build server copies (<checkout root>); the command did not run. Set the bash tool's workdir to that folder instead of using cd.";

#[cfg(unix)]
const RUNON_CD_WINDOWS_ABSOLUTE_MESSAGE: &str = "runon refused: the command changes into <path>, which is not valid on the Windows runner because absolute command paths are not translated (checkout <checkout root>); the command did not run. Set the bash tool's workdir to that folder instead of using cd.";

#[cfg(unix)]
fn runon_snapshot_directory_known(
    checkout: &Path,
    path: &Path,
    git: &Path,
    deadline: Instant,
) -> bool {
    let Ok(relative) = path.strip_prefix(checkout) else {
        return false;
    };
    // The MOTOR build runner's snapshot contract (MOTOR repository,
    // docs/snapshot-contents.md, sections 2–3) carries existing tracked files and
    // non-ignored untracked files, but reserves each checkout's top-level target
    // for runner-owned build output. Ask Git itself about standard excludes and
    // tracked overrides; a directory needs a copied file beneath it as proof.
    // Uncertain membership retains the failure cwd and its checkout-escape
    // refusals, rather than assuming a directory change succeeds remotely.
    if Instant::now() >= deadline || relative.starts_with("target") || !path.is_dir() {
        return false;
    }
    let relative = if relative.as_os_str().is_empty() {
        Path::new(".")
    } else {
        relative
    };
    let mut command = std::process::Command::new(git);
    command
        .current_dir(checkout)
        .arg("--literal-pathspecs")
        .args([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
        ])
        .arg(relative)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // Source-file lists regularly exceed capture_probe_command's 4 KiB limit.
    // Use the normal bounded capture; truncated output still stays unknown.
    crate::format::isolate_in_process_group(&mut command);
    let Ok(child) = command.spawn() else {
        return false;
    };
    let Ok(files) = crate::format::wait_with_deadline(child, "git", deadline, || false) else {
        return false;
    };
    // Capture decodes bytes lossily; a replacement path is not evidence that
    // the listed file still exists in the working tree.
    files.status.success()
        && !files.truncated
        && !files.stdout.contains('\u{fffd}')
        && files.stdout.split_terminator('\0').any(|file| {
            !Path::new(file).starts_with("target")
                && std::fs::symlink_metadata(checkout.join(file))
                    .is_ok_and(|metadata| !metadata.is_dir())
        })
}

#[cfg(unix)]
fn runon_cd_refusal(command: &str, request: &RunRequest) -> Result<Option<String>, exec::Error> {
    runon_cd_refusal_with_git(command, request, Path::new("git"))
}

#[cfg(unix)]
fn runon_cd_refusal_with_git(
    command: &str,
    request: &RunRequest,
    git: &Path,
) -> Result<Option<String>, exec::Error> {
    // All directory probes share one budget so a long cd list cannot delay
    // remote dispatch by spending a fresh timeout on every destination.
    let deadline = Instant::now() + Duration::from_secs(2);
    let checkout = Path::new(&request.workspace_key);
    let cwd = exec::DirectoryKind::WorkingDirectory.canonicalize(Path::new(&request.cwd))?;
    // Sibling wire paths are absolute and are not rebased on Linux. A linked
    // worktree also gets a mount beside the workspace under each sibling name.
    let alias_parent = checkout
        .join(".git")
        .is_file()
        .then(|| checkout.parent())
        .flatten();
    let mut siblings = Vec::new();
    for sibling in &request.siblings {
        let path = Path::new(sibling);
        siblings.push(
            path.canonicalize()
                .unwrap_or_else(|_| crate::path_identity::lexical_normalize(path)),
        );
        if let Some((parent, name)) = alias_parent.zip(path.file_name()) {
            siblings.push(crate::path_identity::lexical_normalize(&parent.join(name)));
        }
    }
    let mut homes: Vec<PathBuf> = crate::commands::configure::resolve_account_home_dir()
        .into_iter()
        .collect();
    if let Some(home) = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
    {
        homes.push(
            home.canonicalize()
                .unwrap_or_else(|_| crate::path_identity::lexical_normalize(&home)),
        );
    }
    let windows = request.platform == Some(Platform::Windows);
    let copied = |path: &Path| {
        path.starts_with(checkout) || siblings.iter().any(|sibling| path.starts_with(sibling))
    };
    let known_directories = std::cell::RefCell::new(BTreeMap::<PathBuf, bool>::new());
    let known_directory = |path: &Path| {
        if Instant::now() >= deadline {
            return false;
        }
        if let Some(known) = known_directories.borrow().get(path) {
            return *known;
        }
        let known = std::iter::once(checkout)
            .chain(siblings.iter().map(PathBuf::as_path))
            .filter(|root| path.starts_with(root))
            .max_by_key(|root| root.components().count())
            .is_some_and(|root| runon_snapshot_directory_known(root, path, git, deadline));
        known_directories
            .borrow_mut()
            .insert(path.to_path_buf(), known);
        known
    };
    Ok(
        crate::bash_permissions::scan::top_level_directory_targets(command, &cwd, &known_directory)
            .into_iter()
            .find_map(|target| {
                let template = if windows && target.literal_absolute {
                    RUNON_CD_WINDOWS_ABSOLUTE_MESSAGE
                } else {
                    let invalid = |path: &Path| {
                        // System and unknown paths may exist on the runner. Reject
                        // only local home content or another checkout.
                        let local_only = homes.iter().any(|home| path.starts_with(home))
                            || crate::root_identity::nearest_git_marker(path)
                                .ok()
                                .flatten()
                                .is_some();
                        !copied(path) && local_only
                    };
                    // An unknown shell mode gives mutually exclusive candidates,
                    // unlike separate reachable command branches: both must fail.
                    if !invalid(&target.path)
                        || target
                            .alternate_path
                            .as_ref()
                            .is_some_and(|path| !invalid(path))
                    {
                        return None;
                    }
                    RUNON_CD_OUTSIDE_CHECKOUT_MESSAGE
                };
                Some(
                    template
                        .replace("<checkout root>", &checkout.display().to_string())
                        .replace("<path>", &target.path.display().to_string()),
                )
            }),
    )
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
        memory_limit_mib: Option<u64>,
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
            memory_limit_mib,
            storage_dir,
            slot,
            notify,
            compressed,
            root,
            false,
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
        memory_limit_mib: Option<u64>,
        storage_dir: PathBuf,
        slot: super::super::TaskSlot,
        notify: bool,
        compressed: bool,
        root: Option<PathBuf>,
        linux_scope: bool,
    ) -> Result<String, RegistryError> {
        let mut timings = super::super::take_runon_timings();
        timings.enter("task_setup");
        let canonical_project_root = canonicalized_path(root.as_deref().unwrap_or(&workdir));
        let _admission = self.background_admission_guard(slot);
        self.check_background_slot_typed(slot, &session_id)?;
        let layout = match plan.prepared_task() {
            Some(p) => p.resolved_task(),
            None => allocate_task_layout(&storage_dir, &session_id).map_err(|e| e.to_string())?,
        };
        let layout_cleanup = UncommittedLayout::new(&layout);
        timings.enter("sandbox_payload");
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
        timings.enter("task_metadata");
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
            submission: None,
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
            cancel_pending: false,
            owner_pause_cancel: None,
            terminal: None,
            pre_start_refusal: None,
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
            expired_gap_unverified: Vec::new(),
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
        timings.enter("fallback_snapshot");
        let linux_scope_max_runtime_secs = self.inner.task_scopes.max_runtime().as_secs();
        let digest = crate::sandbox_spawn::save_local_launch(
            &plan,
            root.as_deref().unwrap_or(&workdir),
            &workdir,
            &crate::sandbox_spawn::current_authenticated_principal(),
            &shell_path,
            linux_scope,
            memory_limit_mib,
            linux_scope_max_runtime_secs,
        )?;
        metadata.remote.as_mut().unwrap().fallback_digest = Some(digest);
        timings.enter("task_registration");
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
        self.insert_registered_task(
            metadata,
            layout.paths,
            false,
            crate::bash_changed_files::current(),
            canonical_project_root,
        )?;
        let task = self.task(&task_id).unwrap();
        task.holds_background_slot
            .store(slot.holds_slot(), Ordering::SeqCst);
        task.state
            .lock()
            .map_err(|_| "task lock poisoned")?
            .io_handles = Some(handles);
        let request_root = root.as_deref().unwrap_or(&workdir);
        timings.enter("remote_environment");
        env.remove(super::LINUX_SCOPE_ENV);
        // Configured `bash.env` values apply to local commands only: the
        // ambient environment must not carry one to a build server just
        // because it happens to be exported here. The request's own map is a
        // different thing -- it holds the per-call `env` values plus the user
        // names `bash.runon_env` forwarded -- so it is layered on top and
        // never filtered. `commands::bash_remote_env` owns the disclosure of
        // what a job does not receive.
        let configured_env_names = launch.configured_env_names.clone();
        let shell_environment: crate::sandbox_spawn::ChildEnvironment =
            if !plan.is_native_launcher() && plan.host_shell_path().is_none() {
                std::env::vars_os()
                    .filter(|(key, _)| {
                        !key.to_str()
                            .is_some_and(|name| configured_env_names.iter().any(|n| n == name))
                    })
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
        let explicit_runon = launch.explicit_runon;
        timings.enter("workdir_repository_git");
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
        timings.enter("cd_snapshot_git");
        let request = request
            .map_err(RemoteRequestError::Build)
            .and_then(|request| {
                if explicit_runon {
                    if let Some(message) =
                        runon_cd_refusal(command, &request).map_err(RemoteRequestError::Build)?
                    {
                        return Err(RemoteRequestError::CdOutsideCheckout(message));
                    }
                }
                Ok(request)
            });
        let fallback = LocalFallback {
            plan,
            shell_path,
            linux_scope,
            linux_scope_max_runtime_secs,
            memory_limit_mib,
            env,
            capture_pipeline,
        };
        timings.enter("worker_dispatch");
        self.start_remote_worker(task, Some((request, fallback)), Some(timings))?;
        self.start_watchdog();
        Ok(task_id)
    }

    pub(super) fn resume_remote_task(&self, task_id: &str) -> Result<(), String> {
        self.start_remote_worker(self.task(task_id).ok_or("remote task missing")?, None, None)
    }

    async fn key_fresh_request(
        &self,
        client: &ExecRemoteClient,
        task: &Arc<BgTask>,
        request: RunRequest,
        timings: &mut exec::LaunchTimings,
    ) -> Result<RunRequest, String> {
        timings.enter("supports_submission_key");
        let supported = client
            .supports_submission_key()
            .await
            .map_err(|e| e.to_string())?;
        timings.enter("submission_persistence");
        let previous = task
            .state
            .lock()
            .map_err(|_| "task lock poisoned")?
            .metadata
            .remote
            .clone()
            .unwrap();
        let submission = if supported {
            let digest = match previous
                .retry
                .as_ref()
                .map(|r| &r.request_digest)
                .or_else(|| previous.submission.as_ref().map(|s| &s.request_digest))
            {
                Some(digest) => digest.clone(),
                None => save_retry_request(task, &request)?,
            };
            Some(KeyedSubmission {
                key: if previous.submission.is_some() {
                    new_submission_key(&task.task_id)?
                } else {
                    format!("aft:{}", task.task_id)
                },
                created_at_ms: unix_millis(),
                request_digest: digest,
                attempts: 0,
                fresh_key_used: false,
                resuming: false,
                reattached: false,
                refusal: None,
            })
        } else {
            None
        };
        let request = match &submission {
            Some(submission) => request.with_submission_key(&submission.key),
            None => request,
        };
        let mut db = DeferredDbWrites::new(self, task);
        let mut state = task.state.lock().map_err(|_| "task lock poisoned")?;
        state.metadata.remote.as_mut().unwrap().submission = submission;
        self.persist_task_locked(task, &state.metadata, &mut db)
            .map_err(|e| e.to_string())?;
        Ok(request)
    }

    /// Retry only a durable key, while both the caller's timeout and the
    /// executor's key retention are open. Attempts survive AFT restarts too.
    async fn recover_submission(
        &self,
        client: &mut ExecRemoteClient,
        sink: &mut TaskSink,
        root: &Path,
        mut fresh: bool,
    ) -> Result<Option<exec::RemoteStream>, String> {
        sink.checkpoint_progress().map_err(|e| e.to_string())?;
        loop {
            let mut timings = exec::LaunchTimings::new(Instant::now(), "submission_recovery");
            let metadata = sink
                .task
                .state
                .lock()
                .map_err(|_| "task lock poisoned")?
                .metadata
                .clone();
            let remote = metadata.remote.as_ref().unwrap();
            let Some(mut submission) = remote.submission.clone() else {
                return Ok(None);
            };
            let now = unix_millis();
            let expires = submission.expires_at(&metadata);
            if remote.cancel_requested
                || now >= expires
                || now < submission.created_at_ms
                || submission.attempts >= SUBMISSION_RECOVERY_ATTEMPTS
            {
                return Ok(None);
            }
            let delay = Duration::from_millis(100 << submission.attempts);
            if delay.as_millis() as u64 >= expires - now {
                return Ok(None);
            }
            tokio::time::sleep(delay).await;
            if fresh {
                if submission.fresh_key_used {
                    return Ok(None);
                }
                submission.key = new_submission_key(&sink.task.task_id)?;
                submission.created_at_ms = unix_millis();
                submission.fresh_key_used = true;
            }
            let mut request = restore_retry_request(&sink.task, &submission.request_digest)?
                .with_submission_key(&submission.key);
            if !fresh {
                if let Some(seq) = remote.last_seq {
                    request = request.with_resume_from_seq(seq);
                }
            }
            submission.attempts += 1;
            submission.resuming = !fresh;
            submission.refusal = None;
            // Save the attempt and key before dispatch. A crash can then repeat
            // only the same request, never invent an untracked second job.
            let mut cancelled = false;
            sink.commit(|r| {
                cancelled = r.cancel_requested;
                if cancelled {
                    return;
                }
                r.submission = Some(submission);
                r.terminal = None;
                if fresh {
                    r.job_id = None;
                    r.last_seq = None;
                    r.accepted = false;
                    r.started = None;
                }
            })
            .map_err(|e| e.to_string())?;
            if cancelled {
                return Ok(None);
            }
            fresh = false;
            let remaining = expires.saturating_sub(unix_millis());
            if remaining == 0 {
                return Ok(None);
            }
            let submit = async {
                timings.enter("supports_submission_key");
                if !client.supports_submission_key().await? {
                    return Ok(None);
                }
                let result = run_diagnosed(client, &sink.task, &request, timings).await;
                #[cfg(test)]
                let result = if result.is_ok() && tests::lose_submission_replies(root) {
                    // A round trip confirms dispatch reached the fake executor
                    // before losing the caller's dispatch result.
                    let _ = client.status().await;
                    Err(exec::Error::RecoveryRequired {
                        resume: None,
                        reason: "submission dispatch reply lost".into(),
                    })
                } else {
                    result
                };
                result.map(Some)
            };
            match tokio::time::timeout(Duration::from_millis(remaining), submit).await {
                Ok(Ok(Some(stream))) => return Ok(Some(stream)),
                Ok(Ok(None)) => return Ok(None),
                _ => {
                    // The SDK reconnects the transport after a daemon restart,
                    // but its old route is stale. Reopen that route with the
                    // same caller identity instead of dialing a second client.
                    if let Ok(Ok(reconnected)) = tokio::time::timeout(
                        Duration::from_millis(expires.saturating_sub(unix_millis())),
                        client.reopen_route(subc_protocol::BindIdentity::new(
                            root.display().to_string(),
                            remote.harness.clone(),
                            remote.session.clone(),
                        )),
                    )
                    .await
                    {
                        *client = reconnected;
                    }
                }
            }
        }
    }

    fn start_remote_worker(
        &self,
        task: Arc<BgTask>,
        initial: Option<(Result<RunRequest, RemoteRequestError>, LocalFallback)>,
        timings: Option<exec::LaunchTimings>,
    ) -> Result<(), String> {
        {
            // A finished task whose owed runner cancel the startup pass
            // (`start_owed_cancel_pass`) sends or has sent gets no worker:
            // the cancel goes out once per job.
            let mut owed = owed_cancels()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let owes_cancel = task.state.lock().is_ok_and(|state| {
                state.metadata.is_terminal()
                    && state
                        .metadata
                        .remote
                        .as_ref()
                        .is_some_and(|remote| remote.cancel_pending)
            });
            if owes_cancel {
                if let Some(outcome) = owed.settled.get(&task.task_id) {
                    // This registry read the record before the pass saved
                    // the outcome, and may have rewritten it since: save the
                    // outcome again with the task's own record.
                    apply_owed_cancel_outcome(&task, outcome);
                    let mut db = DeferredDbWrites::new(self, &task);
                    if let Ok(state) = task.state.lock() {
                        let saved = self.persist_task_locked(&task, &state.metadata, &mut db);
                        let still_owed = state
                            .metadata
                            .remote
                            .as_ref()
                            .is_some_and(|remote| remote.cancel_pending);
                        if saved.is_ok() && !still_owed {
                            remove_owed_cancel_marker(&task.paths);
                        }
                    }
                    return Ok(());
                }
                if let Some(OwedCancelOwner::Pass { adopters }) = owed.claims.get_mut(&task.task_id)
                {
                    adopters.push(Arc::downgrade(&task));
                    return Ok(());
                }
            }
            // Two recovery paths can restore the same saved task (see
            // `SavedRemoteTaskClaim`); a second worker would act on the task
            // without seeing what the first one decided, so only one may run.
            if task.remote_worker_running.swap(true, Ordering::AcqRel) {
                return Ok(());
            }
            // While this worker runs, the startup pass leaves the task's
            // cancel to it.
            owed.claims
                .entry(task.task_id.clone())
                .or_insert(OwedCancelOwner::Worker);
        }
        #[cfg(test)]
        tests::note_remote_worker_start(&task.task_id);
        let registry = self.clone();
        let spawned = Arc::clone(&task);
        std::thread::Builder::new()
            .name(format!("aft-remote-{}", task.task_id))
            .spawn(move || {
                let run = || {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|e| e.to_string())
                        .and_then(|rt| {
                            rt.block_on(registry.run_remote_task(task.clone(), initial, timings))
                        })
                };
                #[cfg(test)]
                let result = tests::capture_remote_launch_logs(&task, run);
                #[cfg(not(test))]
                let result = run();
                if let Err(error) = result {
                    if registry.finish_owner_pause_cancel(&task, Err(error.clone())) {
                        task.remote_launch_ready.store(true, Ordering::Release);
                        release_worker_owed_cancel_claim(&task.task_id);
                        task.remote_worker_running.store(false, Ordering::Release);
                        #[cfg(test)]
                        tests::note_remote_worker_exit(&task.task_id);
                        return;
                    }
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
                release_worker_owed_cancel_claim(&task.task_id);
                task.remote_worker_running.store(false, Ordering::Release);
                #[cfg(test)]
                tests::note_remote_worker_exit(&task.task_id);
            })
            .map_err(|e| {
                release_worker_owed_cancel_claim(&spawned.task_id);
                spawned
                    .remote_worker_running
                    .store(false, Ordering::Release);
                e.to_string()
            })?;
        Ok(())
    }

    async fn run_remote_task(
        &self,
        task: Arc<BgTask>,
        mut initial: Option<(Result<RunRequest, RemoteRequestError>, LocalFallback)>,
        timings: Option<exec::LaunchTimings>,
    ) -> Result<(), String> {
        let mut timings =
            timings.unwrap_or_else(|| exec::LaunchTimings::new(Instant::now(), "recovery_setup"));
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
        timings.enter("worker_setup");
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
        let resume_submission = initial.is_none()
            && remote.resubmit_for_recovery()
            && (remote.terminal.as_ref().is_some_and(|record| {
                exec::grade(record) == Verdict::OutcomeUnknown
                    || matches!(
                        record.outcome,
                        Outcome::RefusedBeforeStart {
                            reason: RefusalReason::SubmissionNeverAccepted
                        }
                    )
            }) || remote.terminal.is_none());
        if let Some(record) = remote.terminal.as_ref().filter(|_| !resume_submission) {
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
                    let result = self.restored_remote_fallback(
                        &task,
                        &remote,
                        &reason,
                        refusal_detail.as_deref(),
                    );
                    // An owner pause recorded before this worker started already
                    // ended the task, and the fallback above declined to launch.
                    // The runner refused the job, so withholding the local run
                    // is the whole cancellation; record it, as `bash.kill_scope`
                    // waits for it.
                    self.settle_owner_pause_without_resubmit(&task);
                    return result;
                }
                self.remote_terminal(&task, exec::grade(record), None);
                // The runner had already finished the job; there is nothing
                // left to cancel for an owner pause recorded before this worker.
                self.settle_owner_pause_without_resubmit(&task);
                return Ok(());
            }
        }
        if let Some((Err(RemoteRequestError::CdOutsideCheckout(message)), _)) = initial.as_ref() {
            self.refuse_cd_before_start(&task, message);
            return Ok(());
        }
        if let Some((
            Err(RemoteRequestError::Build(exec::Error::ResolveDirectory { kind, path, source })),
            _,
        )) = initial.as_ref().filter(|_| remote.explicit_runon)
        {
            // A local path error must not be hidden by a failed connection to the runner.
            let reason = if source.kind() == io::ErrorKind::NotFound {
                "does not exist"
            } else {
                "cannot be resolved"
            };
            let os_error = source
                .raw_os_error()
                .map(|code| format!("os error {code}"))
                .unwrap_or_else(|| source.to_string());
            self.remote_terminal(
                &task,
                Verdict::RunLocally {
                    reason: RefusalReason::Unreachable,
                    refusal_detail: None,
                },
                Some(format!(
                    "runon refused: {kind} {reason}: {}; command did not run ({os_error})",
                    path.display()
                )),
            );
            return Ok(());
        }
        timings.enter("connect_route");
        let connection = remote
            .connection_file
            .as_ref()
            .ok_or_else(|| "daemon connection file unavailable".to_string());
        let mut keyed_connect_attempts = 0;
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
            let result = if resume_submission {
                let metadata = task
                    .state
                    .lock()
                    .map_err(|_| "task lock poisoned")?
                    .metadata
                    .clone();
                let submission = remote.submission.as_ref().unwrap();
                let now = unix_millis();
                let expires = submission.expires_at(&metadata);
                if now < submission.created_at_ms || now >= expires {
                    return Err("remote outcome unknown; keyed recovery time limit passed".into());
                }
                keyed_connect_attempts += 1;
                tokio::time::timeout(Duration::from_millis(expires - now), connect)
                    .await
                    .unwrap_or_else(|_| Err("remote connection timed out".into()))
            } else if initial.is_none() && retry_request.is_none() {
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
                    if resume_submission {
                        if keyed_connect_attempts < SUBMISSION_RECOVERY_ATTEMPTS {
                            tokio::time::sleep(Duration::from_millis(
                                100 << (keyed_connect_attempts - 1),
                            ))
                            .await;
                            continue;
                        }
                        return Err(format!("remote outcome unknown; keyed recovery connection attempts exhausted: {error}"));
                    }
                    if retry_request.is_some() {
                        self.refuse_remote_before_dispatch(&task, &error)?;
                        // A pause that landed while reconnecting already ended
                        // the task; the refusal changed nothing.
                        self.settle_owner_pause_without_resubmit(&task);
                        return Ok(());
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
        timings.enter("retry_preparation");
        let mut fallback = None;
        let mut hinted: Option<HintedRetry> = None;
        if remote.accepted && !platform_acknowledged(&remote) {
            if let Some(job_id) = remote.job_id {
                let _ = client.cancel_job(job_id).await;
            }
            return Err("remote outcome unknown: Windows platform was not acknowledged; cancel requested; result not trusted".into());
        }
        let mut stream = if resume_submission {
            let refusal = remote
                .submission
                .as_ref()
                .and_then(|s| s.refusal.as_ref())
                .or_else(|| {
                    remote.terminal.as_ref().filter(|r| {
                        matches!(
                            r.outcome,
                            Outcome::RefusedBeforeStart {
                                reason: RefusalReason::SubmissionNeverAccepted
                            }
                        )
                    })
                });
            if let Some(record) = refusal {
                if let Verdict::RunLocally {
                    reason,
                    refusal_detail,
                } = exec::grade(record)
                {
                    if reason != RefusalReason::SubmissionNeverAccepted {
                        return Err(format!(
                            "remote keyed resubmit refused: {}; {}; original job outcome unknown",
                            String::from(reason),
                            refusal_detail.unwrap_or_default()
                        ));
                    }
                }
            }
            let fresh = refusal.is_some_and(|r| {
                matches!(
                    &r.outcome,
                    Outcome::RefusedBeforeStart {
                        reason: RefusalReason::SubmissionNeverAccepted
                    }
                )
            });
            match self
                .recover_submission(&mut client, &mut sink, &root, fresh)
                .await?
            {
                Some(stream) => stream,
                None if fresh => {
                    sink.settle_never_accepted().map_err(|e| e.to_string())?;
                    return self.restored_remote_fallback(
                        &task,
                        &remote,
                        "submission_never_accepted",
                        None,
                    );
                }
                None => {
                    return Err(
                        "remote outcome unknown; keyed recovery unavailable or exhausted".into(),
                    )
                }
            }
        } else if retry_request.is_some() {
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
            let request = self
                .key_fresh_request(&client, &task, request, &mut timings)
                .await?;
            if task
                .state
                .lock()
                .map_err(|_| "task lock poisoned")?
                .metadata
                .remote
                .as_ref()
                .is_some_and(|remote| remote.cancel_requested)
            {
                if !self.finish_owner_pause_cancel(&task, Ok(())) {
                    self.remote_terminal(&task, Verdict::Cancelled, None);
                }
                return Ok(());
            }
            match run_diagnosed(&client, &task, &request, timings).await {
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
                Err(error) => self
                    .recover_submission(&mut client, &mut sink, &root, false)
                    .await?
                    .ok_or_else(|| format!("remote outcome unknown; not resubmitted: {error}"))?,
            }
        } else {
            let point = remote.point().ok_or("remote outcome unknown: acceptance job ID was not persisted; command was not resubmitted")?;
            match self
                .attach_remote(&mut client, point, &remote, &root, &task, &mut recovery)
                .await?
            {
                Attached::Stream(stream) => stream,
                Attached::OwnerPauseSettled => return Ok(()),
            }
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
                    // Stands in for the daemon stopping before it sent the
                    // cancellation to the runner.
                    #[cfg(test)]
                    if tests::stop_draining_worker(&root) {
                        return Ok(());
                    }
                    let result = client
                        .cancel_job(point.job_id)
                        .await
                        .map(|_| ())
                        .map_err(|e| e.to_string());
                    if self.finish_owner_pause_cancel(&task, result) {
                        return Ok(());
                    }
                    stream = match self
                        .attach_remote(&mut client, point, &remote, &root, &task, &mut recovery)
                        .await?
                    {
                        Attached::Stream(stream) => stream,
                        Attached::OwnerPauseSettled => return Ok(()),
                    };
                    cancelled = true;
                } else {
                    // Before the runner accepts the submission, there is no job
                    // ID for exec.cancel or attach. Cancel the active call instead.
                    // The SDK closes its receiver, so this cannot prove the job stopped.
                    let result = client
                        .cancel_submission(&stream)
                        .await
                        .map_err(|error| format!("{PRE_ACCEPTANCE_CANCEL_FAILED}: {error}"));
                    if self.finish_owner_pause_cancel(&task, result.clone()) {
                        return Ok(());
                    }
                    result?;
                    return Err(PRE_ACCEPTANCE_CANCELLED.into());
                }
            }
            sink.sent_at = stream.sent_at();
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
            if let Ok(progress) = &result {
                recovery.observe(progress);
            }
            match result {
                Ok(StreamProgress::AdmissionProgress | StreamProgress::Record) => {}
                Ok(StreamProgress::Complete(Verdict::RunLocally {
                    reason,
                    refusal_detail,
                })) => {
                    #[cfg(test)]
                    if reason == RefusalReason::SubmissionNeverAccepted
                        && tests::stop_draining_worker(&root)
                    {
                        return Ok(());
                    }
                    let submission = task
                        .state
                        .lock()
                        .map_err(|_| "task lock poisoned")?
                        .metadata
                        .remote
                        .as_ref()
                        .and_then(|r| r.submission.clone());
                    if let Some(submission) = submission {
                        if reason == RefusalReason::SubmissionNeverAccepted {
                            if let Some(next) = self
                                .recover_submission(&mut client, &mut sink, &root, true)
                                .await?
                            {
                                stream = next;
                                continue;
                            }
                            sink.settle_never_accepted().map_err(|e| e.to_string())?;
                        } else if submission.resuming {
                            return Err(format!("remote keyed resubmit refused: {}; {}; original job outcome unknown", String::from(reason), refusal_detail.unwrap_or_default()));
                        }
                    }
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
                    #[cfg(test)]
                    tests::draining_gate(&root, tests::DrainingGate::BeforeRefusalReport).await;
                    let reason = serde_json::to_value(reason)
                        .unwrap()
                        .as_str()
                        .unwrap_or("unknown")
                        .to_owned();
                    let result = if let Some(fallback) = fallback.take() {
                        // A live refusal uses the launch plan captured before
                        // dispatch. The private disk snapshot is for restarts.
                        self.remote_fallback(&task, fallback, &reason, refusal_detail.as_deref())
                    } else {
                        self.restored_remote_fallback(
                            &task,
                            &remote,
                            &reason,
                            refusal_detail.as_deref(),
                        )
                    };
                    // If the owner's pause landed after this refusal arrived
                    // but before the lines above reported it, the pause had
                    // already ended the task (so the report changed nothing)
                    // and the fallback declined to run the command locally.
                    // The runner refused the job, so nothing is left running:
                    // record the pause's outcome, which `bash.kill_scope`
                    // waits for. (A failed fallback is recorded as failed
                    // when this worker exits.)
                    if result.is_ok() {
                        self.settle_owner_pause_without_resubmit(&task);
                    }
                    return result;
                }
                Ok(StreamProgress::Complete(verdict)) => {
                    #[cfg(test)]
                    if verdict == Verdict::OutcomeUnknown && tests::stop_draining_worker(&root) {
                        return Ok(());
                    }
                    if !platform_ok {
                        return Err("remote outcome unknown: Windows platform was not acknowledged; result not trusted".into());
                    }
                    if verdict == Verdict::OutcomeUnknown {
                        if let Some(next) = self
                            .recover_submission(&mut client, &mut sink, &root, false)
                            .await?
                        {
                            stream = next;
                            continue;
                        }
                    }
                    self.remote_terminal(&task, verdict, None);
                    return Ok(());
                }
                Err(error) => {
                    #[cfg(test)]
                    if stream
                        .resume_point()
                        .is_some_and(|point| point.gap_recovery.is_some())
                        && tests::stop_draining_worker(&root)
                    {
                        return Ok(());
                    }
                    let keyed = task
                        .state
                        .lock()
                        .map_err(|_| "task lock poisoned")?
                        .metadata
                        .remote
                        .as_ref()
                        .is_some_and(RemoteTask::resubmit_for_recovery);
                    if keyed {
                        if let Some(next) = self
                            .recover_submission(&mut client, &mut sink, &root, false)
                            .await?
                        {
                            stream = next;
                            continue;
                        }
                        return Err(format!("remote outcome unknown; keyed recovery unavailable or exhausted: {error}"));
                    }
                    let Some(point) = stream.resume_point() else {
                        return Err(format!("remote outcome unknown; not resubmitted: {error}"));
                    };
                    // Even an undecodable frame proves this was not an empty
                    // attach. A quiet but open stream has no output deadline.
                    if stream.received_frame() {
                        recovery.reset();
                    }
                    // Without a submission key, only attach can recover an
                    // accepted job without risking a second execution.
                    recovery.pause(point.job_id).await?;
                    stream = match self
                        .attach_remote(&mut client, point, &remote, &root, &task, &mut recovery)
                        .await?
                    {
                        Attached::Stream(stream) => stream,
                        Attached::OwnerPauseSettled => return Ok(()),
                    };
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
                self.cancel_draining_retry(task);
                return Ok(None);
            }
            #[cfg(test)]
            if tests::stop_draining_worker(root) {
                return Ok(None);
            }
            #[cfg(test)]
            tests::draining_gate(root, tests::DrainingGate::BeforeBudgetCheck).await;
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
                self.refuse_remote_executor(task, "runner_draining", refusal_detail.as_deref())?;
                // An owner pause that landed after the cancellation check above
                // already ended the task, so the refusal changed nothing.
                self.settle_owner_pause_without_resubmit(task);
                return Ok(None);
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
        let mut timings = exec::LaunchTimings::new(Instant::now(), "draining_retry");
        // Recheck after connecting: reconnect time also consumes the retry budget.
        let Some(request) = self.wait_draining_retry(task, root).await? else {
            return Ok(None);
        };
        #[cfg(test)]
        tests::draining_gate(root, tests::DrainingGate::BeforeResubmit).await;
        let keyed_request = self
            .key_fresh_request(client, task, request.clone(), &mut timings)
            .await?;
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
                self.cancel_draining_retry(task);
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
        match resubmit(client, task, &keyed_request, root, timings).await {
            Ok(stream) => Ok(Some((stream, hinted))),
            Err(error) if proves_no_start(&error) => {
                #[cfg(test)]
                tests::draining_gate(root, tests::DrainingGate::BeforeNoStartRefusal).await;
                self.refuse_remote_before_dispatch(task, &error.to_string())?;
                // A pause that landed meanwhile already ended the task; the
                // resubmission never reached the runner, so nothing runs.
                self.settle_owner_pause_without_resubmit(task);
                Ok(None)
            }
            Err(error) => Err(format!("remote outcome unknown; not resubmitted: {error}")),
        }
    }

    /// Act on a refusal the runner marked transient with a retry hint: wait
    /// for the hint (at least five seconds) plus jitter, then send the same
    /// request again. The refusal is reported unchanged when it carries no
    /// usable hint, the jittered delay exceeds the job's admission time left,
    /// that time has run out by the moment of
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
        let mut timings = exec::LaunchTimings::new(Instant::now(), "hinted_retry");
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
        let delay = draining_delay_ms(1, Some(hint), retry_jitter_for_task(task));
        let now = draining_now_ms(root);
        if delay > retry.deadline_ms.saturating_sub(now) {
            return Ok(HintedRetryStep::Surface);
        }
        let reason = match &terminal.outcome {
            Outcome::RefusedBeforeStart { reason } => String::from(reason.clone()),
            _ => String::new(),
        };
        crate::slog_info!(
            "runon: resubmitting job {} after its refusal before start (reason={reason} detail={} retry_after_ms={hint} delay_ms={delay}); resubmission {} of at most {HINTED_RETRY_MAX_RESUBMITS}",
            terminal.job_id,
            terminal.refusal_detail().unwrap_or_default(),
            retry.resubmits + 1,
        );
        let until = now.saturating_add(delay);
        // Stands in for the daemon stopping while it waits: the worker ends
        // and leaves only the refusal it has already saved.
        #[cfg(test)]
        if tests::stop_draining_worker(root) {
            return Ok(HintedRetryStep::Ended);
        }
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
                // The runner refused this submission, so for an owner pause
                // withholding the resubmission is the whole cancellation.
                self.cancel_draining_retry(task);
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
        #[cfg(test)]
        tests::draining_gate(root, tests::DrainingGate::BeforeHintedResubmit).await;
        let request = self
            .key_fresh_request(client, task, retry.request.clone(), &mut timings)
            .await?;
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
                self.cancel_draining_retry(task);
                return Ok(HintedRetryStep::Ended);
            }
            // The wait can end late, so check the admission time again right
            // before sending: once it has run out, report the saved refusal as it is.
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
        match resubmit(client, task, &request, root, timings).await {
            Ok(stream) => Ok(HintedRetryStep::Resubmitted(stream)),
            Err(error) if proves_no_start(&error) => {
                #[cfg(test)]
                tests::draining_gate(root, tests::DrainingGate::BeforeNoStartRefusal).await;
                self.refuse_remote_before_dispatch(task, &error.to_string())?;
                // A pause that landed meanwhile already ended the task; the
                // resubmission never reached the runner, so nothing runs.
                self.settle_owner_pause_without_resubmit(task);
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
    ) -> Result<Attached, String> {
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
                        let result = client
                            .cancel_job(point.job_id)
                            .await
                            .map(|_| ())
                            .map_err(|e| e.to_string());
                        if self.finish_owner_pause_cancel(task, result) {
                            return Some(Attached::OwnerPauseSettled);
                        }
                    }
                    client
                        .attach(point.clone())
                        .await
                        .ok()
                        .map(Attached::Stream)
                })
                .await?;
            if let Some(attached) = attempt {
                return Ok(attached);
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
        // Checked under the state lock that every cancellation intent is
        // recorded under, so an intent recorded first always prevents the
        // local start.
        let cancel_requested = state
            .metadata
            .remote
            .as_ref()
            .is_some_and(|r| r.cancel_requested)
            || durable.remote.as_ref().is_some_and(|r| r.cancel_requested);
        if cancel_requested {
            if let Some(remote) = state.metadata.remote.as_mut() {
                remote.cancel_requested = true;
            }
            drop(state);
            drop(db);
            // For an owner pause, not starting the command is the whole
            // cancellation: the runner refused the job.
            if self.finish_owner_pause_cancel(task, Ok(())) {
                return Ok(());
            }
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
        let hard_kill = if metadata.default_hard_kill {
            super::super::HardKill::Default
        } else {
            super::super::HardKill::After(Duration::from_millis(metadata.timeout_ms.unwrap_or(0)))
        };
        let mut scope = self
            .inner
            .task_scopes
            .policy(
                local.linux_scope,
                Some(Duration::from_secs(local.linux_scope_max_runtime_secs)),
                local.plan.is_native_launcher(),
            )
            .map(|policy| {
                self.inner.task_scopes.prepare(
                    policy,
                    &task.task_id,
                    hard_kill,
                    local.memory_limit_mib,
                    local.plan.is_native_launcher(),
                )
            })
            .transpose()?
            .flatten();
        let mut memory_monitor = if scope.is_some() {
            None
        } else {
            Self::prepare_memory_limit(local.memory_limit_mib, &task.task_id)?
        };
        let gated = scope.is_some() || memory_monitor.is_some();
        let mut spawn_child = |scope: Option<&super::super::task_scope::ScopeLaunch>| {
            spawn_detached_child(
                &local.plan,
                &metadata.command,
                super::super::BashShell::Bash,
                &local.shell_path,
                &task.paths,
                &metadata.workdir,
                &local.env,
                state.io_handles.as_mut().ok_or("task handles absent")?,
                local.capture_pipeline,
                local.memory_limit_mib.is_some(),
                scope,
                if gated {
                    LocalIdentityGate::Required
                } else {
                    LocalIdentityGate::CommittedFallback
                },
            )
        };
        let mut child = spawn_child(scope.as_ref()).and_then(|mut child| {
            if let Some(launch) = scope.as_ref() {
                if self.place_task_scope(&mut child, launch)? {
                    scope = None;
                    memory_monitor =
                        Self::prepare_memory_limit(local.memory_limit_mib, &task.task_id)?;
                    return spawn_child(None);
                }
            }
            Ok(child)
        })?;
        if scope.is_some() {
            match self.scoped_memory_limit(local.memory_limit_mib, &task.task_id) {
                Ok(monitor) => memory_monitor = monitor,
                Err(error) => {
                    drop(child.stdin.take());
                    if let Some(scope) = scope.as_ref() {
                        let _ = scope.stop();
                    }
                    let _ = child.wait();
                    return Err(error);
                }
            }
        }
        if let Some(mut monitor) = memory_monitor.take() {
            if let Err(error) = monitor.attach(child.id()) {
                super::super::process::terminate_pgid(child.id() as i32, None);
                self.kill_task_scope(&task.task_id)?;
                if let Some(scope) = scope.as_ref() {
                    let _ = scope.stop();
                }
                let _ = child.wait();
                return Err(error);
            }
            state.metadata.memory_policy = Some(monitor.policy.clone());
            // Persist the chosen enforcer before the gated fallback command can run.
            if let Err(error) = self.persist_task_locked(task, &state.metadata, &mut db) {
                drop(child.stdin.take());
                if let Some(scope) = scope.as_ref() {
                    let _ = scope.stop();
                }
                let _ = child.wait();
                return Err(error.to_string());
            }
            self.inner
                .memory_monitors
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .insert(task.task_id.clone(), monitor);
        }
        if gated {
            if local.memory_limit_mib.is_none() {
                state.metadata.status = BgTaskStatus::Starting;
                state.metadata.child_pid = Some(child.id());
                state.metadata.pgid = Some(child.id() as i32);
                state.metadata.child_start_time_ms =
                    crate::root_cache::process_start_time_ms(child.id());
                if let Err(error) = self.persist_task_locked(task, &state.metadata, &mut db) {
                    drop(child.stdin.take());
                    if let Some(scope) = scope.as_ref() {
                        let _ = scope.stop();
                    }
                    let _ = child.wait();
                    return Err(error.to_string());
                }
            }
            use std::io::Write;
            child
                .stdin
                .take()
                .ok_or("local identity launch gate missing")?
                .write_all(b"run\n")
                .map_err(|error| error.to_string())?;
        }
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
            let (
                plan,
                shell_path,
                env,
                linux_scope,
                memory_limit_mib,
                linux_scope_max_runtime_secs,
            ) = crate::sandbox_spawn::restore_local_launch(
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
                    linux_scope_max_runtime_secs,
                    memory_limit_mib,
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
        let workspace = |metadata: &PersistedTask| {
            worktree_root_containing(&metadata.workdir, exec::DirectoryKind::WorkingDirectory)
                .ok()
                .flatten()
        };
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

    /// The path check runs before runner connection or submission. Record that
    /// the command did not run, without a runner verdict or local fallback.
    fn refuse_cd_before_start(&self, task: &Arc<BgTask>, message: &str) {
        {
            let mut db = DeferredDbWrites::new(self, task);
            let Ok(mut state) = task.state.lock() else {
                return;
            };
            if state.metadata.is_terminal() {
                return;
            }
            let Some(remote) = state.metadata.remote.as_mut() else {
                return;
            };
            remote.pre_start_refusal = Some(RemotePreStartRefusal::CdOutsideCheckout {
                message: message.to_owned(),
                ran: Ran::None,
            });
            state.metadata.execution_note = Some(message.to_owned());
            state
                .metadata
                .mark_terminal(BgTaskStatus::Failed, None, Some(message.to_owned()));
            persist_terminal_artifacts(&task.paths, &mut state.metadata);
            state.metadata.pipeline_status_unavailable =
                Some("command refused before start; no pipeline segments ran".into());
            task.mark_terminal_now();
            let _ = self.persist_task_locked(task, &state.metadata, &mut db);
            task.kill_settled.notify_all();
        }
        let _ = self.post_terminal_transition(task, true);
        self.inner.terminal_transition.notify_waiters();
        let _ = self.inner.wake_tx.try_send(());
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
        // Known terminal outcomes keep their status. Unknown outcomes reach
        // this point only after recovery was unavailable or exhausted.
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
        self.record_remote_cancel_owed(task, false)
    }

    /// Record the cancellation intent on a remote task that is not finished
    /// yet, and wake its worker. With `owed`, also record that the runner
    /// is still owed the cancel (`RemoteTask::cancel_pending`), in the same
    /// write: a caller about to mark the task finished before the worker has
    /// sent it must leave that debt in the saved record. Nothing is owed for
    /// a job the runner already reported a terminal for. Returns false when
    /// the task runs locally.
    pub(super) fn record_remote_cancel_owed(
        &self,
        task: &Arc<BgTask>,
        owed: bool,
    ) -> Result<bool, String> {
        {
            let mut db = DeferredDbWrites::new(self, task);
            let mut state = task.state.lock().map_err(|_| "task lock poisoned")?;
            if !state.metadata.is_terminal() {
                let Some(remote) = state.metadata.remote.as_mut() else {
                    return Ok(false);
                };
                remote.cancel_requested = true;
                if owed && remote.terminal.is_none() {
                    // Indexed before the record says so (see
                    // `write_owed_cancel_marker`).
                    write_owed_cancel_marker(&task.paths);
                    remote.cancel_pending = true;
                }
                self.persist_task_locked(task, &state.metadata, &mut db)
                    .map_err(|e| e.to_string())?;
            }
        }
        task.remote_cancel_notify.notify_one();
        Ok(true)
    }

    pub(super) fn kill_remote_task(&self, task: &Arc<BgTask>) -> Result<BgTaskSnapshot, String> {
        let metadata = task
            .lock_state()
            .map_err(|_| "task lock poisoned")?
            .metadata
            .clone();
        if let Some(job_id) = unknown_remote_cancel_job(&metadata) {
            // The original follower has stopped. Probe cancellation separately,
            // without replacing its saved unknown outcome with an acknowledgement.
            let bound = remote_kill_settle_bound().min(ReattachBudget::until_job_end(&metadata).0);
            let result = std::thread::scope(|scope| {
                scope
                    .spawn(|| {
                        let runtime = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .map_err(|error| error.to_string())?;
                        runtime.block_on(async {
                            tokio::time::timeout(bound, cancel_unknown_remote(&metadata, job_id))
                                .await
                                .map_err(|_| "runner confirmation timed out".to_string())?
                        })
                    })
                    .join()
            })
            .unwrap_or_else(|_| Err("runner cancellation probe stopped".into()));
            let mut snapshot = self.snapshot_with_terminal_cache(task, 8 * 1024);
            snapshot.remote_cancel = Some(result.unwrap_or_else(|error| {
                format!("remote cancellation unconfirmed; outcome is still unknown: {error}")
            }));
            return Ok(snapshot);
        }
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

    /// Record an owner's pause on a remote task before `bash.kill_scope`
    /// stops anything: the pause reason, the cancellation intent and the
    /// cancel the runner is owed, in one write. From then on its worker
    /// cancels the job and settles the pause, and a refusal can no longer
    /// turn it into a local command (`remote_fallback` checks the intent).
    /// A task already finished or running locally is left alone.
    pub(super) fn record_owner_pause_intent(&self, task: &Arc<BgTask>) -> Result<(), String> {
        {
            let mut db = DeferredDbWrites::new(self, task);
            let mut state = task.state.lock().map_err(|_| "task lock poisoned")?;
            if state.metadata.is_terminal() || state.metadata.remote.is_none() {
                return Ok(());
            }
            state.metadata.status_reason = Some(super::OWNER_PAUSED_REASON.into());
            let remote = state.metadata.remote.as_mut().unwrap();
            remote.cancel_requested = true;
            if remote.terminal.is_none() {
                write_owed_cancel_marker(&task.paths);
                remote.cancel_pending = true;
            }
            self.persist_task_locked(task, &state.metadata, &mut db)
                .map_err(|e| e.to_string())?;
        }
        task.remote_cancel_notify.notify_one();
        Ok(())
    }

    pub(super) fn record_owner_pause_remote(&self, task: &Arc<BgTask>) -> Result<(), String> {
        // The task is marked finished below, before its worker can have sent
        // the cancellation; the saved record keeps it owed until then.
        self.record_remote_cancel_owed(task, true)?;
        self.remote_terminal(task, Verdict::Cancelled, None);
        Ok(())
    }

    /// Stop a draining retry, or a retry after a hinted signer refusal, that
    /// has a cancellation intent instead of resubmitting it. The runner refused
    /// the job, so there is no runner job to cancel: for an owner pause,
    /// withholding the resubmission is the whole cancellation, and it is
    /// recorded as sent.
    fn cancel_draining_retry(&self, task: &Arc<BgTask>) {
        if !self.finish_owner_pause_cancel(task, Ok(())) {
            self.remote_terminal(task, Verdict::Cancelled, None);
        }
    }

    /// A draining-retry worker that stops without resubmitting is the last
    /// thing that can record an owner pause's cancellation outcome, which
    /// `bash.kill_scope` waits for. Record it as sent unless the worker
    /// recorded one already; a task that was not paused is left alone.
    fn settle_owner_pause_without_resubmit(&self, task: &Arc<BgTask>) {
        let unsettled = task.state.lock().is_ok_and(|state| {
            state
                .metadata
                .remote
                .as_ref()
                .is_some_and(|remote| remote.owner_pause_cancel.is_none())
        });
        if unsettled {
            self.finish_owner_pause_cancel(task, Ok(()));
        }
    }

    // A pause ends AFT's local task even if the runner cannot be reached. Store
    // the remote cancellation result separately so the owner can see failures
    // without reporting the expected local stop as a command failure.
    fn finish_owner_pause_cancel(&self, task: &Arc<BgTask>, result: Result<(), String>) -> bool {
        let terminal = {
            let mut db = DeferredDbWrites::new(self, task);
            let Ok(mut state) = task.state.lock() else {
                return false;
            };
            if state.metadata.status_reason.as_deref() != Some(super::OWNER_PAUSED_REASON) {
                return false;
            }
            let terminal = state.metadata.is_terminal();
            if let Some(remote) = state.metadata.remote.as_mut() {
                // A failed cancel stays owed, so a replay after a restart
                // tries again, unless no runner job id is known: then no
                // later worker could reach the job either.
                if result.is_ok() || remote.job_id.is_none() {
                    remote.cancel_pending = false;
                }
                remote.owner_pause_cancel = Some(match result {
                    Ok(()) => "sent".into(),
                    Err(error) => format!("failed: {error}"),
                });
            }
            let saved = self.persist_task_locked(task, &state.metadata, &mut db);
            let still_owed = state
                .metadata
                .remote
                .as_ref()
                .is_some_and(|remote| remote.cancel_pending);
            if saved.is_ok() && !still_owed {
                remove_owed_cancel_marker(&task.paths);
            }
            terminal
        };
        task.kill_settled.notify_all();
        if !terminal {
            self.remote_terminal(task, Verdict::Cancelled, None);
        }
        true
    }
}

/// A locally terminal unknown outcome is not proof that the runner stopped.
/// Use the latest submission's queue/run bounds only to limit cancellation
/// attempts, never as evidence of a known outcome.
#[cfg(unix)]
pub(super) fn unknown_remote_cancel_job(metadata: &PersistedTask) -> Option<Uuid> {
    if metadata.status != super::BgTaskStatus::FateUnknown
        || ReattachBudget::until_job_end(metadata).0.is_zero()
    {
        return None;
    }
    let remote = metadata.remote.as_ref()?;
    if remote
        .terminal
        .as_ref()
        .is_some_and(|terminal| terminal.execution_ended())
    {
        return None;
    }
    remote
        .job_id
        .or_else(|| remote.terminal.as_ref().map(|terminal| terminal.job_id))
}

/// Collects a new runner terminal record without modifying the finished task's artifacts.
#[cfg(unix)]
#[derive(Default)]
struct CancelProbe {
    terminal: Option<TerminalRecord>,
}

#[cfg(unix)]
impl OutputSink for CancelProbe {
    fn accepted(&mut self, _: &Accepted) -> io::Result<()> {
        Ok(())
    }
    fn output(&mut self, _: u64, _: OutputStream, _: &[u8]) -> io::Result<()> {
        Ok(())
    }
    fn truncated(&mut self, _: u64) -> io::Result<()> {
        Ok(())
    }
    fn unknown_output(&mut self, _: u64, _: &[u8]) -> io::Result<()> {
        Ok(())
    }
    fn gap_recovery(&mut self, recovery: &exec::GapRecovery) -> io::Result<()> {
        self.terminal = Some(recovery.terminal.clone());
        Ok(())
    }
    fn terminal(&mut self, record: &TerminalRecord, _: &Verdict) -> io::Result<()> {
        self.terminal = Some(record.clone());
        Ok(())
    }
}

#[cfg(unix)]
async fn cancel_unknown_remote(metadata: &PersistedTask, job_id: Uuid) -> Result<String, String> {
    let remote = metadata
        .remote
        .as_ref()
        .ok_or("remote metadata unavailable")?;
    let connection = remote
        .connection_file
        .as_ref()
        .ok_or("no runner connection was saved")?;
    let root = metadata.project_root.as_deref().unwrap_or(Path::new("/"));
    let client = ExecRemoteClient::connect(
        connection,
        subc_protocol::BindIdentity::new(
            root.display().to_string(),
            remote.harness.clone(),
            remote.session.clone(),
        ),
    )
    .await
    .map_err(|error| error.to_string())?;
    client
        .cancel_job(job_id)
        .await
        .map_err(|error| error.to_string())?;
    // A cancel reply only acknowledges intent. Do not reuse a terminal record
    // from gap recovery: the attach after cancellation must observe a new result.
    let mut stream = client
        .attach(ResumePoint {
            job_id,
            last_seq: remote.last_seq,
            gap_recovery: None,
            last_output_seq: remote.last_output_seq(),
        })
        .await
        .map_err(|error| error.to_string())?;
    let mut probe = CancelProbe::default();
    loop {
        let progress = stream.next(&mut probe).await;
        if let Some(terminal) = probe.terminal.as_ref() {
            return Ok(match exec::grade(terminal) {
                Verdict::Cancelled | Verdict::CancelKilled => "runner confirmed cancellation",
                Verdict::Exited { .. }
                | Verdict::Signalled { .. }
                | Verdict::DeadlineKilled
                | Verdict::RunLocally { .. } => "runner reports job already ended",
                _ if terminal.execution_ended() => "runner reports job already ended",
                _ => "remote cancellation requested; outcome is still unknown",
            }
            .into());
        }
        progress.map_err(|error| error.to_string())?;
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

/// Who in this process is sending a saved remote task's owed runner cancel
/// (`RemoteTask::cancel_pending`).
#[cfg(unix)]
enum OwedCancelOwner {
    /// The task's remote worker is running; it sends the cancel itself.
    Worker,
    /// The startup pass (`start_owed_cancel_pass`) is sending it. Registries
    /// that restore the task meanwhile register here as adopters instead of
    /// starting a worker; the pass gives them its outcome when it is done.
    Pass {
        adopters: Vec<std::sync::Weak<BgTask>>,
    },
}

#[cfg(unix)]
#[derive(Default)]
struct OwedCancels {
    /// At most one sender per task id, so the cancel goes out once per job.
    claims: HashMap<String, OwedCancelOwner>,
    /// Owed cancels the startup pass settled, by task id, with the outcome it
    /// recorded. A replay that read the task's record before the pass saved
    /// it takes the outcome from here instead of sending the cancel again.
    settled: HashMap<String, String>,
}

/// The one claim table for owed cancels in this process. Taken with no lock
/// held except, in `start_remote_worker`, the saved-task claim lock (which is
/// always outermost); a task's `state` may be taken while it is held.
#[cfg(unix)]
fn owed_cancels() -> &'static Mutex<OwedCancels> {
    static OWED: std::sync::OnceLock<Mutex<OwedCancels>> = std::sync::OnceLock::new();
    OWED.get_or_init(Default::default)
}

/// A remote worker stopped: the startup pass may send the task's cancel if
/// it is still owed.
#[cfg(unix)]
fn release_worker_owed_cancel_claim(task_id: &str) {
    let mut owed = owed_cancels()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if matches!(owed.claims.get(task_id), Some(OwedCancelOwner::Worker)) {
        owed.claims.remove(task_id);
    }
}

/// Record in a task held in memory the outcome of the startup pass's
/// attempt to send its owed cancel.
#[cfg(unix)]
fn apply_owed_cancel_outcome(task: &BgTask, outcome: &str) {
    if let Ok(mut state) = task.state.lock() {
        if let Some(remote) = state.metadata.remote.as_mut() {
            if !outcome.starts_with("failed") || remote.job_id.is_none() {
                remote.cancel_pending = false;
            }
            remote.owner_pause_cancel = Some(outcome.to_owned());
        }
    }
    task.kill_settled.notify_all();
}

/// The owed-cancel index: next to each storage directory's `bash-tasks`
/// (the storage root's own and each harness directory's), one marker file
/// per saved remote task that owes its runner a cancel
/// (`RemoteTask::cancel_pending`), named by task id and holding its session
/// directory. It sits beside `bash-tasks`, not inside it, because the
/// persisted-task sweep treats every directory in `bash-tasks` as a session.
///
/// A marker is written before the record that sets `cancel_pending` and
/// removed after the record that clears it is saved, so a crash leaves at
/// worst a marker whose record owes nothing, which the startup pass removes.
/// If writing a marker fails (logged), that cancel is not indexed: replay of
/// its project still sends it, but the startup pass will not find it. The
/// pass lists only these markers.
#[cfg(unix)]
const OWED_CANCEL_MARKERS: &str = "bash-owed-cancels";

/// The owed-cancel marker for a task, beside its storage directory's
/// `bash-tasks` (see `OWED_CANCEL_MARKERS`).
#[cfg(unix)]
fn owed_cancel_marker(paths: &TaskPaths) -> Option<PathBuf> {
    let storage_dir = paths.session_dir.parent()?.parent()?;
    Some(storage_dir.join(OWED_CANCEL_MARKERS).join(&paths.task_id))
}

/// Index a cancel the task's record is about to owe. A failure is logged:
/// the record still owes the cancel, and replay still sends it, but the
/// startup pass will not find it.
#[cfg(unix)]
fn write_owed_cancel_marker(paths: &TaskPaths) {
    let Some(marker) = owed_cancel_marker(paths) else {
        return;
    };
    let written = marker
        .parent()
        .map_or(Ok(()), crate::private_storage::create_dir_all)
        .and_then(|()| {
            crate::private_storage::write(&marker, paths.session_dir.as_os_str().as_encoded_bytes())
        });
    if let Err(error) = written {
        crate::slog_warn!(
            "owed runner cancel for task {} not indexed at {}: {error}",
            paths.task_id,
            marker.display()
        );
    }
}

/// Drop a task's owed-cancel marker once its saved record owes nothing.
#[cfg(unix)]
fn remove_owed_cancel_marker(paths: &TaskPaths) {
    if let Some(marker) = owed_cancel_marker(paths) {
        remove_marker_file(&marker);
    }
}

#[cfg(unix)]
fn remove_marker_file(marker: &Path) {
    match fs::remove_file(marker) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => crate::slog_warn!(
            "owed runner cancel marker {} not removed: {error}",
            marker.display()
        ),
    }
}

/// Connection attempts per owed cancel, and the first wait between them; the
/// wait doubles after each failed attempt: 1, 2, 4, 8 and 16 seconds in
/// production, from 50 ms in test builds.
#[cfg(unix)]
const OWED_CANCEL_ATTEMPTS: u32 = 6;
#[cfg(all(unix, not(test)))]
const OWED_CANCEL_FIRST_BACKOFF: Duration = Duration::from_secs(1);
#[cfg(all(unix, test))]
const OWED_CANCEL_FIRST_BACKOFF: Duration = Duration::from_millis(50);

/// What one startup pass did, for its log line and for tests.
#[cfg(unix)]
#[derive(Debug, Default)]
pub(crate) struct OwedCancelReport {
    /// Owed-cancel markers read; no other task record is opened.
    pub examined: usize,
    pub sent: usize,
    pub failed: usize,
    pub cleared_without_job: usize,
    /// Left to a remote worker of this process, or to the replay of a task
    /// saved unfinished, which sends the cancel itself.
    pub left_to_worker: usize,
    /// Markers whose record owed nothing (or was gone), removed unsent.
    pub stale_markers: usize,
}

#[cfg(unix)]
impl BgTaskRegistry {
    /// Send the runner cancels that saved, finished remote tasks under
    /// `storage_root` still owe, without waiting for their projects to bind.
    ///
    /// An owner pause marks a task finished before its worker sends the
    /// runner its cancel. If the module stops in between, the record keeps
    /// the cancel owed (`RemoteTask::cancel_pending`); replay sends it, but a
    /// paused worker's checkout may never bind again, and its runner job then
    /// holds a runner slot until its own deadline. This pass lists the
    /// owed-cancel index (`OWED_CANCEL_MARKERS`) of the storage root and of
    /// each harness directory beneath it, so its cost follows the number of
    /// owed cancels, not the task history, reads only the indexed records,
    /// and sends their cancels one after another on one dedicated background
    /// thread, never on the request loop. It starts no project, binds no
    /// root and restores no task. Runs once per storage root per process
    /// (see `note_storage_root`); later calls return `None`.
    pub(crate) fn start_owed_cancel_pass(
        storage_root: &Path,
    ) -> Option<std::thread::JoinHandle<OwedCancelReport>> {
        static STARTED: std::sync::OnceLock<Mutex<HashSet<PathBuf>>> = std::sync::OnceLock::new();
        let key = canonicalized_path(storage_root);
        if !STARTED
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key)
        {
            return None;
        }
        let storage_root = storage_root.to_path_buf();
        std::thread::Builder::new()
            .name("aft-owed-cancels".into())
            .spawn(move || {
                let report = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime.block_on(send_owed_cancels(&storage_root)),
                    Err(error) => {
                        crate::slog_warn!("owed runner cancels not sent: {error}");
                        OwedCancelReport::default()
                    }
                };
                crate::slog_info!(
                    "owed runner cancels under {}: examined={} sent={} failed={} cleared_without_job={} left_to_worker={} stale_markers={}",
                    storage_root.display(),
                    report.examined,
                    report.sent,
                    report.failed,
                    report.cleared_without_job,
                    report.left_to_worker,
                    report.stale_markers
                );
                report
            })
            .map_err(|error| crate::slog_warn!("owed runner cancel pass not started: {error}"))
            .ok()
    }
}

/// One indexed owed cancel: its marker, the task's session directory and
/// its task id.
#[cfg(unix)]
struct OwedCancelMarker {
    marker: PathBuf,
    session_dir: PathBuf,
    task_id: String,
}

/// The owed-cancel markers under `storage_root`: its own index and each
/// harness directory's (see `OWED_CANCEL_MARKERS`). A marker whose name is
/// not a task id, or that cannot be read, is skipped and logged.
#[cfg(unix)]
fn owed_cancel_markers(storage_root: &Path) -> Vec<OwedCancelMarker> {
    let mut index_dirs = vec![storage_root.join(OWED_CANCEL_MARKERS)];
    if let Ok(entries) = fs::read_dir(storage_root) {
        let mut harness_indexes = entries
            .flatten()
            .map(|entry| entry.path().join(OWED_CANCEL_MARKERS))
            .filter(|index| index.is_dir())
            .collect::<Vec<_>>();
        harness_indexes.sort();
        index_dirs.extend(harness_indexes);
    }
    let mut markers = Vec::new();
    for index in index_dirs {
        let Ok(entries) = fs::read_dir(&index) else {
            continue;
        };
        let mut entries = entries
            .flatten()
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        entries.sort();
        for marker in entries {
            let Some(task_id) = marker
                .file_name()
                .and_then(OsStr::to_str)
                .filter(|name| validate_task_id(name).is_ok())
                .map(str::to_owned)
            else {
                crate::slog_warn!(
                    "owed runner cancel index entry {} ignored",
                    marker.display()
                );
                continue;
            };
            match fs::read(&marker) {
                Ok(bytes) => markers.push(OwedCancelMarker {
                    session_dir: PathBuf::from(String::from_utf8_lossy(&bytes).into_owned()),
                    marker,
                    task_id,
                }),
                Err(error) => crate::slog_warn!(
                    "owed runner cancel marker {} unreadable: {error}",
                    marker.display()
                ),
            }
        }
    }
    markers
}

#[cfg(unix)]
async fn send_owed_cancels(storage_root: &Path) -> OwedCancelReport {
    let markers = owed_cancel_markers(storage_root);
    let mut report = OwedCancelReport {
        examined: markers.len(),
        ..Default::default()
    };
    for OwedCancelMarker {
        marker,
        session_dir,
        task_id,
    } in markers
    {
        {
            let mut owed = owed_cancels()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if owed.claims.contains_key(&task_id) || owed.settled.contains_key(&task_id) {
                report.left_to_worker += 1;
                continue;
            }
            owed.claims.insert(
                task_id.clone(),
                OwedCancelOwner::Pass {
                    adopters: Vec::new(),
                },
            );
        }
        #[cfg(test)]
        tests::draining_gate(storage_root, tests::DrainingGate::OwedCancelClaimed).await;
        let sent = send_owed_cancel(&session_dir, &task_id).await;
        // Saved under the claim table's lock: a registry restoring the task
        // meanwhile either adopts the claim (and gets the outcome below) or
        // finds the outcome settled and saves it again over the older record
        // it read.
        let mut owed = owed_cancels()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let adopters = match owed.claims.remove(&task_id) {
            Some(OwedCancelOwner::Pass { adopters }) => adopters,
            _ => Vec::new(),
        };
        // Registries holding the task get the outcome before the record is
        // saved: their own later save of the task writes what they hold, and
        // must not put the cancel back as owed.
        if let OwedCancelSend::Done { outcome, .. } = &sent {
            for task in adopters.iter().filter_map(std::sync::Weak::upgrade) {
                apply_owed_cancel_outcome(&task, outcome);
            }
        }
        let outcome = save_owed_cancel_outcome(&session_dir, &task_id, &marker, sent);
        match outcome {
            OwedCancelOutcome::Stale => report.stale_markers += 1,
            OwedCancelOutcome::Kept => report.left_to_worker += 1,
            OwedCancelOutcome::Settled(outcome) => {
                if outcome == "sent" {
                    report.sent += 1;
                } else {
                    report.cleared_without_job += 1;
                }
                owed.settled.insert(task_id, outcome);
            }
            OwedCancelOutcome::Failed => report.failed += 1,
        }
    }
    report
}

#[cfg(unix)]
enum OwedCancelOutcome {
    /// Nothing was sent, and the marker was removed: the record is gone, or
    /// owes no cancel (a worker settled it first, or a crash left the marker
    /// after its record was cleared).
    Stale,
    /// Nothing was sent, and the marker stays: the record could not be read,
    /// or the task was saved unfinished, so its replay sends the cancel.
    Kept,
    /// No longer owed: the runner acknowledged the cancel, or no job id was
    /// saved for any cancel to reach. Writing the outcome to the record is
    /// best effort; a failed write is logged.
    Settled(String),
    /// The send failed and the cancel stays owed, so the next process start
    /// tries again (the pass runs once per storage root per process).
    Failed,
}

/// What sending one owed cancel did, before it is saved.
#[cfg(unix)]
enum OwedCancelSend {
    /// The record is gone or owes no cancel.
    NotOwed,
    /// The record could not be read, or the task is saved unfinished.
    Kept,
    /// Whether the record stops owing the cancel, and the outcome to save.
    Done { cleared: bool, outcome: String },
}

/// Save one owed cancel's outcome to the task's record: `cancel_pending` and
/// `owner_pause_cancel`, merged into the latest record under its write lock;
/// no other field is written back. The record is finished, so it is written
/// directly: `update_task_at` keeps a finished record's fields unchanged.
#[cfg(unix)]
fn save_owed_cancel_outcome(
    session_dir: &Path,
    task_id: &str,
    marker: &Path,
    sent: OwedCancelSend,
) -> OwedCancelOutcome {
    let (cleared, outcome) = match sent {
        OwedCancelSend::NotOwed => {
            remove_marker_file(marker);
            return OwedCancelOutcome::Stale;
        }
        OwedCancelSend::Kept => return OwedCancelOutcome::Kept,
        OwedCancelSend::Done { cleared, outcome } => (cleared, outcome),
    };
    let saved = resolve_task_layout(session_dir, task_id).and_then(|layout| {
        let session_id = read_task_at(&layout)?.session_id;
        // The task's record write lock, the one a registry holding the task
        // takes for every save of it (shared through `DbWriteOrder`): the
        // record is read again under it and only this pass's two fields are
        // changed, so a save that landed meanwhile (a cleared cancelled-call
        // notice, say) is kept, never written back to its older value.
        let order = DbWriteOrder::for_task(&session_id, task_id);
        let mut json_writes = order.lock_json_writes();
        let mut record = read_task_at(&layout)?;
        #[cfg(test)]
        tests::owed_cancel_save_gate(task_id);
        if let Some(remote) = record.remote.as_mut() {
            if cleared {
                remote.cancel_pending = false;
            }
            remote.owner_pause_cancel = Some(outcome.clone());
        }
        write_task_at(&layout, &record)?;
        // A checkpoint snapshot taken before this write must not replace it.
        *json_writes += 1;
        Ok(())
    });
    match saved {
        // The marker goes only once the record says nothing is owed.
        Ok(()) if cleared => remove_marker_file(marker),
        Ok(()) => {}
        Err(error) => crate::slog_warn!(
            "owed runner cancel for task {task_id}: outcome {outcome} not saved: {error}"
        ),
    }
    if cleared {
        OwedCancelOutcome::Settled(outcome)
    } else {
        OwedCancelOutcome::Failed
    }
}

/// Send one saved task's owed cancel.
#[cfg(unix)]
async fn send_owed_cancel(session_dir: &Path, task_id: &str) -> OwedCancelSend {
    // Read under the claim: a worker that stopped before the claim was
    // taken may have sent the cancel already.
    let metadata = match resolve_task_layout(session_dir, task_id)
        .and_then(|layout| read_task_at(&layout))
    {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return OwedCancelSend::NotOwed,
        Err(error) => {
            crate::slog_warn!(
                "owed runner cancel for task {task_id}: record unreadable, left owed: {error}"
            );
            return OwedCancelSend::Kept;
        }
    };
    let Some(remote) = metadata
        .remote
        .as_ref()
        .filter(|remote| remote.cancel_pending && metadata.task_id == task_id)
    else {
        return OwedCancelSend::NotOwed;
    };
    if !metadata.is_terminal() {
        // Saved unfinished: the replay of its project restores the task and
        // its worker cancels the job.
        return OwedCancelSend::Kept;
    }
    let (cleared, outcome) = match remote.job_id {
        None => {
            crate::slog_info!(
                "owed runner cancel for task {task_id}: no runner job id was saved, so no cancel can reach it"
            );
            (
                true,
                "failed: no runner job id was saved, so the runner could not be asked to cancel"
                    .to_string(),
            )
        }
        Some(job_id) => match cancel_with_backoff(&metadata, remote, task_id, job_id).await {
            Ok(()) => (true, "sent".to_string()),
            Err(error) => (false, format!("failed: {error}")),
        },
    };
    OwedCancelSend::Done { cleared, outcome }
}

/// Connect to the task's runner and send `exec.cancel` for `job_id`,
/// retrying a failed connection or call with doubling waits.
#[cfg(unix)]
async fn cancel_with_backoff(
    metadata: &PersistedTask,
    remote: &RemoteTask,
    task_id: &str,
    job_id: Uuid,
) -> Result<(), String> {
    let Some(connection) = remote.connection_file.as_ref() else {
        crate::slog_warn!(
            "owed runner cancel for task {task_id} job {job_id}: no runner connection was saved"
        );
        return Err("no runner connection was saved".into());
    };
    let root = metadata
        .project_root
        .clone()
        .unwrap_or_else(|| PathBuf::from("/"));
    let mut wait = OWED_CANCEL_FIRST_BACKOFF;
    let mut last_error = String::new();
    for attempt in 1..=OWED_CANCEL_ATTEMPTS {
        let result = match ExecRemoteClient::connect(
            connection,
            subc_protocol::BindIdentity::new(
                root.display().to_string(),
                remote.harness.clone(),
                remote.session.clone(),
            ),
        )
        .await
        {
            Ok(client) => client.cancel_job(job_id).await.map(|_| ()),
            Err(error) => Err(error),
        };
        match result {
            Ok(()) => {
                crate::slog_info!(
                    "owed runner cancel for task {task_id} job {job_id}: sent (attempt {attempt})"
                );
                return Ok(());
            }
            Err(error) => {
                last_error = error.to_string();
                crate::slog_warn!(
                    "owed runner cancel for task {task_id} job {job_id}: attempt {attempt} of {OWED_CANCEL_ATTEMPTS} failed: {last_error}"
                );
                #[cfg(test)]
                tests::note_owed_cancel_attempt_failed(task_id);
            }
        }
        if attempt < OWED_CANCEL_ATTEMPTS {
            tokio::time::sleep(wait).await;
            wait = wait.saturating_mul(2);
        }
    }
    Err(last_error)
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
    let session_root = exec::DirectoryKind::ProjectFolder.canonicalize(session_root)?;
    let session_worktree =
        worktree_root_containing(&session_root, exec::DirectoryKind::ProjectFolder)?
            .unwrap_or(session_root);
    let Some(worktree_root) = worktree_root_containing(cwd, exec::DirectoryKind::WorkingDirectory)?
    else {
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
fn worktree_root_containing(
    path: &Path,
    kind: exec::DirectoryKind,
) -> Result<Option<PathBuf>, exec::Error> {
    let path = kind.canonicalize(path)?;
    for ancestor in path.ancestors() {
        let git_marker = ancestor.join(".git");
        if git_marker.is_file() || git_marker.is_dir() {
            return kind.canonicalize(ancestor).map(Some);
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

/// Why records between the saved cursor and a later one are missing.
#[cfg(unix)]
#[derive(Clone, Copy)]
enum MissingRecords {
    /// The runner trimmed them from its retained history.
    NoLongerRetained,
    /// They never arrived, even after the re-attaches gap recovery makes.
    NeverDelivered,
    /// They never arrived and precede an expired replay's manifest without
    /// chunk counts whose derived count did not match.
    ExpiredGapUnverified,
}

/// The agent-facing line for a gap before an expired replay's manifest that
/// AFT could not verify: the runner sent no per-file chunk counts, the count
/// AFT derived from the file sizes did not match, and re-attaching did not
/// fill the gap. It is kept apart from lost output in
/// `RemoteTask::expired_gap_unverified`. `{first}` and `{last}` are the seqs;
/// a single record reads "records N to N", like the lost-output line beside
/// it.
#[cfg(unix)]
pub(crate) const EXPIRED_GAP_UNVERIFIED_TEXT: &str = "records {first} to {last} could not be verified: they may be expired returned-file data or lost output, and the runner did not report chunk counts";

#[cfg(unix)]
fn expired_gap_unverified_line(first: u64, last: u64) -> String {
    EXPIRED_GAP_UNVERIFIED_TEXT
        .replace("{first}", &first.to_string())
        .replace("{last}", &last.to_string())
}

#[cfg(unix)]
fn append_output_loss(metadata: &mut PersistedTask) {
    append_environment_disclosure(metadata);
    append_executor_environment_disclosure(metadata);
    let unverified = metadata
        .remote
        .as_ref()
        .map(|remote| remote.expired_gap_unverified.clone())
        .unwrap_or_default();
    for (first, last) in unverified {
        let warning = expired_gap_unverified_line(first, last);
        let note = metadata
            .execution_note
            .get_or_insert_with(|| format!("remote execution on {RUNNER_ID}"));
        if !note.contains(&warning) {
            note.push_str(&format!("\n{warning}"));
        }
    }
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
        // The build-server runner never fills the Git, untracked-file, or
        // ignored-write fields: its checkout is read-only and every file
        // change arrives in `workspace_changes`. An absent field therefore
        // carries no information, so it stays silent instead of printing a
        // "not reported" line.
        None => {}
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
        // Same as above: an absent untracked-file report carries no
        // information from this runner, so it stays silent.
        None => {}
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
        // Same as above: an absent ignored-write report carries no
        // information from this runner, so it stays silent.
        None => {}
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

/// Send a retry's resubmission. Tests can make it fail as a call that never
/// reached the runner (see `tests::refuse_resubmission`).
#[cfg(unix)]
async fn resubmit(
    client: &ExecRemoteClient,
    task: &Arc<BgTask>,
    request: &RunRequest,
    _root: &Path,
    timings: exec::LaunchTimings,
) -> Result<exec::RemoteStream, exec::Error> {
    #[cfg(test)]
    if tests::refuse_resubmission(_root) {
        return Err(exec::Error::Protocol(
            "test: resubmission failed before it was sent".into(),
        ));
    }
    run_diagnosed(client, task, request, timings).await
}

#[cfg(unix)]
async fn run_diagnosed(
    client: &ExecRemoteClient,
    task: &Arc<BgTask>,
    request: &RunRequest,
    timings: exec::LaunchTimings,
) -> Result<exec::RemoteStream, exec::Error> {
    client
        .run_with_diagnostics(
            request,
            Some(exec::RunDiagnostics {
                task_id: &task.task_id,
                session: &task.session_id,
                timings,
            }),
        )
        .await
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

#[cfg(all(test, unix))]
pub(crate) use tests::{
    captured_task_after_losing_replay_for_test, uncaptured_terminal_before_render_for_test,
};
