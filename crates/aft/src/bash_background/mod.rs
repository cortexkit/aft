//! Background bash task management: spawning detached tasks, the watchdog that
//! reaps them, output buffering/compression, and on-disk persistence so tasks
//! survive a bridge restart.

pub mod buffer;
#[cfg(unix)]
mod exit_observer;
mod gc_cursor;
mod gc_memo;
mod memory_limit;
pub mod output;
pub mod persistence;
pub mod process;
pub mod pty_process;
pub mod pty_runtime;
pub mod registry;
mod temp_cleanup;
pub mod watchdog;
pub mod watches;
pub(crate) mod worker_thread;

use crate::bash_permissions::PermissionAsk;
use crate::context::AppContext;
use crate::protocol::Response;
#[cfg(unix)]
use crate::sandbox_spawn::native_sandbox_enforced;
use crate::sandbox_spawn::{
    current_authenticated_principal, resolve_sandbox_spawn, HostEscalationAttempt,
    RequestedSandboxTier, SandboxTaskKind,
};
use persistence::BgMode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

pub use gc_memo::{persisted_gc_health, PersistedGcFailure, PersistedGcHealth, PersistedGcSummary};
pub use registry::{BgCompletion, BgTaskHealthCounts, BgTaskRegistry, WatchdogPassCause};

#[cfg(all(test, unix))]
mod slot_limit_tests;

/// A shell startup has a reply budget even before it has an executor worker.
/// The short state lock fences process creation against a deadline refusal;
/// no filesystem operation, process creation, or registry lock runs under it.
///
/// The state leaves `Pending` exactly once, under that lock: either the
/// startup commits (a task record exists and process creation follows, or an
/// in-process rewrite is about to run) or the reply deadline refuses it. Both
/// end states are final, so a caller told "refused" can rely on the command
/// never starting later, and a caller told "committed" gets the task id.
pub(crate) struct SpawnReceipt {
    changed_files_deadline: std::sync::OnceLock<crate::bash_changed_files::SharedCallDeadline>,
    state: std::sync::Mutex<SpawnReceiptState>,
    deadline: std::time::Instant,
    startup_steps: std::sync::Mutex<StartupSteps>,
    #[cfg(feature = "test-timing-hooks")]
    startup_clock_for_test: Option<StartupClockForTest>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BashStartupStep {
    Admission,
    Setup,
    Environment,
    GitPolicy,
    Profile,
    TaskDirectory,
    ControlFiles,
    SpawnPreparation,
}

impl BashStartupStep {
    const ALL: [Self; 8] = [
        Self::Admission,
        Self::Setup,
        Self::Environment,
        Self::GitPolicy,
        Self::Profile,
        Self::TaskDirectory,
        Self::ControlFiles,
        Self::SpawnPreparation,
    ];

    const fn index(self) -> usize {
        match self {
            Self::Admission => 0,
            Self::Setup => 1,
            Self::Environment => 2,
            Self::GitPolicy => 3,
            Self::Profile => 4,
            Self::TaskDirectory => 5,
            Self::ControlFiles => 6,
            Self::SpawnPreparation => 7,
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Admission => "admission",
            Self::Setup => "setup",
            Self::Environment => "environment",
            Self::GitPolicy => "git_policy",
            Self::Profile => "profile",
            Self::TaskDirectory => "task_directory",
            Self::ControlFiles => "control_files",
            Self::SpawnPreparation => "spawn_prep",
        }
    }

    pub(crate) const fn description(self) -> &'static str {
        match self {
            Self::Admission => "executor admission",
            Self::Setup => "request setup",
            Self::Environment => "environment build",
            Self::GitPolicy => "Git policy discovery",
            Self::Profile => "sandbox profile build",
            Self::TaskDirectory => "task directory creation",
            Self::ControlFiles => "task and control-file creation",
            Self::SpawnPreparation => "spawn preparation",
        }
    }
}

const STARTUP_STEP_COUNT: usize = BashStartupStep::ALL.len();

#[derive(Clone, Copy)]
struct StartupSteps {
    completed: [std::time::Duration; STARTUP_STEP_COUNT],
    seen: u16,
    current: BashStartupStep,
    entered_at: std::time::Instant,
}

#[derive(Clone, Copy)]
pub(crate) struct BashStartupStepSnapshot {
    completed: [std::time::Duration; STARTUP_STEP_COUNT],
    seen: u16,
    current: BashStartupStep,
    running_for: std::time::Duration,
}

impl StartupSteps {
    fn new(started_at: std::time::Instant) -> Self {
        Self {
            completed: [std::time::Duration::ZERO; STARTUP_STEP_COUNT],
            seen: 1 << BashStartupStep::Admission.index(),
            current: BashStartupStep::Admission,
            entered_at: started_at,
        }
    }

    fn enter(&mut self, step: BashStartupStep, now: std::time::Instant) {
        if self.current == step {
            return;
        }
        let index = self.current.index();
        self.completed[index] += now.saturating_duration_since(self.entered_at);
        self.current = step;
        self.entered_at = now;
        self.seen |= 1 << step.index();
    }

    fn snapshot(self, now: std::time::Instant) -> BashStartupStepSnapshot {
        BashStartupStepSnapshot {
            completed: self.completed,
            seen: self.seen,
            current: self.current,
            running_for: now.saturating_duration_since(self.entered_at),
        }
    }
}

impl BashStartupStepSnapshot {
    pub(crate) fn current(self) -> BashStartupStep {
        self.current
    }

    pub(crate) fn format_steps(self) -> String {
        use std::fmt::Write as _;

        let mut formatted = String::with_capacity(128);
        for step in BashStartupStep::ALL {
            if self.seen & (1 << step.index()) == 0 {
                continue;
            }
            if !formatted.is_empty() {
                formatted.push(',');
            }
            let elapsed = self.completed[step.index()]
                + if step == self.current {
                    self.running_for
                } else {
                    std::time::Duration::ZERO
                };
            let _ = write!(formatted, "{}:{}ms", step.label(), elapsed.as_millis());
            if step == self.current {
                formatted.push_str("(running)");
            }
        }
        formatted
    }
}

#[cfg(feature = "test-timing-hooks")]
#[derive(Clone, Copy)]
/// Fixture clock shared by the startup reply timer and receipt commit fence.
/// Freezing time never overrides an already refused or committed receipt.
pub(crate) enum StartupClockForTest {
    Frozen,
    Expired,
}

#[derive(Default)]
enum SpawnReceiptState {
    #[default]
    Pending,
    Refused,
    Committed(String),
    /// A bash rewrite is executing the command inside this process (for
    /// example an append turned into a file edit). It has no task id, so the
    /// only way to learn its outcome is to wait for its reply.
    CommittedInline,
}

/// What the reply deadline found when it settled a startup's receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StartupOutcome {
    /// Nothing had committed; the receipt is now refused and the command can
    /// never start under it.
    Refused,
    /// A task record exists and its process is being (or has been) created.
    Task(String),
    /// The command is running inside this process as a bash rewrite.
    Inline,
}

impl SpawnReceipt {
    pub(crate) fn bind_changed_files_deadline(
        &self,
        deadline: crate::bash_changed_files::SharedCallDeadline,
    ) {
        let _ = self.changed_files_deadline.set(deadline);
    }
    pub(crate) fn limit_changed_files_deadline(&self, deadline: std::time::Instant) {
        if let Some(shared) = self.changed_files_deadline.get() {
            let mut bound = shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // None means the foreground reply no longer needs this deadline;
            // late spawn completion must not reactivate it.
            if let Some(current) = bound.as_mut() {
                *current = (*current).min(deadline);
            }
        }
    }
    #[cfg(test)]
    pub(crate) fn new(deadline: std::time::Instant) -> Self {
        Self::new_with_start_time(deadline, std::time::Instant::now())
    }

    pub(crate) fn new_with_start_time(
        deadline: std::time::Instant,
        startup_started_at: std::time::Instant,
    ) -> Self {
        Self {
            state: std::sync::Mutex::new(SpawnReceiptState::Pending),
            changed_files_deadline: std::sync::OnceLock::new(),
            deadline,
            startup_steps: std::sync::Mutex::new(StartupSteps::new(startup_started_at)),
            #[cfg(feature = "test-timing-hooks")]
            startup_clock_for_test: match std::env::var("AFT_TEST_SUBC_BASH_STARTUP_CLOCK")
                .as_deref()
            {
                Ok("frozen") => Some(StartupClockForTest::Frozen),
                Ok("expired") => Some(StartupClockForTest::Expired),
                _ => None,
            },
        }
    }

    pub(crate) fn enter_startup_step(&self, step: BashStartupStep) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !matches!(*state, SpawnReceiptState::Pending)
            || std::time::Instant::now() >= self.deadline
        {
            return;
        }
        self.startup_steps
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .enter(step, std::time::Instant::now());
    }

    pub(crate) fn startup_step_snapshot(&self) -> BashStartupStepSnapshot {
        self.startup_steps
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .snapshot(std::time::Instant::now())
    }

    #[cfg(feature = "test-timing-hooks")]
    pub(crate) fn startup_clock_for_test(&self) -> Option<StartupClockForTest> {
        self.startup_clock_for_test
    }

    /// Settles the receipt at the reply deadline. A pending receipt becomes
    /// refused under the same lock every commit takes, so it is impossible
    /// for this to return `Refused` and for a commit to succeed afterwards.
    pub(crate) fn expire(&self) -> StartupOutcome {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &*state {
            SpawnReceiptState::Committed(id) => StartupOutcome::Task(id.clone()),
            SpawnReceiptState::CommittedInline => StartupOutcome::Inline,
            SpawnReceiptState::Pending | SpawnReceiptState::Refused => {
                *state = SpawnReceiptState::Refused;
                StartupOutcome::Refused
            }
        }
    }

    fn commit(&self, committed: SpawnReceiptState) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = std::time::Instant::now();
        #[cfg(feature = "test-timing-hooks")]
        let now = match self.startup_clock_for_test {
            Some(StartupClockForTest::Frozen) => self.deadline - std::time::Duration::from_nanos(1),
            Some(StartupClockForTest::Expired) => self.deadline,
            None => now,
        };
        #[cfg(test)]
        let now = SPAWN_COMMIT_TIME_FOR_TEST.with(|clock| clock.get().unwrap_or(now));
        if !matches!(*state, SpawnReceiptState::Pending) || now >= self.deadline {
            // A second commit on one receipt is a bug; refusing it keeps an
            // earlier commit's answer intact instead of overwriting it.
            if matches!(*state, SpawnReceiptState::Pending) {
                *state = SpawnReceiptState::Refused;
            }
            return Err(
                "bash startup deadline expired before process creation; command was not started"
                    .into(),
            );
        }
        *state = committed;
        Ok(())
    }

    pub(crate) fn refused(&self) -> bool {
        matches!(
            *self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            SpawnReceiptState::Refused
        )
    }
}

fn spawn_receipt_committed() -> bool {
    CURRENT_SPAWN_RECEIPT.with(|slot| {
        slot.borrow().as_ref().is_some_and(|receipt| {
            matches!(
                *receipt
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                SpawnReceiptState::Committed(_)
            )
        })
    })
}

thread_local! {
    static CURRENT_SPAWN_RECEIPT: std::cell::RefCell<Option<std::sync::Arc<SpawnReceipt>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
thread_local! {
    static SPAWN_COMMIT_TIME_FOR_TEST: std::cell::Cell<Option<std::time::Instant>> = const { std::cell::Cell::new(None) };
}

/// Overrides only the receipt's clock on this thread, leaving other startups
/// and the real process/persistence path untouched.
#[cfg(test)]
pub(crate) fn with_spawn_commit_time_for_test<T>(
    now: std::time::Instant,
    run: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<std::time::Instant>);
    impl Drop for Restore {
        fn drop(&mut self) {
            SPAWN_COMMIT_TIME_FOR_TEST.with(|clock| clock.set(self.0));
        }
    }
    let previous = SPAWN_COMMIT_TIME_FOR_TEST.with(|clock| clock.replace(Some(now)));
    let _restore = Restore(previous);
    run()
}

pub(crate) fn with_spawn_receipt<T>(
    receipt: std::sync::Arc<SpawnReceipt>,
    run: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<std::sync::Arc<SpawnReceipt>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CURRENT_SPAWN_RECEIPT.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }
    let previous = CURRENT_SPAWN_RECEIPT.with(|slot| slot.replace(Some(receipt)));
    let _restore = Restore(previous);
    run()
}
pub(crate) fn changed_files_deadline_for_spawn(
) -> Option<crate::bash_changed_files::SharedCallDeadline> {
    CURRENT_SPAWN_RECEIPT.with(|slot| {
        slot.borrow()
            .as_ref()?
            .changed_files_deadline
            .get()
            .cloned()
    })
}

pub(crate) fn enter_bash_startup_step(step: BashStartupStep) {
    CURRENT_SPAWN_RECEIPT.with(|slot| {
        if let Some(receipt) = slot.borrow().as_ref() {
            receipt.enter_startup_step(step);
        }
    });
}

/// Called only after the starting record and every control/output file exist,
/// immediately before process creation. Once committed, the caller can always
/// get the task id, including while running-record persistence is blocked.
pub(crate) fn commit_spawn_receipt(task_id: &str) -> Result<(), String> {
    enter_bash_startup_step(BashStartupStep::SpawnPreparation);
    CURRENT_SPAWN_RECEIPT.with(|slot| match slot.borrow().as_ref() {
        Some(receipt) => receipt.commit(SpawnReceiptState::Committed(task_id.into())),
        None => Ok(()),
    })
}

/// Called by a bash rewrite immediately before it executes the command inside
/// this process. A rewrite can mutate files (an append becomes an edit), so it
/// is fenced exactly like process creation: once the deadline has refused the
/// startup, the rewrite must not run.
pub(crate) fn commit_spawn_receipt_inline() -> Result<(), String> {
    enter_bash_startup_step(BashStartupStep::SpawnPreparation);
    CURRENT_SPAWN_RECEIPT.with(|slot| match slot.borrow().as_ref() {
        Some(receipt) => receipt.commit(SpawnReceiptState::CommittedInline),
        None => Ok(()),
    })
}

#[cfg(test)]
mod spawn_receipt_tests {
    use super::{SpawnReceipt, SpawnReceiptState, StartupOutcome};
    use std::sync::{Arc, Barrier};
    use std::time::{Duration, Instant};

    fn far_future() -> Instant {
        Instant::now() + Duration::from_secs(600)
    }

    fn commit_task(receipt: &SpawnReceipt) -> Result<(), String> {
        receipt.commit(SpawnReceiptState::Committed("bgb-task".into()))
    }

    #[test]
    fn refused_receipt_never_commits_and_committed_receipt_is_never_refused() {
        let refused = SpawnReceipt::new(far_future());
        assert_eq!(refused.expire(), StartupOutcome::Refused);
        assert!(commit_task(&refused).is_err());
        assert!(refused.commit(SpawnReceiptState::CommittedInline).is_err());
        assert_eq!(refused.expire(), StartupOutcome::Refused);

        let committed = SpawnReceipt::new(far_future());
        assert!(commit_task(&committed).is_ok());
        assert_eq!(committed.expire(), StartupOutcome::Task("bgb-task".into()));
        // A second commit must not overwrite the first one's answer.
        assert!(commit_task(&committed).is_err());
        assert_eq!(committed.expire(), StartupOutcome::Task("bgb-task".into()));

        let inline = SpawnReceipt::new(far_future());
        assert!(inline.commit(SpawnReceiptState::CommittedInline).is_ok());
        assert_eq!(inline.expire(), StartupOutcome::Inline);
    }

    #[test]
    fn commit_after_the_deadline_is_refused_even_before_the_timer_settles_it() {
        let receipt = SpawnReceipt::new(Instant::now());
        assert!(commit_task(&receipt).is_err());
        assert_eq!(receipt.expire(), StartupOutcome::Refused);
    }

    #[test]
    fn racing_commit_and_expire_always_agree() {
        // Start both sides together many times. Whichever takes the receipt
        // lock first decides; the two answers must describe the same fact.
        for _ in 0..2000 {
            let receipt = Arc::new(SpawnReceipt::new(far_future()));
            let start = Arc::new(Barrier::new(2));
            let committer = {
                let receipt = Arc::clone(&receipt);
                let start = Arc::clone(&start);
                std::thread::spawn(move || {
                    start.wait();
                    commit_task(&receipt)
                })
            };
            start.wait();
            let outcome = receipt.expire();
            let committed = committer.join().unwrap();
            match outcome {
                StartupOutcome::Task(id) => {
                    assert_eq!(id, "bgb-task");
                    assert!(committed.is_ok());
                }
                StartupOutcome::Refused => assert!(committed.is_err()),
                StartupOutcome::Inline => panic!("no inline commit in this race"),
            }
        }
    }
}

/// Who started a background task and the key they gave the call, recorded on
/// the task so a consumer can find the task its own call started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskCallKey {
    /// The principal of the route the call arrived on (`direct`,
    /// `reserved:<module>`, `unverified`, or `absent`), or `first-party` for
    /// a call with no subc route (standalone mode).
    pub requester: String,
    /// The consumer's `call_key`, or the task id when it sent none.
    pub key: String,
    /// True when the consumer sent no key and the task id AFT minted stands
    /// in for it, so a reader can tell a fallback from a key the consumer chose.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub minted: bool,
}

thread_local! {
    /// The `call_key` of the tool call whose dispatch is running on this
    /// thread. Installed around the dispatch like the authenticated principal
    /// (see [`with_call_key`]), because the bash handler that creates the task
    /// only sees the tool's own arguments, which the agent controls.
    static CURRENT_CALL_KEY: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
    /// The call-ledger carrier to record as a new task's requester when it is
    /// not the authenticated principal's id (see [`with_call_requester`]).
    static CURRENT_CALL_REQUESTER: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
    static CURRENT_REMOTE: std::cell::RefCell<Option<RemoteLaunch>> = const { std::cell::RefCell::new(None) };
    /// True only while dispatching a bash call admitted under the catalog's
    /// worker preset. Plugin worker-session flags do not set this marker.
    static CURRENT_WORKER_PRESET: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[derive(Clone, Debug)]
pub(crate) struct RemoteLaunch {
    pub requested_platform: Option<crate::exec_remote::types::Platform>,
    pub params: crate::exec_remote::FrozenParams,
    #[cfg_attr(not(unix), allow(dead_code))]
    pub explicit_runon: bool,
    // Remote dispatch runs only on Unix; Windows carries the policy but never dials.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub connection_file: Option<PathBuf>,
    #[cfg_attr(not(unix), allow(dead_code))]
    pub harness: String,
    #[cfg_attr(not(unix), allow(dead_code))]
    pub session: String,
    pub requested_vcpus: Option<u32>,
    pub requested_network: bool,
}

/// Decide where a bash call that set `runon` runs, before anything is spawned.
///
/// The call names the remote runner itself; nothing about the command line
/// is inspected. Every reason it cannot run remotely is a refusal naming that
/// reason, never a silent local run. `runon` is the caller's demand, `pty`,
/// `powershell` and `host_sandbox` describe the rest of the call, and the
/// session's remote policy is the one installed by [`with_remote_policy`].
pub(crate) fn remote_for_runon(
    config: &crate::config::Config,
    runon: &str,
    pty: bool,
    powershell: bool,
    host_sandbox: bool,
) -> Result<RemoteLaunch, String> {
    if !config.bash.runon_enabled {
        return Err("runon is disabled by the user safety setting bash.runon_enabled".into());
    }
    if cfg!(not(unix)) {
        return Err(
            "runon is not available on Windows: remote runs need AFT on macOS or Linux".into(),
        );
    }
    if pty {
        return Err("runon cannot be combined with pty:true: a remote run has no terminal".into());
    }
    if powershell {
        return Err(
            "runon cannot run PowerShell: the remote runner runs the line with bash".into(),
        );
    }
    if host_sandbox {
        return Err("runon cannot be combined with sandbox: \"host\"; the command runs on the remote server, not on this host".into());
    }
    // An unknown demand is named before anything else is decided about it.
    let requested = runon.trim();
    if !requested.is_empty() {
        crate::exec_remote::policy::parse_demand(requested)?;
    }
    if config.remote_exec.project_off {
        return Err("remote runs are off for this project".into());
    }
    let mut launch = CURRENT_REMOTE
        .with(|s| s.borrow().clone())
        .filter(|launch| {
            launch
                .params
                .remote_exec
                .as_ref()
                .is_some_and(|policy| policy.enabled)
        })
        .ok_or_else(|| "this session has no remote runner".to_string())?;
    let resolved = crate::exec_remote::policy::resolve_demand(
        runon,
        launch
            .params
            .remote_exec
            .as_ref()
            .and_then(|policy| policy.default_demand.as_deref()),
    )?;
    launch.requested_vcpus = resolved.weight_hint;
    launch.requested_platform = resolved.platform;
    launch.requested_network = resolved.network;
    launch.explicit_runon = true;
    Ok(launch)
}

/// Preserve automatic routing only for deployed plans carrying a prefix list.
pub(crate) fn remote_for_legacy_command(
    config: &crate::config::Config,
    command: &str,
    pty: bool,
    powershell: bool,
) -> Option<RemoteLaunch> {
    if !cfg!(unix) || powershell || config.remote_exec.project_off {
        return None;
    }
    CURRENT_REMOTE
        .with(|current| current.borrow().clone())
        .filter(|launch| {
            launch.params.remote_exec.as_ref().is_some_and(|policy| {
                crate::exec_remote::policy::matches(policy, command, pty, false)
            })
        })
        .map(|mut launch| {
            launch.explicit_runon = false;
            launch
        })
}

pub(crate) fn with_remote_policy<T>(policy: Option<RemoteLaunch>, run: impl FnOnce() -> T) -> T {
    struct Restore(Option<RemoteLaunch>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CURRENT_REMOTE.with(|s| *s.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(CURRENT_REMOTE.with(|s| s.replace(policy)));
    run()
}

pub(crate) fn with_worker_preset<T>(worker_preset: bool, run: impl FnOnce() -> T) -> T {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            CURRENT_WORKER_PRESET.with(|current| current.set(self.0));
        }
    }
    let _restore = Restore(CURRENT_WORKER_PRESET.with(|current| current.replace(worker_preset)));
    run()
}

pub(crate) fn worker_preset_active() -> bool {
    CURRENT_WORKER_PRESET.with(std::cell::Cell::get)
}

/// Run `run` with `call_key` as the current call's key, restoring the
/// previous one afterwards, even if `run` panics.
pub(crate) fn with_call_key<T>(call_key: Option<String>, run: impl FnOnce() -> T) -> T {
    struct Restore(Option<Option<String>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            if let Some(previous) = self.0.take() {
                CURRENT_CALL_KEY.with(|slot| *slot.borrow_mut() = previous);
            }
        }
    }
    let previous = CURRENT_CALL_KEY.with(|slot| slot.replace(call_key));
    let _restore = Restore(Some(previous));
    run()
}

/// Run `run` with `requester` recorded as the requester of any task it
/// creates, instead of the authenticated principal's id. A codemode
/// program's shell call runs with its head agent's principal while its
/// call-ledger row belongs to the run route that sent it; the task must name
/// that same requester so the ledger can find it. `None` keeps the
/// principal's id.
pub(crate) fn with_call_requester<T>(requester: Option<String>, run: impl FnOnce() -> T) -> T {
    struct Restore(Option<Option<String>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            if let Some(previous) = self.0.take() {
                CURRENT_CALL_REQUESTER.with(|slot| *slot.borrow_mut() = previous);
            }
        }
    }
    let previous = CURRENT_CALL_REQUESTER.with(|slot| slot.replace(requester));
    let _restore = Restore(Some(previous));
    run()
}

/// The requester and key to record on a task being created now: the
/// consumer's key when the call carried one, otherwise the task id.
pub(crate) fn call_key_for_new_task(task_id: &str) -> TaskCallKey {
    let requester = CURRENT_CALL_REQUESTER.with(|slot| slot.borrow().clone());
    let requester = requester.unwrap_or_else(|| match current_authenticated_principal() {
        crate::sandbox_spawn::AuthenticatedPrincipal::FirstParty => "first-party".to_string(),
        crate::sandbox_spawn::AuthenticatedPrincipal::RouteBind { principal_id, .. } => {
            principal_id.unwrap_or_else(|| "absent".to_string())
        }
    });
    match CURRENT_CALL_KEY.with(|slot| slot.borrow().clone()) {
        Some(key) => TaskCallKey {
            requester,
            key,
            minted: false,
        },
        None => TaskCallKey {
            requester,
            key: task_id.to_string(),
            minted: true,
        },
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BashShell {
    #[default]
    Bash,
    Powershell,
}

impl BashShell {
    pub(crate) fn is_powershell(self) -> bool {
        matches!(self, Self::Powershell)
    }

    pub(crate) fn command_text(self, command: &str) -> String {
        if self.is_powershell() {
            // Match Pi's optional tool: both .NET and PowerShell's pipeline use
            // UTF-8 before user code runs, so redirected native output remains
            // readable across macOS, Linux, and Windows.
            format!(
                "[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false); $OutputEncoding = [Console]::OutputEncoding;\n{command}"
            )
        } else {
            command.to_string()
        }
    }
}

fn resolve_powershell_path_with(
    lookup: impl FnOnce(&str) -> Option<PathBuf>,
) -> Result<PathBuf, String> {
    #[cfg(windows)]
    let candidate = "pwsh.exe";
    #[cfg(not(windows))]
    let candidate = "pwsh";
    // The command is refused rather than handed to bash: a script written for
    // PowerShell rarely means the same thing under bash, so running it anyway
    // would be worse than a clear refusal. The message names both ways out.
    lookup(candidate)
        .ok_or_else(|| powershell_missing_message(crate::host_tool_names::DEFAULT_SHELL_TOOL))
}

/// The refusal for a PowerShell command on a host without `pwsh`. `shell_tool`
/// is the name the host registered AFT's command tool under, which is what the
/// agent has to call again (`bash`, or `shell` on OpenCode 2).
pub(crate) fn powershell_missing_message(shell_tool: &str) -> String {
    format!(
        "PowerShell (pwsh) is not installed or is not on PATH, so this command was not run. \
         Rewrite it in bash syntax and call the {shell_tool} tool without `shell: \"powershell\"`, \
         or install PowerShell 7+: https://aka.ms/powershell"
    )
}

/// Whether a PowerShell command could run on this host right now: the same
/// `pwsh` lookup the executor performs before spawning. The subc manifest uses
/// it to advertise the `powershell` tool only where it can actually run.
pub(crate) fn powershell_available() -> bool {
    resolve_powershell_path_with(|candidate| which::which(candidate).ok()).is_ok()
}

pub(crate) fn resolve_shell_path(pty: bool, shell: BashShell) -> Result<PathBuf, String> {
    if shell.is_powershell() {
        return resolve_powershell_path_with(|candidate| which::which(candidate).ok());
    }

    #[cfg(unix)]
    {
        // One interpreter for both modes: the tool is named `bash`, the model
        // writes bash syntax, and a PTY only changes how the child's terminal is
        // wired, not which shell reads the command. Resolving the PTY launcher
        // from $SHELL (fish on many machines) made `$?` and `&&` fail only when
        // `pty: true` was set, which read as a flaky tool rather than a syntax
        // mismatch.
        let _ = pty;
        Ok(registry::resolve_posix_shell())
    }
    #[cfg(windows)]
    {
        let _ = pty;
        Ok(PathBuf::from("cmd.exe"))
    }
}

/// Add path context only after an operating-system process spawn has failed.
///
/// Spawn errors can mean that the child working directory is missing, or that
/// the executable itself cannot be found. Checking here keeps filesystem
/// probes off the successful process-launch path.
pub(crate) fn format_spawn_failure(
    context: &str,
    program: &std::path::Path,
    workdir: &std::path::Path,
    error: impl std::fmt::Display,
) -> String {
    let workdir_problem = match std::fs::metadata(workdir) {
        Ok(metadata) if !metadata.is_dir() => Some("is not a directory"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some("does not exist"),
        _ => None,
    };
    let program_exists = if program.is_absolute() {
        program.exists()
    } else if program.components().count() > 1 {
        workdir.join(program).exists()
    } else {
        which::which(program.as_os_str()).is_ok()
    };

    if let Some(problem) = workdir_problem {
        format!(
            "{context}: working directory {problem}: {}: {error}",
            workdir.display()
        )
    } else if !program_exists {
        format!(
            "{context}: program not found: {}: {error}",
            program.display()
        )
    } else {
        format!("{context}: {error}")
    }
}

#[cfg(test)]
mod spawn_failure_tests {
    #[test]
    fn bash_background_spawn_error_names_missing_program() {
        let workdir = tempfile::tempdir().unwrap();
        let missing_program = workdir.path().join("missing-shell");
        let error = std::process::Command::new(&missing_program)
            .current_dir(workdir.path())
            .spawn()
            .unwrap_err();

        let message = super::format_spawn_failure(
            "failed to spawn background bash command",
            &missing_program,
            workdir.path(),
            error,
        );
        assert!(
            message.contains(&format!("program not found: {}", missing_program.display())),
            "spawn error did not identify the missing program: {message}"
        );
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BgTaskInfo {
    pub task_id: String,
    pub status: BgTaskStatus,
    pub command: String,
    pub mode: BgMode,
    pub started_at: u64,
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BgTaskStatus {
    Starting,
    Running,
    Killing,
    Completed,
    Failed,
    Killed,
    TimedOut,
    FateUnknown,
}

impl BgTaskStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            BgTaskStatus::Completed
                | BgTaskStatus::Failed
                | BgTaskStatus::Killed
                | BgTaskStatus::TimedOut
                | BgTaskStatus::FateUnknown
        )
    }
}

/// When a background bash task is killed for running too long (its hard kill).
///
/// Every spawn names one explicitly, so no caller can drop the default by
/// passing an empty timeout by accident. There is deliberately no "never"
/// variant: a delegated worker that blocked on a command with no hard kill once
/// sat behind a stuck test run for fifteen hours. Instead a worker's wait is
/// capped (`bash.worker_wait_max_ms`), and each wait it makes pushes a
/// [`HardKill::Default`] task's kill later (see
/// [`registry::BgTaskRegistry::renew_hard_kill`]), so a long build the worker
/// keeps watching runs to completion while one it stops watching is still
/// killed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardKill {
    /// The registry's default, [`registry::DEFAULT_BG_TIMEOUT`] (30 minutes),
    /// extended by a delegated worker's waits on the task.
    Default,
    /// Killed once it has run this long (the caller's explicit `timeout`).
    /// Never extended: the caller chose the limit.
    After(Duration),
}

impl HardKill {
    /// The hard kill for an optional caller timeout in milliseconds, falling
    /// back to [`HardKill::Default`].
    pub fn from_timeout_ms(timeout_ms: Option<u64>) -> Self {
        timeout_ms.map_or(Self::Default, |ms| Self::After(Duration::from_millis(ms)))
    }

    /// How long the task may run before its first renewal.
    pub fn limit(self) -> Duration {
        match self {
            Self::Default => registry::DEFAULT_BG_TIMEOUT,
            Self::After(limit) => limit,
        }
    }

    /// Whether a delegated worker's wait may push the kill later. Only the
    /// implicit default is; an explicit timeout is the caller's own limit.
    pub fn renewable(self) -> bool {
        matches!(self, Self::Default)
    }
}

/// Whether a launch takes one of the project root's background-task slots
/// (`max_background_bash_tasks`). A project root's registry is shared by every
/// session working in that root, so the slots are too; another root has its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskSlot {
    /// An ordinary foreground command. It always starts, even when every
    /// background slot is taken: the cap exists to stop runaway detached work,
    /// not to block the command an agent is waiting on. It holds no slot while
    /// its caller waits on it. If it outlives its wait window and is promoted
    /// to a background task, it is still promoted (it is already running, and
    /// killing or refusing it then would lose its work) and holds a slot from
    /// then on, so the number of slot holders can briefly exceed the cap until
    /// tasks finish.
    Foreground,
    /// A launch that runs in the background from the start (`background:
    /// true`, or a PTY). Refused when `max` background tasks are already
    /// running in this project root. Local and remote tasks share these slots.
    Background { max: usize },
}

impl TaskSlot {
    /// Whether a task launched this way holds a background slot from the start.
    pub fn holds_slot(self) -> bool {
        matches!(self, Self::Background { .. })
    }
}

/// Spawn a bash command as a task. Returns a task_id immediately.
///
/// `require_background_flag` marks a background launch: it needs the
/// background feature and takes a background slot. A foreground command
/// (`false`, and not a PTY) always starts; see [`TaskSlot::Foreground`].
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn(
    request_id: &str,
    session_id: &str,
    command: &str,
    shell: BashShell,
    shell_path: PathBuf,
    workdir: Option<PathBuf>,
    env: Option<HashMap<String, String>>,
    hard_kill: HardKill,
    ctx: &AppContext,
    require_background_flag: bool,
    notify_on_completion: bool,
    compressed: bool,
    pty: bool,
    pty_rows: u16,
    pty_cols: u16,
    scanner_report: Vec<PermissionAsk>,
    scanner_requires_approval: bool,
    host_escalation: Option<HostEscalationAttempt>,
    remote: Option<RemoteLaunch>,
) -> Response {
    if require_background_flag && !ctx.config().experimental_bash_background {
        return Response::error(
            request_id,
            "feature_disabled",
            "background bash is disabled; set `bash: { background: true }` (or `bash: true`) in aft.jsonc",
        );
    }

    let workdir = workdir.unwrap_or_else(|| {
        ctx.config().project_root.clone().unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        })
    });
    let storage_dir = task_storage_dir(ctx);
    let max_running = ctx.config().max_background_bash_tasks;
    let slot = if require_background_flag || pty {
        TaskSlot::Background { max: max_running }
    } else {
        TaskSlot::Foreground
    };
    let project_root = ctx
        .config()
        .project_root
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .and_then(|path| std::fs::canonicalize(&path).ok().or(Some(path)));

    enter_bash_startup_step(BashStartupStep::Environment);
    let mut env = env.unwrap_or_default();
    let config = ctx.config();
    let child_storage_root = self::storage_dir(config.storage_dir.as_deref());
    // The ticket lets this command's `gh` shim relay bot writes for the
    // session that spawned it. Dropping it on any early return revokes it; a
    // spawned task's terminal transition revokes it after `bind_task`.
    let gh_shim_ticket = crate::gh_shim_ticket::PendingTicket::issue_for_project(
        session_id,
        project_root.as_deref(),
    );
    if let Err(error) = crate::agent_child_env::inject(
        config.as_ref(),
        &child_storage_root,
        &mut env,
        gh_shim_ticket.value(),
    ) {
        return Response::error(request_id, "child_environment_unavailable", error);
    }
    if worker_preset_active() {
        let worktree_root = project_root.as_deref().unwrap_or(&workdir);
        crate::agent_child_env::inject_worker_test_threads(worktree_root, &mut env);
    }
    crate::sandbox_spawn::apply_worker_jail_environment(
        &mut env,
        crate::sandbox_spawn::effective_sandbox_mode(
            ctx,
            &crate::sandbox_spawn::current_authenticated_principal(),
        ),
    );
    #[cfg(target_os = "linux")]
    if !pty && config.bash.linux_scope {
        env.insert(registry::LINUX_SCOPE_ENV.to_string(), "1".to_string());
    }
    let task_kind = if pty {
        SandboxTaskKind::BashPty
    } else if require_background_flag {
        SandboxTaskKind::BashBackground
    } else {
        SandboxTaskKind::BashForeground
    };
    let principal = current_authenticated_principal();
    let requested_tier = if host_escalation.is_some() {
        RequestedSandboxTier::Host
    } else if crate::sandbox_spawn::sandbox_required(ctx) {
        RequestedSandboxTier::Native
    } else {
        RequestedSandboxTier::Disabled
    };
    let session_dir = persistence::session_tasks_dir(&storage_dir, session_id);
    #[cfg(unix)]
    let (spawn_plan, unregistered_task) = if native_sandbox_enforced(ctx, &principal)
        && host_escalation.is_none()
    {
        enter_bash_startup_step(BashStartupStep::TaskDirectory);
        let task = match persistence::allocate_task_layout(&storage_dir, session_id) {
            Ok(task) => task,
            Err(error) => {
                return Response::error(
                    request_id,
                    "sandbox_unavailable",
                    format!(
                        "native sandbox failed to create the task artifact directory: {error}; select sandbox.profile off to disable native sandboxing"
                    ),
                );
            }
        };
        enter_bash_startup_step(BashStartupStep::Profile);
        let plan = resolve_sandbox_spawn(
            ctx,
            &principal,
            requested_tier,
            task_kind,
            &task.paths.io_dir,
            None,
        );
        if plan.refusal_code().is_some() {
            (plan, Some(task))
        } else {
            let root = project_root.as_deref().unwrap_or(&workdir);
            provision_worker_read_broker(
                &plan,
                remote.is_some(),
                &gh_shim_ticket,
                &task.paths.task_id,
                std::time::Instant::now() + hard_kill.limit(),
                ctx.bash_background().clone(),
                &mut env,
                config.github.shim,
            );
            enter_bash_startup_step(BashStartupStep::Environment);
            let environment = crate::sandbox_spawn::approved_environment_for_plan(&plan, &env);
            enter_bash_startup_step(BashStartupStep::ControlFiles);
            match crate::sandbox_spawn::prepare_task_payload(
                &task,
                command.as_bytes(),
                root,
                &workdir,
                &principal,
                &shell_path,
                &environment,
            ) {
                Ok(prepared) => (plan.with_prepared_task(prepared), Some(task)),
                Err(error) => {
                    let _ = persistence::delete_resolved_task(&task);
                    return Response::error(
                        request_id,
                        "sandbox_unavailable",
                        format!("native sandbox failed to materialize task payload: {error}"),
                    );
                }
            }
        }
    } else {
        enter_bash_startup_step(BashStartupStep::Profile);
        (
            resolve_sandbox_spawn(
                ctx,
                &principal,
                requested_tier,
                task_kind,
                &session_dir,
                host_escalation.as_ref(),
            ),
            None,
        )
    };
    #[cfg(not(unix))]
    enter_bash_startup_step(BashStartupStep::Profile);
    #[cfg(not(unix))]
    let spawn_plan = resolve_sandbox_spawn(
        ctx,
        &principal,
        requested_tier,
        task_kind,
        &session_dir,
        host_escalation.as_ref(),
    );
    if let Some(code) = spawn_plan.refusal_code() {
        #[cfg(unix)]
        if let Some(task) = unregistered_task.as_ref() {
            let _ = persistence::delete_resolved_task(task);
        }
        let message = spawn_plan
            .refusal_message()
            .unwrap_or("bash process creation refused by sandbox policy");
        return match spawn_plan.refusal_mismatch_class() {
            Some(class) => Response::error_with_data(
                request_id,
                code,
                message,
                json!({ "mismatch_class": class }),
            ),
            None => Response::error(request_id, code, message),
        };
    }

    // The launcher can also carry off or relaxed profiles. Only the resolved
    // containment policy can replace a pending permission approval.
    if scanner_requires_approval && !spawn_plan.enforces_shell_containment() {
        spawn_plan.cleanup_unspawned();
        #[cfg(unix)]
        if let Some(task) = unregistered_task.as_ref() {
            let _ = persistence::delete_resolved_task(task);
        }
        return Response::error_with_data(
            request_id,
            crate::protocol::ERROR_PERMISSION_REQUIRED,
            "bash command requires permission",
            json!({ "asks": scanner_report }),
        );
    }

    let cleanup_plan = spawn_plan.clone();
    // The watchdog compresses this task's output through the shared filter
    // handle, which reads the registry as it is. An idle-evicted root has
    // released its registry, so load it again before the task can finish.
    ctx.reload_released_filter_registry();
    let _changed_files =
        crate::bash_changed_files::Capture::start_for_spawn(ctx, request_id, &spawn_plan)
            .map(|capture| crate::bash_changed_files::scope(Some(capture)));
    #[cfg(unix)]
    ctx.bash_background()
        .set_db_schema_hints(ctx.config().bash.db_schema_hints);
    #[cfg(unix)]
    let remote_result = remote.map(|launch| {
        ctx.bash_background().spawn_remote_typed(
            launch,
            spawn_plan.clone(),
            command,
            shell_path.clone(),
            session_id.to_string(),
            workdir.clone(),
            env.clone(),
            hard_kill,
            config.bash.memory_limit_mib,
            storage_dir.clone(),
            slot,
            notify_on_completion,
            compressed,
            project_root.clone(),
        )
    });
    #[cfg(not(unix))]
    let remote_result: Option<Result<String, registry::RegistryError>> = {
        let _ = remote;
        None
    };
    let spawn_result = if let Some(result) = remote_result {
        result
    } else if pty {
        // A PTY always runs in the background, so it always takes a slot.
        ctx.bash_background().spawn_pty_with_shell_typed(
            spawn_plan,
            command,
            shell,
            shell_path,
            session_id.to_string(),
            workdir,
            env,
            hard_kill,
            config.bash.memory_limit_mib,
            storage_dir,
            max_running,
            notify_on_completion,
            compressed,
            project_root,
            pty_rows,
            pty_cols,
        )
    } else {
        ctx.bash_background().spawn_with_shell_in_slot_typed(
            spawn_plan,
            command,
            shell,
            shell_path,
            session_id.to_string(),
            workdir,
            env,
            hard_kill,
            config.bash.memory_limit_mib,
            storage_dir,
            slot,
            notify_on_completion,
            compressed,
            project_root,
        )
    };

    match spawn_result {
        Ok(task_id) => {
            gh_shim_ticket.bind_task(&task_id);
            // A command that finishes very quickly can be terminal before the
            // ticket was bound to it, in which case its terminal write found
            // nothing to revoke.
            if ctx.bash_background().is_task_terminal(&task_id) {
                crate::gh_shim_ticket::revoke_task(&task_id);
            }
            if let Err(error) =
                ctx.bash_background()
                    .record_scanner_report(&task_id, session_id, scanner_report)
            {
                crate::slog_warn!("{error}");
            }
            successful_spawn_response(request_id, session_id, &task_id, pty, ctx)
        }
        Err(registry::RegistryError::TaskLimit(message)) => {
            cleanup_plan.cleanup_unspawned();
            #[cfg(unix)]
            if let Some(task) = unregistered_task.as_ref() {
                let _ = persistence::delete_resolved_task(task);
            }
            Response::error(request_id, "background_task_limit_exceeded", message)
        }
        Err(error) => {
            let startup_deadline = matches!(error, registry::RegistryError::StartupDeadline(_));
            let message = error.to_string();
            // A deadline may already have handed the starting task id to its
            // caller. Keep its terminal failure record available to status.
            if !spawn_receipt_committed() {
                cleanup_plan.cleanup_unspawned();
            }
            #[cfg(unix)]
            if let Some(task) = unregistered_task
                .as_ref()
                .filter(|_| !spawn_receipt_committed())
            {
                let _ = persistence::delete_resolved_task(task);
            }
            if startup_deadline {
                Response::error(request_id, "bash_start_deadline", message)
            } else {
                execution_error_response(request_id, cleanup_plan.is_native_launcher(), message)
            }
        }
    }
}

/// Decide, in one place, whether a jailed worker's shell receives the host-local
/// GitHub read broker (socket, capability file and shim state directory in its
/// private task temp). Strict and relaxed native launches both go through
/// here, so their broker behaviour cannot drift apart. A remote launch or a
/// profile that cannot protect the broker removes any broker paths from the
/// child environment and logs the reason; provisioning failures are logged by
/// `provision_optional`. Head shells are not workers and get no broker.
#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn provision_worker_read_broker(
    plan: &crate::sandbox_spawn::SpawnPlan,
    remote: bool,
    ticket: &crate::gh_shim_ticket::PendingTicket,
    task_id: &str,
    deadline: std::time::Instant,
    tasks: BgTaskRegistry,
    env: &mut HashMap<String, String>,
    shim_enabled: bool,
) {
    if !crate::sandbox_spawn::worker_jail_active() {
        return;
    }
    // A host-local socket is not delegated to remote execution or its later
    // local fallback. Neither path requires broker setup.
    let skipped = if remote {
        Some("remote launch or local fallback has no host broker")
    } else {
        plan.worker_read_broker_unavailable()
    };
    if let Some(reason) = skipped {
        for key in [
            crate::gh_shim_worker_broker::SOCKET_ENV,
            crate::gh_shim_worker_broker::CAPABILITY_FILE_ENV,
            crate::gh_shim_worker_broker::STATE_ENV,
        ] {
            env.remove(key);
        }
        crate::gh_shim_worker_broker::log_unavailable(task_id, reason);
        return;
    }
    crate::gh_shim_worker_broker::provision_optional(
        ticket,
        task_id,
        plan.temp_dir(),
        deadline,
        tasks,
        env,
        shim_enabled,
    );
}

fn execution_error_response(request_id: &str, native: bool, message: String) -> Response {
    if message.starts_with("failed to start PTY reader:")
        || message.starts_with("failed to start PTY waiter:")
    {
        // A failure to create a helper thread does not mean the sandbox failed.
        // The spawned child may already have executed, so do not claim it never
        // ran or advise disabling sandbox protection.
        Response::error(request_id, "internal_error", message)
    } else if native {
        Response::error(
            request_id,
            "sandbox_unavailable",
            format!("native sandbox failed before command execution: {message}; select sandbox.profile off to disable native sandboxing"),
        )
    } else {
        Response::error(request_id, "execution_failed", message)
    }
}

#[cfg(test)]
mod worker_admission_response_tests {
    use super::*;

    #[test]
    fn approved_pty_worker_error_is_not_misreported_as_sandbox_failure() {
        for native in [true, false] {
            let response = execution_error_response(
                "worker-refused",
                native,
                "failed to start PTY reader: injected worker admission failure".into(),
            );
            let wire = serde_json::to_value(response).unwrap();
            assert_eq!(wire["code"], "internal_error");
            assert!(!wire["message"]
                .as_str()
                .unwrap()
                .contains("before command execution"));
            assert!(!wire["message"]
                .as_str()
                .unwrap()
                .contains("disable native sandboxing"));
        }
    }
}

fn successful_spawn_response(
    request_id: &str,
    session_id: &str,
    task_id: &str,
    pty: bool,
    ctx: &AppContext,
) -> Response {
    // Remote launch replies use running as an internal handoff to the capture
    // and foreground wait paths. Those paths publish acceptance or terminal
    // output themselves, even if submission has already finished here.
    let status = if ctx.bash_background().is_remote_task(task_id, session_id) {
        BgTaskStatus::Running
    } else {
        ctx.bash_background()
            .observed_status(task_id, session_id, 0)
            .map(|snapshot| snapshot.info.status)
            .unwrap_or(BgTaskStatus::Running)
    };
    Response::success(
        request_id,
        json!({
            "task_id": task_id,
            "status": status,
            "mode": if pty { "pty" } else { "pipes" },
        }),
    )
}

#[cfg(all(test, unix))]
pub(crate) fn completed_spawn_for_test(
    request: crate::protocol::RawRequest,
    ctx: &AppContext,
) -> Response {
    ctx.update_config(|config| config.experimental_bash_background = true);
    let launched = crate::commands::bash::handle(&request, ctx);
    assert!(launched.success, "{launched:?}");
    let task_id = launched.data["task_id"].as_str().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if ctx
            .bash_background()
            .observed_status(task_id, request.session(), 0)
            .is_some_and(|snapshot| snapshot.info.status.is_terminal())
        {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "task did not finish");
        std::thread::sleep(Duration::from_millis(5));
    }
    // Construct the real spawn reply only after exit so the test does not
    // depend on the child winning a race against task registration.
    successful_spawn_response(
        &request.id,
        request.session(),
        task_id,
        launched.data["mode"] == "pty",
        ctx,
    )
}

#[cfg(all(test, unix))]
mod spawn_reply_tests {
    use super::*;

    #[test]
    fn spawn_reply_reports_observed_terminal_state() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config {
            project_root: Some(project.path().into()),
            storage_dir: Some(storage.path().into()),
            ..crate::config::Config::default()
        };
        config.sandbox.profile.head = crate::config::SandboxMode::Off;
        let ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
        let request = serde_json::from_value(json!({
            "id": "spawn-reply-test",
            "command": "bash",
            "session_id": "spawn-reply-session",
            "params": { "command": "sleep 30" },
        }))
        .unwrap();
        let launched = crate::commands::bash::handle(&request, &ctx);
        assert!(launched.success, "{:?}", launched.data);
        let task_id = launched.data["task_id"].as_str().unwrap();
        assert_eq!(launched.data["status"], "running");
        ctx.bash_background()
            .kill(task_id, "spawn-reply-session")
            .unwrap();
        // Finish the task before constructing the reply, rather than relying on
        // a fast child to win a race with spawn registration.
        let reply = successful_spawn_response("reply", "spawn-reply-session", task_id, false, &ctx);
        assert_eq!(reply.data["status"], "killed");
        assert_eq!(reply.data["task_id"], task_id);
        assert_eq!(reply.data["mode"], "pipes");

        let terminal =
            match crate::commands::bash_orchestrate::build_bash_outcome(&request, &ctx, reply) {
                crate::response_finalize::DispatchOutcome::Immediate(response) => response,
                crate::response_finalize::DispatchOutcome::Deferred(mut pending) => {
                    (pending.poll)(&ctx).expect("task already terminal")
                }
            };
        assert_eq!(terminal.data["status"], "killed");
        assert!(terminal.data["output"].is_string(), "{terminal:?}");
    }
}

// A root context is shared by routes from different harnesses. Determine a
// command's owner from its route binding, not the last harness to configure the root.
pub(crate) fn route_harness() -> Option<crate::harness::Harness> {
    match current_authenticated_principal() {
        crate::sandbox_spawn::AuthenticatedPrincipal::RouteBind { harness, .. } => {
            harness.parse().ok()
        }
        _ => None,
    }
}

pub(crate) fn task_storage_dir(ctx: &AppContext) -> PathBuf {
    let config = ctx.config();
    let root = storage_dir(config.storage_dir.as_deref());
    route_harness()
        .or_else(|| config.harness.clone())
        .as_ref()
        .map(|harness| root.join(harness.storage_segment()))
        .unwrap_or(root)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StoragePlatform {
    Windows,
    Other,
}

impl StoragePlatform {
    fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Other
        }
    }
}

/// Resolve the process-state storage root exactly once for every Rust entry point.
/// The environment override is checked here so it wins over a stale plugin wire
/// value, while both plugin-less fallback and plugin-injected paths share one root.
///
/// Last re-derived 2026-09-06 against subconscious d5e09914b0791a66f2a5a00a9bb3422860ade95e:
/// compare the ordered variables, platform gates, and empty-value guards below with
/// `subc-core/src/daemon_config.rs::default_data_home`, resolve its named constants,
/// then preserve the documented Windows cache-class divergence.
/// AFT now uses `storage_root` for background tasks: start this process's
/// one pass over it that sends runner cancels saved paused tasks still owe
/// (`BgTaskRegistry::start_owed_cancel_pass`), on its own thread. Called
/// where each mode first learns its storage root: as a SubC module starts
/// (before any project binds) and by `configure` (the first configure of a
/// standalone process; later and SubC configures find the pass started).
/// Repeated calls for the same root do nothing.
pub(crate) fn note_storage_root(storage_root: &std::path::Path) {
    // A test process must never send cancels for a developer's real store:
    // test builds start the pass only for temporary storage roots.
    #[cfg(test)]
    {
        let temp = std::env::temp_dir();
        let temp = temp.canonicalize().unwrap_or(temp);
        let root = storage_root
            .canonicalize()
            .unwrap_or_else(|_| storage_root.to_path_buf());
        if !root.starts_with(&temp) {
            return;
        }
    }
    #[cfg(unix)]
    {
        let _ = registry::BgTaskRegistry::start_owed_cancel_pass(storage_root);
    }
    #[cfg(not(unix))]
    let _ = storage_root;
}

pub fn storage_dir(configured: Option<&std::path::Path>) -> PathBuf {
    let lookup = |name: &str| std::env::var_os(name);
    let fallback_home = std::env::home_dir();
    let current_dir = std::env::current_dir().ok();
    let root = storage_dir_from(
        configured,
        &lookup,
        StoragePlatform::current(),
        fallback_home.as_deref(),
        current_dir.as_deref(),
    );
    crate::project_content::register_storage_root(&root);
    root
}

/// Resolve the root that holds rebuildable cache artifacts (search, semantic,
/// symbol, call-graph, inspect and view caches).
///
/// `cache_root` is a root the caller already resolved, normally
/// `Config::cache_storage_dir()`, which is `<storage.cache_dir>/aft` when a
/// separate cache volume is configured. That root must be used exactly as
/// given. [`storage_dir`] is the wrong helper here: it lets `AFT_STORAGE_DIR`
/// replace the supplied path, which is right for durable state but would move
/// cache artifacts back onto the durable disk, where the split-storage guard
/// then refuses to create them. Without a resolved root the caches share the
/// default durable root, as they always have.
pub fn cache_artifact_root(cache_root: Option<&std::path::Path>) -> PathBuf {
    match cache_root.filter(|path| !path.as_os_str().is_empty()) {
        Some(root) => {
            crate::project_content::register_storage_root(root);
            root.to_path_buf()
        }
        None => storage_dir(None),
    }
}

/// Resolve the current test environment's default, excluding AFT-specific
/// overrides. HOME and XDG may deliberately name a fixture, not the live store.
#[cfg(test)]
pub(crate) fn storage_dir_without_overrides_for_test() -> PathBuf {
    let lookup = |name: &str| {
        if matches!(name, "AFT_STORAGE_DIR" | "AFT_CACHE_DIR") {
            None
        } else {
            std::env::var_os(name)
        }
    };
    storage_dir_from_test_environment(&lookup)
}

/// Let the persistence fence use the production ladder with account-owned homes
/// instead of the temporary homes installed by a test or its runner.
#[cfg(test)]
pub(crate) fn storage_dir_from_test_environment(
    lookup: &impl Fn(&str) -> Option<std::ffi::OsString>,
) -> PathBuf {
    storage_dir_from(
        None,
        &lookup,
        StoragePlatform::current(),
        std::env::home_dir().as_deref(),
        std::env::current_dir().ok().as_deref(),
    )
}

fn storage_dir_from(
    configured: Option<&std::path::Path>,
    lookup: &impl Fn(&str) -> Option<std::ffi::OsString>,
    platform: StoragePlatform,
    fallback_home: Option<&std::path::Path>,
    current_dir: Option<&std::path::Path>,
) -> PathBuf {
    let resolve = |path: &std::path::Path| {
        resolve_storage_path_from(path, lookup, platform, fallback_home, current_dir)
    };

    if let Some(dir) = non_empty_env_path_from(lookup, "AFT_STORAGE_DIR") {
        return resolve(&dir);
    }
    if let Some(dir) = configured.filter(|path| !path.as_os_str().is_empty()) {
        // Explicit process-state paths are already caller-owned. Preserve their
        // spelling so every downstream read/write uses the exact configured root.
        return dir.to_path_buf();
    }
    // AFT_CACHE_DIR predates AFT_STORAGE_DIR as the storage sandbox lever. It
    // remains above the shared data-home ladder so old isolated invocations do
    // not escape into the operator's data root.
    if let Some(dir) = non_empty_env_path_from(lookup, "AFT_CACHE_DIR") {
        return resolve(&dir).join("aft");
    }

    resolve(&cortexkit_data_root_from(lookup, platform))
        .join("cortexkit")
        .join("aft")
}

fn non_empty_env_path_from(
    lookup: &impl Fn(&str) -> Option<std::ffi::OsString>,
    name: &str,
) -> Option<PathBuf> {
    lookup(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn storage_home_dir_from(
    lookup: &impl Fn(&str) -> Option<std::ffi::OsString>,
    platform: StoragePlatform,
    fallback_home: Option<&std::path::Path>,
) -> Option<PathBuf> {
    let configured = if platform == StoragePlatform::Windows {
        non_empty_env_path_from(lookup, "USERPROFILE")
            .or_else(|| non_empty_env_path_from(lookup, "HOME"))
    } else {
        non_empty_env_path_from(lookup, "HOME")
            .or_else(|| non_empty_env_path_from(lookup, "USERPROFILE"))
    };
    configured.or_else(|| fallback_home.map(PathBuf::from))
}

fn cortexkit_data_root_from(
    lookup: &impl Fn(&str) -> Option<std::ffi::OsString>,
    platform: StoragePlatform,
) -> PathBuf {
    if let Some(dir) = non_empty_env_path_from(lookup, "XDG_DATA_HOME") {
        return dir;
    }
    if platform == StoragePlatform::Windows {
        // AFT stores indexes, backups, and checkpoints here.
        // cache-class storage; stable for existing installs.
        // Do not move this shipped ladder to Roaming.
        if let Some(dir) = non_empty_env_path_from(lookup, "LOCALAPPDATA") {
            return dir;
        }
        if let Some(home) = non_empty_env_path_from(lookup, "USERPROFILE") {
            return home.join("AppData").join("Local");
        }
    }
    if let Some(home) = non_empty_env_path_from(lookup, "HOME") {
        return home.join(".local").join("share");
    }
    PathBuf::from(".local").join("share")
}

fn resolve_storage_path_from(
    path: &std::path::Path,
    lookup: &impl Fn(&str) -> Option<std::ffi::OsString>,
    platform: StoragePlatform,
    fallback_home: Option<&std::path::Path>,
    current_dir: Option<&std::path::Path>,
) -> PathBuf {
    let storage_home = || storage_home_dir_from(lookup, platform, fallback_home);
    let expanded = if path == std::path::Path::new("~") {
        storage_home().unwrap_or_else(|| path.to_path_buf())
    } else if let Some(raw) = path.to_str() {
        if raw.starts_with("~/") || raw.starts_with("~\\") {
            storage_home()
                .map(|home| home.join(&raw[2..]))
                .unwrap_or_else(|| path.to_path_buf())
        } else {
            path.to_path_buf()
        }
    } else {
        path.to_path_buf()
    };
    let absolute = if expanded.is_absolute() {
        expanded
    } else if let Some(current_dir) = current_dir {
        current_dir.join(expanded)
    } else {
        expanded
    };
    normalize_absolute_path(&absolute)
}

fn normalize_absolute_path(path: &std::path::Path) -> PathBuf {
    use std::path::Component;

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push(component.as_os_str());
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

pub fn repair_legacy_root_tasks(storage_root: &std::path::Path, harness: crate::harness::Harness) {
    let root_tasks = storage_root.join("bash-tasks");
    if !dir_has_entries(&root_tasks) {
        return;
    }
    // Detached writers retain both open output handles and absolute marker paths.
    // Even a same-volume rename can strand their later path-based writes. Keep
    // the entire legacy tree readable in place until every task is quiescent.
    if !legacy_root_tasks_quiescent(&root_tasks).unwrap_or(false) {
        return;
    }

    let harness_tasks = storage_root
        .join(harness.storage_segment())
        .join("bash-tasks");
    if dir_has_entries(&harness_tasks) {
        return;
    }
    if let Some(parent) = harness_tasks.parent() {
        if let Err(error) = crate::private_storage::create_dir_all(parent) {
            crate::slog_warn!(
                "failed to create harness bash task dir {}: {}",
                parent.display(),
                error
            );
            return;
        }
    }
    if harness_tasks.exists() {
        let _ = std::fs::remove_dir(&harness_tasks);
    }

    // The harness folder and the root normally share a volume, but a move
    // that crosses volumes falls back to a copy instead of failing.
    match crate::cross_volume::move_path(&root_tasks, &harness_tasks) {
        Ok(moved) => {
            moved.warn_if_incomplete(&root_tasks, &harness_tasks);
            crate::slog_info!(
                "moved legacy root bash tasks into harness namespace: {}",
                harness_tasks.display()
            );
        }
        Err(error) => {
            crate::slog_warn!(
                "failed to move legacy root bash tasks into {}: {}; trying child merge",
                harness_tasks.display(),
                error
            );
            if let Err(error) = crate::private_storage::create_dir_all(&harness_tasks) {
                crate::slog_warn!(
                    "legacy root bash tasks kept in {}: cannot create {}: {}",
                    root_tasks.display(),
                    harness_tasks.display(),
                    error
                );
                return;
            }
            if let Ok(entries) = std::fs::read_dir(&root_tasks) {
                for entry in entries.flatten() {
                    let source = entry.path();
                    let target = harness_tasks.join(entry.file_name());
                    if target.exists() {
                        continue;
                    }
                    match crate::cross_volume::move_path(&source, &target) {
                        Ok(moved) => moved.warn_if_incomplete(&source, &target),
                        Err(error) => {
                            // A task that cannot be moved stays readable in the
                            // root `bash-tasks/` folder; log the move failure
                            // instead of dropping it.
                            crate::slog_warn!(
                                "legacy bash task {} kept in place: cannot move it to {}: {}",
                                source.display(),
                                target.display(),
                                error
                            );
                        }
                    }
                }
            }
            // Removes the legacy folder only when every task left it.
            let _ = std::fs::remove_dir(&root_tasks);
        }
    }
}

fn legacy_root_tasks_quiescent(root: &std::path::Path) -> std::io::Result<bool> {
    use persistence::{discover_task_ids, read_exit_marker, read_task_at, resolve_task_layout};
    for session in std::fs::read_dir(root)? {
        let session = session?;
        if !session.file_type()?.is_dir() {
            return Ok(false);
        }
        let (ids, invalid) = discover_task_ids(&session.path())?;
        if !invalid.is_empty() {
            return Ok(false);
        }
        for id in ids {
            let task = resolve_task_layout(&session.path(), &id)?;
            let metadata = read_task_at(&task)?;
            if read_exit_marker(&task.paths)?.is_some() {
                continue;
            }
            if !metadata.is_terminal() {
                return Ok(false);
            }
            if metadata.child_pid.is_none() && metadata.pgid.is_none() {
                continue;
            }
            #[cfg(unix)]
            if let Some(pgid) = metadata.pgid.filter(|pgid| *pgid > 0) {
                // Only ESRCH proves that no group member can still write. A
                // permission error or an unsupported probe keeps the task put.
                if unsafe { libc::kill(-pgid, 0) } == -1
                    && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
                {
                    continue;
                }
            }
            return Ok(false);
        }
    }
    Ok(true)
}

fn dir_has_entries(path: &std::path::Path) -> bool {
    std::fs::read_dir(path)
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false)
}

#[cfg(test)]
mod storage_root_tests {
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::path::{Path, PathBuf};

    struct NonPanickingCleanup<F: FnOnce()> {
        cleanup: Option<F>,
    }

    impl<F: FnOnce()> NonPanickingCleanup<F> {
        fn new(cleanup: F) -> Self {
            Self {
                cleanup: Some(cleanup),
            }
        }
    }

    impl<F: FnOnce()> Drop for NonPanickingCleanup<F> {
        fn drop(&mut self) {
            let Some(cleanup) = self.cleanup.take() else {
                return;
            };
            // A cleanup panic while the test is already unwinding aborts the whole
            // libtest process, so cleanup failures must remain contained here.
            let _ = catch_unwind(AssertUnwindSafe(cleanup));
        }
    }

    fn resolve_storage_fixture(
        env: &HashMap<&str, OsString>,
        configured: Option<&Path>,
        platform: super::StoragePlatform,
        fallback_home: Option<&Path>,
        current_dir: Option<&Path>,
    ) -> PathBuf {
        super::storage_dir_from(
            configured,
            &|name| env.get(name).cloned(),
            platform,
            fallback_home,
            current_dir,
        )
    }

    #[test]
    fn storage_ladder_matches_daemon_except_for_stable_windows_cache_class_storage() {
        let current_dir = Path::new("/work");
        let fallback_home = Path::new("/system-home");
        let module_suffix = Path::new("cortexkit").join("aft");
        let mut env = HashMap::from([
            ("AFT_STORAGE_DIR", OsString::new()),
            ("AFT_CACHE_DIR", OsString::new()),
            ("XDG_DATA_HOME", OsString::new()),
            ("APPDATA", OsString::from("/wrong-roaming-data")),
            ("USERPROFILE", OsString::new()),
            ("HOME", OsString::new()),
            ("LOCALAPPDATA", OsString::new()),
        ]);

        for platform in [
            super::StoragePlatform::Other,
            super::StoragePlatform::Windows,
        ] {
            assert_eq!(
                resolve_storage_fixture(
                    &env,
                    Some(Path::new("")),
                    platform,
                    Some(fallback_home),
                    Some(current_dir),
                ),
                current_dir.join(".local/share").join(&module_suffix),
                "empty values and an empty configured root are unset"
            );
        }
        assert_eq!(
            resolve_storage_fixture(&env, None, super::StoragePlatform::Other, None, None,),
            PathBuf::from(".local/share/cortexkit/aft"),
            "an unavailable cwd preserves the honest relative path"
        );

        env.insert("HOME", OsString::from("/home/operator"));
        env.insert("USERPROFILE", OsString::from("/wrong-profile"));
        assert_eq!(
            resolve_storage_fixture(
                &env,
                None,
                super::StoragePlatform::Other,
                Some(fallback_home),
                Some(current_dir),
            ),
            Path::new("/home/operator/.local/share").join(&module_suffix)
        );

        env.insert("LOCALAPPDATA", OsString::from("/local-data"));
        assert_eq!(
            resolve_storage_fixture(
                &env,
                None,
                super::StoragePlatform::Windows,
                Some(fallback_home),
                Some(current_dir),
            ),
            Path::new("/local-data").join(&module_suffix)
        );
        env.insert("LOCALAPPDATA", OsString::new());
        assert_eq!(
            resolve_storage_fixture(
                &env,
                None,
                super::StoragePlatform::Windows,
                Some(fallback_home),
                Some(current_dir),
            ),
            Path::new("/wrong-profile/AppData/Local").join(&module_suffix)
        );

        env.insert("XDG_DATA_HOME", OsString::from("relative-data"));
        assert_eq!(
            resolve_storage_fixture(
                &env,
                None,
                super::StoragePlatform::Other,
                Some(fallback_home),
                Some(current_dir),
            ),
            current_dir.join("relative-data").join(&module_suffix)
        );

        env.insert("AFT_CACHE_DIR", OsString::from("/legacy-cache"));
        assert_eq!(
            resolve_storage_fixture(
                &env,
                None,
                super::StoragePlatform::Other,
                Some(fallback_home),
                Some(current_dir),
            ),
            PathBuf::from("/legacy-cache/aft")
        );
        let configured = Path::new("configured/../configured-aft");
        assert_eq!(
            resolve_storage_fixture(
                &env,
                Some(configured),
                super::StoragePlatform::Other,
                Some(fallback_home),
                Some(current_dir),
            ),
            configured,
            "the caller-owned configured spelling outranks the legacy cache lever"
        );

        env.insert("AFT_STORAGE_DIR", OsString::from("~/operator-aft"));
        assert_eq!(
            resolve_storage_fixture(
                &env,
                Some(configured),
                super::StoragePlatform::Other,
                Some(fallback_home),
                Some(current_dir),
            ),
            PathBuf::from("/home/operator/operator-aft")
        );
    }

    #[test]
    fn powershell_absence_has_an_honest_install_remedy() {
        let error = super::resolve_powershell_path_with(|_| None).expect_err("pwsh is absent");
        assert!(error.contains("PowerShell (pwsh) is not installed"));
        assert!(error.contains("https://aka.ms/powershell"));
        // The refusal must name the no-install fix too, not only installation.
        assert!(error.contains("bash tool without `shell: \"powershell\"`"));
        // A host that registered the command tool under another name gets that
        // name in the same sentence.
        assert!(super::powershell_missing_message("shell")
            .contains("call the shell tool without `shell: \"powershell\"`"));
    }

    #[test]
    fn cleanup_panic_during_unwind_does_not_abort_libtest() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let cleanup_ran = AtomicBool::new(false);
        let unwind = catch_unwind(AssertUnwindSafe(|| {
            let _cleanup = NonPanickingCleanup::new(|| {
                cleanup_ran.store(true, Ordering::SeqCst);
                panic!("forced cleanup failure");
            });
            panic!("primary test failure");
        }));

        assert!(cleanup_ran.load(Ordering::SeqCst));
        assert_eq!(
            unwind
                .expect_err("primary panic must escape the inner scope")
                .downcast_ref::<&str>(),
            Some(&"primary test failure")
        );
    }
}

#[cfg(all(test, unix))]
mod shell_resolution_tests {
    use super::{resolve_shell_path, BashShell};

    /// `pty: true` must not change which shell reads the command. The model
    /// writes bash for a tool named `bash`; a fish `$SHELL` on the host used to
    /// make only the PTY path interpret it, so `$?` and `&&` failed there and
    /// nowhere else.
    #[test]
    fn pty_and_pipe_modes_resolve_the_same_shell_regardless_of_shell_env() {
        let _env = crate::test_env::process_env_lock();
        let previous = std::env::var_os("SHELL");
        // A path that exists on every macOS/Linux box so the old $SHELL-first
        // resolver would have accepted it; the assertion below is what proves
        // the resolver no longer looks.
        std::env::set_var("SHELL", "/bin/sh");

        let pty = resolve_shell_path(true, BashShell::Bash).expect("pty shell resolves");
        let pipe = resolve_shell_path(false, BashShell::Bash).expect("pipe shell resolves");

        match previous {
            Some(value) => std::env::set_var("SHELL", value),
            None => std::env::remove_var("SHELL"),
        }

        assert_eq!(
            pty, pipe,
            "PTY and pipe modes must launch the same interpreter"
        );
        assert_ne!(
            pty.file_name().and_then(|name| name.to_str()),
            Some("sh"),
            "the PTY launcher must come from the bash resolver, not from $SHELL"
        );
    }
}

#[cfg(test)]
mod legacy_root_task_repair_tests {
    use std::fs;

    use crate::harness::Harness;
    use crate::walk_boundary::simulated_volumes::simulate;

    #[cfg(unix)]
    #[test]
    fn bash_background_legacy_live_writer_survives_cross_volume_repair_and_replay() {
        use super::persistence::*;
        use super::registry::BgTaskRegistry;
        use super::BgTaskStatus;
        use std::process::{Command, Stdio};

        let storage = tempfile::tempdir().unwrap();
        let session = "legacy-live-writer";
        let id = "bash-0123456789abcdef";
        let task = create_task_layout(storage.path(), session, id).unwrap();
        let handles = TaskIoHandles::create(&task, BgMode::Pipes, false).unwrap();
        let release = storage.path().join("release");
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "printf before; while ! test -f \"$1\"; do sleep 0.02; done; printf after; printf error-after >&2; printf 37 > \"$2\"; exit 37",
            "legacy-writer",
        ]);
        command.arg(&release).arg(&task.paths.exit);
        command.stdout(Stdio::from(
            handles.clone_file(TaskArtifact::Stdout).unwrap(),
        ));
        command.stderr(Stdio::from(
            handles.clone_file(TaskArtifact::Stderr).unwrap(),
        ));
        super::process::start_new_session(&mut command);
        let mut child = command.spawn().unwrap();
        let mut metadata = PersistedTask::starting(
            id.into(),
            session.into(),
            "legacy writer".into(),
            storage.path().into(),
            None,
            None,
            true,
            false,
        );
        metadata.mark_running(child.id(), child.id() as i32);
        write_task_at(&task, &metadata).unwrap();
        let harness = storage.path().join(Harness::Opencode.storage_segment());
        fs::create_dir_all(&harness).unwrap();
        let _volume = simulate(&harness, u64::MAX - 20);

        super::repair_legacy_root_tasks(storage.path(), Harness::Opencode);
        let registry = BgTaskRegistry::default();
        registry.replay_session(&harness, session).unwrap();
        let running = registry.status(id, session, None, None, 4096).unwrap();
        let replay_exit = registry.task_exit_path(id, session);
        // Release even on the broken implementation so no child is left waiting.
        fs::write(&release, b"finish").unwrap();
        assert_eq!(child.wait().unwrap().code(), Some(37));
        assert_eq!(running.info.status, BgTaskStatus::Running);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let completed = loop {
            let snapshot = registry.status(id, session, None, None, 4096).unwrap();
            if snapshot.info.status.is_terminal() {
                break snapshot;
            }
            assert!(std::time::Instant::now() < deadline, "task did not finish");
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        assert_eq!(completed.exit_code, Some(37));
        assert_eq!(completed.info.status, BgTaskStatus::Failed);
        assert_eq!(replay_exit, Some(task.paths.exit.clone()));
        assert_eq!(fs::read(&task.paths.stdout).unwrap(), b"beforeafter");
        assert_eq!(fs::read(&task.paths.stderr).unwrap(), b"error-after");
        assert!(task.paths.exit.exists());
        registry.repair_legacy_root_tasks(storage.path(), Harness::Opencode);
        assert!(
            task.paths.exit.exists(),
            "repeated configure must retain pinned paths"
        );
        registry.detach();
        drop(registry);
        drop(handles);
        drop(task);

        super::repair_legacy_root_tasks(storage.path(), Harness::Opencode);
        assert!(!storage.path().join("bash-tasks").exists());
        let moved = resolve_task(&harness, session, id).unwrap();
        assert_eq!(read_task_at(&moved).unwrap().exit_code, Some(37));
        assert_eq!(fs::read(&moved.paths.stdout).unwrap(), b"beforeafter");
        assert_eq!(
            read_exit_marker(&moved.paths).unwrap(),
            Some(ExitMarker::Code(37))
        );
    }

    #[test]
    fn legacy_root_tasks_move_into_a_harness_folder_on_another_volume() {
        use super::persistence::*;
        let storage = tempfile::tempdir().unwrap();
        let root_tasks = storage.path().join("bash-tasks");
        let id = "bash-0123456789abcdef";
        let task = create_task_layout(storage.path(), "terminal", id).unwrap();
        let mut metadata = PersistedTask::starting(
            id.into(),
            "terminal".into(),
            "done".into(),
            storage.path().into(),
            None,
            None,
            true,
            false,
        );
        metadata.mark_terminal(super::BgTaskStatus::Completed, Some(0), None);
        write_task_at(&task, &metadata).unwrap();
        drop(task);
        let harness_dir = storage.path().join(Harness::Opencode.storage_segment());
        fs::create_dir_all(&harness_dir).unwrap();
        // Every rename from the root into the harness folder now fails with EXDEV.
        let _volume = simulate(&harness_dir, u64::MAX - 20);

        super::repair_legacy_root_tasks(storage.path(), Harness::Opencode);

        assert_eq!(
            read_task_at(&resolve_task(&harness_dir, "terminal", id).unwrap())
                .unwrap()
                .exit_code,
            Some(0),
            "the task must reach the harness folder"
        );
        assert!(
            !root_tasks.exists(),
            "the legacy folder is removed after the copy"
        );
    }
}

#[cfg(all(test, unix))]
pub(crate) use registry::push_projection_fixture;
