use super::seams::*;
use super::*;
use crate::bash_background::persistence::{read_task, write_task};
use crate::exec_remote::wire_fixture::{id, Script};

pub const READ_ONLY_NOTE: &str = "note: the build server keeps tracked files read-only during a job, so the write to src/x.rs was refused (Operation not permitted). Edit files on this machine; the job's copy is discarded.";

pub struct PausedReattachClock {
    root: PathBuf,
    clock: Arc<ReattachClock>,
}

impl PausedReattachClock {
    pub fn new(root: &Path) -> Self {
        let root = test_clock_key(root);
        let (ticks, _) = tokio::sync::watch::channel(Duration::ZERO);
        let (resets, _) = tokio::sync::watch::channel(0);
        let (waits, _) = tokio::sync::watch::channel(0);
        let clock = Arc::new(ReattachClock {
            ticks,
            resets,
            waits,
        });
        assert!(reattach_clocks()
            .lock()
            .unwrap()
            .insert(root.clone(), clock.clone())
            .is_none());
        Self { root, clock }
    }

    #[cfg(test)]
    pub(super) async fn after_resets(&self, count: u64) {
        let mut resets = self.clock.resets.subscribe();
        tokio::time::timeout(
            Duration::from_secs(30),
            resets.wait_for(|resets| *resets >= count),
        )
        .await
        .expect("consumed records must reset the recovery budget")
        .unwrap();
    }

    #[cfg(test)]
    pub(super) async fn after_waits(&self, count: u64) {
        let mut waits = self.clock.waits.subscribe();
        tokio::time::timeout(
            Duration::from_secs(30),
            waits.wait_for(|waits| *waits >= count),
        )
        .await
        .expect("recovery must enter its budgeted wait")
        .unwrap();
    }

    pub fn advance(&self, duration: Duration) {
        self.clock.ticks.send_modify(|now| *now += duration);
    }
}

impl Drop for PausedReattachClock {
    fn drop(&mut self) {
        reattach_clocks().lock().unwrap().remove(&self.root);
        self.clock.ticks.send_replace(Duration::MAX);
    }
}

pub fn registry() -> BgTaskRegistry {
    BgTaskRegistry::new(Arc::new(Mutex::new(None)))
}

/// How many remote workers were started for `task`, and how many stopped.
pub fn remote_workers(task: &str) -> (usize, usize) {
    remote_worker_counts()
        .lock()
        .unwrap()
        .get(task)
        .copied()
        .unwrap_or_default()
}

/// Wait until the task's remote worker reaches its last step, so a test can
/// assert what the worker did not do afterwards without sleeping.
pub async fn remote_worker_exited(task: &str) {
    wait_for("the remote worker must stop", || {
        exited_remote_workers()
            .lock()
            .unwrap()
            .contains(task)
            .then_some(())
    })
    .await
}

pub fn launch(connection: PathBuf) -> crate::bash_background::RemoteLaunch {
    crate::bash_background::RemoteLaunch {
        explicit_runon: false,
        connection_file: Some(connection),
        harness: "broca".into(),
        session: "session".into(),
        requested_platform: None,
        requested_vcpus: None,
        requested_memory_mib: None,
        requested_network: false,
        configured_env_names: Vec::new(),
        return_paths: Vec::new(),
        params: exec::FrozenParams {
            remote_exec: Some(exec::policy::RemoteExecPolicy {
                enabled: true,
                ..Default::default()
            }),
            ..Default::default()
        },
    }
}

pub(super) fn start(registry: &BgTaskRegistry, dir: &Path, connection: PathBuf) -> String {
    start_with_hard_kill(
        registry,
        dir,
        connection,
        crate::bash_background::HardKill::After(Duration::from_secs(30)),
    )
}

pub(super) fn start_with_hard_kill(
    registry: &BgTaskRegistry,
    dir: &Path,
    connection: PathBuf,
    hard_kill: crate::bash_background::HardKill,
) -> String {
    start_with_launch(registry, dir, launch(connection), hard_kill)
}

pub fn start_with_launch(
    registry: &BgTaskRegistry,
    dir: &Path,
    launch: crate::bash_background::RemoteLaunch,
    hard_kill: crate::bash_background::HardKill,
) -> String {
    ensure_git_fixture(dir);
    registry
        .spawn_remote(
            launch,
            SpawnPlan::Unsandboxed,
            "printf local-proof",
            resolve_posix_shell(),
            "session".into(),
            dir.into(),
            HashMap::from([("BUILD_REMOTE_ENV_TEST".into(), "environment-proof".into())]),
            hard_kill,
            None,
            dir.into(),
            crate::bash_background::TaskSlot::Background { max: 10 },
            true,
            false,
            Some(dir.into()),
        )
        .unwrap()
}

/// Every path under `root` (relative, sorted, `.git` skipped) and every
/// persisted-format refusal recorded for bash-task records under it, for
/// failure messages about a task the registry does not hold.
pub(super) fn storage_report(root: &Path) -> String {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.file_name().is_some_and(|name| name == ".git") {
                continue;
            }
            out.push(
                path.strip_prefix(root)
                    .unwrap_or(&path)
                    .display()
                    .to_string(),
            );
            if path.is_dir() {
                walk(&path, root, out);
            }
        }
    }
    let mut paths = Vec::new();
    walk(root, root, &mut paths);
    paths.sort();
    // A refusal recorded for `root` or a directory above it, then any recorded
    // for paths inside it.
    use crate::persisted_format::{refusal_covering, refusals_under, PersistedStore};
    let mut refusals: Vec<String> = refusal_covering(PersistedStore::BashTask, root)
        .into_iter()
        .chain(
            refusals_under(root)
                .into_iter()
                .filter(|refusal| refusal.store == PersistedStore::BashTask),
        )
        .map(|refusal| refusal.to_string())
        .collect();
    refusals.dedup();
    format!(
        "files under {}: {paths:?}; bash-task format refusals: {refusals:?}",
        root.display()
    )
}

/// Poll until the task reaches a terminal status. A task that is not in the
/// registry is a failure, not something to wait out: the callers registered it
/// before polling, and nothing in these tests acknowledges a completion, which
/// is what retention cleanup needs before it removes a task.
pub async fn terminal(registry: &BgTaskRegistry, task: &str) -> BgTaskSnapshot {
    // The task's bundle directory from the first poll that found it, so a task
    // that later disappears is reported with what its session directory holds.
    let mut seen_in: Option<PathBuf> = None;
    let started = Instant::now();
    wait_for("fake executor must terminate", || {
        let Some(snapshot) = registry.observed_status(task, "session", 8192) else {
            let held_under = registry
                .task_for_test(task)
                .map(|held| held.session_id.clone());
            match &seen_in {
                Some(bundle) => panic!(
                    "task {task} disappeared from the registry {:?} into the wait for a terminal status (held under session {held_under:?}); {}",
                    started.elapsed(),
                    storage_report(bundle.parent().unwrap_or(bundle))
                ),
                None => panic!(
                    "task {task} was not registered for session `session` when the wait for a terminal status began (held under session {held_under:?})"
                ),
            }
        };
        if seen_in.is_none() {
            seen_in = registry.task_for_test(task).map(|held| held.paths.dir.clone());
        }
        snapshot.info.status.is_terminal().then_some(snapshot)
    })
    .await
}

pub struct ReleaseOnDrop(PathBuf);
impl ReleaseOnDrop {
    pub fn new(path: PathBuf) -> Self {
        Self(path)
    }
}

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        let _ = fs::write(&self.0, b"release");
    }
}

pub fn assert_runon_refusal(response: &crate::protocol::Response, reason: &str) {
    assert!(!response.success, "{response:?}");
    assert_eq!(response.data["code"], "remote_unavailable", "{response:?}");
    let job_id = response
        .data
        .get("remote_job_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            response
                .data
                .get("output")
                .and_then(serde_json::Value::as_str)
                .and_then(|output| {
                    output.lines().find_map(|line| {
                        line.strip_prefix("remote executor refused before start (job ")
                            .and_then(|line| line.strip_suffix(')'))
                            .map(str::to_owned)
                    })
                })
        });
    let expected = format!(
        "runon refused: remote refused: {reason}; command was not run; retry, or omit runon to run locally{}",
        job_id.map_or_else(String::new, |job_id| format!("; remote job {job_id}")),
    );
    assert_eq!(response.data["message"], expected, "{response:?}");
}

pub const EXPECTED_REMOTE_JOB_HEADER: &str =
    "ran remotely on ck-motor (4 vCPUs requested, job 0192a64a-1234-7000-8000-000000000001)";

pub struct UncapturedTerminalBeforeRenderForTest {
    pub(crate) dir: tempfile::TempDir,
    pub(crate) registry: BgTaskRegistry,
    pub(crate) task: Arc<BgTask>,
    release: crossbeam_channel::Sender<()>,
    publisher: Option<std::thread::JoinHandle<()>>,
}

pub fn captured_task_after_losing_replay_for_test(
) -> (tempfile::TempDir, BgTaskRegistry, Arc<BgTask>) {
    let dir = tempfile::tempdir().unwrap();
    let (source, task_id, paths) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf unused");
    let mut live = source
        .task(&task_id)
        .unwrap()
        .state
        .lock()
        .unwrap()
        .metadata
        .clone();
    live.remote.as_mut().unwrap().explicit_runon = true;
    live.timeout_ms = Some(60_000);
    live.execution_note = Some("live input capture recorded".into());
    let mut stale = live.clone();
    stale.remote.as_mut().unwrap().accepted = false;
    stale.timeout_ms = Some(10_000);
    stale.execution_note = Some("stale pre-capture replay".into());
    let registry = self::registry();
    let _reserved = registry.drain_reply_cache(&task_id, "session");
    let (reached_tx, reached_rx) = crossbeam_channel::bounded(1);
    let (release_tx, release_rx) = crossbeam_channel::bounded(1);
    REHYDRATED_REGISTRATION_GATES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .insert(
            task_id.clone(),
            KillSignalGate {
                reached: reached_tx,
                release: release_rx,
            },
        );
    struct ReplayJoin {
        release: crossbeam_channel::Sender<()>,
        thread: Option<std::thread::JoinHandle<Result<(), String>>>,
    }
    impl Drop for ReplayJoin {
        fn drop(&mut self) {
            let _ = self.release.send(());
            if let Some(thread) = self.thread.take() {
                let result = thread.join();
                if !std::thread::panicking() {
                    result.unwrap().unwrap();
                }
            }
        }
    }
    let replay = ReplayJoin {
        release: release_tx,
        thread: Some({
            let registry = registry.clone();
            let paths = paths.clone();
            std::thread::spawn(move || registry.insert_rehydrated_task(stale, paths, true))
        }),
    };
    reached_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("replay paused before registration");
    registry.insert_rehydrated_task(live, paths, false).unwrap();
    let task = registry.task(&task_id).unwrap();
    assert_eq!(
        task.drain_reply.snapshot().inputs_captured,
        Some(true),
        "live registration must publish first"
    );
    // The replay resumes with its older snapshot only after the accepted task
    // owns the registry entry. It must not mutate that task's shared cache.
    drop(replay);
    assert!(Arc::ptr_eq(&task, &registry.task(&task_id).unwrap()));
    (dir, registry, task)
}

pub fn uncaptured_terminal_before_render_for_test(
    refused: bool,
) -> UncapturedTerminalBeforeRenderForTest {
    let verdict = if refused {
        Verdict::RunLocally {
            reason: RefusalReason::Unreachable,
            refusal_detail: None,
        }
    } else {
        Verdict::Signalled { signal: 9 }
    };
    uncaptured_verdict_before_render_for_test(
        verdict,
        refused.then(|| "runon refused: remote refused: unreachable; command was not run".into()),
    )
}

pub fn uncaptured_unknown_before_render_for_test() -> UncapturedTerminalBeforeRenderForTest {
    uncaptured_verdict_before_render_for_test(
        Verdict::OutcomeUnknown,
        Some("remote route interrupted before acceptance".into()),
    )
}

pub(super) fn uncaptured_verdict_before_render_for_test(
    verdict: Verdict,
    error: Option<String>,
) -> UncapturedTerminalBeforeRenderForTest {
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, _) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf unused");
    let task = registry.task(&task_id).unwrap();
    {
        let mut state = task.state.lock().unwrap();
        let remote = state.metadata.remote.as_mut().unwrap();
        remote.explicit_runon = true;
        remote.accepted = false;
        remote.job_id = None;
    }
    let (reached_tx, reached_rx) = crossbeam_channel::bounded(1);
    let (release, release_rx) = crossbeam_channel::bounded(1);
    TERMINAL_PUBLICATION_GATES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .insert(
            task_id,
            KillSignalGate {
                reached: reached_tx,
                release: release_rx,
            },
        );
    let publisher = {
        let registry = registry.clone();
        let task = task.clone();
        std::thread::spawn(move || {
            registry.remote_terminal(&task, verdict, error);
        })
    };
    let fixture = UncapturedTerminalBeforeRenderForTest {
        dir,
        registry,
        task,
        release,
        publisher: Some(publisher),
    };
    reached_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("terminal metadata published before rendering");
    fixture
}

pub fn cancel_unknown_terminal() -> TerminalRecord {
    let mut terminal = crate::exec_remote::wire_fixture::cancel_exit_status_unknown_terminal();
    terminal.job_id = id();
    terminal
}

pub fn runon_restart_context(project: &Path, storage: &Path) -> Arc<crate::context::AppContext> {
    Arc::new(crate::context::AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(project.into()),
            storage_dir: Some(storage.into()),
            harness: Some(crate::harness::Harness::Runner),
            experimental_bash_background: true,
            ..Default::default()
        },
    ))
}

pub fn runon_restart_principal(project: &Path) -> crate::sandbox_spawn::AuthenticatedPrincipal {
    crate::sandbox_spawn::AuthenticatedPrincipal::RouteBind {
        trust: crate::sandbox_spawn::PrincipalTrust::FirstParty,
        route_channel: 41,
        route_epoch: 1,
        project_root: project.into(),
        harness: "runner".into(),
        session_id: "session".into(),
        principal_id: Some("reserved:broca".into()),
    }
}

pub fn persist_accepted_with_snapshot(
    dir: &Path,
    connection: PathBuf,
    command: &str,
) -> (BgTaskRegistry, String, TaskPaths) {
    persist_accepted_with_snapshot_cap(dir, connection, command, None)
}

pub(super) fn persist_accepted_with_snapshot_cap(
    dir: &Path,
    connection: PathBuf,
    command: &str,
    memory_limit_mib: Option<u64>,
) -> (BgTaskRegistry, String, TaskPaths) {
    persist_accepted_with_snapshot_scope(dir, connection, command, memory_limit_mib, false, 86400)
}

pub fn restarted_context_with_runon(dir: &Path, enabled: bool) -> crate::context::AppContext {
    ensure_git_fixture(dir);
    // A fresh configuration would choose ordinary unsandboxed inheritance;
    // the saved Host plan must still clear ambient HOME before its command.
    crate::context::AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(dir.into()),
            remote_exec: crate::config::RemoteExecConfig {
                enabled,
                ..Default::default()
            },
            sandbox: crate::config::SandboxConfig {
                ..Default::default()
            },
            ..Default::default()
        },
    )
}

pub fn exec_runs(daemon: &crate::exec_remote::wire_fixture::Daemon) -> Vec<serde_json::Value> {
    daemon
        .log
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, body)| body["method"] == "exec.run")
        .map(|(_, body)| body["params"].clone())
        .collect()
}

pub async fn windows_daemon(
    code: Option<u32>,
    acknowledge: bool,
) -> crate::exec_remote::wire_fixture::Daemon {
    crate::exec_remote::wire_fixture::daemon_with_capabilities(
        Script::Windows { code, acknowledge },
        &["exec-remote/v1", "exec-remote-windows/v1"],
        1,
    )
    .await
}

#[cfg(unix)]
pub(super) fn hermetic_git(cwd: &Path, args: &[&str]) {
    let mut command = std::process::Command::new("git");
    let output = crate::test_env::apply_hermetic_git_env(command.current_dir(cwd))
        .args(args)
        .output()
        .expect("run git fixture command");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
pub fn git_fixture(root: &Path) {
    std::fs::create_dir_all(root).unwrap();
    hermetic_git(root, &["init", "--quiet"]);
    hermetic_git(root, &["config", "user.name", "AFT Test"]);
    hermetic_git(root, &["config", "user.email", "aft-test@example.invalid"]);
    // Execution sentinels are disposable output, not source edits. Keep them
    // ignored so the checkout-write preflight does not mask the executor or cd
    // refusal that each test is meant to exercise.
    std::fs::write(
        root.join(".git/info/exclude"),
        "must-not-run-locally\nmust-not-run\nlocal-spawn-proof\n",
    )
    .unwrap();
    std::fs::write(root.join("tracked.txt"), "fixture\n").unwrap();
    hermetic_git(root, &["add", "tracked.txt"]);
    hermetic_git(root, &["commit", "--quiet", "-m", "fixture"]);
}

#[cfg(unix)]
pub fn ensure_git_fixture(root: &Path) {
    if !root.join(".git").exists() {
        git_fixture(root);
    }
}

#[cfg(unix)]
pub fn remote_test_context(root: &Path) -> crate::context::AppContext {
    ensure_git_fixture(root);
    restarted_context(root)
}

#[cfg(unix)]
pub fn remote_test_context_with_runon(root: &Path, enabled: bool) -> crate::context::AppContext {
    ensure_git_fixture(root);
    restarted_context_with_runon(root, enabled)
}

#[cfg(unix)]
pub fn add_git_worktree(repository: &Path, path: &Path, branch: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let path = path.to_str().unwrap();
    hermetic_git(
        repository,
        &["worktree", "add", "--quiet", "-b", branch, path, "HEAD"],
    );
}

/// The bash call the OpenCode and Pi plugins send for a model's `runon` call.
pub fn orchestrated_request(params: serde_json::Value) -> crate::protocol::RawRequest {
    crate::protocol::RawRequest {
        id: "runon".into(),
        command: "bash".into(),
        session_id: Some("session".into()),
        lsp_hints: None,
        params,
    }
}

/// Every cancellation AFT sent the runner: `exec.cancel`, or a Cancel frame
/// (sent when a held-open run stream is dropped).
pub fn runner_cancels(daemon: &crate::exec_remote::wire_fixture::Daemon) -> usize {
    daemon
        .log
        .lock()
        .unwrap()
        .iter()
        .filter(|(header, body)| {
            body["method"] == "exec.cancel" || header.ty == subc_protocol::FrameType::Cancel
        })
        .count()
}

/// The plugins' transport deadline for a blocking call with this `timeout`
/// (`orchestratedTransportTimeoutMs`: the timeout plus a 10-second margin).
pub fn plugin_transport_deadline(timeout_ms: u64) -> Duration {
    Duration::from_millis(timeout_ms + 10_000)
}

pub fn remote_job_id_context(dir: &Path) -> Arc<crate::context::AppContext> {
    ensure_git_fixture(dir);
    Arc::new(crate::context::AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(dir.into()),
            storage_dir: Some(dir.into()),
            ..Default::default()
        },
    ))
}

pub fn shorten_draining_budget(root: &Path, ms: u64) {
    draining_budgets().lock().unwrap().insert(root.into(), ms);
}

pub struct PausedDrainingClock {
    root: PathBuf,
    clock: Arc<DrainingClock>,
    events: tokio::sync::mpsc::UnboundedReceiver<DrainingClockEvent>,
}

impl PausedDrainingClock {
    pub fn new(root: &Path) -> Self {
        let root = test_clock_key(root);
        let (ticks, _) = tokio::sync::watch::channel(1_000_000);
        let (events, receiver) = tokio::sync::mpsc::unbounded_channel();
        let clock = Arc::new(DrainingClock {
            ticks,
            events,
            retry_jitter_ms: std::sync::atomic::AtomicU64::new(0),
        });
        assert!(draining_clocks()
            .lock()
            .unwrap()
            .insert(root.clone(), clock.clone())
            .is_none());
        Self {
            root,
            clock,
            events: receiver,
        }
    }

    pub fn now(&self) -> u64 {
        self.clock.now()
    }

    pub fn advance(&self, ms: u64) {
        self.clock.ticks.send_modify(|now| *now += ms);
    }

    #[cfg(test)]
    pub(super) fn set_retry_jitter_ms(&self, ms: u64) {
        self.clock.retry_jitter_ms.store(ms, Ordering::SeqCst);
    }

    pub async fn expect_event(&mut self, expected: DrainingClockEvent) {
        // This is only a deadlock watchdog; the event's deadline, not elapsed
        // wall time, proves which budget or retry delay the worker selected.
        let event = tokio::time::timeout(Duration::from_secs(30), self.events.recv())
            .await
            .expect("draining worker must acknowledge its wait or stop")
            .expect("draining clock event channel closed");
        assert_eq!(event, expected);
    }

    pub async fn expect_sleep(&mut self, from: u64, until: u64) {
        self.expect_event(DrainingClockEvent::Sleeping { from, until })
            .await;
    }
}

impl Drop for PausedDrainingClock {
    fn drop(&mut self) {
        draining_clocks().lock().unwrap().remove(&self.root);
        // Release a worker even when an assertion failed with its clock paused.
        self.clock.ticks.send_replace(u64::MAX);
    }
}

pub fn start_runon_retry(registry: &BgTaskRegistry, dir: &Path, connection: PathBuf) -> String {
    ensure_git_fixture(dir);
    let mut policy = launch(connection);
    policy.explicit_runon = true;
    registry
        .spawn_remote(
            policy,
            SpawnPlan::Unsandboxed,
            "printf local-proof",
            resolve_posix_shell(),
            "session".into(),
            dir.into(),
            HashMap::from([(
                "REMOTE_REQUEST_PRIVATE_VALUE".into(),
                "private-value-proof".into(),
            )]),
            crate::bash_background::HardKill::After(Duration::from_secs(30)),
            None,
            dir.into(),
            crate::bash_background::TaskSlot::Background { max: 10 },
            true,
            false,
            Some(dir.into()),
        )
        .unwrap()
}

pub fn retry_backoff_script(signer: bool, hint: u64) -> Script {
    if signer {
        Script::SignerRefused {
            refusals: 1,
            detail: "runner_sign_unavailable",
            retry_after_ms: Some(hint),
        }
    } else {
        Script::Draining {
            refusals: 1,
            retry_after_ms: Some(hint),
        }
    }
}

pub struct HeldDrainingGate {
    key: DrainingGateKey,
    reached: tokio::sync::watch::Receiver<bool>,
    release: tokio::sync::watch::Sender<bool>,
}

impl HeldDrainingGate {
    pub fn arm(root: &Path, gate: DrainingGate) -> Self {
        let key = (test_clock_key(root), gate);
        let (reached_tx, reached) = tokio::sync::watch::channel(false);
        let (release, release_rx) = tokio::sync::watch::channel(false);
        assert!(draining_gates()
            .lock()
            .unwrap()
            .insert(key.clone(), (reached_tx, release_rx))
            .is_none());
        Self {
            key,
            reached,
            release,
        }
    }

    pub async fn reached(&mut self) {
        tokio::time::timeout(
            Duration::from_secs(30),
            self.reached.wait_for(|reached| *reached),
        )
        .await
        .expect("draining worker must reach the gate")
        .expect("draining gate dropped");
    }

    pub fn release(&self) {
        self.release.send_replace(true);
    }
}

impl Drop for HeldDrainingGate {
    fn drop(&mut self) {
        draining_gates().lock().unwrap().remove(&self.key);
        // Release a held worker even when an assertion failed.
        self.release.send_replace(true);
    }
}

pub fn owner_scope(scope_ref: &str) -> subc_protocol::scope::ScopeStamp {
    subc_protocol::scope::ScopeStamp::new(
        subc_protocol::Principal::Reserved {
            module_id: "prefrontal-core".into(),
        },
        scope_ref,
        1,
        subc_protocol::scope::ScopeKind::Worker,
    )
}

/// The pause has ended the task: it recorded its cancellation intent and the
/// paused terminal state. A worker held at a gate can be released from here on
/// to observe it. Releasing it earlier would let the worker race the pause to
/// the task's terminal state.
pub async fn pause_recorded(registry: &BgTaskRegistry, task: &str) {
    wait_for("the pause must end the task", || {
        let task = registry.task(task)?;
        let state = task.state.lock().unwrap();
        (state.metadata.remote.as_ref()?.cancel_requested
            && state.metadata.is_terminal()
            && state.metadata.status_reason.as_deref()
                == Some(crate::bash_background::registry::OWNER_PAUSED_REASON))
        .then_some(())
    })
    .await
}

/// A worker that settles the owner's pause answers `bash.kill_scope` at once.
/// One that never settles it leaves the reply waiting out its whole 10-second
/// deadline and then reporting a failed cancellation. Half that deadline tells
/// the two apart without depending on machine speed, and the reply text must
/// also show the settled outcome rather than the deadline's.
pub async fn assert_pause_settles_promptly(
    registry: &BgTaskRegistry,
    operation: tokio::task::JoinHandle<crate::protocol::Response>,
    task: &str,
) {
    let reply = tokio::time::timeout(Duration::from_secs(5), operation)
        .await
        .expect("bash.kill_scope must settle well before its 10-second deadline")
        .unwrap();
    assert!(reply.success, "{reply:?}");
    let killed = reply.data["killed"].as_array().unwrap();
    assert_eq!(
        killed.len(),
        1,
        "{reply:?}; task: {:?}",
        registry.status(task, "session", None, None, 0)
    );
    assert_eq!(killed[0]["task_id"], task, "{reply:?}");
    // A refused submission leaves no runner job to cancel, so withholding the
    // retry is the whole cancellation and is reported as sent.
    assert_eq!(killed[0]["remote"]["cancel"], "sent", "{reply:?}");
}

pub fn assert_paused(registry: &BgTaskRegistry, task: &str) {
    let done = registry.status(task, "session", None, None, 0).unwrap();
    assert_eq!(done.info.status, BgTaskStatus::Killed, "{done:?}");
    assert_eq!(done.info.status_reason.as_deref(), Some("paused_by_owner"));
}

pub fn start_scoped_runon_retry(
    registry: &BgTaskRegistry,
    dir: &Path,
    connection: PathBuf,
    scope: &subc_protocol::scope::ScopeStamp,
) -> String {
    super::super::with_scope_spawn(Some(scope.clone()), || {
        start_runon_retry(registry, dir, connection)
    })
}

/// Start a command of `scope` that ignores SIGTERM, saved under `storage` as
/// the module saves its tasks, then let its registry go without stopping it,
/// as a module restart does. `bash.kill_scope` stops local tasks before
/// remote ones, and stopping this one takes the whole termination grace
/// period. A remote worker that recovery started before the pause therefore
/// has that long to act on its own. Returns the task id and the command's
/// process id.
pub async fn start_saved_term_ignoring_task(
    storage: &Path,
    scope: &subc_protocol::scope::ScopeStamp,
) -> (String, u32) {
    let previous = registry();
    let pid_file = storage.join("term-ignoring-pid");
    let task = super::super::with_scope_spawn(Some(scope.clone()), || {
        previous.spawn_with_shell_in_slot(
            SpawnPlan::Unsandboxed,
            &format!(
                "trap '' TERM; echo $$ > '{}'; while :; do /bin/sleep 0.05; done",
                pid_file.display()
            ),
            crate::bash_background::BashShell::Bash,
            resolve_posix_shell(),
            "local-session".into(),
            storage.into(),
            HashMap::new(),
            crate::bash_background::HardKill::After(Duration::from_secs(30)),
            None,
            storage.into(),
            crate::bash_background::TaskSlot::Background { max: 10 },
            true,
            false,
            Some(storage.into()),
        )
    })
    .unwrap();
    let pid = wait_for("the saved local command must start", || {
        fs::read_to_string(&pid_file)
            .ok()?
            .trim()
            .parse::<u32>()
            .ok()
    })
    .await;
    previous.detach();
    (task, pid)
}

/// The reply names both stopped tasks, the remote one settled as sent, and
/// the saved local command is gone.
pub async fn assert_saved_pause_reply(
    reply: &crate::protocol::Response,
    remote_task: &str,
    local_task: &str,
    local_pid: u32,
) {
    assert!(reply.success, "{reply:?}");
    let killed = reply.data["killed"].as_array().unwrap();
    assert_eq!(killed.len(), 2, "{reply:?}");
    let remote = killed
        .iter()
        .find(|entry| entry["task_id"] == remote_task)
        .unwrap_or_else(|| panic!("{reply:?}"));
    assert_eq!(remote["from_saved_record"], true, "{reply:?}");
    // The pause was recorded before any worker ran, so the worker withheld
    // the command (or cancelled the runner job) and reported it as sent.
    assert_eq!(remote["remote"]["cancel"], "sent", "{reply:?}");
    let local = killed
        .iter()
        .find(|entry| entry["task_id"] == local_task)
        .unwrap_or_else(|| panic!("{reply:?}"));
    assert!(local["remote"].is_null(), "{reply:?}");
    wait_for("the saved local command must be stopped", || {
        (!crate::process_identity::is_process_alive(local_pid)).then_some(())
    })
    .await;
}

pub fn assert_saved_paused(json: &Path) {
    let saved = read_task(json).unwrap();
    assert_eq!(saved.status, BgTaskStatus::Killed, "{saved:?}");
    assert_eq!(saved.status_reason.as_deref(), Some("paused_by_owner"));
    assert!(!saved.local_fallback_started, "{saved:?}");
    assert_eq!(
        saved.remote.unwrap().owner_pause_cancel.as_deref(),
        Some("sent")
    );
}

pub fn exec_cancels(daemon: &crate::exec_remote::wire_fixture::Daemon) -> Vec<serde_json::Value> {
    daemon
        .log
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, body)| body["method"] == "exec.cancel")
        .map(|(_, body)| body["params"].clone())
        .collect()
}

/// A `runon` task of a fresh owner scope whose job the runner (accepting up
/// to `clients` connections) has accepted, paused while its worker was
/// stopped right before it would send the cancel, and saved by a registry
/// that is then dropped, as a module restart leaves it. Returns the runner,
/// the task id, the job id and the path of the saved record, which is
/// finished and still owes the runner the cancel.
pub async fn saved_task_owing_a_cancel(
    dir: &Path,
    clients: usize,
) -> (
    crate::exec_remote::wire_fixture::Daemon,
    String,
    Uuid,
    PathBuf,
) {
    let daemon = crate::exec_remote::wire_fixture::daemon_with_clients(
        Script::Cancel,
        "exec-remote/v1",
        clients,
    )
    .await;
    let scope = owner_scope("owed-cancel");
    let original = registry();
    let task_id = start_scoped_runon_retry(&original, dir, daemon.connection.clone(), &scope);
    let job_id = wait_for("the runner must accept the job", || {
        let task = original.task(&task_id)?;
        let state = task.state.lock().unwrap();
        state.metadata.remote.as_ref()?.job_id
    })
    .await;
    stopped_draining_roots().lock().unwrap().insert(dir.into());
    let members = original.begin_scope_pause(&scope).unwrap();
    original.pause_scope(&scope, true, &members).unwrap();
    remote_worker_exited(&task_id).await;
    let json = original.task(&task_id).unwrap().paths.json.clone();
    original.detach();
    let saved = read_task(&json).unwrap();
    assert_eq!(saved.status, BgTaskStatus::Killed, "{saved:?}");
    assert!(
        saved.remote.unwrap().cancel_pending,
        "the cancel must be owed"
    );
    assert!(
        owed_marker(dir, &task_id).is_file(),
        "the owed cancel must be indexed"
    );
    assert!(exec_cancels(&daemon).is_empty(), "the cancel was sent");
    (daemon, task_id, job_id, json)
}

/// The owed-cancel index entry for a task saved under storage `dir`.
pub(super) fn owed_marker(dir: &Path, task_id: &str) -> PathBuf {
    dir.join("bash-owed-cancels").join(task_id)
}

/// The saved record shows the owed cancel sent once and nothing owed.
pub fn assert_owed_cancel_sent_once(
    daemon: &crate::exec_remote::wire_fixture::Daemon,
    job_id: Uuid,
    json: &Path,
) {
    let cancels = exec_cancels(daemon);
    assert_eq!(cancels.len(), 1, "{cancels:?}");
    assert!(
        cancels[0].to_string().contains(&job_id.to_string()),
        "{cancels:?}"
    );
    let saved = read_task(json).unwrap();
    assert_eq!(saved.status, BgTaskStatus::Killed, "{saved:?}");
    assert_eq!(saved.status_reason.as_deref(), Some("paused_by_owner"));
    let remote = saved.remote.unwrap();
    assert!(!remote.cancel_pending, "{remote:?}");
    assert_eq!(remote.owner_pause_cancel.as_deref(), Some("sent"));
    assert_eq!(exec_runs(daemon).len(), 1, "never resubmitted");
    let storage = json
        .ancestors()
        .find(|dir| dir.join("bash-tasks").is_dir())
        .unwrap();
    assert!(
        !owed_marker(storage, &saved.task_id).exists(),
        "the settled cancel must leave the index"
    );
}

/// A `runon` task of `scope` whose draining retry is saved and due, as a
/// module restart leaves it: its worker stopped and its registry is gone.
/// Returns the runner, the paused retry clock, the task id and the path of
/// its saved record. The runner refuses the first `refusals` runs as
/// draining and accepts the rest, which exit 0. It accepts a second
/// connection, so a worker started after the restart can reach it.
pub async fn saved_due_draining_retry(
    dir: &Path,
    scope: &subc_protocol::scope::ScopeStamp,
    refusals: u32,
) -> (
    crate::exec_remote::wire_fixture::Daemon,
    PausedDrainingClock,
    String,
    PathBuf,
) {
    let daemon = crate::exec_remote::wire_fixture::daemon_with_clients(
        Script::Draining {
            refusals,
            retry_after_ms: Some(200),
        },
        "exec-remote/v1",
        2,
    )
    .await;
    let mut clock = PausedDrainingClock::new(dir);
    stopped_draining_roots().lock().unwrap().insert(dir.into());
    let original = registry();
    let task_id = start_scoped_runon_retry(&original, dir, daemon.connection.clone(), scope);
    clock.expect_event(DrainingClockEvent::Stopped).await;
    wait_for("the original worker must stop", || {
        (remote_workers(&task_id) == (1, 1)).then_some(())
    })
    .await;
    let json = original.task(&task_id).unwrap().paths.json.clone();
    original.detach();
    clock.advance(5000);
    (daemon, clock, task_id, json)
}

/// The reply of a pause that stopped the saved task `task` and settled it.
pub fn assert_one_saved_task_paused(reply: &crate::protocol::Response, task: &str) {
    assert!(reply.success, "{reply:?}");
    let killed = reply.data["killed"].as_array().unwrap();
    assert_eq!(killed.len(), 1, "{reply:?}");
    assert_eq!(killed[0]["task_id"], task, "{reply:?}");
    assert_eq!(killed[0]["from_saved_record"], true, "{reply:?}");
    assert_eq!(killed[0]["remote"]["cancel"], "sent", "{reply:?}");
}

/// The bound project's session hears of the owner's stop exactly once: one
/// completion for `task`, and one cancelled-call notice naming it, which is
/// not repeated. The `bash.kill_scope` recovery registry that recorded the
/// stop has no session of its own and is detached afterwards.
pub fn assert_project_told_of_the_stop_once(project: &BgTaskRegistry, task: &str) {
    let completions = project
        .drain_completions_for_session(Some("session"))
        .into_iter()
        .filter(|completion| completion.task_id == task)
        .collect::<Vec<_>>();
    assert_eq!(completions.len(), 1, "{completions:?}");
    assert_eq!(completions[0].status, BgTaskStatus::Killed);
    let notices = project.take_cancelled_call_notices("session");
    assert_eq!(
        notices
            .iter()
            .filter(|notice| notice.contains(task))
            .count(),
        1,
        "{notices:?}"
    );
    assert!(
        project.take_cancelled_call_notices("session").is_empty(),
        "the cancelled-call notice was repeated"
    );
}

#[cfg(test)]
pub(super) fn held<T>(lock: &Mutex<T>) -> bool {
    matches!(lock.try_lock(), Err(std::sync::TryLockError::WouldBlock))
}

pub fn grant_sibling_read(
    ctx: &crate::context::AppContext,
    sibling: &Path,
) -> (tempfile::TempDir, crate::access_policy::ScopeGuard) {
    use crate::access_policy::{AccessGrant, AccessOperation, GrantKind};
    use crate::db::access_grants::{self as grants, ScopeKey};

    assert!(ctx
        .validate_read_path_without_approval(
            "sibling-read",
            "session",
            &sibling.canonicalize().unwrap()
        )
        .is_err());
    let storage = tempfile::tempdir().unwrap();
    let conn = crate::db::open(&storage.path().join("grants.db")).unwrap();
    let scope = ScopeKey {
        owner: "reserved:prefrontal-core".into(),
        scope_ref: "runon-siblings".into(),
        epoch: 1,
    };
    grants::register_scope(&conn, &scope, None, &ctx.config()).unwrap();
    grants::insert(
        &conn,
        &scope,
        &AccessGrant {
            path: sibling.to_path_buf(),
            kind: GrantKind::Subtree,
            operation: AccessOperation::Read,
        },
    )
    .unwrap();
    ctx.set_db(std::sync::Arc::new(std::sync::Mutex::new(conn)));
    (storage, crate::access_policy::scope_guard(Some(scope)))
}

/// Shorten the time a job submitted under `root` has for the runner to admit
/// it, which bounds how long AFT keeps resubmitting after hinted refusals.
pub fn shorten_hinted_retry_budget(root: &Path, ms: u64) {
    hinted_retry_budgets()
        .lock()
        .unwrap()
        .insert(test_clock_key(root), ms);
}

/// The worker asked the paused clock for no sleep since the last one a test
/// consumed.
pub fn assert_no_retry_sleep(clock: &mut PausedDrainingClock) {
    if let Ok(event) = clock.events.try_recv() {
        panic!("no retry wait was expected, got {event:?}");
    }
}

pub const SIGNER_REFUSED: &str = "runner_authentication_failed (runner_sign_unavailable)";

pub fn signer_retry_script(refusals: u32) -> Script {
    Script::SignerRefused {
        refusals,
        detail: "runner_sign_unavailable",
        retry_after_ms: Some(2000),
    }
}

/// The saved record of a task the owner paused while AFT held back a signer
/// refusal's resubmission: paused, the pause's cancellation recorded as sent
/// (the runner had refused the job, so withholding the resubmission was the
/// whole cancellation), and the runner's refusal kept as the proof that the
/// command never started.
pub fn assert_signer_retry_pause_saved(json: &Path) {
    assert_refused_pause_saved(json, "runner_sign_unavailable");
}

/// The saved record of a task the owner paused after the runner refused it
/// with `detail`: paused, the cancellation recorded as sent, no sign that the
/// command started, and the runner's refusal (`ran: none`) kept as the proof
/// that it never did.
pub fn assert_refused_pause_saved(json: &Path, detail: &str) {
    assert_saved_paused(json);
    let remote = read_task(json).unwrap().remote.unwrap();
    assert!(
        remote.started.is_none() && !remote.output_received,
        "{remote:?}"
    );
    let refusal = remote
        .terminal
        .as_ref()
        .unwrap_or_else(|| panic!("{remote:?}"));
    assert_eq!(refusal.ran, Some(Ran::None), "{remote:?}");
    assert_eq!(refusal.refusal_detail(), Some(detail));
}

/// The saved record of a task the owner paused while its retry's resubmission
/// failed without reaching the runner: paused, the cancellation recorded as
/// sent, and no runner job or sign that the command started.
pub fn assert_unsent_pause_saved(json: &Path) {
    assert_saved_paused(json);
    let remote = read_task(json).unwrap().remote.unwrap();
    assert!(
        remote.started.is_none() && !remote.output_received && remote.job_id.is_none(),
        "{remote:?}"
    );
}

pub const RETURN_CLAIMS: &[&str] = &["exec-remote/v1", "exec-remote-return/v2"];

pub async fn return_daemon(vector: &'static str) -> crate::exec_remote::wire_fixture::Daemon {
    crate::exec_remote::wire_fixture::daemon_with_capabilities(
        Script::ReturnVector(vector),
        RETURN_CLAIMS,
        1,
    )
    .await
}

pub fn return_call(paths: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "command": "make dist", "runon": "linux", "wait": true,
        "foreground_orchestrate": true, "compressed": false,
        "return_paths": paths,
    })
}

pub fn seed_large_local_return_output(root: &Path) {
    fs::create_dir(root.join("dist")).unwrap();
    fs::File::create(root.join("dist/empty.txt"))
        .unwrap()
        .set_len(5 * 1024 * 1024 + 1)
        .unwrap();
}

pub fn restarted_context(dir: &Path) -> crate::context::AppContext {
    restarted_context_with_runon(dir, true)
}

pub(super) fn finished_remote_for_cancel(
    dir: &Path,
    connection: PathBuf,
    record: TerminalRecord,
) -> (BgTaskRegistry, String, TaskPaths) {
    let (registry, task_id, paths) =
        persist_accepted_with_snapshot(dir, connection, "printf remote");
    let task = registry.task(&task_id).unwrap();
    task.state.lock().unwrap().metadata.scope = Some(owner_scope("unknown-outcome"));
    let verdict = exec::grade(&record);
    TaskSink::new(&registry, task.clone())
        .unwrap()
        .terminal(&record, &verdict)
        .unwrap();
    registry.remote_terminal(&task, verdict, None);
    (registry, task_id, paths)
}

pub fn cancel_requests(
    daemon: &crate::exec_remote::wire_fixture::Daemon,
) -> Vec<serde_json::Value> {
    daemon
        .log
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, body)| body["method"] == "exec.cancel")
        .map(|(_, body)| body.clone())
        .collect()
}

pub fn delivered_unknown_restart_fixture(
    dir: &Path,
    connection: PathBuf,
    expired: bool,
) -> (String, TaskPaths, crate::context::AppContext) {
    let storage = dir.join("store");
    let namespace = storage.join("opencode");
    let project = dir.join("project");
    fs::create_dir_all(&namespace).unwrap();
    fs::create_dir_all(&project).unwrap();
    let (registry, task, paths) =
        finished_remote_for_cancel(&namespace, connection, cancel_unknown_terminal());
    registry.detach();
    drop(registry);
    let mut metadata = read_task(&paths.json).unwrap();
    metadata.project_root = Some(project.clone());
    metadata.workdir = project.clone();
    metadata.harness = Some("opencode".into());
    metadata.scope.as_mut().unwrap().scope_epoch = 9;
    metadata.completion_delivered = true;
    if expired {
        metadata.started_at = 0;
    }
    write_task(&paths.json, &metadata).unwrap();
    let ctx = crate::context::AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(project.clone()),
            storage_dir: Some(storage),
            harness: Some(crate::harness::Harness::Opencode),
            ..Default::default()
        },
    );
    ctx.bash_background()
        .set_harness(crate::harness::Harness::Opencode);
    ctx.bash_background()
        .replay_session_for_project(&namespace, "session", &project)
        .unwrap();
    assert!(
        !ctx.bash_background().holds_task(&task),
        "project replay must skip the delivered terminal"
    );
    (task, paths, ctx)
}

pub(super) fn persist_accepted_with_snapshot_scope(
    dir: &Path,
    connection: PathBuf,
    command: &str,
    memory_limit_mib: Option<u64>,
    linux_scope: bool,
    max_runtime_secs: u64,
) -> (BgTaskRegistry, String, TaskPaths) {
    let registry = registry();
    let layout = allocate_task_layout(dir, "session").unwrap();
    let task_id = layout.paths.task_id.clone();
    let shell = resolve_posix_shell();
    let mut environment = BTreeMap::from([(
        std::ffi::OsString::from("ORIGINAL_PLAN"),
        std::ffi::OsString::from("host-proof"),
    )]);
    if linux_scope {
        for key in ["XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"] {
            if let Some(value) = std::env::var_os(key) {
                environment.insert(key.into(), value);
            }
        }
    }
    let prepared = crate::sandbox_spawn::prepare_task_payload(
        &layout,
        command.as_bytes(),
        dir,
        dir,
        &crate::sandbox_spawn::AuthenticatedPrincipal::FirstParty,
        &shell,
        &environment,
    )
    .unwrap();
    let plan = SpawnPlan::Host {
        shell_path: shell.clone(),
        environment,
    }
    .with_prepared_task(prepared);
    let digest = crate::sandbox_spawn::save_local_launch(
        &plan,
        dir,
        dir,
        &crate::sandbox_spawn::AuthenticatedPrincipal::FirstParty,
        &shell,
        linux_scope,
        memory_limit_mib,
        max_runtime_secs,
    )
    .unwrap();
    let mut metadata = PersistedTask::starting(
        task_id.clone(),
        "session".into(),
        command.into(),
        dir.into(),
        Some(dir.into()),
        Some(30000),
        true,
        false,
    );
    metadata.status = BgTaskStatus::Running;
    metadata.execution_note = Some("ran remotely on ck-motor".into());
    metadata.remote = Some(RemoteTask {
        submission: None,
        accepted_platform: None,
        explicit_runon: false,
        connection_file: Some(connection),
        harness: "runner".into(),
        session: "session".into(),
        requested_platform: None,
        requested_vcpus: None,
        requested_network: false,
        granted_network: false,
        accepted: true,
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
        fallback_digest: Some(digest),
        env_not_forwarded: None,
        queue_position: None,
        queue_wait_ms: None,
        output_received: false,
        retry: None,
        output_incomplete: None,
        workspace_holder: None,
        return_paths: Vec::new(),
        returned_files: None,
        return_first_seq: None,
        return_staging: Default::default(),
        return_max_bytes: exec::return_max_bytes(),
        return_cap_retried: false,
        expired_gap_unverified: Vec::new(),
    });
    let handles = TaskIoHandles::create(&layout, BgMode::Pipes, true).unwrap();
    write_task_at(&layout, &metadata).unwrap();
    registry
        .insert_rehydrated_task(metadata, layout.paths.clone(), false)
        .unwrap();
    let task = registry.task(&task_id).unwrap();
    task.state.lock().unwrap().io_handles = Some(handles);
    let mut sink = TaskSink::new(&registry, task).unwrap();
    sink.accepted(&Accepted::new(id(), 1)).unwrap();
    (registry, task_id, layout.paths)
}

// Reattachment is recorded in metadata without adding agent-visible wording.
#[cfg(test)]
pub(super) const REATTACHED_RESULT_SENTENCE: Option<&str> = None;

impl Drop for UncapturedTerminalBeforeRenderForTest {
    fn drop(&mut self) {
        let _ = self.release.send(());
        if let Some(publisher) = self.publisher.take() {
            let result = publisher.join();
            if !std::thread::panicking() {
                result.unwrap();
            }
        }
    }
}

impl UncapturedTerminalBeforeRenderForTest {
    pub fn dir(&self) -> &tempfile::TempDir {
        &self.dir
    }
    pub fn registry(&self) -> &BgTaskRegistry {
        &self.registry
    }
    pub fn task(&self) -> &Arc<BgTask> {
        &self.task
    }
}
