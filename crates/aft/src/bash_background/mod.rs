//! Background bash task management: spawning detached tasks, the watchdog that
//! reaps them, output buffering/compression, and on-disk persistence so tasks
//! survive a bridge restart.

pub mod buffer;
#[cfg(unix)]
mod exit_observer;
mod gc_cursor;
pub mod output;
pub mod persistence;
pub mod process;
pub mod pty_process;
pub mod pty_runtime;
pub mod registry;
pub mod watchdog;
pub mod watches;

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
    state: std::sync::Mutex<SpawnReceiptState>,
    deadline: std::time::Instant,
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
    pub(crate) fn new(deadline: std::time::Instant) -> Self {
        Self {
            state: std::sync::Mutex::new(SpawnReceiptState::Pending),
            deadline,
        }
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
        if !matches!(*state, SpawnReceiptState::Pending)
            || std::time::Instant::now() >= self.deadline
        {
            // A second commit on one receipt is a bug; refusing it keeps an
            // earlier commit's answer intact instead of overwriting it.
            if matches!(*state, SpawnReceiptState::Pending) {
                *state = SpawnReceiptState::Refused;
            }
            return Err("bash startup deadline expired before process creation".into());
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

/// Called only after the starting record and every control/output file exist,
/// immediately before process creation. Once committed, the caller can always
/// get the task id, including while running-record persistence is blocked.
pub(crate) fn commit_spawn_receipt(task_id: &str) -> Result<(), String> {
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
    static CURRENT_REMOTE: std::cell::RefCell<Option<RemoteLaunch>> = const { std::cell::RefCell::new(None) };
    /// True only while dispatching a bash call admitted under the catalog's
    /// worker preset. Plugin worker-session flags do not set this marker.
    static CURRENT_WORKER_PRESET: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[derive(Clone, Debug)]
pub(crate) struct RemoteLaunch {
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

/// The requester and key to record on a task being created now: the
/// consumer's key when the call carried one, otherwise the task id.
pub(crate) fn call_key_for_new_task(task_id: &str) -> TaskCallKey {
    let requester = match current_authenticated_principal() {
        crate::sandbox_spawn::AuthenticatedPrincipal::FirstParty => "first-party".to_string(),
        crate::sandbox_spawn::AuthenticatedPrincipal::RouteBind { principal_id, .. } => {
            principal_id.unwrap_or_else(|| "absent".to_string())
        }
    };
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
    lookup(candidate).ok_or_else(|| {
        "PowerShell (pwsh) is not installed or is not on PATH, so this command was not run. \
         Rewrite it in bash syntax and call the bash tool without `shell: \"powershell\"`, \
         or install PowerShell 7+: https://aka.ms/powershell"
            .to_string()
    })
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

    let mut env = env.unwrap_or_default();
    let config = ctx.config();
    let child_storage_root = self::storage_dir(config.storage_dir.as_deref());
    // The ticket lets this command's `gh` shim relay bot writes for the
    // session that spawned it. Dropping it on any early return revokes it; a
    // spawned task's terminal transition revokes it after `bind_task`.
    let gh_shim_ticket = crate::gh_shim_ticket::PendingTicket::issue(
        session_id,
        &project_root
            .as_deref()
            .unwrap_or(&workdir)
            .display()
            .to_string(),
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
    } else if ctx.config().sandbox.enabled {
        RequestedSandboxTier::Native
    } else {
        RequestedSandboxTier::Disabled
    };
    let session_dir = persistence::session_tasks_dir(&storage_dir, session_id);
    #[cfg(unix)]
    let (spawn_plan, unregistered_task) = if native_sandbox_enforced(ctx, &principal)
        && host_escalation.is_none()
    {
        let task = match persistence::allocate_task_layout(&storage_dir, session_id) {
            Ok(task) => task,
            Err(error) => {
                return Response::error(
                    request_id,
                    "sandbox_unavailable",
                    format!(
                        "native sandbox failed to create the task artifact directory: {error}; set sandbox.enabled=false to disable native sandboxing"
                    ),
                );
            }
        };
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
            let environment = crate::sandbox_spawn::approved_environment_for_plan(&plan, &env);
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

    let cleanup_plan = spawn_plan.clone();
    #[cfg(unix)]
    ctx.bash_background()
        .set_db_schema_hints(ctx.config().bash.db_schema_hints);
    #[cfg(unix)]
    let remote_result = remote.map(|launch| {
        ctx.bash_background().spawn_remote(
            launch,
            spawn_plan.clone(),
            command,
            shell_path.clone(),
            session_id.to_string(),
            workdir.clone(),
            env.clone(),
            hard_kill,
            storage_dir.clone(),
            slot,
            notify_on_completion,
            compressed,
            project_root.clone(),
        )
    });
    #[cfg(not(unix))]
    let remote_result: Option<Result<String, String>> = {
        let _ = remote;
        None
    };
    let spawn_result = if let Some(result) = remote_result {
        result
    } else if pty {
        // A PTY always runs in the background, so it always takes a slot.
        ctx.bash_background().spawn_pty_with_shell(
            spawn_plan,
            command,
            shell,
            shell_path,
            session_id.to_string(),
            workdir,
            env,
            hard_kill,
            storage_dir,
            max_running,
            notify_on_completion,
            compressed,
            project_root,
            pty_rows,
            pty_cols,
        )
    } else {
        ctx.bash_background().spawn_with_shell_in_slot(
            spawn_plan,
            command,
            shell,
            shell_path,
            session_id.to_string(),
            workdir,
            env,
            hard_kill,
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
            Response::success(
                request_id,
                json!({
                    "task_id": task_id,
                    "status": BgTaskStatus::Running,
                    "mode": if pty { "pty" } else { "pipes" },
                }),
            )
        }
        Err(message) if message.contains("limit exceeded") => {
            cleanup_plan.cleanup_unspawned();
            #[cfg(unix)]
            if let Some(task) = unregistered_task.as_ref() {
                let _ = persistence::delete_resolved_task(task);
            }
            Response::error(request_id, "background_task_limit_exceeded", message)
        }
        Err(message) => {
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
            if message.contains("startup deadline expired") {
                Response::error(request_id, "bash_start_deadline", message)
            } else if cleanup_plan.is_native_launcher() {
                Response::error(
                    request_id,
                    "sandbox_unavailable",
                    format!(
                        "native sandbox failed before command execution: {message}; set sandbox.enabled=false to disable native sandboxing"
                    ),
                )
            } else {
                Response::error(request_id, "execution_failed", message)
            }
        }
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
pub fn storage_dir(configured: Option<&std::path::Path>) -> PathBuf {
    let lookup = |name: &str| std::env::var_os(name);
    let fallback_home = std::env::home_dir();
    let current_dir = std::env::current_dir().ok();
    storage_dir_from(
        configured,
        &lookup,
        StoragePlatform::current(),
        fallback_home.as_deref(),
        current_dir.as_deref(),
    )
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

    match std::fs::rename(&root_tasks, &harness_tasks) {
        Ok(()) => crate::slog_info!(
            "moved legacy root bash tasks into harness namespace: {}",
            harness_tasks.display()
        ),
        Err(error) => {
            crate::slog_warn!(
                "failed to move legacy root bash tasks into {}: {}; trying child merge",
                harness_tasks.display(),
                error
            );
            if crate::private_storage::create_dir_all(&harness_tasks).is_err() {
                return;
            }
            if let Ok(entries) = std::fs::read_dir(&root_tasks) {
                for entry in entries.flatten() {
                    let source = entry.path();
                    let target = harness_tasks.join(entry.file_name());
                    if !target.exists() {
                        let _ = std::fs::rename(source, target);
                    }
                }
            }
            let _ = std::fs::remove_dir(&root_tasks);
        }
    }
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
