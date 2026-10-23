use super::*;
use crate::bash_background::persistence::{read_task, write_task};
use crate::exec_remote::wire_tests::{daemon, id, Script};
fn reattach_budgets() -> &'static Mutex<HashMap<PathBuf, Duration>> {
    static BUDGETS: std::sync::OnceLock<Mutex<HashMap<PathBuf, Duration>>> =
        std::sync::OnceLock::new();
    BUDGETS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) fn take_reattach_budget(root: &Path) -> Option<Duration> {
    reattach_budgets()
        .lock()
        .unwrap()
        .remove(&test_clock_key(root))
}

fn shorten_reattach_budget(root: &Path) {
    // Scope the hook to one worker, so parallel remote tests keep the real budget.
    // Tests advance this logical budget after observing the relevant recovery
    // attempts; scheduler latency must not decide how many attempts fit.
    reattach_budgets()
        .lock()
        .unwrap()
        .insert(test_clock_key(root), Duration::from_millis(1_500));
}

pub(super) struct ReattachClock {
    pub(super) ticks: tokio::sync::watch::Sender<Duration>,
    pub(super) resets: tokio::sync::watch::Sender<u64>,
    pub(super) waits: tokio::sync::watch::Sender<u64>,
}

impl ReattachClock {
    pub(super) fn now(&self) -> Duration {
        *self.ticks.borrow()
    }
}

fn reattach_clocks() -> &'static Mutex<HashMap<PathBuf, Arc<ReattachClock>>> {
    static CLOCKS: std::sync::OnceLock<Mutex<HashMap<PathBuf, Arc<ReattachClock>>>> =
        std::sync::OnceLock::new();
    CLOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) fn reattach_clock(root: &Path) -> Option<Arc<ReattachClock>> {
    reattach_clocks()
        .lock()
        .unwrap()
        .get(&test_clock_key(root))
        .cloned()
}

// Remote workers run on separate runtimes. Advance their recovery budget only
// after a consumed record has reset it, without charging scheduler latency.
struct PausedReattachClock {
    root: PathBuf,
    clock: Arc<ReattachClock>,
}

impl PausedReattachClock {
    fn new(root: &Path) -> Self {
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

    async fn after_resets(&self, count: u64) {
        let mut resets = self.clock.resets.subscribe();
        tokio::time::timeout(
            Duration::from_secs(30),
            resets.wait_for(|resets| *resets >= count),
        )
        .await
        .expect("consumed records must reset the recovery budget")
        .unwrap();
    }

    async fn after_waits(&self, count: u64) {
        let mut waits = self.clock.waits.subscribe();
        tokio::time::timeout(
            Duration::from_secs(30),
            waits.wait_for(|waits| *waits >= count),
        )
        .await
        .expect("recovery must enter its budgeted wait")
        .unwrap();
    }

    fn advance(&self, duration: Duration) {
        self.clock.ticks.send_modify(|now| *now += duration);
    }
}

impl Drop for PausedReattachClock {
    fn drop(&mut self) {
        reattach_clocks().lock().unwrap().remove(&self.root);
        self.clock.ticks.send_replace(Duration::MAX);
    }
}

fn crash_tasks() -> &'static Mutex<HashSet<String>> {
    static TASKS: std::sync::OnceLock<Mutex<HashSet<String>>> = std::sync::OnceLock::new();
    TASKS.get_or_init(|| Mutex::new(HashSet::new()))
}
pub(super) fn simulate_crash_before_local_pid(task: &str) -> bool {
    crash_tasks().lock().unwrap().remove(task)
}

fn post_spawn_failures() -> &'static Mutex<HashSet<String>> {
    static TASKS: std::sync::OnceLock<Mutex<HashSet<String>>> = std::sync::OnceLock::new();
    TASKS.get_or_init(|| Mutex::new(HashSet::new()))
}
pub(super) fn fail_running_metadata_after_spawn(task: &str) -> Result<(), String> {
    if post_spawn_failures().lock().unwrap().remove(task) {
        Err("injected running-metadata failure after spawn".into())
    } else {
        Ok(())
    }
}

struct MarkerGate {
    entered: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}
fn marker_gates() -> &'static Mutex<HashMap<String, MarkerGate>> {
    static GATES: std::sync::OnceLock<Mutex<HashMap<String, MarkerGate>>> =
        std::sync::OnceLock::new();
    GATES.get_or_init(|| Mutex::new(HashMap::new()))
}
pub(super) fn after_local_fallback_marker(task: &str) {
    if let Some(gate) = marker_gates().lock().unwrap().remove(task) {
        gate.entered.send(()).unwrap();
        gate.release.recv_timeout(Duration::from_secs(10)).unwrap();
    }
}

fn registry() -> BgTaskRegistry {
    BgTaskRegistry::new(Arc::new(Mutex::new(None)))
}

pub(crate) fn launch(connection: PathBuf) -> crate::bash_background::RemoteLaunch {
    crate::bash_background::RemoteLaunch {
        explicit_runon: false,
        connection_file: Some(connection),
        harness: "broca".into(),
        session: "session".into(),
        requested_platform: None,
        requested_vcpus: None,
        requested_network: false,
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

fn start(registry: &BgTaskRegistry, dir: &Path, connection: PathBuf) -> String {
    start_with_hard_kill(
        registry,
        dir,
        connection,
        crate::bash_background::HardKill::After(Duration::from_secs(30)),
    )
}

fn start_with_hard_kill(
    registry: &BgTaskRegistry,
    dir: &Path,
    connection: PathBuf,
    hard_kill: crate::bash_background::HardKill,
) -> String {
    ensure_git_fixture(dir);
    registry
        .spawn_remote(
            launch(connection),
            SpawnPlan::Unsandboxed,
            "printf local-proof",
            resolve_posix_shell(),
            "session".into(),
            dir.into(),
            HashMap::from([("BUILD_REMOTE_ENV_TEST".into(), "environment-proof".into())]),
            hard_kill,
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
fn storage_report(root: &Path) -> String {
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

/// Fail unless `replay` left the task registered for `session`. Replay
/// registers every task it restores before it returns, so a missing task means
/// the record was skipped (deferred, refused or quarantined) rather than slow.
fn assert_replayed(registry: &BgTaskRegistry, task: &str, storage: &Path, replay: &str) {
    if registry.observed_status(task, "session", 0).is_none() {
        panic!(
            "{replay} did not register task {task} for session `session` (held under session {:?}); {}",
            registry.task_for_test(task).map(|held| held.session_id.clone()),
            storage_report(storage)
        );
    }
}

/// Poll until the task reaches a terminal status. A task that is not in the
/// registry is a failure, not something to wait out: the callers registered it
/// before polling, and nothing in these tests acknowledges a completion, which
/// is what retention cleanup needs before it removes a task.
async fn terminal(registry: &BgTaskRegistry, task: &str) -> BgTaskSnapshot {
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

// Poll observable task state rather than inferring progress from elapsed time.
// The timeout is a hang guard; only a successful probe releases the caller.
async fn wait_for<T>(reason: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(value) = probe() {
                return value;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{reason}"))
}

struct ReleaseOnDrop(PathBuf);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        let _ = fs::write(&self.0, b"release");
    }
}

#[tokio::test]
async fn every_local_spawn_entrypoint_executes_command_once() {
    use crate::bash_background::{BashShell, HardKill, TaskSlot};
    for entry in [
        "foreground",
        "background",
        "pty",
        "deferred-wait",
        "replay-running",
        "remote-fallback",
        "replay-fallback",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let counter = dir.path().join("executions");
        let release = dir.path().join("release");
        let _release_on_failure = ReleaseOnDrop(release.clone());
        let command = format!("printf x >> '{}'; while [ ! -f '{}' ]; do /bin/sleep 0.01; done; printf entrypoint-proof", counter.display(), release.display());
        if entry == "deferred-wait" {
            crate::subc::test_support::assert_deferred_wait_entrypoint(&command, &release).await;
            assert_eq!(
                fs::read(&counter).unwrap(),
                b"x",
                "deferred wait must run its command once"
            );
            eprintln!(
                "spawn entrypoint deferred-wait: command executed and held-lock wait completed"
            );
            continue;
        }
        let mut registry = registry();
        let mut fake = None;
        let task_id = match entry {
            "foreground" => registry
                .spawn_with_shell_in_slot(
                    SpawnPlan::Unsandboxed,
                    &command,
                    BashShell::Bash,
                    resolve_posix_shell(),
                    "session".into(),
                    dir.path().into(),
                    HashMap::new(),
                    HardKill::After(Duration::from_secs(30)),
                    dir.path().into(),
                    TaskSlot::Foreground,
                    true,
                    false,
                    Some(dir.path().into()),
                )
                .unwrap(),
            "background" | "replay-running" => registry
                .spawn(
                    SpawnPlan::Unsandboxed,
                    &command,
                    "session".into(),
                    dir.path().into(),
                    HashMap::new(),
                    HardKill::After(Duration::from_secs(30)),
                    dir.path().into(),
                    10,
                    true,
                    false,
                    Some(dir.path().into()),
                )
                .unwrap(),
            "pty" => registry
                .spawn_pty(
                    SpawnPlan::Unsandboxed,
                    &command,
                    "session".into(),
                    dir.path().into(),
                    HashMap::new(),
                    HardKill::After(Duration::from_secs(30)),
                    dir.path().into(),
                    10,
                    true,
                    false,
                    Some(dir.path().into()),
                    24,
                    80,
                )
                .unwrap(),
            "remote-fallback" => {
                ensure_git_fixture(dir.path());
                let daemon = daemon(Script::Refused, "exec-remote/v1").await;
                let id = registry
                    .spawn_remote(
                        launch(daemon.connection.clone()),
                        SpawnPlan::Unsandboxed,
                        &command,
                        resolve_posix_shell(),
                        "session".into(),
                        dir.path().into(),
                        HashMap::new(),
                        HardKill::After(Duration::from_secs(30)),
                        dir.path().into(),
                        TaskSlot::Background { max: 10 },
                        true,
                        false,
                        Some(dir.path().into()),
                    )
                    .unwrap();
                fake = Some(daemon);
                id
            }
            "replay-fallback" => {
                let daemon = daemon(Script::AttachRefused, "exec-remote/v1").await;
                let (original, id, _) =
                    persist_accepted_with_snapshot(dir.path(), daemon.connection.clone(), &command);
                drop(original);
                registry.replay_session(dir.path(), "session").unwrap();
                fake = Some(daemon);
                id
            }
            _ => unreachable!(),
        };
        wait_for("command must cross its startup boundary", || {
            fs::read(&counter).ok().filter(|bytes| bytes == b"x")
        })
        .await;
        if entry == "replay-running" {
            registry.inner.shutdown.store(true, Ordering::SeqCst);
            let replacement = super::tests::registry();
            replacement.replay_session(dir.path(), "session").unwrap();
            registry = replacement;
        }
        fs::write(&release, b"release").unwrap();
        let done = terminal(&registry, &task_id).await;
        assert_eq!(
            done.info.status,
            BgTaskStatus::Completed,
            "{entry}: {done:?}"
        );
        assert_eq!(
            fs::read(&counter).unwrap(),
            b"x",
            "{entry}: duplicate execution"
        );
        let replay = super::tests::registry();
        replay.replay_session(dir.path(), "session").unwrap();
        assert_eq!(
            terminal(&replay, &task_id).await.info.status,
            BgTaskStatus::Completed,
            "{entry}: terminal replay"
        );
        assert_eq!(
            fs::read(&counter).unwrap(),
            b"x",
            "{entry}: replay ran command again"
        );
        eprintln!("spawn entrypoint {entry}: command executed once and replay completed");
        registry.inner.shutdown.store(true, Ordering::SeqCst);
        replay.inner.shutdown.store(true, Ordering::SeqCst);
        drop(fake);
    }
}

#[tokio::test]
async fn exec_remote_bash_background_slots_are_shared_with_local_tasks() {
    use crate::bash_background::{HardKill, TaskSlot};

    let dir = tempfile::tempdir().unwrap();
    ensure_git_fixture(dir.path());
    let registry = registry();
    let local = registry
        .spawn_with_shell(
            SpawnPlan::Unsandboxed,
            "sleep 30 # local-slot-holder",
            crate::bash_background::BashShell::Bash,
            resolve_posix_shell(),
            "session".into(),
            dir.path().into(),
            HashMap::new(),
            HardKill::After(Duration::from_secs(60)),
            dir.path().into(),
            1,
            false,
            false,
            Some(dir.path().into()),
        )
        .unwrap();
    let daemon = daemon(Script::Cancel, "exec-remote/v1").await;
    let remote_spawn = |slot| {
        let mut launch = launch(daemon.connection.clone());
        launch.explicit_runon = true;
        registry.spawn_remote(
            launch,
            SpawnPlan::Unsandboxed,
            "printf remote-slot-proof",
            resolve_posix_shell(),
            "session".into(),
            dir.path().into(),
            HashMap::new(),
            HardKill::After(Duration::from_secs(60)),
            dir.path().into(),
            slot,
            false,
            false,
            Some(dir.path().into()),
        )
    };
    // Remote background work uses the same project-local budget, whereas a
    // remote foreground call is admitted even when local work fills it.
    let refusal = remote_spawn(TaskSlot::Background { max: 1 }).unwrap_err();
    assert!(refusal.contains(&local), "{refusal}");
    assert!(refusal.contains("local-slot-holder"), "{refusal}");
    let foreground = remote_spawn(TaskSlot::Foreground).unwrap();
    // The fake executor runs on this thread's Tokio runtime. Keep it serving
    // the foreground worker's handshake while the local process group exits.
    let (kill_reached, release_kill) = install_kill_signal_gate_for_test(&local);
    let killing = registry.clone();
    let local_kill = tokio::task::spawn_blocking(move || killing.kill(&local, "session").unwrap());
    tokio::task::spawn_blocking(move || {
        kill_reached
            .recv_timeout(Duration::from_secs(30))
            .expect("local kill reached signal checkpoint");
    })
    .await
    .unwrap();
    wait_for(
        "fake executor must accept the foreground command while local kill is held",
        || {
            registry
                .task(&foreground)
                .unwrap()
                .state
                .lock()
                .unwrap()
                .metadata
                .remote
                .as_ref()
                .unwrap()
                .job_id
                .map(|_| ())
        },
    )
    .await;
    release_kill.send(()).unwrap();
    local_kill.await.unwrap();
    registry
        .check_background_slot(TaskSlot::Background { max: 1 }, "session")
        .expect("a waiting remote foreground command holds no slot");
    registry.promote(&foreground, "session").unwrap();
    let refusal = registry
        .check_background_slot(TaskSlot::Background { max: 1 }, "session")
        .expect_err(&format!(
            "promoted task holds no slot: {:?}",
            registry.task(&foreground).map(|task| task.snapshot(0))
        ));
    assert!(refusal.contains(&foreground), "{refusal}");
    assert!(refusal.contains("remote-slot-proof"), "{refusal}");
    // Keep the runtime free to serve the fake executor's cancel response too.
    let killing = registry.clone();
    let kill_task = foreground.clone();
    tokio::task::spawn_blocking(move || killing.kill(&kill_task, "session").unwrap())
        .await
        .unwrap();
    let done = terminal(&registry, &foreground).await;
    assert_eq!(done.info.status, BgTaskStatus::Killed);
}

#[tokio::test]
async fn exec_remote_bash_refusal_runs_local_with_disclosure() {
    for (script, reason) in [
        (Script::Refused, "future_refusal"),
        (Script::KnownRefused, "unreachable"),
        (Script::WorkspaceSetupRefused, "workspace_setup_failed"),
    ] {
        let daemon = daemon(script, "exec-remote/v1").await;
        let dir = tempfile::tempdir().unwrap();
        let registry = registry();
        let task = start(&registry, dir.path(), daemon.connection.clone());
        let done = terminal(&registry, &task).await;
        assert_eq!(done.exit_code, Some(0));
        assert!(
            done.output_preview.contains("local-proof"),
            "{}",
            done.output_preview
        );
        assert_eq!(
            done.output_preview.lines().next(),
            Some(
                format!(
                    "ran locally on {}: remote refused: {reason}",
                    local_os_name()
                )
                .as_str()
            ),
            "{}",
            done.output_preview
        );
        let log = daemon.log.lock().unwrap();
        let request = log.iter().find(|(_, b)| b["method"] == "exec.run").unwrap();
        assert_eq!(
            request.1["params"]["env"]["BUILD_REMOTE_ENV_TEST"],
            "environment-proof"
        );
        assert_eq!(request.1["params"]["cwd"], dir.path().display().to_string());
    }
}

fn refusal_response(snapshot: BgTaskSnapshot) -> crate::protocol::Response {
    let now = std::time::Instant::now();
    match crate::commands::bash_orchestrate::decide_bash_step(
        snapshot, now, true, false, now, "runon",
    ) {
        crate::commands::bash_orchestrate::BashStep::Done(response) => response,
        _ => panic!("a terminal refusal must finish the foreground call"),
    }
}

fn assert_runon_refusal(response: &crate::protocol::Response, reason: &str) {
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

#[test]
fn refusal_detail_is_sanitized_and_truncated_at_utf8_boundary() {
    let detail = format!("first\nsecond\t{}", "é".repeat(510));
    let rendered = rendered_refusal_reason("runner_draining", Some(&detail));
    assert_eq!(
        rendered,
        format!("runner_draining (first second {})", "é".repeat(505))
    );
    let rendered_detail = rendered
        .strip_prefix("runner_draining (")
        .unwrap()
        .strip_suffix(')')
        .unwrap();
    assert_eq!(rendered_detail.len(), 1023);
    assert!(!rendered_detail.chars().any(char::is_control));
}

#[test]
fn absent_refusal_detail_keeps_the_existing_message_exactly() {
    assert_eq!(
        runon_refusal_message("unreachable", None),
        "runon refused: remote refused: unreachable; command was not run; retry, or omit runon to run locally"
    );
}

#[tokio::test]
async fn runon_executor_refusal_never_spawns_locally_and_returns_error() {
    for (script, reason) in [
        (Script::KnownRefused, "unreachable"),
        (Script::WorkspaceSetupRefused, "workspace_setup_failed"),
        (Script::Refused, "future_refusal"),
    ] {
        let daemon = daemon(script, "exec-remote/v1").await;
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("must-not-run-locally");
        let ctx = remote_test_context(dir.path());
        let response = handle_with_policy(
            &ctx,
            Some(launch(daemon.connection.clone())),
            serde_json::json!({
                "command": format!("printf local-proof > '{}'", marker.display()),
                "runon": "linux", "compressed": false
            }),
        );
        assert!(
            response.success,
            "the asynchronous launch returns a task: {response:?}"
        );
        let task_id = response.data["task_id"].as_str().unwrap();
        let done = terminal(ctx.bash_background(), task_id).await;
        assert!(
            !marker.exists(),
            "explicit runon spawned locally after {reason}"
        );
        assert_eq!(done.child_pid, None);
        assert_eq!(done.info.status, BgTaskStatus::Failed);
        let task = ctx.bash_background().task(task_id).unwrap();
        assert!(!task.state.lock().unwrap().metadata.local_fallback_started);
        assert_runon_refusal(&refusal_response(done), reason);
        let status = crate::commands::bash_status::handle(
            &crate::protocol::RawRequest {
                id: "runon-status".into(),
                command: "bash_status".into(),
                session_id: Some("session".into()),
                lsp_hints: None,
                params: serde_json::json!({"task_id": task_id}),
            },
            &ctx,
        );
        assert_runon_refusal(&status, reason);
        assert_eq!(status.data["remote_refusal"]["code"], "remote_unavailable");
        assert_eq!(exec_runs(&daemon).len(), 1);
    }
}

#[tokio::test]
async fn runon_refusal_detail_is_rendered_and_sanitized() {
    let daemon = daemon(Script::RefusedWithDetail, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    shorten_draining_budget(&std::fs::canonicalize(dir.path()).unwrap(), 100);
    let marker = dir.path().join("must-not-run-locally");
    let ctx = restarted_context(dir.path());
    let response = handle_with_policy(
        &ctx,
        Some(launch(daemon.connection.clone())),
        serde_json::json!({
            "command": format!("printf local-proof > '{}'", marker.display()),
            "runon": "linux", "compressed": false
        }),
    );
    assert!(
        response.success,
        "the asynchronous launch returns a task: {response:?}"
    );
    let task_id = response.data["task_id"].as_str().unwrap();
    let done = terminal(ctx.bash_background(), task_id).await;
    assert!(!marker.exists(), "explicit runon must not start locally");
    let response = refusal_response(done);
    let message = response.data["message"].as_str().unwrap();
    assert!(message.starts_with("runon refused: remote refused: runner_draining (runner says: maintenance window) after retrying for "), "{message}");
    assert!(
        message.ends_with("; command was not run; retry, or omit runon to run locally; remote job 0192a64a-1234-7000-8000-000000000001"),
        "{message}"
    );
    assert!(
        message.contains("remote job 0192a64a-1234-7000-8000-000000000001"),
        "{message}"
    );
    assert!(!message.contains(['\n', '\t']), "{message}");
}

#[tokio::test]
async fn local_fallback_refusal_detail_is_rendered_and_sanitized() {
    let daemon = daemon(Script::RefusedWithDetail, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("local-fallback");
    let ctx = restarted_context(dir.path());
    let mut policy = launch(daemon.connection.clone());
    policy.params.remote_exec.as_mut().unwrap().legacy_commands =
        Some(serde_json::json!(["touch"]));
    let response = handle_with_policy(
        &ctx,
        Some(policy),
        serde_json::json!({
            "command": format!("touch '{}'", marker.display()),
            "compressed": false
        }),
    );
    assert!(response.success, "{response:?}");
    let done = terminal(
        ctx.bash_background(),
        response.data["task_id"].as_str().unwrap(),
    )
    .await;
    assert!(marker.exists(), "implicit routing still runs locally");
    assert!(
        done.output_preview.contains(&format!(
            "ran locally on {}: remote refused: runner_draining (runner says: maintenance window)",
            local_os_name()
        )),
        "{}",
        done.output_preview
    );
}

#[tokio::test]
async fn prefix_routing_executor_refusal_still_spawns_locally_with_advisory() {
    for (script, reason) in [
        (Script::KnownRefused, "unreachable"),
        (Script::WorkspaceSetupRefused, "workspace_setup_failed"),
        (Script::Refused, "future_refusal"),
    ] {
        let daemon = daemon(script, "exec-remote/v1").await;
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("local-fallback");
        let ctx = remote_test_context(dir.path());
        let mut policy = launch(daemon.connection.clone());
        policy.params.remote_exec.as_mut().unwrap().legacy_commands =
            Some(serde_json::json!(["touch"]));
        let response = handle_with_policy(
            &ctx,
            Some(policy),
            serde_json::json!({
                "command": format!("touch '{}'", marker.display()),
                "compressed": false
            }),
        );
        assert!(response.success, "{response:?}");
        let done = terminal(
            ctx.bash_background(),
            response.data["task_id"].as_str().unwrap(),
        )
        .await;
        assert!(
            marker.exists(),
            "prefix routing must run locally after {reason}"
        );
        assert_eq!(done.exit_code, Some(0));
        assert_eq!(
            exec_runs(&daemon).len(),
            1,
            "the prefix must actually route remotely"
        );
        let status_request = crate::protocol::RawRequest {
            id: "prefix-status".into(),
            command: "bash_status".into(),
            session_id: Some("session".into()),
            lsp_hints: None,
            params: serde_json::json!({"task_id": response.data["task_id"]}),
        };
        let status = crate::commands::bash_status::handle(&status_request, &ctx);
        assert!(status.success, "{status:?}");
        assert!(
            status.data["output_preview"]
                .as_str()
                .unwrap_or_default()
                .contains(&format!(
                    "ran locally on {}: remote refused: {reason}",
                    local_os_name()
                )),
            "tool status: {status:?}; execution note: {:?}",
            ctx.bash_background()
                .execution_note(response.data["task_id"].as_str().unwrap(), "session")
        );
        assert!(refusal_response(done).success);
        assert_eq!(exec_runs(&daemon).len(), 1);
    }
}

#[tokio::test]
async fn exec_remote_bash_unknown_reply_names_job_and_safe_rerun() {
    let daemon = daemon(Script::Lost, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let task = start(&registry, dir.path(), daemon.connection.clone());
    let done = terminal(&registry, &task).await;
    let expected = format!("remote outcome unknown (job {}); the remote job could not affect this machine or the network, so rerunning is safe; job {} may still be running on ck-motor and holding this workspace for up to its run timeout of 30s, and AFT can no longer fetch its result: a rerun in this workspace waits behind it and can be refused with queue_wait_exceeded, so wait for it to end, then rerun", id(), id());
    assert_eq!(done.info.status_reason.as_deref(), Some(expected.as_str()));
    assert!(
        done.output_preview.contains(&expected),
        "{}",
        done.output_preview
    );
    assert!(!done.output_preview.contains("never rerun"));
}

#[test]
fn exec_remote_bash_unknown_reply_without_job_omits_status_hint() {
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, _) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf unused");
    let task = registry.task(&task_id).unwrap();
    {
        let mut state = task.state.lock().unwrap();
        state.metadata.remote.as_mut().unwrap().job_id = None;
        state.metadata.execution_note = Some("ran remotely on ck-motor".into());
    }
    registry.remote_terminal(
        &task,
        Verdict::OutcomeUnknown,
        Some("remote route interrupted before acceptance".into()),
    );
    let done = registry.observed_status(&task_id, "session", 8192).unwrap();
    let expected = "remote outcome unknown; the remote job could not affect this machine or the network, so rerunning is safe";
    assert_eq!(done.info.status_reason.as_deref(), Some(expected));
    assert!(
        done.output_preview.contains(expected),
        "{}",
        done.output_preview
    );
    assert!(!done.output_preview.contains("exec.status"));
    assert!(!done.output_preview.contains("(job"));
    assert_eq!(
        done.output_preview.lines().next(),
        Some("ran remotely on ck-motor")
    );
    assert!(!done.output_preview.contains("never rerun"));
    assert!(done
        .output_preview
        .contains("remote route interrupted before acceptance"));
    let completion = registry.completion_snapshot_for_task(&task).unwrap();
    assert_eq!(
        completion.output_preview.lines().next(),
        Some("ran remotely on ck-motor")
    );
}

fn remote_terminal_preview_with_job(
    verdict: Verdict,
    accepted_refusal: bool,
) -> (String, Option<RemoteRefusal>) {
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, _) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf unused");
    let task = registry.task(&task_id).unwrap();
    {
        let mut state = task.state.lock().unwrap();
        let remote = state.metadata.remote.as_mut().unwrap();
        remote.explicit_runon = true;
        remote.job_id = Some(id());
        remote.requested_vcpus = Some(4);
        if accepted_refusal {
            remote.terminal = Some(TerminalRecord::new(
                id(),
                Outcome::RefusedBeforeStart {
                    reason: RefusalReason::Unreachable,
                },
                0,
                0,
                0,
            ));
        }
    }
    registry.remote_terminal(&task, verdict, None);
    let snapshot = registry.observed_status(&task_id, "session", 8192).unwrap();
    let expected_header = if accepted_refusal {
        "remote executor refused before start (job 0192a64a-1234-7000-8000-000000000001)"
    } else {
        EXPECTED_REMOTE_JOB_HEADER
    };
    assert!(
        snapshot.output_preview.starts_with(expected_header),
        "{}",
        snapshot.output_preview
    );
    let foreground = crate::commands::bash_orchestrate::format_foreground_result(&snapshot);
    assert!(foreground.contains(expected_header), "{foreground}");
    let completion = registry.completion_snapshot_for_task(&task).unwrap();
    assert!(
        completion.output_preview.starts_with(expected_header),
        "{}",
        completion.output_preview
    );
    (snapshot.output_preview, snapshot.remote_refusal)
}

const EXPECTED_REMOTE_JOB_HEADER: &str =
    "ran remotely on ck-motor (4 vCPUs requested, job 0192a64a-1234-7000-8000-000000000001)";

#[test]
fn remote_success_result_names_the_full_job_id() {
    let (output, _) = remote_terminal_preview_with_job(Verdict::Exited { code: 0 }, false);
    assert_eq!(output.lines().next(), Some(EXPECTED_REMOTE_JOB_HEADER));
}

#[test]
fn remote_nonzero_exit_result_names_the_full_job_id() {
    let (output, _) = remote_terminal_preview_with_job(Verdict::Exited { code: 7 }, false);
    assert_eq!(output.lines().next(), Some(EXPECTED_REMOTE_JOB_HEADER));
}

#[test]
fn remote_refusal_after_acceptance_names_the_full_job_id() {
    let (output, refusal) = remote_terminal_preview_with_job(
        Verdict::RunLocally {
            reason: RefusalReason::Unreachable,
            refusal_detail: None,
        },
        true,
    );
    assert_eq!(
        output.lines().next(),
        Some("remote executor refused before start (job 0192a64a-1234-7000-8000-000000000001)")
    );
    assert!(refusal
        .unwrap()
        .message
        .contains("remote job 0192a64a-1234-7000-8000-000000000001"));
}

#[test]
fn remote_timeout_result_names_the_full_job_id() {
    let (output, _) = remote_terminal_preview_with_job(Verdict::DeadlineKilled, false);
    assert_eq!(output.lines().next(), Some(EXPECTED_REMOTE_JOB_HEADER));
}

#[test]
fn remote_cancel_result_names_the_full_job_id() {
    let (output, _) = remote_terminal_preview_with_job(Verdict::Cancelled, false);
    assert_eq!(output.lines().next(), Some(EXPECTED_REMOTE_JOB_HEADER));
}

#[test]
fn remote_kill_result_names_the_full_job_id() {
    let (output, _) = remote_terminal_preview_with_job(Verdict::CancelKilled, false);
    assert_eq!(output.lines().next(), Some(EXPECTED_REMOTE_JOB_HEADER));
}

#[test]
fn remote_execution_header_without_a_job_id_keeps_its_existing_text() {
    let mut remote = RemoteTask {
        accepted_platform: None,
        explicit_runon: false,
        connection_file: None,
        harness: "runner".into(),
        session: "session".into(),
        requested_platform: None,
        requested_vcpus: Some(4),
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
        terminal: None,
        fallback_digest: None,
        env_not_forwarded: None,
        queue_position: None,
        output_received: false,
        retry: None,
        output_incomplete: None,
        workspace_holder: None,
        return_paths: Vec::new(),
        returned_files: None,
        return_first_seq: None,
        return_staging: Default::default(),
    };
    assert_eq!(
        remote_execution_note(&remote),
        "ran remotely on ck-motor (4 vCPUs requested)"
    );
    remote.requested_vcpus = None;
    assert_eq!(remote_execution_note(&remote), "ran remotely on ck-motor");
}

/// Promotion, wait-mode detach, remote hand-back, module-drain detach and the
/// module loop's held-call answer all pass their live-task reply through
/// `with_runon_capture_state`. Before the runner's `Accepted`, a live task
/// reads `starting`; once the task has ended (here: outcome unknown after the
/// connection dropped), the reply must carry that saved outcome and its rerun
/// advice instead.
#[test]
fn runon_uncaptured_reply_reports_terminal_outcome_not_starting() {
    use crate::commands::bash_orchestrate::{
        with_runon_capture_state, REMOTE_INPUT_CAPTURE_WARNING,
    };
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
    let live_reply = || {
        crate::protocol::Response::success(
            "promote",
            serde_json::json!({
                "output": "moved to the background",
                "task_id": task_id,
                "status": "running",
            }),
        )
    };
    let pending = with_runon_capture_state(&registry, &task_id, "session", live_reply());
    assert_eq!(pending.data["status"], "starting", "{pending:?}");
    assert_eq!(pending.data["phase"], "submitting", "{pending:?}");

    registry.remote_terminal(
        &task,
        Verdict::OutcomeUnknown,
        Some("remote route interrupted before acceptance".into()),
    );
    let ended = with_runon_capture_state(&registry, &task_id, "session", live_reply());
    assert_eq!(ended.data["status"], "fate_unknown", "{ended:?}");
    assert!(ended.data.get("phase").is_none(), "{ended:?}");
    assert_eq!(ended.data["task_id"], task_id.as_str());
    let output = ended.data["output"].as_str().unwrap();
    assert!(
        output.contains("remote outcome unknown; the remote job could not affect this machine or the network, so rerunning is safe"),
        "{output}"
    );
    assert!(!output.contains("moved to the background"), "{output}");
    // The local task is terminal (the remote outcome is unknown), so this
    // reply will never move to queued or running: the in-flight capture
    // warning would be a false instruction here.
    assert!(!output.contains(REMOTE_INPUT_CAPTURE_WARNING), "{output}");
}

#[test]
fn runon_network_lost_acceptance_does_not_claim_rerun_safe() {
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, _) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf unused");
    let task = registry.task(&task_id).unwrap();
    {
        let mut state = task.state.lock().unwrap();
        let remote = state.metadata.remote.as_mut().unwrap();
        remote.job_id = None;
        remote.requested_network = true;
        remote.accepted = false;
    }
    registry.remote_terminal(
        &task,
        Verdict::OutcomeUnknown,
        Some("remote acceptance lost".into()),
    );
    let done = registry.observed_status(&task_id, "session", 8192).unwrap();
    assert!(
        done.output_preview.contains("check before rerunning"),
        "{done:?}"
    );
    assert!(
        !done.output_preview.contains("rerunning is safe"),
        "{done:?}"
    );
}

#[test]
fn exec_remote_bash_local_unknown_reply_is_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, _) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf unused");
    let task = registry.task(&task_id).unwrap();
    task.state.lock().unwrap().metadata.remote = None;
    let expected = "the command started locally; AFT lost track of it: injected running-metadata failure after spawn; never rerun";
    registry.remote_terminal(&task, Verdict::OutcomeUnknown, Some(expected.into()));
    let done = registry.observed_status(&task_id, "session", 8192).unwrap();
    assert_eq!(done.info.status_reason.as_deref(), Some(expected));
    assert!(done.output_preview.contains(expected));
    assert!(!done.output_preview.contains("rerunning is safe"));
}

#[tokio::test]
async fn exec_remote_bash_unknown_never_reruns() {
    for script in [Script::Lost, Script::FutureOutcome, Script::Expired] {
        let daemon = daemon(script, "exec-remote/v1").await;
        let dir = tempfile::tempdir().unwrap();
        let registry = registry();
        let task = start(&registry, dir.path(), daemon.connection.clone());
        let done = terminal(&registry, &task).await;
        assert_eq!(done.info.status, BgTaskStatus::FateUnknown);
        assert!(
            !done.output_preview.contains("local-proof"),
            "unknown must not run locally"
        );
        assert!(
            done.output_preview.contains("unknown")
                || done.output_preview.contains("history_expired")
        );
        assert_eq!(
            daemon
                .log
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, b)| b["method"] == "exec.run")
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn exec_remote_bash_loss_reattaches_without_resubmission() {
    let daemon = daemon(Script::Restart, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let task = start(&registry, dir.path(), daemon.connection.clone());
    let done = terminal(&registry, &task).await;
    assert!(done.output_preview.contains("ABCD"));
    let log = daemon.log.lock().unwrap();
    assert_eq!(
        log.iter()
            .filter(|(_, b)| b["method"] == "exec.run")
            .count(),
        1
    );
    assert_eq!(
        log.iter()
            .find(|(_, b)| b["method"] == "exec.attach")
            .unwrap()
            .1["params"]["from_seq"],
        2
    );
}

#[tokio::test]
async fn exec_remote_bash_empty_attach_budget_reports_job_without_rerun() {
    let daemon = daemon(Script::MissingTerminal, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    shorten_reattach_budget(dir.path());
    let clock = PausedReattachClock::new(dir.path());
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    daemon.wait_for_requests("exec.attach", 2).await;
    clock.advance(Duration::from_millis(1_500));
    let done = tokio::time::timeout(Duration::from_secs(30), terminal(&registry, &task_id))
        .await
        .expect("empty attaches must exhaust the recovery budget");
    assert_eq!(done.info.status, BgTaskStatus::FateUnknown);
    assert_eq!(
        done.info.status_reason.as_deref(),
        Some(format!("remote outcome unknown (job {}); the remote job could not affect this machine or the network, so rerunning is safe; job {} may still be running on ck-motor and holding this workspace for up to its run timeout of 30s, and AFT can no longer fetch its result: a rerun in this workspace waits behind it and can be refused with queue_wait_exceeded, so wait for it to end, then rerun", id(), id()).as_str())
    );
    assert!(done.output_preview.contains(&format!(
        "job {} could not be re-attached before its own time limit passed",
        id()
    )));
    assert!(!done.output_preview.contains("local-proof"));
    let task = registry.task(&task_id).unwrap();
    let durable = crate::bash_background::persistence::read_task(&task.paths.json).unwrap();
    assert_eq!(durable.status, BgTaskStatus::FateUnknown);
    assert_eq!(durable.status_reason, done.info.status_reason);
    let log = daemon.log.lock().unwrap();
    assert!(
        log.iter()
            .filter(|(_, b)| b["method"] == "exec.attach")
            .count()
            >= 2
    );
    assert_eq!(
        log.iter()
            .filter(|(_, b)| b["method"] == "exec.run")
            .count(),
        1
    );
}

#[tokio::test]
async fn exec_remote_bash_gapped_attach_records_reset_budget_and_complete() {
    let daemon = daemon(Script::GappedAttach, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    shorten_reattach_budget(dir.path());
    let clock = PausedReattachClock::new(dir.path());
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    let task = registry.task(&task_id).unwrap();
    for seq in 0..3 {
        daemon
            .wait_for_requests("exec.attach", seq as usize + 1)
            .await;
        wait_for(
            "attach output must be consumed before releasing its end",
            // Output is visible before the coalesced replay cursor becomes durable.
            || (fs::metadata(&task.paths.stdout).unwrap().len() == seq + 1).then_some(()),
        )
        .await;
        // Accepted and the initial stream end each reset once, then each
        // attach contributes an output record and (except the last) an end.
        clock.after_resets(3 + seq * 2).await;
        clock.advance(Duration::from_millis(1_501));
        assert_eq!(
            registry
                .observed_status(&task_id, "session", 8192)
                .unwrap()
                .info
                .status,
            BgTaskStatus::Running,
            "an open attach is not subject to an output-idle deadline"
        );
        daemon.completion_gate.notify_one();
    }
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Completed);
    assert_eq!(fs::read(&task.paths.stdout).unwrap(), b"ABC");
    let log = daemon.log.lock().unwrap();
    let from: Vec<_> = log
        .iter()
        .filter(|(_, b)| b["method"] == "exec.attach")
        .map(|(_, b)| b["params"]["from_seq"].as_u64().unwrap())
        .collect();
    assert_eq!(from, [0, 1, 2]);
    assert_eq!(
        log.iter()
            .filter(|(_, b)| b["method"] == "exec.run")
            .count(),
        1
    );
}

#[tokio::test]
async fn exec_remote_bash_persistent_terminal_gap_is_bounded() {
    let daemon = daemon(Script::PersistentTerminalGap, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Failed);
    assert_eq!(done.exit_code, Some(7));
    let task = registry.task(&task_id).unwrap();
    assert_eq!(fs::read(&task.paths.stdout).unwrap(), b"BC");
    let durable = read_task(&task.paths.json).unwrap();
    assert_eq!(durable.incomplete_output, [(0, 0)]);
    assert!(durable
        .execution_note
        .unwrap()
        .contains("output lost between seq 0 and 0: the executor never delivered it"));
    let log = daemon.log.lock().unwrap();
    let from: Vec<_> = log
        .iter()
        .filter(|(_, b)| b["method"] == "exec.attach")
        .map(|(_, b)| b["params"]["from_seq"].as_u64().unwrap())
        .collect();
    assert_eq!(from, [0, 0, 0]);
    assert_eq!(
        log.iter()
            .filter(|(_, b)| b["method"] == "exec.run")
            .count(),
        1
    );
}

#[tokio::test]
async fn exec_remote_bash_transient_terminal_gap_recovers_without_loss() {
    let daemon = daemon(Script::TransientTerminalGap, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Failed);
    assert_eq!(done.exit_code, Some(7));
    let task = registry.task(&task_id).unwrap();
    assert_eq!(fs::read(&task.paths.stdout).unwrap(), b"ABC");
    let durable = read_task(&task.paths.json).unwrap();
    assert!(durable.incomplete_output.is_empty());
    assert!(!durable
        .execution_note
        .unwrap_or_default()
        .contains("output lost"));
    let log = daemon.log.lock().unwrap();
    assert_eq!(
        log.iter()
            .filter(|(_, b)| b["method"] == "exec.attach")
            .count(),
        1
    );
    assert_eq!(
        log.iter()
            .filter(|(_, b)| b["method"] == "exec.run")
            .count(),
        1
    );
}

#[tokio::test]
async fn exec_remote_bash_reattach_budget_cancel_observes_real_terminal() {
    let daemon = daemon(Script::GappedCancel, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    shorten_reattach_budget(dir.path());
    let _clock = PausedReattachClock::new(dir.path());
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    let task = registry.task(&task_id).unwrap();
    wait_for(
        "acceptance must be consumed before recording cancellation",
        || {
            task.state
                .lock()
                .unwrap()
                .metadata
                .remote
                .as_ref()
                .unwrap()
                .job_id
                .map(|_| ())
        },
    )
    .await;
    assert!(registry.record_remote_cancel(&task).unwrap());
    daemon.wait_for_requests("exec.cancel", 1).await;
    daemon.wait_for_requests("exec.attach", 1).await;
    daemon.completion_gate.notify_one();
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Killed);
    let state = task.state.lock().unwrap();
    let terminal = state
        .metadata
        .remote
        .as_ref()
        .unwrap()
        .terminal
        .as_ref()
        .unwrap();
    assert_eq!(terminal.outcome, Outcome::Signal { signal: 15 });
    assert_eq!(terminal.killed, Some(Killed::Cancel));
    drop(state);
    let log = daemon.log.lock().unwrap();
    let cancel = log
        .iter()
        .position(|(_, b)| b["method"] == "exec.cancel")
        .unwrap();
    let attach = log
        .iter()
        .position(|(_, b)| b["method"] == "exec.attach")
        .unwrap();
    assert!(
        cancel < attach,
        "cancel intent must be sent before attaching for its terminal"
    );
    assert_eq!(
        log.iter()
            .filter(|(_, b)| b["method"] == "exec.run")
            .count(),
        1
    );
}

#[tokio::test]
async fn exec_remote_bash_attach_call_and_reconnect_failures_exhaust_budget() {
    let daemon = daemon(Script::AttachDisconnected, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    shorten_reattach_budget(dir.path());
    let clock = PausedReattachClock::new(dir.path());
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    daemon.wait_for_requests("exec.attach", 1).await;
    clock.advance(Duration::from_millis(1_500));
    let done = tokio::time::timeout(Duration::from_secs(30), terminal(&registry, &task_id))
        .await
        .expect("failed attach calls and reconnects must exhaust the recovery budget");
    assert_eq!(done.info.status, BgTaskStatus::FateUnknown);
    assert!(done
        .info
        .status_reason
        .unwrap()
        .contains("rerunning is safe"));
    assert!(done.output_preview.contains(&format!(
        "job {} could not be re-attached before its own time limit passed (",
        id()
    )));
    assert!(!done.output_preview.contains("local-proof"));
    let log = daemon.log.lock().unwrap();
    assert!(log.iter().any(|(_, b)| b["method"] == "exec.attach"));
    assert_eq!(
        log.iter()
            .filter(|(_, b)| b["method"] == "exec.run")
            .count(),
        1
    );
}

#[tokio::test]
async fn exec_remote_bash_restart_connect_failures_exhaust_reattach_budget() {
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, paths) = persist_accepted_with_snapshot(
        dir.path(),
        dir.path().join("missing-connection.json"),
        "printf local-proof",
    );
    shorten_reattach_budget(dir.path());
    let clock = PausedReattachClock::new(dir.path());
    registry.resume_remote_task(&task_id).unwrap();
    clock.after_waits(1).await;
    clock.advance(Duration::from_millis(1_500));
    let done = tokio::time::timeout(Duration::from_secs(30), terminal(&registry, &task_id))
        .await
        .expect("restarting an accepted job cannot retry connect forever");
    assert_eq!(done.info.status, BgTaskStatus::FateUnknown);
    assert!(done
        .info
        .status_reason
        .unwrap()
        .contains("rerunning is safe"));
    assert!(done.output_preview.contains(&format!(
        "job {} could not be re-attached before its own time limit passed (",
        id()
    )));
    assert_eq!(fs::read(&paths.stdout).unwrap(), b"");
}

/// ck-motor restarted mid-job and its provider stayed away longer than the
/// five minutes AFT used to allow, while the job kept running on the build
/// server. AFT must keep re-attaching (never resubmitting) and deliver the
/// job's real result once the runner is back.
#[tokio::test]
async fn exec_remote_bash_route_outage_past_five_minutes_still_delivers_result() {
    let daemon = crate::exec_remote::wire_tests::outage_daemon().await;
    let dir = tempfile::tempdir().unwrap();
    let clock = PausedReattachClock::new(dir.path());
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    daemon.after_refused_connects(3).await;
    // Past the old five-minute cap, short of this job's own limits (a 5m queue
    // wait limit plus a 30s run timeout, plus 5m of grace).
    clock.advance(Duration::from_secs(6 * 60));
    let refused = *daemon.refused_connects.borrow();
    let still_trying = wait_for("AFT must keep reconnecting during the outage", || {
        let snapshot = registry.observed_status(&task_id, "session", 8192).unwrap();
        if snapshot.info.status.is_terminal() {
            return Some(Err(snapshot));
        }
        (*daemon.refused_connects.borrow() >= refused + 3).then_some(Ok(()))
    })
    .await;
    if let Err(snapshot) = still_trying {
        panic!(
            "AFT gave up six minutes into the outage: {:?}: {}",
            snapshot.info.status, snapshot.output_preview
        );
    }
    daemon.restore();
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");
    assert_eq!(done.exit_code, Some(0));
    let task = registry.task(&task_id).unwrap();
    assert_eq!(fs::read(&task.paths.stdout).unwrap(), b"AB");
    assert_eq!(daemon.methods("exec.run").len(), 1, "never resubmitted");
    let attaches = daemon.methods("exec.attach");
    assert_eq!(attaches.last().unwrap()["params"]["from_seq"], 2);
    drop(clock);
}

/// The common agent call: `runon` with no `timeout`. It gets the 30-minute
/// background default as its run timeout, which AFT records and sends to the
/// runner, so a restart outage far past five minutes must still end with the
/// job's result.
#[tokio::test]
async fn exec_remote_bash_untimed_runon_survives_an_outage_past_five_minutes() {
    let daemon = crate::exec_remote::wire_tests::outage_daemon().await;
    let dir = tempfile::tempdir().unwrap();
    let clock = PausedReattachClock::new(dir.path());
    let registry = registry();
    // `bash` without `timeout` builds its hard kill exactly like this.
    let task_id = start_with_hard_kill(
        &registry,
        dir.path(),
        daemon.connection.clone(),
        crate::bash_background::HardKill::from_timeout_ms(None),
    );
    let task = registry.task(&task_id).unwrap();
    // Recorded no lower than the 30-minute default that was sent: inserting
    // the record can raise a default kill to one worker wait from now.
    let recorded = task.state.lock().unwrap().metadata.timeout_ms;
    assert!(
        recorded.is_some_and(|ms| ms >= 30 * 60 * 1000),
        "an untimed runon records at least the background default as its run timeout: {recorded:?}"
    );
    daemon.after_refused_connects(3).await;
    assert_eq!(
        daemon.methods("exec.run")[0]["params"]["timeout"],
        30 * 60,
        "the runner enforces the same limit AFT records"
    );
    // Far past the old five-minute cap, short of 5m queue + 30m run + 5m grace.
    clock.advance(Duration::from_secs(25 * 60));
    let refused = *daemon.refused_connects.borrow();
    let still_trying = wait_for("AFT must keep reconnecting during the outage", || {
        let snapshot = registry.observed_status(&task_id, "session", 8192).unwrap();
        if snapshot.info.status.is_terminal() {
            return Some(Err(snapshot));
        }
        (*daemon.refused_connects.borrow() >= refused + 3).then_some(Ok(()))
    })
    .await;
    if let Err(snapshot) = still_trying {
        panic!(
            "AFT gave up 25 minutes into the outage of an untimed runon: {:?}: {}",
            snapshot.info.status, snapshot.output_preview
        );
    }
    daemon.restore();
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");
    assert_eq!(fs::read(&task.paths.stdout).unwrap(), b"AB");
    assert_eq!(daemon.methods("exec.run").len(), 1, "never resubmitted");
    drop(clock);
}

/// The same outage outlasting the job's own limits ends the wait, and the
/// result says why, names the job and gives advice the agent can follow.
#[tokio::test]
async fn exec_remote_bash_route_outage_gives_up_after_the_jobs_own_limits() {
    let daemon = crate::exec_remote::wire_tests::outage_daemon().await;
    let dir = tempfile::tempdir().unwrap();
    let clock = PausedReattachClock::new(dir.path());
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    daemon.after_refused_connects(3).await;
    clock.advance(Duration::from_secs(12 * 60));
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::FateUnknown, "{done:?}");
    assert!(
        done.output_preview.contains(&format!(
            "job {} could not be re-attached before its own time limit passed (ck-motor's queue wait limit of 5m and its run timeout of 30s have passed since AFT submitted it, plus 5m of grace); command not rerun",
            id()
        )),
        "{}",
        done.output_preview
    );
    let reason = done.info.status_reason.unwrap();
    assert!(
        reason.contains(&format!("job {} may still be running on ck-motor", id())),
        "{reason}"
    );
    assert!(
        reason.contains("wait for it to end, then rerun"),
        "{reason}"
    );
    assert!(!done.output_preview.contains("exec.status"));
    assert!(!done.output_preview.contains("local-proof"));
    assert_eq!(daemon.methods("exec.run").len(), 1, "never resubmitted");
}

#[test]
fn reattach_backoff_doubles_from_a_tenth_of_a_second_up_to_thirty_seconds() {
    let schedule: Vec<_> = (0..12).map(reattach_backoff).collect();
    assert_eq!(
        schedule,
        [100, 200, 400, 800, 1_600, 3_200, 6_400, 12_800, 25_600, 30_000, 30_000, 30_000]
            .map(Duration::from_millis)
    );
    assert_eq!(reattach_backoff(u32::MAX), Duration::from_secs(30));
}

#[test]
fn workspace_owner_holders_reads_ck_motor_queue_report() {
    let holder: Uuid = "0192a64a-1234-7000-8000-0000000000aa".parse().unwrap();
    let other: Uuid = "0192a64a-1234-7000-8000-0000000000bb".parse().unwrap();
    let detail = crate::exec_remote::wire_tests::queue_owner_detail(holder);
    assert_eq!(workspace_owner_holders(&detail), [holder, other]);
    // Another blocker is not a workspace owner, even with running holders.
    let other_phase = detail.replacen("waiting_on=workspace owner", "waiting_on=permits", 1);
    assert!(workspace_owner_holders(&other_phase).is_empty());
    // A checkout held because another workspace's job mounts it as a sibling,
    // or a sibling user held back behind an earlier queued job of the same
    // repository, is never this session's own earlier job, even when the
    // report lists the same job ids. These are ck-motor's phase names.
    for phase in ["sibling user of this checkout", "queued workspace job"] {
        let sibling = detail.replacen(
            "waiting_on=workspace owner;",
            &format!(
                "waiting_on={phase}; sibling_users=[job={holder} workspace=/work/other permits=4];"
            ),
            1,
        );
        assert!(workspace_owner_holders(&sibling).is_empty(), "{phase}");
    }
    assert!(workspace_owner_holders("maintenance").is_empty());
}

async fn queue_wait_refusal(holder: Uuid, earlier_job: Uuid) -> (BgTaskSnapshot, String, String) {
    let daemon = daemon(Script::QueueWaitOwner { holder }, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    ensure_git_fixture(dir.path());
    let (registry, earlier, _) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf earlier");
    registry
        .task(&earlier)
        .unwrap()
        .state
        .lock()
        .unwrap()
        .metadata
        .remote
        .as_mut()
        .unwrap()
        .job_id = Some(earlier_job);
    let task_id = start_runon_retry(&registry, dir.path(), daemon.connection.clone());
    let done = terminal(&registry, &task_id).await;
    let task = registry.task(&task_id).unwrap();
    // A restarted reader renders the refusal from the saved record alone.
    let durable = read_task(&task.paths.json).unwrap();
    let restarted = remote_refusal(&durable).unwrap().message;
    (done, restarted, earlier)
}

#[tokio::test]
async fn runon_queue_wait_refusal_names_this_sessions_earlier_job_in_the_workspace() {
    let holder: Uuid = "0192a64a-1234-7000-8000-0000000000aa".parse().unwrap();
    let (done, restarted, earlier) = queue_wait_refusal(holder, holder).await;
    let expected = format!(
        "runon refused: your earlier job {holder} in this workspace is still running (task {earlier}); this command waited behind it until the runner's queue wait limit passed and was not run; wait for task {earlier} to end (bash_status shows it), then retry; remote job 0192a64a-1234-7000-8000-000000000001"
    );
    assert_eq!(done.info.status_reason.as_deref(), Some(expected.as_str()));
    assert_eq!(restarted, expected);
}

#[tokio::test]
async fn runon_queue_wait_refusal_behind_another_owner_keeps_the_runner_report() {
    let holder: Uuid = "0192a64a-1234-7000-8000-0000000000aa".parse().unwrap();
    let unrelated: Uuid = "0192a64a-1234-7000-8000-0000000000cc".parse().unwrap();
    let (done, restarted, _) = queue_wait_refusal(holder, unrelated).await;
    let reason = done.info.status_reason.unwrap();
    assert!(
        reason.starts_with(
            "runon refused: remote refused: queue_wait_exceeded (waiting_on=workspace owner;"
        ),
        "{reason}"
    );
    assert!(!reason.contains("your earlier job"), "{reason}");
    assert_eq!(restarted, reason);
}

#[tokio::test]
async fn exec_remote_bash_raw_utf8_pipeline_and_workspace_changes() {
    let daemon = daemon(Script::Utf8, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let task = start(&registry, dir.path(), daemon.connection.clone());
    let done = terminal(&registry, &task).await;
    let task = registry.task(&task).unwrap();
    assert_eq!(fs::read(&task.paths.stdout).unwrap(), "€".as_bytes());
    assert_eq!(fs::read(&task.paths.stderr).unwrap(), "😀".as_bytes());
    assert_eq!(
        fs::read_to_string(&task.paths.pipeline_status).unwrap(),
        "3\n0\n"
    );
    assert_eq!(
        done.output_preview.lines().next(),
        Some("ran remotely on ck-motor (job 0192a64a-1234-7000-8000-000000000001)")
    );
    // The changed file is named after the output, and it was not copied back.
    assert!(
        done.output_preview.ends_with(
            "These files changed on the server and were NOT copied back:\n  generated.txt\n\
             git state: not reported by the runner\n\
             untracked files: not reported by the runner\n\
             ignored writes: not reported by the runner"
        ),
        "{}",
        done.output_preview
    );
    assert!(!dir.path().join("generated.txt").exists());
}

#[tokio::test]
async fn exec_remote_bash_from_a_jailed_worker_is_submitted_unchanged() {
    // A jailed worker's shell plan is a native-launcher plan. runon still
    // snapshots and submits from AFT itself, outside the shell sandbox; the
    // launcher plan only backs a local fallback, which this run never needs.
    let daemon = daemon(Script::Utf8, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    ensure_git_fixture(dir.path());
    let temp = dir.path().join("sandbox-temp");
    fs::create_dir_all(&temp).unwrap();
    let profile = crate::sandbox_profile::SandboxProfile::build(
        vec![dir.path().into()],
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        temp,
    )
    .unwrap();
    let registry = registry();
    let task = crate::sandbox_spawn::with_worker_jail(true, || {
        registry
            .spawn_remote(
                launch(daemon.connection.clone()),
                SpawnPlan::launcher_for_test(profile, "/nonexistent/aft-launcher".into()),
                "printf local-proof",
                resolve_posix_shell(),
                "session".into(),
                dir.path().into(),
                HashMap::new(),
                crate::bash_background::HardKill::After(Duration::from_secs(30)),
                dir.path().into(),
                crate::bash_background::TaskSlot::Background { max: 10 },
                true,
                false,
                Some(dir.path().into()),
            )
            .unwrap()
    });
    let done = terminal(&registry, &task).await;
    let task = registry.task(&task).unwrap();
    assert_eq!(fs::read(&task.paths.stdout).unwrap(), "€".as_bytes());
    assert_eq!(
        done.output_preview.lines().next(),
        Some("ran remotely on ck-motor (job 0192a64a-1234-7000-8000-000000000001)")
    );
    assert!(!done.output_preview.contains("local-proof"));
}

#[tokio::test]
async fn exec_remote_bash_discloses_only_names_aft_stripped() {
    let daemon = daemon(Script::Utf8, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    ensure_git_fixture(dir.path());
    let registry = registry();
    let env = HashMap::from([
        ("BUILD_LABEL".into(), "ordinary-build".into()),
        ("SHELL_SECRET".into(), "secret-fixture-value".into()),
        ("AWS_REGION".into(), "not-off-host".into()),
        ("AFT_CONTROL_PATH".into(), "private-control-value".into()),
        ("NEXTEST_TEST_THREADS".into(), "5".into()),
        ("RUST_TEST_THREADS".into(), "6".into()),
    ]);
    let task_id = registry
        .spawn_remote(
            launch(daemon.connection.clone()),
            SpawnPlan::Unsandboxed,
            "printf unused",
            resolve_posix_shell(),
            "session".into(),
            dir.path().into(),
            env,
            crate::bash_background::HardKill::After(Duration::from_secs(30)),
            dir.path().into(),
            crate::bash_background::TaskSlot::Background { max: 10 },
            true,
            false,
            Some(dir.path().into()),
        )
        .unwrap();
    let done = terminal(&registry, &task_id).await;
    let log = daemon.log.lock().unwrap();
    let request = &log
        .iter()
        .find(|(_, body)| body["method"] == "exec.run")
        .unwrap()
        .1;
    assert!(request["params"]["env"].get("SHELL_SECRET").is_none());
    assert!(request["params"]["env"].get("AWS_REGION").is_none());
    assert!(request["params"]["env"].get("AFT_CONTROL_PATH").is_none());
    assert!(request["params"]["env"]
        .get("NEXTEST_TEST_THREADS")
        .is_none());
    assert!(request["params"]["env"].get("RUST_TEST_THREADS").is_none());
    assert_eq!(request["params"]["env"]["BUILD_LABEL"], "ordinary-build");
    assert!(
        done.output_preview.contains("AFT stripped env names:"),
        "{done:?}"
    );
    for name in [
        "SHELL_SECRET",
        "AWS_REGION",
        "AFT_CONTROL_PATH",
        "NEXTEST_TEST_THREADS",
        "RUST_TEST_THREADS",
    ] {
        assert!(done.output_preview.contains(name), "{done:?}");
    }
    for value in [
        "secret-fixture-value",
        "not-off-host",
        "private-control-value",
    ] {
        assert!(!done.output_preview.contains(value));
        assert!(!request.to_string().contains(value));
    }
    assert!(!done
        .output_preview
        .contains("the remote job does not receive"));
}

#[tokio::test]
async fn exec_remote_bash_executor_env_disclosure_survives_reattach_and_caps_names() {
    let StreamRecord::Accepted(base) =
        serde_json::from_str(include_str!("../exec_remote/fixtures/frames/accepted.json")).unwrap()
    else {
        panic!("accepted vector")
    };
    let names: Vec<String> = (0..13).map(|n| format!("VAR_{n:02}")).collect();
    for dropped in [base.env_not_forwarded.clone(), None, Some(names.clone())] {
        let daemon = daemon(Script::Utf8, "exec-remote/v1").await;
        let dir = tempfile::tempdir().unwrap();
        let (original, task_id, paths) = persist_accepted_with_snapshot(
            dir.path(),
            daemon.connection.clone(),
            "printf must-not-run",
        );
        let task = original.task(&task_id).unwrap();
        let mut sink = TaskSink::new(&original, task).unwrap();
        let mut accepted = base.clone();
        accepted.env_not_forwarded = dropped.clone();
        sink.accepted(&accepted).unwrap();
        drop(sink);
        drop(original);
        let restarted = registry();
        restarted.replay_session(dir.path(), "session").unwrap();
        assert_replayed(&restarted, &task_id, dir.path(), "the first replay");
        let done = terminal(&restarted, &task_id).await;
        let line = "the remote job does not receive:";
        if dropped.as_ref().is_some_and(|names| !names.is_empty()) {
            assert!(
                done.output_preview
                    .contains(&format!("{line} {}; +3 more", names[..10].join(", "))),
                "{done:?}"
            );
            assert!(!done.output_preview.contains("VAR_10"));
        } else {
            assert!(!done.output_preview.contains(line), "{done:?}");
        }
        assert!(!done.output_preview.contains("all forwarded"));
        let json: serde_json::Value =
            serde_json::from_slice(&fs::read(&paths.json).unwrap()).unwrap();
        assert_eq!(
            json["remote"]["env_not_forwarded"],
            serde_json::to_value(&dropped).unwrap()
        );
        let replay = registry();
        replay.replay_session(dir.path(), "session").unwrap();
        assert_replayed(&replay, &task_id, dir.path(), "the second replay");
        let done = terminal(&replay, &task_id).await;
        assert_eq!(
            done.output_preview.contains(line),
            dropped.as_ref().is_some_and(|names| !names.is_empty())
        );
        assert!(!daemon
            .log
            .lock()
            .unwrap()
            .iter()
            .any(|(_, b)| b["method"] == "exec.run"));
    }
}

#[tokio::test]
async fn exec_remote_bash_kill_sends_cancel_and_reports_terminal() {
    let daemon = daemon(Script::Cancel, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let task = start(&registry, dir.path(), daemon.connection.clone());
    wait_for(
        "acceptance must be consumed before killing the task",
        || {
            registry
                .task(&task)
                .unwrap()
                .state
                .lock()
                .unwrap()
                .metadata
                .remote
                .as_ref()
                .unwrap()
                .job_id
                .map(|_| ())
        },
    )
    .await;
    let killing = registry.clone();
    let kill_task = task.clone();
    let done = tokio::task::spawn_blocking(move || killing.kill(&kill_task, "session").unwrap())
        .await
        .unwrap();
    assert_eq!(done.info.status, BgTaskStatus::Killed);
    assert!(done.info.status_reason.unwrap().contains("cancel"));
    assert!(daemon
        .log
        .lock()
        .unwrap()
        .iter()
        .any(|(_, b)| b["method"] == "exec.cancel"));
}

const TERMINAL_PENDING: &str =
    "remote cancellation requested; terminal pending; inspect bash_status (never rerun)";

/// Start a remote task on a runner that confirms a cancel but sends the
/// terminal only once the test releases `completion_gate`, and wait until the
/// runner has accepted it.
async fn start_held_terminal_task(
    registry: &BgTaskRegistry,
    dir: &Path,
) -> (crate::exec_remote::wire_tests::Daemon, String) {
    let daemon = daemon(Script::GappedCancel, "exec-remote/v1").await;
    let task_id = start(registry, dir, daemon.connection.clone());
    let task = registry.task(&task_id).unwrap();
    wait_for(
        "acceptance must be consumed before killing the task",
        || {
            task.state
                .lock()
                .unwrap()
                .metadata
                .remote
                .as_ref()
                .unwrap()
                .job_id
                .map(|_| ())
        },
    )
    .await;
    (daemon, task_id)
}

/// Kill `task_id` on a blocking thread, inside a request budget of `budget`
/// when one is given, and time the call.
async fn timed_kill(
    registry: &BgTaskRegistry,
    task_id: &str,
    budget: Option<Duration>,
) -> (Result<BgTaskSnapshot, String>, Duration) {
    let (killing, task_id) = (registry.clone(), task_id.to_string());
    tokio::task::spawn_blocking(move || {
        let started = Instant::now();
        let result =
            crate::bounded_io::with_budget(budget.map(crate::bounded_io::Budget::after), || {
                killing.kill(&task_id, "session")
            });
        (result, started.elapsed())
    })
    .await
    .unwrap()
}

/// The cancellation is recorded even when the kill stopped waiting: once the
/// runner sends the terminal, the task still ends as killed.
async fn assert_held_terminal_still_lands(
    registry: &BgTaskRegistry,
    daemon: &crate::exec_remote::wire_tests::Daemon,
    task_id: &str,
) {
    daemon.wait_for_requests("exec.cancel", 1).await;
    daemon.wait_for_requests("exec.attach", 1).await;
    daemon.completion_gate.notify_one();
    let done = terminal(registry, task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Killed);
}

/// A runner that confirms the cancel but holds back the terminal must not keep
/// a `bash_kill` past its request's budget: the kill stops waiting a reply
/// margin before the budget ends and answers "terminal pending", instead of
/// waiting until the caller's own deadline passes and the reply is lost.
#[tokio::test]
async fn exec_remote_bash_kill_with_held_terminal_replies_pending_within_request_budget() {
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let (daemon, task_id) = start_held_terminal_task(&registry, dir.path()).await;
    let budget = REMOTE_KILL_REPLY_MARGIN + Duration::from_secs(1);
    let (result, elapsed) = timed_kill(&registry, &task_id, Some(budget)).await;
    assert_eq!(result.unwrap_err(), TERMINAL_PENDING);
    assert!(
        elapsed < budget,
        "the kill must reply before its request budget runs out: took {elapsed:?} of a {budget:?} budget"
    );
    assert_held_terminal_still_lands(&registry, &daemon, &task_id).await;
}

/// Without a request budget, or inside an ordinary 25-second tool call budget,
/// the wait for a held-back terminal still stops at a small fixed cap, so a
/// kill never holds its executor worker (and its project's other mutating
/// calls) for long. The bound is absolute, not derived from the cap, so
/// raising the cap toward the old 30-second wait goes red here; it leaves room
/// above the cap for a task-record write stalled on a busy test disk.
#[tokio::test]
async fn exec_remote_bash_kill_with_held_terminal_stops_waiting_at_the_cap() {
    for budget in [None, Some(Duration::from_secs(25))] {
        let dir = tempfile::tempdir().unwrap();
        let registry = registry();
        let (daemon, task_id) = start_held_terminal_task(&registry, dir.path()).await;
        let (result, elapsed) = timed_kill(&registry, &task_id, budget).await;
        assert_eq!(result.unwrap_err(), TERMINAL_PENDING);
        assert!(
            elapsed < Duration::from_secs(12),
            "the kill must stop waiting at the cap (budget {budget:?}): took {elapsed:?}"
        );
        assert_held_terminal_still_lands(&registry, &daemon, &task_id).await;
    }
}

/// Inside an ordinary tool call's budget, a terminal that arrives promptly is
/// still waited for, and the kill returns the killed snapshot.
#[tokio::test]
async fn exec_remote_bash_kill_inside_tool_call_budget_returns_prompt_terminal() {
    let daemon = daemon(Script::Cancel, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    let task = registry.task(&task_id).unwrap();
    wait_for(
        "acceptance must be consumed before killing the task",
        || {
            task.state
                .lock()
                .unwrap()
                .metadata
                .remote
                .as_ref()
                .unwrap()
                .job_id
                .map(|_| ())
        },
    )
    .await;
    let (result, _) = timed_kill(&registry, &task_id, Some(Duration::from_secs(25))).await;
    let done = result.unwrap();
    assert_eq!(done.info.status, BgTaskStatus::Killed);
    assert!(done.info.status_reason.unwrap().contains("cancel"));
}

#[tokio::test]
async fn exec_remote_bash_continuous_output_cannot_starve_cancel() {
    let daemon = daemon(Script::Continuous, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    let task = registry.task(&task_id).unwrap();
    // Cancel during active output without waiting for a coalesced durable checkpoint.
    wait_for(
        "continuous output must be consumed before cancelling",
        || (fs::metadata(&task.paths.stdout).unwrap().len() >= 11).then_some(()),
    )
    .await;
    assert!(registry.record_remote_cancel(&task).unwrap());
    daemon.wait_for_requests("exec.cancel", 1).await;
    assert_eq!(
        terminal(&registry, &task_id).await.info.status,
        BgTaskStatus::Killed
    );
}

#[tokio::test]
async fn exec_remote_launch_is_fenced_by_a_refused_startup_receipt() {
    // The bash reply deadline refused this startup and told the caller the
    // command was not started; the remote launch must then create nothing:
    // no registered task, no task record, no remote request or local fallback.
    let daemon = daemon(Script::Refused, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let receipt = Arc::new(crate::bash_background::SpawnReceipt::new(
        std::time::Instant::now() + Duration::from_secs(600),
    ));
    assert_eq!(
        receipt.expire(),
        crate::bash_background::StartupOutcome::Refused
    );
    let result = crate::bash_background::with_spawn_receipt(receipt, || {
        registry.spawn_remote(
            launch(daemon.connection.clone()),
            SpawnPlan::Unsandboxed,
            "printf local-proof > remote-fence-probe",
            resolve_posix_shell(),
            "session".into(),
            dir.path().into(),
            HashMap::new(),
            crate::bash_background::HardKill::After(Duration::from_secs(30)),
            dir.path().into(),
            crate::bash_background::TaskSlot::Background { max: 10 },
            true,
            false,
            Some(dir.path().into()),
        )
    });
    let error = result.expect_err("a refused receipt must refuse the remote launch");
    assert!(error.contains("startup deadline expired"), "{error}");
    assert!(registry.list(0).is_empty());
    let (ids, _) = discover_task_ids(&session_tasks_dir(dir.path(), "session")).unwrap_or_default();
    assert!(
        ids.is_empty(),
        "refused remote launch left records: {ids:?}"
    );
    // The refused receipt returned before registering a worker, so there is
    // no asynchronous launch left to wait for.
    assert!(!dir.path().join("remote-fence-probe").exists());
    assert!(
        daemon.log.lock().unwrap().is_empty(),
        "no remote request may be sent"
    );
}

#[tokio::test]
async fn exec_remote_bash_deadline_is_not_cancel() {
    let daemon = daemon(Script::Deadline, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let task = start(&registry, dir.path(), daemon.connection.clone());
    let done = terminal(&registry, &task).await;
    assert_eq!(done.info.status, BgTaskStatus::TimedOut);
    assert!(done.info.status_reason.unwrap().contains("deadline"));
    assert!(!daemon
        .log
        .lock()
        .unwrap()
        .iter()
        .any(|(_, b)| b["method"] == "exec.cancel"));
}

#[tokio::test]
async fn exec_remote_bash_restart_uses_persisted_seq_without_duplicates_or_gaps() {
    let daemon = daemon(Script::Restart, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let original = registry();
    let db = Arc::new(Mutex::new(
        crate::db::open(&dir.path().join("aft.db")).unwrap(),
    ));
    original.set_harness(crate::harness::Harness::Runner);
    original.set_db_pool(db.clone());
    let layout = allocate_task_layout(dir.path(), "session").unwrap();
    let task_id = layout.paths.task_id.clone();
    let mut metadata = PersistedTask::starting(
        task_id.clone(),
        "session".into(),
        "printf local-proof".into(),
        dir.path().into(),
        Some(dir.path().into()),
        Some(30000),
        true,
        false,
    );
    metadata.status = BgTaskStatus::Running;
    metadata.harness = Some("runner".into());
    metadata.execution_note = Some("ran remotely on ck-motor".into());
    metadata.remote = Some(RemoteTask {
        accepted_platform: None,
        explicit_runon: false,
        connection_file: Some(daemon.connection.clone()),
        harness: "broca".into(),
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
        terminal: None,
        fallback_digest: None,
        env_not_forwarded: None,
        queue_position: None,
        output_received: false,
        retry: None,
        output_incomplete: None,
        workspace_holder: None,
        return_paths: Vec::new(),
        returned_files: None,
        return_first_seq: None,
        return_staging: Default::default(),
    });
    let handles = TaskIoHandles::create(&layout, BgMode::Pipes, true).unwrap();
    write_task_at(&layout, &metadata).unwrap();
    original.dual_write_task(&layout.paths, &metadata);
    original
        .insert_rehydrated_task(metadata, layout.paths.clone(), false)
        .unwrap();
    let task = original.task(&task_id).unwrap();
    task.state.lock().unwrap().io_handles = Some(handles);
    let mut sink = TaskSink::new(&original, task.clone()).unwrap();
    let mut consumer = exec::StreamConsumer::new();
    consumer
        .consume(StreamRecord::Accepted(Accepted::new(id(), 1)), &mut sink)
        .unwrap();
    for (seq, b) in [(0, b'A'), (1, b'B')] {
        // Checkpoint B explicitly; A's first-output running transition is
        // durable, while ordinary output now checkpoints at most once a second.
        sink.last_progress_persist = Instant::now() - PROGRESS_CHECKPOINT_INTERVAL;
        consumer
            .consume(
                StreamRecord::Output(Output::new(seq, OutputStream::Stdout, BytePayload(vec![b]))),
                &mut sink,
            )
            .unwrap();
    }
    // A crash can leave bytes beyond the durable cursor; recovery truncates
    // them before attaching rather than replaying into duplicated output.
    sink.stdout.write_all(b"uncommitted").unwrap();
    drop(sink);
    drop(consumer);
    original.inner.shutdown.store(true, Ordering::SeqCst);
    drop(task);
    drop(original);
    let restarted = registry();
    restarted.set_harness(crate::harness::Harness::Runner);
    restarted.set_db_pool(db);
    restarted.replay_session(dir.path(), "session").unwrap();
    let done = terminal(&restarted, &task_id).await;
    assert!(done.output_preview.contains("ABCD"));
    let task = restarted.task(&task_id).unwrap();
    assert_eq!(fs::read(&task.paths.stdout).unwrap(), b"ABCD");
    let log = daemon.log.lock().unwrap();
    assert_eq!(
        log.iter()
            .find(|(_, b)| b["method"] == "exec.attach")
            .unwrap()
            .1["params"]["from_seq"],
        2
    );
    assert!(!log.iter().any(|(_, b)| b["method"] == "exec.run"));
}

#[tokio::test]
async fn exec_remote_bash_restart_after_terminal_proof_restores_exit_and_pipeline_files() {
    let dir = tempfile::tempdir().unwrap();
    let (original, task_id, paths) = persist_accepted_with_snapshot(
        dir.path(),
        dir.path().join("unavailable-connection"),
        "false | true",
    );
    let task = original.task(&task_id).unwrap();
    let mut sink = TaskSink::new(&original, task.clone()).unwrap();
    sink.output(0, OutputStream::Stderr, b"381 tests run: 381 passed\n")
        .unwrap();
    let mut record = TerminalRecord::new(id(), Outcome::Exit { code: 0 }, 1, 0, 0);
    {
        let mut state = task.state.lock().unwrap();
        state.metadata.pipeline_segments = vec!["false".into(), "true".into()];
        original.persist_task(&paths, &state.metadata).unwrap();
    }
    record.pipestatus = Some(vec![1, 0]);
    sink.terminal(&record, &exec::grade(&record)).unwrap();
    // The runner finished and its proof is durable, but the daemon has not
    // published terminal status or written the local marker files yet.
    assert_eq!(
        read_task(&paths.json).unwrap().status,
        BgTaskStatus::Running
    );
    assert!(fs::read(&paths.exit).unwrap().is_empty());
    assert!(fs::read(&paths.pipeline_status).unwrap().is_empty());
    let readonly = registry();
    let (disk_status, _) = readonly
        .terminal_status_from_disk(&task_id, "session", Some(dir.path()), dir.path())
        .unwrap();
    assert_eq!(disk_status.info.status, BgTaskStatus::Completed);
    assert_eq!(disk_status.exit_code, Some(0));
    assert!(readonly.list(0).is_empty());
    assert_eq!(
        read_task(&paths.json).unwrap().status,
        BgTaskStatus::Running
    );
    drop(sink);
    drop(task);
    drop(original);

    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    let done = terminal(&restarted, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Completed);
    assert_eq!(done.exit_code, Some(0));
    assert_eq!(read_exit_marker(&paths).unwrap(), Some(ExitMarker::Code(0)));
    assert_eq!(
        fs::read_to_string(&paths.pipeline_status).unwrap(),
        "1\n0\n"
    );
    assert!(done.output_preview.contains("381 tests run: 381 passed"));
    assert!(done.output_preview.contains("segment 1 of 2"));
    let disk = read_task(&paths.json).unwrap();
    assert_eq!(disk.exit_code, Some(0));
    assert_eq!(
        disk.schema_version,
        crate::bash_background::persistence::SCHEMA_VERSION
    );
    drop(restarted);

    // Already-completed records from older daemons also need marker repair.
    fs::write(&paths.exit, "").unwrap();
    fs::write(&paths.pipeline_status, "").unwrap();
    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    assert_eq!(read_exit_marker(&paths).unwrap(), Some(ExitMarker::Code(0)));
    assert_eq!(
        fs::read_to_string(&paths.pipeline_status).unwrap(),
        "1\n0\n"
    );
}

#[tokio::test]
async fn exec_remote_bash_restart_without_terminal_proof_names_unknown_and_removes_empty_markers() {
    let daemon = daemon(Script::MissingTerminal, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let (original, task_id, paths) =
        persist_accepted_with_snapshot(dir.path(), daemon.connection.clone(), "false | true");
    let task = original.task(&task_id).unwrap();
    let mut sink = TaskSink::new(&original, task.clone()).unwrap();
    sink.output(0, OutputStream::Stderr, b"381 tests run: 381 passed\n")
        .unwrap();
    drop(sink);
    drop(task);
    drop(original);
    shorten_reattach_budget(dir.path());

    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    let done = terminal(&restarted, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::FateUnknown);
    assert_eq!(done.exit_code, None);
    assert!(done
        .info
        .status_reason
        .as_deref()
        .unwrap()
        .contains("outcome"));
    assert!(done.output_preview.contains("381 tests run: 381 passed"));
    let disk = read_task(&paths.json).unwrap();
    assert!(disk
        .execution_note
        .as_deref()
        .unwrap()
        .contains("re-attached"));
    assert!(done.output_preview.contains("re-attached"));
    assert!(done.exit_marker_unavailable.is_some());
    assert!(
        !paths.exit.exists(),
        "an unknown outcome must not leave an empty exit marker"
    );
    assert!(
        !paths.pipeline_status.exists(),
        "missing runner proof must not leave an empty pipeline marker"
    );
    assert!(disk
        .pipeline_status_unavailable
        .as_deref()
        .unwrap()
        .contains("terminal"));
    let log = daemon.log.lock().unwrap();
    assert!(log.iter().any(|(_, body)| body["method"] == "exec.attach"));
    assert!(!log.iter().any(|(_, body)| body["method"] == "exec.run"));
}

#[tokio::test]
async fn exec_remote_bash_retained_output_gap_survives_terminal_and_restart() {
    let daemon = daemon(Script::RetainedGap, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let (original, task_id, paths) = persist_accepted_with_snapshot(
        dir.path(),
        daemon.connection.clone(),
        "printf must-not-run",
    );
    let task = original.task(&task_id).unwrap();
    let mut sink = TaskSink::new(&original, task).unwrap();
    sink.output(0, OutputStream::Stdout, b"A").unwrap();
    drop(sink);
    drop(original);
    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    let done = terminal(&restarted, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Completed);
    assert_eq!(fs::read(&paths.stdout).unwrap(), b"AD");
    let warning = "output lost between seq 1 and 2: the executor no longer retained it";
    assert!(done.output_preview.contains(warning), "{done:?}");
    assert_eq!(
        crate::bash_background::persistence::read_task(&paths.json)
            .unwrap()
            .incomplete_output,
        vec![(1, 2)]
    );
    let replay = registry();
    replay.replay_session(dir.path(), "session").unwrap();
    let done = terminal(&replay, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Completed);
    assert!(done.output_preview.contains(warning), "{done:?}");
    assert!(!daemon
        .log
        .lock()
        .unwrap()
        .iter()
        .any(|(_, b)| b["method"] == "exec.run"));
}

#[tokio::test]
async fn exec_remote_bash_terminal_gap_budget_and_outcome_survive_restart() {
    let daemon = daemon(Script::TerminalGapOnce, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let (original, task_id, paths) = persist_accepted_with_snapshot(
        dir.path(),
        daemon.connection.clone(),
        "printf must-not-run",
    );
    let task = original.task(&task_id).unwrap();
    let mut sink = TaskSink::new(&original, task.clone()).unwrap();
    let mut consumer = exec::StreamConsumer::resume(
        task.state
            .lock()
            .unwrap()
            .metadata
            .remote
            .as_ref()
            .unwrap()
            .point()
            .unwrap(),
    );
    for attempt in 0..3 {
        for seq in [2, 1] {
            consumer
                .consume(
                    StreamRecord::Output(Output::new(
                        seq,
                        OutputStream::Stdout,
                        BytePayload(vec![b'A' + seq as u8]),
                    )),
                    &mut sink,
                )
                .unwrap();
        }
        let Err(exec::Error::RecoveryRequired {
            resume: Some(point),
            ..
        }) = consumer.consume(
            StreamRecord::Terminal(TerminalRecord::new(
                id(),
                Outcome::Exit { code: 7 },
                1,
                0,
                0,
            )),
            &mut sink,
        )
        else {
            panic!("recovery should still be possible before the bound");
        };
        assert_eq!(point.gap_recovery.as_ref().unwrap().reattaches, attempt);
        consumer = exec::StreamConsumer::resume(point);
    }
    let durable = read_task(&paths.json).unwrap();
    let remote = durable.remote.unwrap();
    assert_eq!(remote.gap_recovery.as_ref().unwrap().reattaches, 2);
    assert!(
        remote.terminal.is_none(),
        "output must be recovered before finalization"
    );
    drop(sink);
    drop(task);
    drop(original);
    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    let done = terminal(&restarted, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Failed);
    assert_eq!(done.exit_code, Some(7));
    assert_eq!(fs::read(&paths.stdout).unwrap(), b"BC");
    let durable = read_task(&paths.json).unwrap();
    assert_eq!(durable.incomplete_output, [(0, 0)]);
    assert!(durable
        .execution_note
        .unwrap()
        .contains("output lost between seq 0 and 0: the executor never delivered it"));
    assert!(durable.remote.unwrap().gap_recovery.is_none());
    let log = daemon.log.lock().unwrap();
    assert_eq!(
        log.iter()
            .filter(|(_, b)| b["method"] == "exec.attach")
            .count(),
        1
    );
    assert!(!log.iter().any(|(_, b)| b["method"] == "exec.run"));
}

#[tokio::test]
async fn exec_remote_bash_terminal_gap_keeps_outcome_when_attach_is_unavailable() {
    let daemon = daemon(Script::AttachDisconnected, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    shorten_reattach_budget(dir.path());
    let clock = PausedReattachClock::new(dir.path());
    let (original, task_id, paths) = persist_accepted_with_snapshot(
        dir.path(),
        daemon.connection.clone(),
        "printf must-not-run",
    );
    let task = original.task(&task_id).unwrap();
    let mut sink = TaskSink::new(&original, task.clone()).unwrap();
    let point = task
        .state
        .lock()
        .unwrap()
        .metadata
        .remote
        .as_ref()
        .unwrap()
        .point()
        .unwrap();
    let mut consumer = exec::StreamConsumer::resume(point);
    for seq in [2, 1] {
        consumer
            .consume(
                StreamRecord::Output(Output::new(
                    seq,
                    OutputStream::Stdout,
                    BytePayload(vec![b'A' + seq as u8]),
                )),
                &mut sink,
            )
            .unwrap();
    }
    assert!(matches!(
        consumer.consume(
            StreamRecord::Terminal(TerminalRecord::new(
                id(),
                Outcome::Exit { code: 7 },
                1,
                0,
                0,
            )),
            &mut sink
        ),
        Err(exec::Error::RecoveryRequired { .. })
    ));
    drop(sink);
    drop(task);
    drop(original);
    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    daemon.wait_for_requests("exec.attach", 1).await;
    clock.advance(Duration::from_millis(1_500));
    let done = terminal(&restarted, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Failed);
    assert_eq!(done.exit_code, Some(7));
    assert_eq!(fs::read(&paths.stdout).unwrap(), b"BC");
    let durable = read_task(&paths.json).unwrap();
    assert_eq!(durable.incomplete_output, [(0, 0)]);
    assert!(durable.remote.unwrap().terminal.is_some());
    assert!(!daemon
        .log
        .lock()
        .unwrap()
        .iter()
        .any(|(_, b)| b["method"] == "exec.run"));
}

#[test]
fn exec_remote_bash_plain_output_has_no_per_record_durable_io() {
    use crate::bash_background::persistence::work_counts;
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, paths) = persist_accepted_with_snapshot(
        dir.path(),
        dir.path().join("unavailable-connection"),
        "printf remote",
    );
    let task = registry.task(&task_id).unwrap();
    let mut sink = TaskSink::new(&registry, task.clone()).unwrap();
    sink.started(&Started::new(0, 123, 1)).unwrap();
    // Keep this operation-count measurement independent of host speed. The
    // separate checkpoint test advances the persistence deadline explicitly.
    sink.last_progress_persist = Instant::now() + Duration::from_secs(60);
    work_counts::reset();
    let started = Instant::now();
    const RECORDS: u64 = 1000;
    for seq in 1..=RECORDS {
        sink.output(seq, OutputStream::Stdout, b"stdout\n").unwrap();
        sink.output(seq + RECORDS, OutputStream::Stderr, b"stderr\n")
            .unwrap();
        sink.unknown_output(seq + 2 * RECORDS, b"unknown\n")
            .unwrap();
    }
    let counts = work_counts::get();
    eprintln!(
        "relay persistence: {} records, {} sync_all, {} renames, {:?}",
        RECORDS * 3,
        counts.syncs,
        counts.renames,
        started.elapsed()
    );
    assert_eq!(counts.syncs, 0, "plain output must not sync files");
    assert_eq!(counts.renames, 0, "plain output must not replace metadata");
    assert_eq!(
        fs::read(&paths.stdout).unwrap(),
        b"stdout\n".repeat(RECORDS as usize)
    );
    assert_eq!(
        fs::read(&paths.stderr).unwrap(),
        b"stderr\n".repeat(RECORDS as usize)
    );
    assert_eq!(sink.output_progress.last_seq, Some(3 * RECORDS));
    assert_eq!(
        task.state
            .lock()
            .unwrap()
            .metadata
            .remote
            .as_ref()
            .unwrap()
            .last_seq,
        Some(0)
    );
}

#[test]
fn exec_remote_bash_state_transitions_are_durable() {
    use crate::bash_background::persistence::work_counts;
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, paths) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf remote");
    let task = registry.task(&task_id).unwrap();
    let mut sink = TaskSink::new(&registry, task).unwrap();
    work_counts::reset();
    sink.accepted(&Accepted::new(id(), 7)).unwrap();
    let remote = read_task(&paths.json).unwrap().remote.unwrap();
    assert_eq!(remote.job_id, Some(id()));
    assert_eq!(
        RemotePhase::of(&remote),
        RemotePhase::Queued { position: 7 }
    );
    assert!(work_counts::get().syncs >= 2);
    assert!(work_counts::get().renames >= 1);

    work_counts::reset();
    // An older runner's first output is a running transition, not just progress.
    sink.output(0, OutputStream::Stdout, b"old runner").unwrap();
    assert_eq!(
        RemotePhase::of(&read_task(&paths.json).unwrap().remote.unwrap()),
        RemotePhase::Running
    );
    assert_eq!(work_counts::get().syncs, 3);
    assert_eq!(work_counts::get().renames, 1);

    work_counts::reset();
    sink.started(&Started::new(1, 123, 1)).unwrap();
    let remote = read_task(&paths.json).unwrap().remote.unwrap();
    assert_eq!(remote.started.unwrap().seq, 1);
    assert_eq!(remote.last_seq, Some(1));
    assert_eq!(work_counts::get().syncs, 2);
    assert_eq!(work_counts::get().renames, 1);
}

#[test]
fn exec_remote_bash_progress_checkpoints_are_coalesced() {
    use crate::bash_background::persistence::work_counts;
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, paths) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf remote");
    let task = registry.task(&task_id).unwrap();
    let mut sink = TaskSink::new(&registry, task).unwrap();
    sink.started(&Started::new(0, 123, 1)).unwrap();
    sink.output(1, OutputStream::Stdout, b"A").unwrap();
    assert_eq!(
        read_task(&paths.json).unwrap().remote.unwrap().last_seq,
        Some(0)
    );
    sink.last_progress_persist = Instant::now() - PROGRESS_CHECKPOINT_INTERVAL;
    work_counts::reset();
    sink.output(2, OutputStream::Stderr, b"B").unwrap();
    let remote = read_task(&paths.json).unwrap().remote.unwrap();
    assert_eq!(
        (remote.last_seq, remote.stdout_len, remote.stderr_len),
        (Some(2), 1, 1)
    );
    // Two dirty output files and the new metadata file; a progress-only
    // checkpoint leaves the directory unsynced.
    assert_eq!(work_counts::get().syncs, 3);
    assert_eq!(work_counts::get().renames, 1);
    work_counts::reset();
    sink.output(3, OutputStream::Stdout, b"C").unwrap();
    assert_eq!(work_counts::get().syncs, 0);
    assert_eq!(work_counts::get().renames, 0);
    assert_eq!(
        read_task(&paths.json).unwrap().remote.unwrap().last_seq,
        Some(2)
    );
}

#[test]
fn exec_remote_bash_progress_checkpoints_over_a_20s_fake_run() {
    use crate::bash_background::persistence::work_counts;
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, paths) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf remote");
    let task = registry.task(&task_id).unwrap();
    let mut sink = TaskSink::new(&registry, task).unwrap();
    let start = Instant::now();
    let elapsed_ms = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let clock_ticks = Arc::clone(&elapsed_ms);
    sink.set_progress_clock_for_test(Arc::new(move || {
        start + Duration::from_millis(clock_ticks.load(std::sync::atomic::Ordering::Relaxed))
    }));
    sink.started(&Started::new(0, 123, 1)).unwrap();
    work_counts::reset();
    // A fake runner streams 100 records a second, alternating stdout and
    // stderr, for 20 simulated seconds. The injected clock advances without
    // sleeping or depending on how long checkpoint writes take.
    const TICK: Duration = Duration::from_millis(10);
    for seq in 1..=2000_u64 {
        elapsed_ms.fetch_add(
            TICK.as_millis() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        let stream = if seq % 2 == 0 {
            OutputStream::Stdout
        } else {
            OutputStream::Stderr
        };
        sink.output(seq, stream, b"line\n").unwrap();
    }
    let counts = work_counts::get();
    eprintln!(
        "20s fake run: {} checkpoints, {} fsyncs; per minute: {} checkpoints, {} fsyncs",
        counts.renames,
        counts.syncs,
        counts.renames * 3,
        counts.syncs * 3
    );
    assert_eq!(counts.renames, 4, "one checkpoint per five seconds");
    // Each checkpoint syncs the two dirty output files and the new metadata
    // file; a progress-only checkpoint does not sync the directory.
    assert_eq!(counts.syncs, 12);
    let remote = read_task(&paths.json).unwrap().remote.unwrap();
    let saved = remote.last_seq.unwrap();
    assert!(saved >= 1500, "the last checkpoint is recent: {saved}");
    assert_eq!(
        (remote.stdout_len, remote.stderr_len),
        (saved / 2 * 5, saved.div_ceil(2) * 5),
        "the saved lengths match the saved sequence number"
    );
}

#[test]
fn exec_remote_bash_stale_checkpoint_snapshot_loses_to_newer_locked_write() {
    use crate::bash_background::persistence::work_counts;
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, paths) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf remote");
    let task = registry.task(&task_id).unwrap();
    let mut sink = TaskSink::new(&registry, task.clone()).unwrap();
    sink.started(&Started::new(0, 123, 1)).unwrap();
    sink.output(1, OutputStream::Stdout, b"A").unwrap();
    // A cancel lands after the checkpoint took its snapshot (without the
    // cancel) and before the snapshot reaches the file.
    let (cancel_registry, cancel_task) = (registry.clone(), task.clone());
    sink.before_checkpoint_write = Some(Box::new(move || {
        assert!(cancel_registry.record_remote_cancel(&cancel_task).unwrap());
    }));
    sink.last_progress_persist = Instant::now() - PROGRESS_CHECKPOINT_INTERVAL;
    work_counts::reset();
    sink.output(2, OutputStream::Stderr, b"B").unwrap();
    let remote = read_task(&paths.json).unwrap().remote.unwrap();
    assert!(
        remote.cancel_requested,
        "the older checkpoint snapshot must not replace the cancel record"
    );
    assert_eq!(
        (remote.last_seq, remote.stdout_len, remote.stderr_len),
        (Some(2), 1, 1),
        "the cancel record already carries the checkpointed cursor"
    );
    assert_eq!(work_counts::get().renames, 1, "only the cancel was written");
}

#[test]
fn exec_remote_bash_restart_after_progress_checkpoint_replays_from_saved_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, paths) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf remote");
    let task = registry.task(&task_id).unwrap();
    let mut sink = TaskSink::new(&registry, task).unwrap();
    sink.started(&Started::new(0, 123, 1)).unwrap();
    sink.output(1, OutputStream::Stdout, b"A").unwrap();
    sink.output(2, OutputStream::Stderr, b"a").unwrap();
    sink.last_progress_persist = Instant::now() - PROGRESS_CHECKPOINT_INTERVAL;
    // A progress-only checkpoint saves the cursor after this record.
    sink.output(3, OutputStream::Stdout, b"B").unwrap();
    // Received after the checkpoint and never saved in metadata.
    sink.output(4, OutputStream::Stdout, b"C").unwrap();
    sink.output(5, OutputStream::Stderr, b"b").unwrap();
    let disk = read_task(&paths.json).unwrap();
    let point = disk.remote.as_ref().unwrap().point().unwrap();
    drop(sink);
    drop(registry);

    let restarted = self::registry();
    restarted
        .insert_rehydrated_task(disk, paths.clone(), false)
        .unwrap();
    let task = restarted.task(&task_id).unwrap();
    let mut sink = TaskSink::new(&restarted, task).unwrap();
    let mut consumer = exec::StreamConsumer::resume(point);
    let from_seq = consumer
        .resume_point()
        .unwrap()
        .attach_request()
        .unwrap()
        .from_seq;
    // The runner replays every record after the saved sequence number.
    for (seq, stream, bytes) in [
        (4, OutputStream::Stdout, b"C"),
        (5, OutputStream::Stderr, b"b"),
        (6, OutputStream::Stdout, b"D"),
    ] {
        consumer
            .consume(
                StreamRecord::Output(Output::new(seq, stream, BytePayload(bytes.to_vec()))),
                &mut sink,
            )
            .unwrap();
    }
    assert_eq!(fs::read(&paths.stdout).unwrap(), b"ABCD");
    assert_eq!(fs::read(&paths.stderr).unwrap(), b"ab");
    assert_eq!(from_seq, 4, "attach resumes after the progress checkpoint");
}

#[test]
fn exec_remote_bash_cancel_persists_only_durable_output_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, paths) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf remote");
    let task = registry.task(&task_id).unwrap();
    let mut sink = TaskSink::new(&registry, task.clone()).unwrap();
    sink.started(&Started::new(0, 123, 1)).unwrap();
    sink.last_progress_persist = Instant::now() + Duration::from_secs(60);
    sink.output(1, OutputStream::Stdout, b"tail").unwrap();
    registry.record_remote_cancel(&task).unwrap();
    let disk = read_task(&paths.json).unwrap();
    let remote = disk.remote.as_ref().unwrap();
    assert!(remote.cancel_requested);
    assert_eq!(
        (remote.last_seq, remote.stdout_len),
        (Some(0), 0),
        "cancel must not publish the unsynced output cursor"
    );
    let point = remote.point().unwrap();
    drop(sink);
    drop(registry);
    // Simulate a host crash losing the stdout bytes written since the checkpoint.
    fs::write(&paths.stdout, b"").unwrap();
    let restarted = self::registry();
    restarted
        .insert_rehydrated_task(disk, paths.clone(), false)
        .unwrap();
    let task = restarted.task(&task_id).unwrap();
    let mut sink = TaskSink::new(&restarted, task).unwrap();
    let mut consumer = exec::StreamConsumer::resume(point);
    consumer
        .consume(
            StreamRecord::Output(Output::new(
                1,
                OutputStream::Stdout,
                BytePayload(b"tail".to_vec()),
            )),
            &mut sink,
        )
        .unwrap();
    assert_eq!(fs::read(&paths.stdout).unwrap(), b"tail");
}

#[test]
fn exec_remote_bash_checkpoint_syncs_dirty_output_before_metadata() {
    use crate::bash_background::persistence::{with_task_io_fault, work_counts, TaskIoFault};
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, paths) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf remote");
    let task = registry.task(&task_id).unwrap();
    let mut sink = TaskSink::new(&registry, task).unwrap();
    sink.started(&Started::new(0, 123, 1)).unwrap();
    sink.last_progress_persist = Instant::now() + Duration::from_secs(60);
    for seq in 1..=10 {
        sink.output(seq, OutputStream::Stdout, b"A").unwrap();
        sink.output(seq + 10, OutputStream::Stderr, b"B").unwrap();
        sink.unknown_output(seq + 20, b"C").unwrap();
    }
    sink.last_progress_persist = Instant::now() - PROGRESS_CHECKPOINT_INTERVAL;
    work_counts::reset();
    sink.unknown_output(31, b"").unwrap();
    assert_eq!(
        work_counts::get().syncs,
        4,
        "three dirty files plus the metadata file"
    );
    assert_eq!(work_counts::get().renames, 1);
    let remote = read_task(&paths.json).unwrap().remote.unwrap();
    assert_eq!(
        (remote.stdout_len, remote.stderr_len, remote.unknown_len),
        (10, 10, 10)
    );
    assert_eq!(remote.last_seq, Some(31));

    sink.last_progress_persist = Instant::now() - PROGRESS_CHECKPOINT_INTERVAL;
    work_counts::reset();
    sink.unknown_output(32, b"").unwrap();
    assert_eq!(
        work_counts::get().syncs,
        1,
        "clean files must not sync again"
    );

    sink.output(33, OutputStream::Stdout, b"tail").unwrap();
    sink.last_progress_persist = Instant::now() - PROGRESS_CHECKPOINT_INTERVAL;
    work_counts::reset();
    let result = with_task_io_fault(TaskIoFault::RunningEnospc, || sink.unknown_output(34, b""));
    assert!(result.is_err());
    assert_eq!(
        work_counts::get().syncs,
        1,
        "dirty stdout must sync before the metadata write fails"
    );
    assert_eq!(
        read_task(&paths.json).unwrap().remote.unwrap().last_seq,
        Some(32)
    );
    work_counts::reset();
    sink.unknown_output(34, b"").unwrap();
    assert_eq!(
        work_counts::get().syncs,
        1,
        "a metadata retry must not resync clean output"
    );
}

#[test]
fn exec_remote_bash_terminal_syncs_dirty_output_before_metadata() {
    use crate::bash_background::persistence::{with_task_io_fault, work_counts, TaskIoFault};
    for fail_metadata in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (registry, task_id, paths) =
            persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf remote");
        let task = registry.task(&task_id).unwrap();
        let mut sink = TaskSink::new(&registry, task).unwrap();
        sink.started(&Started::new(0, 123, 1)).unwrap();
        sink.last_progress_persist = Instant::now() + Duration::from_secs(60);
        sink.output(1, OutputStream::Stdout, b"A").unwrap();
        sink.output(2, OutputStream::Stderr, b"B").unwrap();
        sink.unknown_output(3, b"C").unwrap();
        let record = TerminalRecord::new(id(), Outcome::Exit { code: 0 }, 3, 0, 0);
        let verdict = Verdict::Exited { code: 0 };
        work_counts::reset();
        if fail_metadata {
            let result = with_task_io_fault(TaskIoFault::RunningEnospc, || {
                sink.terminal(&record, &verdict)
            });
            assert!(result.is_err());
            assert_eq!(
                work_counts::get().syncs,
                3,
                "all dirty output must sync before the terminal proof write"
            );
            assert!(read_task(&paths.json)
                .unwrap()
                .remote
                .unwrap()
                .terminal
                .is_none());
            work_counts::reset();
        }
        sink.terminal(&record, &verdict).unwrap();
        assert_eq!(work_counts::get().syncs, if fail_metadata { 2 } else { 5 });
        assert_eq!(work_counts::get().renames, 1);
        let remote = read_task(&paths.json).unwrap().remote.unwrap();
        assert!(remote.terminal.is_some());
        assert_eq!(
            (
                remote.last_seq,
                remote.stdout_len,
                remote.stderr_len,
                remote.unknown_len
            ),
            (Some(3), 1, 1, 1)
        );
    }
}

#[test]
fn exec_remote_bash_terminal_proof_is_durable_before_completion() {
    use crate::bash_background::persistence::{with_task_io_fault, work_counts, TaskIoFault};
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, paths) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf remote");
    let task = registry.task(&task_id).unwrap();
    let mut sink = TaskSink::new(&registry, task.clone()).unwrap();
    let mut consumer = exec::StreamConsumer::new();
    consumer
        .consume(StreamRecord::Accepted(Accepted::new(id(), 1)), &mut sink)
        .unwrap();
    consumer
        .consume(StreamRecord::Started(Started::new(0, 123, 1)), &mut sink)
        .unwrap();
    sink.output(1, OutputStream::Stdout, b"tail").unwrap();
    let record = TerminalRecord::new(id(), Outcome::Exit { code: 0 }, 1, 0, 0);
    let result = with_task_io_fault(TaskIoFault::RunningEnospc, || {
        consumer.consume(StreamRecord::Terminal(record.clone()), &mut sink)
    });
    assert!(
        result.is_err(),
        "a failed durable write must not complete the stream"
    );
    assert!(consumer.finish().is_err());
    assert!(task
        .state
        .lock()
        .unwrap()
        .metadata
        .remote
        .as_ref()
        .unwrap()
        .terminal
        .is_none());
    assert!(read_task(&paths.json)
        .unwrap()
        .remote
        .unwrap()
        .terminal
        .is_none());
    work_counts::reset();
    consumer
        .consume(StreamRecord::Terminal(record), &mut sink)
        .unwrap();
    assert!(consumer.finish().is_ok());
    assert_eq!(
        work_counts::get().syncs,
        2,
        "proof file and parent directory must both sync"
    );
    assert_eq!(work_counts::get().renames, 1);
    let persisted = read_task(&paths.json).unwrap();
    let remote = persisted.remote.unwrap();
    assert!(remote.terminal.is_some());
    assert_eq!((remote.last_seq, remote.stdout_len), (Some(1), 4));
    // Simulate a crash before remote_terminal publishes the registry status.
    assert_eq!(persisted.status, BgTaskStatus::Running);
    let readonly = self::registry();
    assert_eq!(
        readonly
            .terminal_status_from_disk(&task_id, "session", Some(dir.path()), dir.path())
            .unwrap()
            .0
            .info
            .status,
        BgTaskStatus::Completed
    );
}

#[test]
fn exec_remote_bash_restart_short_checkpoint_never_zero_fills() {
    for (terminal_proof, unknown_missing) in
        [(false, false), (true, false), (false, true), (true, true)]
    {
        let dir = tempfile::tempdir().unwrap();
        let (original, task_id, paths) =
            persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf remote");
        let task = original.task(&task_id).unwrap();
        let mut sink = TaskSink::new(&original, task).unwrap();
        sink.started(&Started::new(0, 123, 1)).unwrap();
        sink.output(1, OutputStream::Stdout, b"stdout").unwrap();
        sink.output(2, OutputStream::Stderr, b"stderr").unwrap();
        sink.unknown_output(3, b"unknown").unwrap();
        if terminal_proof {
            sink.terminal(
                &TerminalRecord::new(id(), Outcome::Exit { code: 0 }, 3, 0, 0),
                &Verdict::Exited { code: 0 },
            )
            .unwrap();
        } else {
            sink.last_progress_persist = Instant::now() - PROGRESS_CHECKPOINT_INTERVAL;
            sink.unknown_output(4, b"").unwrap();
        }
        drop(sink);
        drop(original);
        let unknown_path = paths.io_dir.join("remote-unknown-output");
        fs::write(&paths.stdout, b"out").unwrap();
        fs::write(&paths.stderr, b"err").unwrap();
        if unknown_missing {
            fs::remove_file(&unknown_path).unwrap();
        } else {
            fs::write(&unknown_path, b"unk").unwrap();
        }
        let disk = read_task(&paths.json).unwrap();
        let seq = disk.remote.as_ref().unwrap().last_seq;
        let restarted = registry();
        restarted
            .insert_rehydrated_task(disk, paths.clone(), false)
            .unwrap();
        let task = restarted.task(&task_id).unwrap();
        let mut sink = TaskSink::new(&restarted, task).unwrap();
        assert_eq!(
            fs::read(&paths.stdout).unwrap(),
            b"out",
            "short stdout must not be padded"
        );
        assert_eq!(
            fs::read(&paths.stderr).unwrap(),
            b"err",
            "short stderr must not be padded"
        );
        let disk = read_task(&paths.json).unwrap();
        let incomplete = disk
            .remote
            .as_ref()
            .unwrap()
            .output_incomplete
            .as_ref()
            .expect("short checkpoint loss is stored structurally");
        assert!(
            incomplete.reason == RemoteOutputIncompleteReason::CheckpointShort,
            "{incomplete:?}"
        );
        for stream in [
            RemoteOutputStream::Stdout,
            RemoteOutputStream::Stderr,
            RemoteOutputStream::Unknown,
        ] {
            assert!(
                incomplete.streams.iter().any(|loss| loss.stream == stream),
                "missing {stream:?}: {incomplete:?}"
            );
        }
        assert!(!disk
            .execution_note
            .as_deref()
            .unwrap_or_default()
            .contains("Output incomplete"));
        let remote = disk.remote.unwrap();
        assert_eq!(
            (remote.stdout_len, remote.stderr_len, remote.unknown_len),
            (3, 3, if unknown_missing { 0 } else { 3 })
        );
        assert_eq!(
            remote.last_seq, seq,
            "byte losses have no safe sequence mapping"
        );
        assert_eq!(remote.terminal.is_some(), terminal_proof);
        if !terminal_proof {
            sink.output(seq.unwrap() + 1, OutputStream::Stdout, b"next")
                .unwrap();
            sink.unknown_output(seq.unwrap() + 2, b"new").unwrap();
            assert_eq!(fs::read(&paths.stdout).unwrap(), b"outnext");
            assert_eq!(
                fs::read(&unknown_path).unwrap(),
                if unknown_missing {
                    b"new".as_slice()
                } else {
                    b"unknew".as_slice()
                }
            );
        }
        drop(sink);
        drop(restarted);
        let disk = read_task(&paths.json).unwrap();
        let again = registry();
        again
            .insert_rehydrated_task(disk, paths.clone(), false)
            .unwrap();
        let task = again.task(&task_id).unwrap();
        let _sink = TaskSink::new(&again, task).unwrap();
        let disk = read_task(&paths.json).unwrap();
        assert!(disk.remote.as_ref().unwrap().output_incomplete.is_some());
        assert!(!disk
            .execution_note
            .as_deref()
            .unwrap_or_default()
            .contains("Output incomplete"));
        assert!(!fs::read(&paths.stdout).unwrap().contains(&0));
        assert!(!fs::read(&paths.stderr).unwrap().contains(&0));
    }
}

fn recover_short_checkpoint(
    terminal_proof: bool,
) -> (
    tempfile::TempDir,
    BgTaskRegistry,
    String,
    TaskPaths,
    Arc<BgTask>,
    Option<TerminalRecord>,
) {
    let dir = tempfile::tempdir().unwrap();
    let (original, task_id, paths) = persist_accepted_with_snapshot(
        dir.path(),
        dir.path().join("unused-connection"),
        "printf remote",
    );
    let task = original.task(&task_id).unwrap();
    let mut sink = TaskSink::new(&original, task).unwrap();
    sink.started(&Started::new(0, 123, 1)).unwrap();
    sink.output(1, OutputStream::Stdout, b"stdout").unwrap();
    sink.output(2, OutputStream::Stderr, b"stderr").unwrap();
    sink.unknown_output(3, b"unknown").unwrap();
    let record = TerminalRecord::new(id(), Outcome::Exit { code: 0 }, 3, 0, 0);
    if terminal_proof {
        sink.terminal(&record, &exec::grade(&record)).unwrap();
    } else {
        sink.last_progress_persist = Instant::now() - PROGRESS_CHECKPOINT_INTERVAL;
        sink.unknown_output(4, b"").unwrap();
    }
    drop(sink);
    drop(original);

    fs::write(&paths.stdout, b"out").unwrap();
    fs::write(&paths.stderr, b"err").unwrap();
    fs::write(paths.io_dir.join("remote-unknown-output"), b"unk").unwrap();
    let disk = read_task(&paths.json).unwrap();
    let restarted = registry();
    restarted
        .insert_rehydrated_task(disk, paths.clone(), false)
        .unwrap();
    let task = restarted.task(&task_id).unwrap();
    drop(TaskSink::new(&restarted, task.clone()).unwrap());

    (
        dir,
        restarted,
        task_id,
        paths,
        task,
        terminal_proof.then_some(record),
    )
}

fn assert_checkpoint_warning_is_visible(registry: &BgTaskRegistry, task_id: &str) {
    let task = registry.task(task_id).unwrap();
    let metadata = task.state.lock().unwrap().metadata.clone();
    let incomplete = metadata
        .remote
        .as_ref()
        .unwrap()
        .output_incomplete
        .as_ref()
        .expect("short checkpoint loss remains in remote metadata");
    let warning = incomplete.warning_line();
    let note = registry.execution_note(task_id, "session").unwrap();
    assert!(note.starts_with(&warning), "{note}");
    assert!(!metadata
        .execution_note
        .as_deref()
        .unwrap_or_default()
        .contains("Output incomplete"));
    let snapshot = registry.observed_status(task_id, "session", 8192).unwrap();
    assert_eq!(snapshot.info.status, BgTaskStatus::Completed);
    assert!(snapshot.output_preview.contains(&warning), "{snapshot:?}");
}

fn assert_checkpoint_warning_is_in_completion(registry: &BgTaskRegistry, task_id: &str) {
    let completion = registry
        .drain_completions()
        .into_iter()
        .find(|completion| completion.task_id == task_id)
        .expect("completed remote task queues a completion notice");
    assert!(
        completion
            .output_preview
            .contains("Output incomplete (remote_output_checkpoint_short)"),
        "{completion:?}"
    );
}

#[test]
fn exec_remote_bash_short_checkpoint_warning_survives_normal_completion() {
    let (_dir, registry, task_id, _paths, task, _) = recover_short_checkpoint(false);
    let mut sink = TaskSink::new(&registry, task.clone()).unwrap();
    let record = TerminalRecord::new(id(), Outcome::Exit { code: 0 }, 3, 0, 0);
    sink.terminal(&record, &exec::grade(&record)).unwrap();
    drop(sink);

    registry.remote_terminal(&task, exec::grade(&record), None);

    assert_checkpoint_warning_is_visible(&registry, &task_id);
    assert_checkpoint_warning_is_in_completion(&registry, &task_id);
}

#[test]
fn exec_remote_bash_short_checkpoint_warning_survives_saved_terminal_proof() {
    let (_dir, registry, task_id, _paths, task, record) = recover_short_checkpoint(true);
    registry.remote_terminal(&task, exec::grade(&record.unwrap()), None);

    assert_checkpoint_warning_is_visible(&registry, &task_id);
    assert_checkpoint_warning_is_in_completion(&registry, &task_id);
}

#[test]
fn exec_remote_bash_short_checkpoint_warning_survives_second_restart() {
    let (dir, task_registry, task_id, _paths, task, _) = recover_short_checkpoint(false);
    let mut sink = TaskSink::new(&task_registry, task.clone()).unwrap();
    let record = TerminalRecord::new(id(), Outcome::Exit { code: 0 }, 3, 0, 0);
    sink.terminal(&record, &exec::grade(&record)).unwrap();
    drop(sink);
    task_registry.remote_terminal(&task, exec::grade(&record), None);
    drop(task);
    drop(task_registry);

    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();

    assert_checkpoint_warning_is_visible(&restarted, &task_id);
}

#[test]
fn exec_remote_bash_old_record_without_output_incomplete_loads() {
    let dir = tempfile::tempdir().unwrap();
    let (_registry, _task_id, paths) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf remote");
    let bytes = fs::read(&paths.json).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(value["remote"].get("output_incomplete").is_none());

    let loaded = read_task(&paths.json).unwrap();

    assert!(loaded.remote.unwrap().output_incomplete.is_none());
}

#[test]
fn exec_remote_bash_restart_replays_uncheckpointed_output() {
    let dir = tempfile::tempdir().unwrap();
    let (original, task_id, paths) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf remote");
    let task = original.task(&task_id).unwrap();
    let mut sink = TaskSink::new(&original, task).unwrap();
    sink.started(&Started::new(0, 123, 1)).unwrap();
    sink.output(1, OutputStream::Stdout, b"uncheckpointed")
        .unwrap();
    sink.output(2, OutputStream::Stderr, b"stderr tail")
        .unwrap();
    sink.unknown_output(3, b"unknown tail").unwrap();
    drop(sink);
    drop(original);
    let disk = read_task(&paths.json).unwrap();
    assert_eq!(disk.remote.as_ref().unwrap().last_seq, Some(0));
    let restarted = registry();
    restarted
        .insert_rehydrated_task(disk, paths.clone(), false)
        .unwrap();
    let task = restarted.task(&task_id).unwrap();
    let mut sink = TaskSink::new(&restarted, task.clone()).unwrap();
    assert!(fs::read(&paths.stdout).unwrap().is_empty());
    assert!(fs::read(&paths.stderr).unwrap().is_empty());
    assert!(fs::read(paths.io_dir.join("remote-unknown-output"))
        .unwrap()
        .is_empty());
    let point = task
        .state
        .lock()
        .unwrap()
        .metadata
        .remote
        .as_ref()
        .unwrap()
        .point()
        .unwrap();
    let mut consumer = exec::StreamConsumer::resume(point);
    for (seq, stream, bytes) in [
        (1, OutputStream::Stdout, b"replayed".to_vec()),
        (2, OutputStream::Stderr, b"stderr".to_vec()),
    ] {
        consumer
            .consume(
                StreamRecord::Output(Output::new(seq, stream, BytePayload(bytes))),
                &mut sink,
            )
            .unwrap();
    }
    consumer
        .consume(
            StreamRecord::Terminal(TerminalRecord::new(
                id(),
                Outcome::Exit { code: 0 },
                1,
                0,
                0,
            )),
            &mut sink,
        )
        .unwrap();
    assert!(consumer.finish().is_ok());
    assert_eq!(fs::read(&paths.stdout).unwrap(), b"replayed");
    assert_eq!(fs::read(&paths.stderr).unwrap(), b"stderr");
    assert_eq!(
        read_task(&paths.json).unwrap().remote.unwrap().last_seq,
        Some(2)
    );
}

fn persist_accepted_with_snapshot(
    dir: &Path,
    connection: PathBuf,
    command: &str,
) -> (BgTaskRegistry, String, TaskPaths) {
    let registry = registry();
    let layout = allocate_task_layout(dir, "session").unwrap();
    let task_id = layout.paths.task_id.clone();
    let shell = resolve_posix_shell();
    let environment = BTreeMap::from([(
        std::ffi::OsString::from("ORIGINAL_PLAN"),
        std::ffi::OsString::from("host-proof"),
    )]);
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
        false,
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
        terminal: None,
        fallback_digest: Some(digest),
        env_not_forwarded: None,
        queue_position: None,
        output_received: false,
        retry: None,
        output_incomplete: None,
        workspace_holder: None,
        return_paths: Vec::new(),
        returned_files: None,
        return_first_seq: None,
        return_staging: Default::default(),
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

fn restarted_context(dir: &Path) -> crate::context::AppContext {
    restarted_context_with_runon(dir, true)
}

fn restarted_context_with_runon(dir: &Path, enabled: bool) -> crate::context::AppContext {
    ensure_git_fixture(dir);
    // A fresh configuration would choose ordinary unsandboxed inheritance;
    // the saved Host plan must still clear ambient HOME before its command.
    crate::context::AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(dir.into()),
            bash: crate::config::BashConfig {
                runon_enabled: enabled,
                ..Default::default()
            },
            sandbox: crate::config::SandboxConfig {
                ..Default::default()
            },
            ..Default::default()
        },
    )
}

#[tokio::test]
async fn exec_remote_bash_restart_refusal_uses_original_launch_plan() {
    let daemon = daemon(Script::AttachRefused, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let command =
        "if [ -z \"$HOME\" ]; then printf '%s' \"$ORIGINAL_PLAN\"; else printf wrong-plan; fi";
    let (original, task_id, paths) =
        persist_accepted_with_snapshot(dir.path(), daemon.connection.clone(), command);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(
                paths
                    .control_dir
                    .join(crate::sandbox_spawn::REMOTE_LOCAL_LAUNCH)
            )
            .unwrap()
            .permissions()
            .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&paths.control_dir)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
    drop(original);
    let ctx = restarted_context(dir.path());
    let restarted = ctx.bash_background();
    restarted.replay_session(dir.path(), "session").unwrap();
    let done = terminal(restarted, &task_id).await;
    assert_eq!(fs::read(&paths.stdout).unwrap(), b"host-proof");
    assert!(done.output_preview.contains(&format!(
        "ran locally on {}: remote refused: future_refusal",
        local_os_name()
    )));
    assert!(
        crate::bash_background::persistence::read_task_retrying_replacement_for_test(&paths.json)
            .unwrap()
            .local_fallback_started
    );
    assert!(!daemon
        .log
        .lock()
        .unwrap()
        .iter()
        .any(|(_, b)| b["method"] == "exec.run"));
}

#[tokio::test]
async fn runon_restart_executor_refusal_never_restores_local_launch() {
    // Cover both an attach that learns a refusal and a refusal already saved
    // before the previous process could decide whether to launch locally.
    for saved_terminal in [false, true] {
        let daemon = daemon(Script::AttachRefused, "exec-remote/v1").await;
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("must-not-run-after-restart");
        let command = format!("printf local-proof > '{}'", marker.display());
        let (original, task_id, paths) =
            persist_accepted_with_snapshot(dir.path(), daemon.connection.clone(), &command);
        let mut metadata = read_task(&paths.json).unwrap();
        let remote = metadata.remote.as_mut().unwrap();
        remote.explicit_runon = true;
        if saved_terminal {
            remote.terminal = Some(TerminalRecord::new(
                id(),
                Outcome::RefusedBeforeStart {
                    reason: RefusalReason::Unknown("future_refusal".into()),
                },
                1,
                0,
                0,
            ));
        }
        write_task(&paths.json, &metadata).unwrap();
        drop(original);
        let ctx = restarted_context(dir.path());
        let restarted = ctx.bash_background();
        restarted.replay_session(dir.path(), "session").unwrap();
        let done = terminal(restarted, &task_id).await;
        assert!(
            !marker.exists(),
            "explicit runon restored a local launch after restart"
        );
        assert_eq!(done.child_pid, None);
        assert!(!read_task(&paths.json).unwrap().local_fallback_started);
        assert_runon_refusal(&refusal_response(done), "future_refusal");
        assert!(
            exec_runs(&daemon).is_empty(),
            "restart must not resubmit remotely"
        );
        // The terminal refusal and its structured error also survive another restart.
        drop(ctx);
        let ctx = restarted_context(dir.path());
        ctx.bash_background()
            .replay_session(dir.path(), "session")
            .unwrap();
        let done = terminal(ctx.bash_background(), &task_id).await;
        assert_runon_refusal(&refusal_response(done), "future_refusal");
        assert!(!marker.exists());
    }
}

#[tokio::test]
async fn exec_remote_bash_post_spawn_persistence_failure_never_claims_no_run() {
    for restarted in [true, false] {
        let daemon = daemon(
            if restarted {
                Script::AttachRefused
            } else {
                Script::Refused
            },
            "exec-remote/v1",
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let counter = dir.path().join("started-count");
        let command = format!("printf x >> '{}'", counter.display());
        let (registry, task_id, paths) =
            persist_accepted_with_snapshot(dir.path(), daemon.connection.clone(), &command);
        let task = registry.task(&task_id).unwrap();
        post_spawn_failures()
            .lock()
            .unwrap()
            .insert(task_id.clone());
        if restarted {
            registry.resume_remote_task(&task_id).unwrap();
        } else {
            let layout = resolve_task_layout(&paths.session_dir, &task_id).unwrap();
            let remote = task.state.lock().unwrap().metadata.remote.clone().unwrap();
            let (plan, shell_path, env, linux_scope) = crate::sandbox_spawn::restore_local_launch(
                &layout,
                remote.fallback_digest.as_deref().unwrap(),
            )
            .unwrap();
            let request = exec::build_request(
                dir.path(),
                dir.path(),
                dir.path(),
                &command,
                BTreeMap::new(),
                Some(30),
                &exec::PresetParams::default(),
            );
            registry
                .start_remote_worker(
                    task,
                    Some((
                        request,
                        LocalFallback {
                            plan,
                            shell_path,
                            env,
                            linux_scope,
                            capture_pipeline: false,
                        },
                    )),
                )
                .unwrap();
        }
        let done = terminal(&registry, &task_id).await;
        assert_eq!(
            done.info.status,
            BgTaskStatus::FateUnknown,
            "restarted={restarted}: {done:?}"
        );
        assert!(
            done.output_preview
                .contains("the command started locally; AFT lost track of it"),
            "{done:?}"
        );
        assert!(!done.output_preview.contains("did not run"), "{done:?}");
        // `>>` creates the file before printf writes to it, so existence alone
        // can be observed while the file is still empty; wait for the byte.
        wait_for("fallback must write its execution marker", || {
            fs::read(&counter).ok().filter(|bytes| !bytes.is_empty())
        })
        .await;
        assert_eq!(fs::read(&counter).unwrap(), b"x");
        let replay = super::tests::registry();
        replay.replay_session(dir.path(), "session").unwrap();
        let replayed = terminal(&replay, &task_id).await;
        assert_eq!(replayed.info.status, BgTaskStatus::FateUnknown);
        assert_eq!(fs::read(&counter).unwrap(), b"x");
    }
}

#[tokio::test]
async fn exec_remote_bash_restart_snapshot_tampering_refuses_without_run() {
    for missing in [false, true] {
        let daemon = daemon(Script::AttachRefused, "exec-remote/v1").await;
        let dir = tempfile::tempdir().unwrap();
        let (original, task_id, paths) = persist_accepted_with_snapshot(
            dir.path(),
            daemon.connection.clone(),
            "printf must-not-run",
        );
        drop(original);
        let snapshot = paths
            .control_dir
            .join(crate::sandbox_spawn::REMOTE_LOCAL_LAUNCH);
        if missing {
            fs::remove_file(&snapshot).unwrap();
        } else {
            let mut value: serde_json::Value =
                serde_json::from_slice(&fs::read(&snapshot).unwrap()).unwrap();
            value["policy"] = serde_json::json!("Unsandboxed");
            fs::write(&snapshot, serde_json::to_vec(&value).unwrap()).unwrap();
        }
        let ctx = restarted_context(dir.path());
        let restarted = ctx.bash_background();
        restarted.replay_session(dir.path(), "session").unwrap();
        let done = terminal(restarted, &task_id).await;
        assert_eq!(done.info.status, BgTaskStatus::Failed);
        assert!(done.output_preview.contains(
            "local fallback could not be restored after restart, so the command did not run"
        ));
        assert!(fs::read(&paths.stdout).unwrap().is_empty());
    }
}

#[tokio::test]
async fn exec_remote_bash_double_restart_starts_local_fallback_once() {
    let daemon = daemon(Script::AttachRefused, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let counter = dir.path().join("local-runs");
    let release = dir.path().join("release");
    let _release_on_failure = ReleaseOnDrop(release.clone());
    let command = format!(
        "printf x >> '{}'; while [ ! -f '{}' ]; do /bin/sleep 0.01; done",
        counter.display(),
        release.display()
    );
    let (original, task_id, paths) =
        persist_accepted_with_snapshot(dir.path(), daemon.connection.clone(), &command);
    let stale = crate::bash_background::persistence::read_task(&paths.json).unwrap();
    let db = Arc::new(Mutex::new(
        crate::db::open(&dir.path().join("aft.db")).unwrap(),
    ));
    crate::db::bash_tasks::upsert_bash_task(
        &db.lock().unwrap(),
        &stale.to_bash_task_row("runner", &paths).unwrap(),
    )
    .unwrap();
    drop(original);
    crash_tasks().lock().unwrap().insert(task_id.clone());
    let first = registry();
    first.replay_session(dir.path(), "session").unwrap();
    // Wait for the byte, not the file: `>>` creates it before printf writes.
    wait_for("fallback must write its execution marker", || {
        fs::read(&counter).ok().filter(|bytes| !bytes.is_empty())
    })
    .await;
    first.inner.shutdown.store(true, Ordering::SeqCst);
    drop(first);
    let second = registry();
    second.set_harness(crate::harness::Harness::Runner);
    second.set_db_pool(db.clone());
    second.replay_session(dir.path(), "session").unwrap();
    fs::write(&release, b"release").unwrap();
    let _ = terminal(&second, &task_id).await;
    assert_eq!(
        fs::read(&counter).unwrap(),
        b"x",
        "a replay must not start another local fallback"
    );
    second.inner.shutdown.store(true, Ordering::SeqCst);
    drop(second);
    let third = registry();
    third.set_harness(crate::harness::Harness::Runner);
    third.set_db_pool(db);
    third.replay_session(dir.path(), "session").unwrap();
    let _ = terminal(&third, &task_id).await;
    assert_eq!(fs::read(&counter).unwrap(), b"x");
}

#[tokio::test]
async fn exec_remote_bash_no_policy_and_disabled_are_byte_identical_local_outputs() {
    let mut outputs = Vec::new();
    for policy in [None, Some(false)] {
        let dir = tempfile::tempdir().unwrap();
        let ctx = restarted_context(dir.path());
        let request = crate::protocol::RawRequest {
            id: "parity".into(),
            command: "bash".into(),
            session_id: Some("session".into()),
            lsp_hints: None,
            params: serde_json::json!({"command":"printf local-proof","compressed":false}),
        };
        let mut frozen = launch(dir.path().join("absent-connection"));
        frozen.params.remote_exec.as_mut().unwrap().enabled = false;
        let response = crate::bash_background::with_remote_policy(policy.map(|_| frozen), || {
            crate::commands::bash::handle(&request, &ctx)
        });
        assert!(response.success, "{response:?}");
        let task = response.data["task_id"].as_str().unwrap();
        let done = terminal(ctx.bash_background(), task).await;
        let task = ctx.bash_background().task(task).unwrap();
        assert!(task.state.lock().unwrap().metadata.remote.is_none());
        outputs.push((
            fs::read(&task.paths.stdout).unwrap(),
            fs::read(&task.paths.stderr).unwrap(),
            crate::commands::bash_orchestrate::format_foreground_result(&done),
        ));
    }
    assert_eq!(outputs[0], outputs[1]);
    assert_eq!(outputs[0].0, b"local-proof");
    assert_eq!(outputs[0].2, "local-proof");
}

#[test]
fn exec_remote_bash_unknown_output_bytes_are_durable_without_misattribution() {
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, paths) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "printf unused");
    let task = registry.task(&task_id).unwrap();
    let mut sink = TaskSink::new(&registry, task.clone()).unwrap();
    sink.last_progress_persist = Instant::now() - PROGRESS_CHECKPOINT_INTERVAL;
    sink.unknown_output(0, b"future").unwrap();
    drop(sink);
    let mut sink = TaskSink::new(&registry, task).unwrap();
    sink.output(1, OutputStream::Stdout, b"ordinary").unwrap();
    assert_eq!(
        fs::read(paths.io_dir.join("remote-unknown-output")).unwrap(),
        b"future"
    );
    assert_eq!(fs::read(&paths.stdout).unwrap(), b"ordinary");
    assert!(fs::read(&paths.stderr).unwrap().is_empty());
}

#[test]
fn exec_remote_bash_only_not_sent_errors_prove_local_fallback_safe() {
    assert!(proves_no_start(&exec::Error::Transport(
        subc_client_rs::CallError::NotSent(Box::new(io::Error::other("not sent")))
    )));
    assert!(!proves_no_start(&exec::Error::Transport(
        subc_client_rs::CallError::OutcomeUnknown(Box::new(io::Error::other(
            "sent; transport lost"
        )))
    )));
    assert!(!proves_no_start(&exec::Error::RecoveryRequired {
        resume: Some(ResumePoint {
            job_id: id(),
            last_seq: Some(3),
            gap_recovery: None,
        }),
        reason: "lost".into()
    }));
}

#[tokio::test]
async fn exec_remote_bash_refusal_queue_wait_is_not_local_run_time() {
    let daemon = daemon(Script::AttachRefused, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let release = dir.path().join("release");
    let _release_on_failure = ReleaseOnDrop(release.clone());
    let command = format!(
        "while [ ! -f '{}' ]; do /bin/sleep 0.01; done; printf budget-proof",
        release.display()
    );
    let (original, task_id, paths) =
        persist_accepted_with_snapshot(dir.path(), daemon.connection.clone(), &command);
    let layout = resolve_task_layout(&paths.session_dir, &task_id).unwrap();
    let mut metadata = read_task_at(&layout).unwrap();
    metadata.started_at = unix_millis().saturating_sub(60000);
    metadata.timeout_ms = Some(15000);
    write_task_at(&layout, &metadata).unwrap();
    drop(original);
    let restarted = registry();
    let replay_started_at = unix_millis();
    restarted.replay_session(dir.path(), "session").unwrap();
    let local_started_at = wait_for("fallback must publish its local start", || {
        let task = restarted.task(&task_id).unwrap();
        let state = task.state.lock().unwrap();
        state
            .metadata
            .local_fallback_started
            .then_some(state.metadata.started_at)
    })
    .await;
    assert!(
        local_started_at >= replay_started_at,
        "the minute in the remote queue cannot consume the local run budget"
    );
    fs::write(&release, b"release").unwrap();
    assert_eq!(
        terminal(&restarted, &task_id).await.info.status,
        BgTaskStatus::Completed
    );
    assert_eq!(fs::read(&paths.stdout).unwrap(), b"budget-proof");
}

#[tokio::test]
async fn exec_remote_bash_durable_cancel_before_refusal_suppresses_fallback() {
    let daemon = daemon(Script::AttachRefused, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let (original, task_id, paths) = persist_accepted_with_snapshot(
        dir.path(),
        daemon.connection.clone(),
        "printf must-not-run",
    );
    let task = original.task(&task_id).unwrap();
    assert!(original.record_remote_cancel(&task).unwrap());
    assert!(
        crate::bash_background::persistence::read_task(&paths.json)
            .unwrap()
            .remote
            .unwrap()
            .cancel_requested
    );
    drop(task);
    drop(original);
    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    let done = terminal(&restarted, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Failed);
    assert!(done
        .output_preview
        .contains("local fallback skipped because cancellation was requested"));
    assert!(fs::read(&paths.stdout).unwrap().is_empty());
    let task = restarted.task(&task_id).unwrap();
    let state = task.state.lock().unwrap();
    assert!(matches!(
        state
            .metadata
            .remote
            .as_ref()
            .unwrap()
            .terminal
            .as_ref()
            .unwrap()
            .outcome,
        Outcome::RefusedBeforeStart { .. }
    ));
    assert!(!state.metadata.local_fallback_started);

    // The launch decision reads the durable record, even if another in-memory
    // view has not observed that cancellation yet.
    let dir = tempfile::tempdir().unwrap();
    let (stale, task_id, paths) = persist_accepted_with_snapshot(
        dir.path(),
        dir.path().join("unused"),
        "printf must-not-run",
    );
    let task = stale.task(&task_id).unwrap();
    assert!(stale.record_remote_cancel(&task).unwrap());
    let remote = task.state.lock().unwrap().metadata.remote.clone().unwrap();
    task.state
        .lock()
        .unwrap()
        .metadata
        .remote
        .as_mut()
        .unwrap()
        .cancel_requested = false;
    stale
        .restored_remote_fallback(&task, &remote, "future_refusal", None)
        .unwrap();
    assert!(fs::read(&paths.stdout).unwrap().is_empty());
    assert!(!task.state.lock().unwrap().metadata.local_fallback_started);
}

#[tokio::test]
async fn exec_remote_bash_fallback_marker_before_cancel_uses_local_kill() {
    let daemon = daemon(Script::AttachRefused, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let (original, task_id, paths) =
        persist_accepted_with_snapshot(dir.path(), daemon.connection.clone(), "/bin/sleep 30");
    drop(original);
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    marker_gates().lock().unwrap().insert(
        task_id.clone(),
        MarkerGate {
            entered: entered_tx,
            release: release_rx,
        },
    );
    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    tokio::task::spawn_blocking(move || entered_rx.recv_timeout(Duration::from_secs(10)).unwrap())
        .await
        .unwrap();
    let metadata = crate::bash_background::persistence::read_task(&paths.json).unwrap();
    assert!(metadata.local_fallback_started);
    assert!(metadata.remote.is_none());
    let killing = restarted.clone();
    let id = task_id.clone();
    let (invoked_tx, invoked_rx) = std::sync::mpsc::channel();
    let kill = tokio::task::spawn_blocking(move || {
        invoked_tx.send(()).unwrap();
        killing.kill(&id, "session").unwrap()
    });
    tokio::task::spawn_blocking(move || invoked_rx.recv_timeout(Duration::from_secs(10)).unwrap())
        .await
        .unwrap();
    release_tx.send(()).unwrap();
    assert_eq!(kill.await.unwrap().info.status, BgTaskStatus::Killed);
    assert!(!daemon
        .log
        .lock()
        .unwrap()
        .iter()
        .any(|(_, b)| b["method"] == "exec.cancel"));
}

/// Run `params` through the bash command handler with `policy` installed as
/// the session's remote policy, as the subc tool-call path does.
fn handle_with_policy(
    ctx: &crate::context::AppContext,
    policy: Option<crate::bash_background::RemoteLaunch>,
    mut params: serde_json::Value,
) -> crate::protocol::Response {
    params
        .as_object_mut()
        .unwrap()
        .entry("workdir")
        .or_insert_with(|| serde_json::json!(ctx.config().project_root));
    let request = crate::protocol::RawRequest {
        id: "runon".into(),
        command: "bash".into(),
        session_id: Some("session".into()),
        lsp_hints: None,
        params,
    };
    crate::bash_background::with_remote_policy(policy, || {
        crate::commands::bash::handle(&request, ctx)
    })
}

fn exec_runs(daemon: &crate::exec_remote::wire_tests::Daemon) -> Vec<serde_json::Value> {
    daemon
        .log
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, body)| body["method"] == "exec.run")
        .map(|(_, body)| body["params"].clone())
        .collect()
}

#[tokio::test]
async fn runon_windows_not_advertised_refuses_without_remote_or_local_spawn() {
    let daemon = daemon(Script::Plain, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    let marker = dir.path().join("local-spawn-proof");
    let (reply, _) = orchestrated_reply(
        &ctx,
        &daemon,
        serde_json::json!({
            "command": format!("touch '{}'", marker.display()),
            "runon": "windows", "wait": true, "foreground_orchestrate": true,
            "compressed": false,
        }),
    )
    .await;
    assert!(!reply.success, "{reply:?}");
    assert_eq!(reply.data["code"], "remote_unavailable", "{reply:?}");
    let message = reply.data["message"].as_str().unwrap();
    assert!(message.contains("windows"), "{message}");
    assert!(
        message.contains("doesn't offer Windows jobs yet"),
        "{message}"
    );
    assert!(
        exec_runs(&daemon).is_empty(),
        "unadvertised Windows must never be sent"
    );
    assert!(
        !marker.exists(),
        "explicit Windows must never fall back locally"
    );
}

async fn windows_daemon(
    code: Option<u32>,
    acknowledge: bool,
) -> crate::exec_remote::wire_tests::Daemon {
    crate::exec_remote::wire_tests::daemon_with_capabilities(
        Script::Windows { code, acknowledge },
        &["exec-remote/v1", "exec-remote-windows/v1"],
        1,
    )
    .await
}

#[tokio::test]
async fn runon_windows_advertised_preserves_request_mapping_and_full_exit_code() {
    for code in [256, 0xC000_0005, u32::MAX] {
        let daemon = windows_daemon(Some(code), true).await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = restarted_context(dir.path());
        let storage = dir.path().join("storage");
        ctx.update_config(|config| config.storage_dir = Some(storage.clone()));
        let (reply, _) = orchestrated_reply(
            &ctx,
            &daemon,
            serde_json::json!({
                "command": "cargo test", "runon": "windows", "wait": true,
                "foreground_orchestrate": true, "compressed": false,
            }),
        )
        .await;
        assert!(reply.success, "{reply:?}");
        assert_eq!(reply.data["status"], "failed", "{reply:?}");
        assert_eq!(reply.data["exit_code"], u64::from(code), "{reply:?}");
        assert!(
            reply.data["output"]
                .as_str()
                .unwrap()
                .contains(&format!("[exit code: {code}]")),
            "{reply:?}"
        );
        let runs = exec_runs(&daemon);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0]["platform"], "windows");
        assert!(runs[0].get("weight_hint").is_none());
        assert!(runs[0].get("network").is_none());
        let task_id = reply.data["task_id"].as_str().unwrap();
        let task = ctx.bash_background().task(task_id).unwrap();
        let saved = read_task(&task.paths.json).unwrap();
        assert_eq!(saved.exit_code, Some(i64::from(code)));
        let conn = crate::db::open(&dir.path().join("roundtrip.db")).unwrap();
        let row = saved.to_bash_task_row("broca", &task.paths).unwrap();
        crate::db::bash_tasks::upsert_bash_task(&conn, &row).unwrap();
        let restored_row = crate::db::bash_tasks::get_bash_task(&conn, "broca", "session", task_id)
            .unwrap()
            .unwrap();
        assert_eq!(restored_row.exit_code, Some(i64::from(code)));
        assert_eq!(
            saved
                .remote
                .as_ref()
                .unwrap()
                .started
                .as_ref()
                .unwrap()
                .path_mapping,
            Some(PathMapping::new("/", r"C:\m"))
        );
        let restarted = registry();
        restarted.replay_session(&storage, "session").unwrap();
        let snapshot = restarted.observed_status(task_id, "session", 8192).unwrap();
        assert_eq!(snapshot.exit_code, Some(i64::from(code)));
        let completions = restarted.drain_completions();
        assert!(
            completions
                .iter()
                .any(|completion| completion.task_id == task_id
                    && completion.exit_code == Some(i64::from(code))),
            "{completions:?}"
        );
    }
}

#[tokio::test]
async fn runon_windows_unknown_offline_outcome_keeps_safe_rerun_advice() {
    let daemon = windows_daemon(None, true).await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    let (reply, _) = orchestrated_reply(
        &ctx,
        &daemon,
        serde_json::json!({
            "command": "cargo test", "runon": "windows", "wait": true,
            "foreground_orchestrate": true, "compressed": false,
        }),
    )
    .await;
    assert_eq!(reply.data["status"], "fate_unknown", "{reply:?}");
    assert!(
        reply.data["output"]
            .as_str()
            .unwrap()
            .contains("rerunning is safe"),
        "{reply:?}"
    );
    assert_eq!(exec_runs(&daemon).len(), 1);
}

#[tokio::test]
async fn runon_windows_unacknowledged_platform_cancels_and_distrusts_exit() {
    let daemon = windows_daemon(Some(0), false).await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    let (reply, _) = orchestrated_reply(
        &ctx,
        &daemon,
        serde_json::json!({
            "command": "cargo test", "runon": "windows", "wait": true,
            "foreground_orchestrate": true, "compressed": false,
        }),
    )
    .await;
    assert_eq!(reply.data["status"], "fate_unknown", "{reply:?}");
    assert!(reply.data["exit_code"].is_null(), "{reply:?}");
    assert!(
        reply.data["output"]
            .as_str()
            .unwrap()
            .contains("Windows platform was not acknowledged"),
        "{reply:?}"
    );
    assert!(daemon
        .log
        .lock()
        .unwrap()
        .iter()
        .any(|(_, body)| body["method"] == "exec.cancel"));
    assert_eq!(exec_runs(&daemon).len(), 1);
}

#[tokio::test]
async fn runon_network_is_present_on_wire_only_when_requested() {
    for (runon, network, cpus) in [
        ("linux", None, None),
        ("linux,4c", None, Some(4)),
        ("linux,net", Some("outbound"), None),
        ("linux,4c,net", Some("outbound"), Some(4)),
        ("linux,net,4c", Some("outbound"), Some(4)),
        ("", Some("outbound"), Some(4)),
    ] {
        let daemon = daemon(Script::NetworkGranted, "exec-remote/v1").await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = restarted_context(dir.path());
        let mut policy = launch(daemon.connection.clone());
        policy.params.remote_exec.as_mut().unwrap().default_demand = Some("linux,net,4c".into());
        let response = handle_with_policy(
            &ctx,
            Some(policy),
            serde_json::json!({
                "command": "printf network", "runon": runon, "compressed": false
            }),
        );
        assert!(response.success, "{runon}: {response:?}");
        let done = terminal(
            ctx.bash_background(),
            response.data["task_id"].as_str().unwrap(),
        )
        .await;
        let runs = exec_runs(&daemon);
        assert_eq!(runs.len(), 1);
        let captured: RunRequest = serde_json::from_value(runs[0].clone()).unwrap();
        assert_eq!(
            captured.network,
            network.map(|_| Network::Outbound),
            "{runon}: {runs:?}"
        );
        assert_eq!(
            runs[0].get("network").and_then(serde_json::Value::as_str),
            network,
            "{runon}: {runs:?}"
        );
        assert_eq!(captured.weight_hint, cpus, "{runon}: {runs:?}");
        if network.is_some() {
            let expected = if cpus.is_some() {
                "ran remotely on ck-motor (4 vCPUs requested, job 0192a64a-1234-7000-8000-000000000001, network)"
            } else {
                "ran remotely on ck-motor (job 0192a64a-1234-7000-8000-000000000001, network)"
            };
            assert_eq!(done.output_preview.lines().next(), Some(expected));
            assert!(!done.output_preview.contains("OFFLINE"), "{done:?}");
        }
    }
}

#[tokio::test]
async fn runon_timeout_uses_the_task_limit_and_preserves_long_explicit_timeouts() {
    for (timeout_ms, expected_seconds) in [(None, 30 * 60), (Some(7_201_000), 7_201)] {
        let daemon = daemon(Script::Plain, "exec-remote/v1").await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = restarted_context(dir.path());
        let mut params = serde_json::json!({
            "command": "printf timeout-proof",
            "runon": "linux,4c",
            "compressed": false
        });
        if let Some(timeout_ms) = timeout_ms {
            params["timeout"] = serde_json::json!(timeout_ms);
        }

        let response = handle_with_policy(&ctx, Some(launch(daemon.connection.clone())), params);
        assert!(response.success, "{response:?}");
        let done = terminal(
            ctx.bash_background(),
            response.data["task_id"].as_str().unwrap(),
        )
        .await;
        assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");

        let runs = exec_runs(&daemon);
        assert_eq!(runs.len(), 1);
        let captured: crate::exec_remote::types::RunRequest =
            serde_json::from_value(runs[0].clone()).unwrap();
        assert_eq!(
            captured.timeout,
            Some(expected_seconds),
            "the protocol timeout is required and measured in seconds: {runs:?}"
        );
        assert_eq!(runs[0]["timeout"], expected_seconds);
        assert_eq!(captured.weight_hint, Some(4));
    }
}

#[tokio::test]
async fn local_bash_keeps_its_caller_timeout_without_remote_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context_with_runon(dir.path(), false);
    let response = handle_with_policy(
        &ctx,
        None,
        serde_json::json!({
            "command": "printf local-timeout-proof",
            "timeout": 10_000,
            "compressed": false
        }),
    );
    assert!(response.success, "{response:?}");
    let task_id = response.data["task_id"].as_str().unwrap();
    let done = terminal(ctx.bash_background(), task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");
    assert_eq!(done.output_preview, "local-timeout-proof");

    let task = ctx.bash_background().task(task_id).unwrap();
    let state = task.state.lock().unwrap();
    assert!(state.metadata.remote.is_none());
    assert_eq!(state.metadata.timeout_ms, Some(10_000));
}

#[tokio::test]
async fn runon_network_not_granted_is_plainly_offline_in_reply_and_persistence() {
    for script in [Script::Plain, Script::NetworkUnknown] {
        let daemon = daemon(script, "exec-remote/v1").await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = restarted_context(dir.path());
        let response = handle_with_policy(
            &ctx,
            Some(launch(daemon.connection.clone())),
            serde_json::json!({
                "command": "printf offline", "runon": "linux,net", "compressed": false
            }),
        );
        assert!(response.success, "{response:?}");
        let task_id = response.data["task_id"].as_str().unwrap();
        let done = terminal(ctx.bash_background(), task_id).await;
        assert_eq!(done.info.status, BgTaskStatus::Completed);
        let expected = "OFFLINE: the runner did not grant the requested network access";
        assert!(done.output_preview.contains(expected), "{done:?}");
        assert!(
            crate::commands::bash_orchestrate::format_foreground_result(&done).contains(expected)
        );
        let task = ctx.bash_background().task(task_id).unwrap();
        let persisted = read_task(&task.paths.json).unwrap();
        assert!(persisted.execution_note.unwrap().contains(expected));
        let remote = persisted.remote.unwrap();
        assert!(remote.requested_network);
        assert!(!remote.granted_network);
    }
}

#[tokio::test]
async fn runon_network_unsupported_refuses_by_name_without_local_spawn() {
    let daemon = daemon(Script::NetworkUnsupported, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    let marker = dir.path().join("must-not-run");
    let response = handle_with_policy(
        &ctx,
        Some(launch(daemon.connection.clone())),
        serde_json::json!({
            "command": format!("touch '{}'", marker.display()), "runon": "linux,net"
        }),
    );
    assert!(response.success, "{response:?}");
    let done = terminal(
        ctx.bash_background(),
        response.data["task_id"].as_str().unwrap(),
    )
    .await;
    let refusal = refusal_response(done);
    assert_runon_refusal(
        &refusal,
        "network_unsupported (outbound access disabled by runner policy)",
    );
    assert!(
        refusal.data["message"]
            .as_str()
            .unwrap()
            .contains("outbound access disabled by runner policy"),
        "{refusal:?}"
    );
    assert!(!marker.exists());
    assert_eq!(exec_runs(&daemon).len(), 1);
}

#[tokio::test]
async fn runon_network_unknown_outcome_warns_only_for_granted_outbound() {
    for (script, runon, granted) in [
        (Script::NetworkLost, "linux,net", true),
        (Script::Lost, "linux,net", false),
        (Script::Lost, "linux", false),
    ] {
        let daemon = daemon(script, "exec-remote/v1").await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = restarted_context(dir.path());
        let response = handle_with_policy(
            &ctx,
            Some(launch(daemon.connection.clone())),
            serde_json::json!({
                "command": "printf uncertain", "runon": runon, "compressed": false
            }),
        );
        assert!(response.success, "{response:?}");
        let task_id = response.data["task_id"].as_str().unwrap();
        let done = terminal(ctx.bash_background(), task_id).await;
        assert_eq!(done.info.status, BgTaskStatus::FateUnknown);
        let text = crate::commands::bash_orchestrate::format_foreground_result(&done);
        assert!(
            text.contains(&format!("remote outcome unknown (job {})", id())),
            "{text}"
        );
        assert_eq!(text.contains("rerunning is safe"), !granted, "{text}");
        assert_eq!(
            text.contains("may have had outside effects"),
            granted,
            "{text}"
        );
        assert_eq!(text.contains("check before rerunning"), granted, "{text}");
        assert_eq!(
            exec_runs(&daemon).len(),
            1,
            "unknown outcomes must never be resubmitted"
        );
        let task = ctx.bash_background().task(task_id).unwrap();
        let persisted = read_task(&task.paths.json).unwrap();
        assert_eq!(persisted.remote.unwrap().granted_network, granted);
        assert_eq!(persisted.status_reason, done.info.status_reason);
    }
}

#[tokio::test]
async fn exec_remote_started_sets_running_phase_without_aft_timeout() {
    let daemon = daemon(Script::Started, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    let running = wait_for("Started must publish the running phase", || {
        let snapshot = registry.observed_status(&task_id, "session", 8192).unwrap();
        (snapshot.phase == Some("running")).then_some(snapshot)
    })
    .await;
    assert_eq!(running.info.status, BgTaskStatus::Running);
    assert_eq!(serde_json::to_value(&running).unwrap()["phase"], "running");
    assert!(running.hard_kill.is_none());
    let task = registry.task(&task_id).unwrap();
    let persisted = read_task(&task.paths.json).unwrap();
    let started = persisted.remote.as_ref().unwrap().started.as_ref().unwrap();
    assert_eq!(
        (started.seq, started.queue_wait_ms, started.started_at_ms),
        (0, 123, 1)
    );
    assert_eq!(persisted.remote.as_ref().unwrap().last_seq, Some(0));
    assert!(
        persisted.started_at > 1,
        "runner time must not reset the local start or timeout"
    );
    assert!(running.info.status_reason.is_none());
    let killing = registry.clone();
    tokio::task::spawn_blocking(move || killing.kill_remote_task(&task).unwrap())
        .await
        .unwrap();
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Killed);
    assert!(done.phase.is_none());
    assert_eq!(exec_runs(&daemon).len(), 1);
}

#[tokio::test]
async fn runon_sends_the_whole_compound_line_remote_exactly_as_written() {
    let daemon = daemon(Script::Plain, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = remote_test_context(dir.path());
    // Pipes, an environment prefix, a list and a command the old prefix
    // matcher would never have sent anywhere: all of it goes as one line.
    let line = "BUILD_FLAVOR=ci git status | tr a-z A-Z && printf '%s' \"$HOME\" ; ls | wc -l";
    let response = handle_with_policy(
        &ctx,
        Some(launch(daemon.connection.clone())),
        serde_json::json!({"command": line, "runon": "linux", "compressed": false}),
    );
    assert!(response.success, "{response:?}");
    let task_id = response.data["task_id"].as_str().unwrap().to_string();
    let done = terminal(ctx.bash_background(), &task_id).await;
    let runs = exec_runs(&daemon);
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0]["command"], line);
    assert!(runs[0].get("weight_hint").is_none(), "{runs:?}");
    assert_eq!(
        Path::new(runs[0]["cwd"].as_str().unwrap()),
        dir.path(),
        "the remote run uses the call's working directory"
    );
    let rendered = crate::commands::bash_orchestrate::format_foreground_result(&done);
    assert_eq!(
        rendered.lines().next(),
        Some("ran remotely on ck-motor (job 0192a64a-1234-7000-8000-000000000001)"),
        "{rendered}"
    );
    // A report without a changed-file list says so, after the output.
    assert!(
        rendered.ends_with(
            "changed files: not reported by the runner\n\
             git state: not reported by the runner\n\
             untracked files: not reported by the runner\n\
             ignored writes: not reported by the runner"
        ),
        "{rendered}"
    );
}

#[tokio::test]
async fn runon_vcpu_count_is_sent_as_weight_hint_on_the_wire() {
    let daemon = daemon(Script::Plain, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    let response = handle_with_policy(
        &ctx,
        Some(launch(daemon.connection.clone())),
        serde_json::json!({"command": "printf vcpu", "runon": "linux,4c", "compressed": false}),
    );
    assert!(response.success, "{response:?}");
    let task_id = response.data["task_id"].as_str().unwrap().to_string();
    let done = terminal(ctx.bash_background(), &task_id).await;
    let runs = exec_runs(&daemon);
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0]["weight_hint"], 4);
    let rendered = crate::commands::bash_orchestrate::format_foreground_result(&done);
    assert_eq!(
        rendered.lines().next(),
        Some("ran remotely on ck-motor (4 vCPUs requested, job 0192a64a-1234-7000-8000-000000000001)"),
        "{rendered}"
    );
}

#[tokio::test]
async fn runon_uses_vcpu_count_from_plan_default_demand() {
    let daemon = daemon(Script::Plain, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    let mut policy = launch(daemon.connection.clone());
    policy.params.remote_exec.as_mut().unwrap().default_demand = Some("linux,4c".into());
    let response = handle_with_policy(
        &ctx,
        Some(policy),
        serde_json::json!({"command": "printf default", "runon": "", "compressed": false}),
    );
    assert!(response.success, "{response:?}");
    let task_id = response.data["task_id"].as_str().unwrap().to_string();
    let _done = terminal(ctx.bash_background(), &task_id).await;
    let runs = exec_runs(&daemon);
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0]["weight_hint"], 4);
}

#[tokio::test]
async fn malformed_runon_vcpu_requests_are_refused_before_dispatch() {
    let daemon = daemon(Script::Plain, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("ran-locally");
    let command = format!("printf x > '{}'", marker.display());
    for runon in [
        "linux,0c",
        "linux,4",
        "linux,4cores",
        "linux,c",
        "linux,4c,2c",
        "linux,net,net",
        "linux,unknown",
        "windows,4c",
        "windows,net",
        "windows,wat",
    ] {
        let response = handle_with_policy(
            &restarted_context(dir.path()),
            Some(launch(daemon.connection.clone())),
            serde_json::json!({"command": command.clone(), "runon": runon}),
        );
        assert!(!response.success, "{runon}: {response:?}");
        let message = response.data["message"].as_str().unwrap_or_default();
        assert!(message.contains(runon), "{runon}: {response:?}");
        let reason = if runon == "windows,4c" {
            "fixed at 4 vCPUs"
        } else if runon == "windows,net" {
            "no network"
        } else {
            "not valid"
        };
        assert!(message.contains(reason), "{runon}: {response:?}");
    }
    assert!(exec_runs(&daemon).is_empty());
    assert!(!marker.exists());
}

#[tokio::test]
async fn old_shape_prefix_routing_survives_the_runon_kill_switch_and_runon_works_when_enabled() {
    for enabled in [false, true] {
        let daemon =
            crate::exec_remote::wire_tests::daemon_with_clients(Script::Plain, "exec-remote/v1", 2)
                .await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = remote_test_context_with_runon(dir.path(), enabled);
        let mut old = launch(daemon.connection.clone());
        old.params.remote_exec.as_mut().unwrap().legacy_commands =
            Some(serde_json::json!(["cd", "cargo test"]));
        let plain = handle_with_policy(
            &ctx,
            Some(old.clone()),
            serde_json::json!({"command":"cargo test", "compressed":false}),
        );
        assert!(plain.success, "{plain:?}");
        let done = terminal(
            ctx.bash_background(),
            plain.data["task_id"].as_str().unwrap(),
        )
        .await;
        assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");
        assert_eq!(
            exec_runs(&daemon).len(),
            1,
            "plain cargo must keep using the deployed route"
        );
        let whole = handle_with_policy(
            &ctx,
            Some(old),
            serde_json::json!({"command":"FOO=1 cargo test | tail -1", "runon":"linux"}),
        );
        if enabled {
            assert!(whole.success, "{whole:?}");
            let done = terminal(
                ctx.bash_background(),
                whole.data["task_id"].as_str().unwrap(),
            )
            .await;
            assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");
            assert_eq!(exec_runs(&daemon).len(), 2);
        } else {
            assert!(!whole.success);
            assert!(whole.data["message"]
                .as_str()
                .unwrap()
                .contains("bash.runon_enabled"));
            assert_eq!(exec_runs(&daemon).len(), 1);
        }
    }
}

#[tokio::test]
async fn runon_is_refused_when_the_daemon_has_no_exec_remote_provider() {
    let daemon = daemon(Script::Plain, "exec.remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("must-not-run-locally");
    let ctx = restarted_context(dir.path());
    let response = handle_with_policy(
        &ctx,
        Some(launch(daemon.connection.clone())),
        serde_json::json!({
            "command": format!("echo SHOULD_NOT_RUN > '{}'", marker.display()),
            "runon": "linux"
        }),
    );
    assert!(response.success, "{response:?}");
    let task = response.data["task_id"].as_str().unwrap();
    let snapshot = terminal(ctx.bash_background(), task).await;
    assert_eq!(snapshot.info.status, BgTaskStatus::Failed);
    let rendered = crate::commands::bash_orchestrate::format_foreground_result(&snapshot);
    assert!(rendered.contains("runon refused"), "{rendered}");
    assert!(rendered.contains("exec-remote/v1"), "{rendered}");
    assert!(!rendered.contains("ran locally"), "{rendered}");
    assert!(exec_runs(&daemon).is_empty());
    assert!(
        !marker.exists(),
        "missing providers must not cause local execution"
    );
}

#[tokio::test]
async fn without_runon_new_shape_plans_keep_commands_local() {
    let daemon = daemon(Script::Plain, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    // New-shape plans have no prefix list and require an explicit remote demand.
    let mut policy = launch(daemon.connection.clone());
    policy.params.remote_exec.as_mut().unwrap().default_demand = Some("linux".into());
    let response = handle_with_policy(
        &ctx,
        Some(policy),
        serde_json::json!({"command": "printf local-proof", "compressed": false}),
    );
    assert!(response.success, "{response:?}");
    let task_id = response.data["task_id"].as_str().unwrap().to_string();
    let done = terminal(ctx.bash_background(), &task_id).await;
    assert!(exec_runs(&daemon).is_empty());
    let task = ctx.bash_background().task(&task_id).unwrap();
    assert!(task.state.lock().unwrap().metadata.remote.is_none());
    assert_eq!(
        crate::commands::bash_orchestrate::format_foreground_result(&done),
        "local-proof"
    );
}

#[tokio::test]
async fn runon_is_refused_by_name_whenever_it_cannot_run_remotely() {
    let daemon = daemon(Script::Plain, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("ran-locally");
    let command = format!("printf x > '{}'", marker.display());
    let enabled = || Some(launch(daemon.connection.clone()));
    let mut disabled = launch(daemon.connection.clone());
    disabled.params.remote_exec.as_mut().unwrap().enabled = false;
    let ctx = restarted_context(dir.path());
    let mut project_off = crate::config::Config {
        project_root: Some(dir.path().into()),
        ..Default::default()
    };
    project_off.sandbox.profile.head = crate::config::SandboxMode::Off;
    project_off.remote_exec.project_off = true;
    project_off.bash.runon_enabled = true;
    let project_off_ctx = crate::context::AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        project_off,
    );
    for (ctx, policy, extra, expected) in [
        (
            &project_off_ctx,
            enabled(),
            serde_json::json!({}),
            "remote runs are off for this project",
        ),
        (
            &ctx,
            None,
            serde_json::json!({}),
            "this session has no remote runner",
        ),
        (
            &ctx,
            Some(disabled.clone()),
            serde_json::json!({}),
            "this session has no remote runner",
        ),
        (
            &ctx,
            enabled(),
            serde_json::json!({"pty": true}),
            "runon cannot be combined with pty:true",
        ),
        (
            &ctx,
            enabled(),
            serde_json::json!({"shell": "powershell"}),
            "runon cannot run PowerShell",
        ),
        (
            &ctx,
            enabled(),
            serde_json::json!({"runon": "plan9"}),
            "unknown runner demand \"plan9\"",
        ),
    ] {
        let mut params = serde_json::json!({"command": command, "runon": "linux"});
        for (key, value) in extra.as_object().unwrap() {
            params[key] = value.clone();
        }
        let response = handle_with_policy(ctx, policy, params);
        assert!(!response.success, "{expected}: {response:?}");
        let message = response.data["message"].as_str().unwrap_or_default();
        assert!(message.contains(expected), "{expected}: {response:?}");
    }
    // Each synchronous refusal must return without registering a worker that
    // could dispatch later, either remotely or on this host.
    assert!(ctx.bash_background().list(0).is_empty());
    assert!(project_off_ctx.bash_background().list(0).is_empty());
    assert!(exec_runs(&daemon).is_empty());
    assert!(!marker.exists());
}

#[cfg(unix)]
fn hermetic_git(cwd: &Path, args: &[&str]) {
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
fn git_fixture(root: &Path) {
    std::fs::create_dir_all(root).unwrap();
    hermetic_git(root, &["init", "--quiet"]);
    hermetic_git(root, &["config", "user.name", "AFT Test"]);
    hermetic_git(root, &["config", "user.email", "aft-test@example.invalid"]);
    std::fs::write(root.join("tracked.txt"), "fixture\n").unwrap();
    hermetic_git(root, &["add", "tracked.txt"]);
    hermetic_git(root, &["commit", "--quiet", "-m", "fixture"]);
}

#[cfg(unix)]
fn ensure_git_fixture(root: &Path) {
    if !root.join(".git").exists() {
        git_fixture(root);
    }
}

#[cfg(unix)]
fn remote_test_context(root: &Path) -> crate::context::AppContext {
    ensure_git_fixture(root);
    restarted_context(root)
}

#[cfg(unix)]
fn remote_test_context_with_runon(root: &Path, enabled: bool) -> crate::context::AppContext {
    ensure_git_fixture(root);
    restarted_context_with_runon(root, enabled)
}

#[cfg(unix)]
fn add_git_worktree(repository: &Path, path: &Path, branch: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let path = path.to_str().unwrap();
    hermetic_git(
        repository,
        &["worktree", "add", "--quiet", "-b", branch, path, "HEAD"],
    );
}

#[cfg(unix)]
fn assert_remote_request_paths(
    request: RunRequest,
    workspace_key: &Path,
    repository_root: &Path,
    cwd: &Path,
) {
    let wire = serde_json::to_value(request).unwrap();
    assert_eq!(wire["workspace_key"], workspace_key.to_str().unwrap());
    assert_eq!(wire["repository_root"], repository_root.to_str().unwrap());
    assert_eq!(wire["cwd"], cwd.to_str().unwrap());
}

#[cfg(unix)]
#[test]
fn linked_worktree_remote_requests_use_the_linked_worktree_root() {
    let temp = tempfile::tempdir().unwrap();
    let main = temp.path().join("main");
    git_fixture(&main);
    let main = main.canonicalize().unwrap();
    let linked = temp.path().join("linked");
    add_git_worktree(&main, &linked, "linked-sibling");
    let linked = linked.canonicalize().unwrap();
    let subdir = linked.join("subdir");
    std::fs::create_dir(&subdir).unwrap();

    for cwd in [&linked, &subdir] {
        let request = build_remote_request(
            &main,
            cwd,
            "true",
            BTreeMap::new(),
            30 * 60,
            &exec::PresetParams::default(),
        )
        .expect("linked worktree cwd should be accepted");
        assert_remote_request_paths(request, &linked, &main, cwd);
    }
}

#[cfg(unix)]
#[test]
fn remote_request_for_main_worktree_keeps_the_main_root() {
    let temp = tempfile::tempdir().unwrap();
    let main = temp.path().join("main");
    git_fixture(&main);
    let main = main.canonicalize().unwrap();
    let cwd = main.join("subdir");
    std::fs::create_dir(&cwd).unwrap();

    let request = build_remote_request(
        &main,
        &cwd,
        "true",
        BTreeMap::new(),
        30 * 60,
        &exec::PresetParams::default(),
    )
    .unwrap();
    assert_remote_request_paths(request, &main, &main, &cwd);
}

#[cfg(unix)]
#[test]
fn nested_linked_worktree_remote_request_uses_the_nested_root() {
    let temp = tempfile::tempdir().unwrap();
    let main = temp.path().join("main");
    git_fixture(&main);
    let main = main.canonicalize().unwrap();
    let nested = main.join(".cortexkit/worktrees/nested");
    add_git_worktree(&main, &nested, "linked-nested");
    let nested = nested.canonicalize().unwrap();
    let cwd = nested.join("subdir");
    std::fs::create_dir(&cwd).unwrap();

    let request = build_remote_request(
        &main,
        &cwd,
        "true",
        BTreeMap::new(),
        30 * 60,
        &exec::PresetParams::default(),
    )
    .unwrap();
    assert_remote_request_paths(request, &nested, &main, &cwd);
}

#[cfg(unix)]
#[test]
fn remote_request_refuses_unrelated_repositories_and_non_git_directories() {
    let temp = tempfile::tempdir().unwrap();
    let main = temp.path().join("main");
    let unrelated = temp.path().join("unrelated");
    git_fixture(&main);
    git_fixture(&unrelated);
    let main = main.canonicalize().unwrap();
    let unrelated = unrelated.canonicalize().unwrap();

    let error = build_remote_request(
        &main,
        &unrelated,
        "true",
        BTreeMap::new(),
        30 * 60,
        &exec::PresetParams::default(),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains(&format!(
            "workdir {} is in the checkout {}, which belongs to a different repository than this session's checkout {}",
            unrelated.display(),
            unrelated.display(),
            main.display()
        )),
        "unexpected refusal reason: {error}"
    );

    // The runner may place TMPDIR inside a checkout. Use the system's other
    // Unix temporary root so this directory is in no checkout at all.
    let outside = tempfile::Builder::new()
        .prefix("aft-runon-plain-outside-")
        .tempdir_in("/var/tmp")
        .unwrap();
    assert!(worktree_root_containing(outside.path()).unwrap().is_none());
    let error = build_remote_request(
        &main,
        outside.path(),
        "true",
        BTreeMap::new(),
        30 * 60,
        &exec::PresetParams::default(),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains(&format!(
            "workdir {} is not inside a git checkout; this session's checkout is {}",
            outside.path().canonicalize().unwrap().display(),
            main.display()
        )),
        "unexpected refusal reason: {error}"
    );
}

#[test]
fn published_report_vectors_render_changes_say_nothing_when_empty_and_name_what_is_absent() {
    use crate::exec_remote::wire_tests::report_vector;
    // Reported and changed: every kind of change is named, none copied back.
    let all = "These files changed on the server and were NOT copied back:\n  result.txt\n\
         Git state changed on the server and was NOT copied back:\n  \
         HEAD: 111111111111 -> 222222222222\n  \
         ref: refs/heads/main -> refs/heads/build\n  \
         index tree changed (staged changes differ)\n  \
         stash count: 0 -> 1 (+1)\n\
         These untracked files were created on the server and were NOT copied back:\n  \
         generated/new.txt\n  notes.txt\n\
         25 writes under ignored paths on the server were NOT copied back, for example:\n  \
         scratch/debug.log\n  cache/result.bin";
    assert_eq!(
        render_terminal_report(&report_vector("all"), &[]).as_deref(),
        Some(all)
    );
    // Reported and unchanged: a reporting runner sent every field, all
    // empty, so there is nothing to say.
    let unchanged = report_vector("unchanged");
    assert!(unchanged.git_state_changed.is_some());
    assert_eq!(render_terminal_report(&unchanged, &[]), None);
    // Absent (an older runner): each missing report is named on its own line.
    assert_eq!(
        render_terminal_report(&report_vector("older-runner"), &[]).as_deref(),
        Some(
            "These files changed on the server and were NOT copied back:\n  result.txt\n\
             git state: not reported by the runner\n\
             untracked files: not reported by the runner\n\
             ignored writes: not reported by the runner"
        )
    );
    // A detached HEAD has no ref on either side, so no ref line; an index
    // tree that became available counts as changed.
    let detached = render_terminal_report(&report_vector("detached-head"), &[]).unwrap();
    assert!(!detached.contains("ref:"), "{detached}");
    assert!(detached.contains("HEAD: 111111111111 -> 222222222222\n  index tree changed"));
    // A capped untracked list says the runner listed only some of them.
    let truncated = render_terminal_report(&report_vector("truncated-untracked"), &[]).unwrap();
    assert!(
        truncated
            .contains("  notes.txt\n  (the runner listed only some of them; more were created)\n"),
        "{truncated}"
    );
}

#[tokio::test]
async fn a_reporting_runner_report_is_printed_after_the_output() {
    let daemon = daemon(Script::Reported, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = remote_test_context(dir.path());
    let response = handle_with_policy(
        &ctx,
        Some(launch(daemon.connection.clone())),
        serde_json::json!({"command": "bash build.sh", "runon": "linux", "compressed": false}),
    );
    assert!(response.success, "{response:?}");
    let task_id = response.data["task_id"].as_str().unwrap().to_string();
    let done = terminal(ctx.bash_background(), &task_id).await;
    let rendered = crate::commands::bash_orchestrate::format_foreground_result(&done);
    assert_eq!(
        rendered.lines().next(),
        Some("ran remotely on ck-motor (job 0192a64a-1234-7000-8000-000000000001)"),
        "{rendered}"
    );
    assert!(
        rendered.ends_with(
            &render_terminal_report(&crate::exec_remote::wire_tests::report_vector("all"), &[])
                .unwrap()
        ),
        "{rendered}"
    );
    assert!(
        !rendered.contains("not reported by the runner"),
        "{rendered}"
    );
}

/// The bash call the OpenCode and Pi plugins send for a model's `runon` call.
fn orchestrated_request(params: serde_json::Value) -> crate::protocol::RawRequest {
    crate::protocol::RawRequest {
        id: "runon".into(),
        command: "bash".into(),
        session_id: Some("session".into()),
        lsp_hints: None,
        params,
    }
}

/// Spawn `params` through the bash handler and drive its foreground wait the
/// way the standalone request loop does, until the call answers. Returns the
/// reply and how long its foreground wait took.
async fn orchestrated_reply(
    ctx: &crate::context::AppContext,
    daemon: &crate::exec_remote::wire_tests::Daemon,
    params: serde_json::Value,
) -> (crate::protocol::Response, Duration) {
    // These callers check terminal payloads, not the foreground wait cap.
    // Observe completion before starting that independent call budget.
    orchestrated_reply_with_probe(ctx, daemon, params, |task_id| {
        ctx.bash_background()
            .observed_status(task_id, "session", 8192)
            .is_some_and(|snapshot| snapshot.info.status.is_terminal())
    })
    .await
}

/// Wait for a remote-state probe before starting the foreground wait budget.
/// This separates daemon connection and stream-processing latency from tests
/// of the wait cap in a particular remote phase.
async fn orchestrated_reply_with_probe(
    ctx: &crate::context::AppContext,
    daemon: &crate::exec_remote::wire_tests::Daemon,
    mut params: serde_json::Value,
    ready: impl Fn(&str) -> bool,
) -> (crate::protocol::Response, Duration) {
    // The handler resolves an omitted cwd against the process, not this
    // disposable test project. Keep the remote request inside its own root.
    params
        .as_object_mut()
        .unwrap()
        .entry("workdir")
        .or_insert_with(|| serde_json::json!(ctx.config().project_root));
    let request = orchestrated_request(params);
    let spawned =
        crate::bash_background::with_remote_policy(Some(launch(daemon.connection.clone())), || {
            crate::commands::bash::handle(&request, ctx)
        });
    assert!(spawned.success, "{spawned:?}");
    let task_id = spawned.data["task_id"].as_str().unwrap();
    wait_for(
        "remote task must satisfy the probe before its foreground wait",
        || ready(task_id).then_some(()),
    )
    .await;
    let received = std::time::Instant::now();
    match crate::commands::bash_orchestrate::build_bash_outcome(&request, ctx, spawned) {
        crate::response_finalize::DispatchOutcome::Immediate(response) => {
            (response, received.elapsed())
        }
        crate::response_finalize::DispatchOutcome::Deferred(mut pending) => {
            let response = tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    if let Some(response) = (pending.poll)(ctx) {
                        return response;
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            })
            .await
            .expect("a blocking call must always answer");
            (response, received.elapsed())
        }
    }
}

/// Every cancellation AFT sent the runner: `exec.cancel`, or a Cancel frame
/// (sent when a held-open run stream is dropped).
fn runner_cancels(daemon: &crate::exec_remote::wire_tests::Daemon) -> usize {
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
fn plugin_transport_deadline(timeout_ms: u64) -> Duration {
    Duration::from_millis(timeout_ms + 10_000)
}

#[tokio::test]
async fn blocking_runon_queued_past_its_timeout_answers_with_task_job_and_queue_position() {
    // The runner accepts the job at queue position 11 and holds it queued well
    // past the call's `timeout` before running it.
    let daemon = daemon(Script::GatedStages { position: 11 }, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    let timeout_ms = 1_500;
    let (reply, waited) = orchestrated_reply_with_probe(
        &ctx,
        &daemon,
        serde_json::json!({
            "command": "uname -s; date -u",
            "runon": "linux",
            "wait": true,
            "timeout": timeout_ms,
            "foreground_orchestrate": true,
            "compressed": false,
        }),
        |task_id| {
            ctx.bash_background()
                .remote_progress(task_id, "session")
                .is_some_and(|progress| progress.phase == RemotePhase::Queued { position: 11 })
        },
    )
    .await;
    assert!(reply.success, "{reply:?}");
    assert!(
        waited >= Duration::from_millis(timeout_ms),
        "answered after {waited:?}, before its wait was over"
    );
    assert!(
        waited < plugin_transport_deadline(timeout_ms),
        "answered after {waited:?}, when the plugin had already given up"
    );
    let task_id = reply.data["task_id"].as_str().unwrap().to_string();
    assert_eq!(reply.data["status"], "running", "{reply:?}");
    assert_eq!(reply.data["remote_phase"], "queued", "{reply:?}");
    assert_eq!(reply.data["queue_position"], 11, "{reply:?}");
    assert_eq!(reply.data["remote_job_id"], id().to_string(), "{reply:?}");
    let output = reply.data["output"].as_str().unwrap();
    for expected in [
        task_id.as_str(),
        &id().to_string(),
        "queued at position 11",
        "It was not killed",
    ] {
        assert!(output.contains(expected), "missing {expected:?}: {output}");
    }
    assert_eq!(
        runner_cancels(&daemon),
        0,
        "handing the call back must not cancel the queued job"
    );
    // The job runs once it leaves the queue: AFT did not count its timeout
    // from submission, and nothing cancelled it.
    daemon.output_gate.notify_one();
    wait_for("queued job must consume output before completing", || {
        ctx.bash_background()
            .remote_progress(&task_id, "session")
            .filter(|progress| progress.phase == RemotePhase::Running)
    })
    .await;
    daemon.completion_gate.notify_one();
    let done = terminal(ctx.bash_background(), &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");
    assert!(
        crate::commands::bash_orchestrate::format_foreground_result(&done).contains("started"),
        "{done:?}"
    );
    assert_eq!(runner_cancels(&daemon), 0);
    assert_eq!(
        exec_runs(&daemon).len(),
        1,
        "the job must not be resubmitted"
    );
}

#[tokio::test]
async fn blocking_runon_still_running_at_its_wait_limit_answers_with_running_phase() {
    let daemon = daemon(Script::Started, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    let timeout_ms = 1_500;
    let (reply, waited) = orchestrated_reply_with_probe(
        &ctx,
        &daemon,
        serde_json::json!({
            "command": "cargo test",
            "runon": "linux",
            "wait": true,
            "timeout": timeout_ms,
            "foreground_orchestrate": true,
            "compressed": false,
        }),
        |task_id| {
            ctx.bash_background()
                .remote_progress(task_id, "session")
                .is_some_and(|progress| progress.phase == RemotePhase::Running)
        },
    )
    .await;
    assert!(reply.success, "{reply:?}");
    assert!(waited < plugin_transport_deadline(timeout_ms), "{waited:?}");
    assert_eq!(reply.data["remote_phase"], "running", "{reply:?}");
    assert_eq!(reply.data["queue_position"], serde_json::Value::Null);
    assert!(
        reply.data["output"]
            .as_str()
            .unwrap()
            .contains("running on ck-motor"),
        "{reply:?}"
    );
    assert_eq!(runner_cancels(&daemon), 0);
    let task_id = reply.data["task_id"].as_str().unwrap().to_string();
    let registry = ctx.bash_background().clone();
    tokio::task::spawn_blocking(move || registry.kill(&task_id, "session"))
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn blocking_runon_killed_at_its_runner_deadline_reports_exit_124() {
    let daemon = daemon(Script::Deadline, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    let (reply, _) = orchestrated_reply(
        &ctx,
        &daemon,
        serde_json::json!({
            "command": "sleep 600",
            "runon": "linux",
            "wait": true,
            "timeout": 5_000,
            "foreground_orchestrate": true,
            "compressed": false,
        }),
    )
    .await;
    assert!(reply.success, "{reply:?}");
    assert_eq!(reply.data["timed_out"], true, "{reply:?}");
    assert_eq!(reply.data["exit_code"], 124, "{reply:?}");
    let output = reply.data["output"].as_str().unwrap();
    assert!(output.contains("[command timed out]"), "{output}");
    assert!(output.contains("[exit code: 124]"), "{output}");
    // The runner killed the job itself; AFT has nothing to cancel.
    assert_eq!(runner_cancels(&daemon), 0);
}

#[tokio::test]
async fn non_blocking_runon_is_still_promoted_at_the_foreground_wait_window() {
    let daemon = daemon(Script::GatedStages { position: 3 }, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    ctx.update_config(|config| config.foreground_wait_window_ms = 2_500);
    let (reply, waited) = orchestrated_reply_with_probe(
        &ctx,
        &daemon,
        serde_json::json!({
            "command": "uname -s",
            "runon": "linux",
            "timeout": 500,
            "foreground_orchestrate": true,
            "compressed": false,
        }),
        |task_id| {
            ctx.bash_background()
                .remote_progress(task_id, "session")
                .is_some_and(|progress| progress.phase == RemotePhase::Queued { position: 3 })
        },
    )
    .await;
    assert!(reply.success, "{reply:?}");
    // A non-blocking call ends at the foreground wait window, as before; the
    // blocking-call hand-back (at its 500 ms `timeout`) does not apply to it.
    assert!(waited >= Duration::from_millis(2_500), "{waited:?}");
    assert_eq!(reply.data["status"], "running", "{reply:?}");
    assert!(reply.data.get("remote_phase").is_none(), "{reply:?}");
    assert!(
        reply.data["output"]
            .as_str()
            .unwrap()
            .starts_with("Foreground bash didn't finish within"),
        "{reply:?}"
    );
    assert_eq!(runner_cancels(&daemon), 0);
    daemon.output_gate.notify_one();
    daemon.completion_gate.notify_one();
    let task_id = reply.data["task_id"].as_str().unwrap();
    assert_eq!(
        terminal(ctx.bash_background(), task_id).await.info.status,
        BgTaskStatus::Completed
    );
}

#[test]
fn remote_phase_comes_from_acceptance_started_or_output() {
    let mut remote = RemoteTask {
        accepted_platform: None,
        connection_file: None,
        explicit_runon: true,
        harness: "broca".into(),
        session: "session".into(),
        requested_platform: None,
        requested_vcpus: None,
        requested_network: false,
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
        return_paths: Vec::new(),
        returned_files: None,
        return_first_seq: None,
        return_staging: Default::default(),
    };
    assert_eq!(RemotePhase::of(&remote), RemotePhase::Submitting);
    remote.job_id = Some(id());
    assert_eq!(
        RemotePhase::of(&remote),
        RemotePhase::Submitting,
        "a refusal can carry a job id without acceptance"
    );
    remote.accepted = true;
    assert_eq!(
        RemotePhase::of(&remote),
        RemotePhase::WaitingOnRunner,
        "a job id restored without its queue position does not claim a position"
    );
    remote.queue_position = Some(4);
    assert_eq!(
        RemotePhase::of(&remote),
        RemotePhase::Queued { position: 4 }
    );
    remote.started = Some(Started::new(0, 123, 1));
    assert_eq!(RemotePhase::of(&remote), RemotePhase::Running);
    remote.started = None;
    remote.output_received = true;
    assert_eq!(RemotePhase::of(&remote), RemotePhase::Running);
}

/// bash_status for `task_id` in `ctx`, rendered as the agent sees it.
fn rendered_status(ctx: &crate::context::AppContext, task_id: &str) -> String {
    let request: crate::protocol::RawRequest = serde_json::from_value(serde_json::json!({
        "id": "status", "command": "bash_status", "session_id": "session",
        "params": { "task_id": task_id },
    }))
    .unwrap();
    let response = crate::commands::bash_status::handle(&request, ctx);
    assert!(response.success, "{response:?}");
    crate::subc_format::format_response_with_context(
        "bash_status",
        &response,
        &crate::subc_format::FormatContext::default(),
    )
}

/// A short bash_watch on `task_id` in `ctx`; returns its rendered text.
async fn rendered_watch(ctx: Arc<crate::context::AppContext>, task_id: &str) -> String {
    let request: crate::protocol::RawRequest = serde_json::from_value(serde_json::json!({
        "id": "watch", "command": "bash_watch", "session_id": "session",
        "params": { "task_id": task_id, "timeout_ms": 200 },
    }))
    .unwrap();
    let wake = crate::response_finalize::DeferredResponseWake::default();
    let producer_wake = wake.clone();
    let producer_ctx = Arc::clone(&ctx);
    let outcome = tokio::task::spawn_blocking(move || {
        let _wake = producer_wake.install();
        crate::commands::bash_watch::handle_deferred(&request, producer_ctx)
    })
    .await
    .unwrap();
    let response = match outcome {
        crate::response_finalize::DispatchOutcome::Immediate(response) => response,
        crate::response_finalize::DispatchOutcome::Deferred(mut pending) => {
            tokio::time::timeout(Duration::from_secs(10), wake.notified())
                .await
                .expect("watch answers");
            (pending.poll)(&ctx).expect("response queued before completion wake")
        }
    };
    assert!(response.success, "{response:?}");
    response.data["output"].as_str().unwrap().to_string()
}

#[test]
fn remote_phase_note_says_submitting_then_queued_until_started() {
    let dir = tempfile::tempdir().unwrap();
    let mut metadata = PersistedTask::starting(
        "bash-0000000000000001".into(),
        "session".into(),
        "cargo test".into(),
        dir.path().into(),
        None,
        Some(600_500),
        true,
        false,
    );
    metadata.status = BgTaskStatus::Running;
    metadata.started_at = 1_000_000;
    metadata.remote = Some(RemoteTask {
        accepted_platform: None,
        connection_file: None,
        explicit_runon: true,
        harness: "runner".into(),
        session: "session".into(),
        requested_platform: None,
        requested_vcpus: None,
        requested_network: false,
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
        return_paths: Vec::new(),
        returned_files: None,
        return_first_seq: None,
        return_staging: Default::default(),
    });
    let now = 1_042_900;
    // Before acceptance the runner has not confirmed it copied the inputs.
    assert_eq!(
        remote_phase_note(&metadata, now).as_deref(),
        Some("submitting to the runner (capturing inputs; not accepted yet)")
    );
    let remote = metadata.remote.as_mut().unwrap();
    remote.accepted = true;
    remote.job_id = Some(id());
    remote.queue_position = Some(7);
    assert_eq!(
        remote_phase_note(&metadata, now).as_deref(),
        Some("queued on the runner (42 s, position 7)")
    );
    metadata.remote.as_mut().unwrap().started = Some(Started::new(3, 40_000, 1));
    // The task's timeout_ms is 600_500 (set above); the runner takes whole
    // seconds, so AFT asks it for 601 and the note says the same.
    assert_eq!(
        remote_phase_note(&metadata, now).as_deref(),
        Some("the runner stops it 601 s after it started")
    );
    // A `runon` task the runner refused may fall back to a local process
    // (here, a local child_pid is set); it then has no remote phase note.
    let mut fallback = metadata.clone();
    fallback.child_pid = Some(4242);
    assert_eq!(remote_phase_note(&fallback, now), None);
    metadata.mark_terminal(BgTaskStatus::Completed, Some(0), None);
    assert_eq!(remote_phase_note(&metadata, now), None);
}

#[tokio::test]
async fn bash_status_watch_and_task_line_distinguish_input_capture_from_queueing() {
    let daemon = daemon(Script::GatedAcceptance, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    ensure_git_fixture(dir.path());
    let ctx = Arc::new(restarted_context(dir.path()));
    let mut policy = launch(daemon.connection.clone());
    policy.explicit_runon = true;
    let task_id = ctx
        .bash_background()
        .spawn_remote(
            policy,
            SpawnPlan::Unsandboxed,
            "uname -s",
            resolve_posix_shell(),
            "session".into(),
            dir.path().into(),
            HashMap::new(),
            crate::bash_background::HardKill::After(Duration::from_secs(30)),
            dir.path().into(),
            crate::bash_background::TaskSlot::Background { max: 10 },
            true,
            false,
            Some(dir.path().into()),
        )
        .unwrap();
    daemon.wait_for_requests("exec.run", 1).await;
    let status = rendered_status(&ctx, &task_id);
    let watch = rendered_watch(ctx.clone(), &task_id).await;
    assert!(
        status.starts_with(&format!("Task {task_id}: starting · submitting")),
        "{status}"
    );
    assert!(
        watch.starts_with(&format!("Task {task_id}: starting")),
        "{watch}"
    );
    for text in [&status, &watch] {
        assert!(text.contains("capturing inputs"), "{text}");
        assert!(
            !text.contains("queued on the runner") && !text.contains("still running"),
            "{text}"
        );
    }
    let snapshot = ctx
        .bash_background()
        .observed_status(&task_id, "session", 0)
        .unwrap();
    assert_eq!(
        crate::commands::bash_orchestrate::foreground_task_line(&snapshot),
        format!("[task {task_id}]")
    );
    daemon.acceptance_gate.notify_one();
    wait_for("acceptance must end the input-capture phase", || {
        (ctx.bash_background()
            .runon_launch_pending(&task_id, "session")
            == Some(false))
        .then_some(())
    })
    .await;
    let status = rendered_status(&ctx, &task_id);
    let watch = rendered_watch(ctx.clone(), &task_id).await;
    assert!(status.contains("queued on the runner"), "{status}");
    assert!(watch.contains("Queued on the runner"), "{watch}");
    let snapshot = ctx
        .bash_background()
        .observed_status(&task_id, "session", 0)
        .unwrap();
    assert_eq!(
        crate::commands::bash_orchestrate::foreground_task_line(&snapshot),
        format!("[task {task_id} (job {})]", id())
    );
    let registry = ctx.bash_background().clone();
    tokio::task::spawn_blocking(move || registry.kill(&task_id, "session"))
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn bash_status_and_watch_show_the_persisted_remote_phase_after_a_restart() {
    for started in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        // Accepted at queue position 1 and persisted; the connection file is
        // missing, so the restarted registry keeps trying to re-attach and
        // the task stays running with exactly the persisted stream records.
        let (original, task_id, paths) = persist_accepted_with_snapshot(
            dir.path(),
            dir.path().join("missing-connection.json"),
            "cargo test",
        );
        drop(original);
        if started {
            let mut metadata = read_task(&paths.json).unwrap();
            metadata.remote.as_mut().unwrap().started = Some(Started::new(0, 2_000, 1));
            write_task(&paths.json, &metadata).unwrap();
        }
        let ctx = Arc::new(restarted_context(dir.path()));
        ctx.bash_background()
            .replay_session(dir.path(), "session")
            .unwrap();

        let status = rendered_status(&ctx, &task_id);
        let watch = rendered_watch(Arc::clone(&ctx), &task_id).await;
        if started {
            assert!(
                status.starts_with(&format!(
                    "Task {task_id}: running · remote job {} · the runner stops it 30 s after it started\n",
                    id()
                )),
                "{status}"
            );
            assert!(
                watch.ends_with("\nThe runner stops it 30 s after it started."),
                "{watch}"
            );
        } else {
            let header = format!(
                "Task {task_id}: running · remote job {} · queued on the runner (",
                id()
            );
            assert!(status.starts_with(&header), "{status}");
            assert!(
                status.lines().next().unwrap().ends_with(" s, position 1)"),
                "{status}"
            );
            let last = watch.lines().last().unwrap();
            assert!(
                last.starts_with("Queued on the runner (") && last.ends_with(" s, position 1)."),
                "{watch}"
            );
        }
        assert!(!watch.contains("no kill deadline"), "{watch}");
        ctx.bash_background().detach();
    }
}

/// A recovered refusal is a finished task whether or not the runner sent
/// Accepted. Neither bash_status nor the finished-command reply may carry the
/// in-flight capture warning, which tells the caller to wait for queued or
/// running states the task will never reach.
#[tokio::test]
async fn runon_capture_recovered_terminal_status_omits_in_flight_warning() {
    use crate::commands::bash_orchestrate::REMOTE_INPUT_CAPTURE_WARNING;
    for accepted in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (original, task_id, paths) = persist_accepted_with_snapshot(
            dir.path(),
            dir.path().join("missing-connection.json"),
            "cargo test",
        );
        drop(original);
        let mut metadata = read_task(&paths.json).unwrap();
        let remote = metadata.remote.as_mut().unwrap();
        remote.explicit_runon = true;
        remote.accepted = accepted;
        remote.terminal = Some(TerminalRecord::new(
            id(),
            Outcome::RefusedBeforeStart {
                reason: RefusalReason::Unreachable,
            },
            1,
            0,
            0,
        ));
        metadata.status = BgTaskStatus::Failed;
        write_task(&paths.json, &metadata).unwrap();
        let ctx = restarted_context(dir.path());
        ctx.bash_background()
            .replay_session(dir.path(), "session")
            .unwrap();
        let request = crate::protocol::RawRequest {
            id: "recovered-capture-status".into(),
            command: "bash_status".into(),
            params: serde_json::json!({"task_id":task_id}),
            session_id: Some("session".into()),
            lsp_hints: None,
        };
        let response = crate::commands::bash_status::handle(&request, &ctx);
        assert!(!response.success, "{response:?}");
        assert_eq!(response.data["status"], "failed", "{response:?}");
        assert!(
            !response.data["output_preview"]
                .as_str()
                .unwrap()
                .contains(REMOTE_INPUT_CAPTURE_WARNING),
            "accepted={accepted}: {response:?}"
        );
        let (disk, _) = ctx
            .bash_background()
            .terminal_status_from_disk(&task_id, "session", Some(dir.path()), dir.path())
            .unwrap();
        assert_eq!(disk.remote_inputs_captured, Some(accepted));
        let response = crate::commands::bash_orchestrate::decide_bash_step(
            disk,
            Instant::now(),
            false,
            false,
            Instant::now(),
            "disk-capture-result",
        );
        let crate::commands::bash_orchestrate::BashStep::Done(response) = response else {
            panic!("recovered refusal must remain terminal");
        };
        assert!(!response.success, "{response:?}");
        assert!(
            !response.data["output"]
                .as_str()
                .unwrap()
                .contains(REMOTE_INPUT_CAPTURE_WARNING),
            "accepted={accepted}: {response:?}"
        );
    }
}

#[test]
fn cancelled_call_notice_names_a_remote_task_and_its_job_once() {
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, paths) = persist_accepted_with_snapshot(
        dir.path(),
        dir.path().join("missing-connection.json"),
        "cargo test",
    );
    registry.note_cancelled_call(&task_id, "session");
    assert!(read_task(&paths.json).unwrap().cancelled_call_unannounced);
    assert!(registry
        .take_cancelled_call_notices("other-session")
        .is_empty());
    assert_eq!(
        registry.take_cancelled_call_notices("session"),
        vec![format!(
            "task {task_id} (job {}) from your cancelled call is still running; check it with bash_status or stop it with bash_kill",
            id()
        )]
    );
    assert!(registry.take_cancelled_call_notices("session").is_empty());
    assert!(!read_task(&paths.json).unwrap().cancelled_call_unannounced);
    registry.detach();
}

#[tokio::test]
async fn runon_draining_twice_then_accepts_and_discloses_wait() {
    let daemon = daemon(
        Script::Draining {
            refusals: 2,
            retry_after_ms: Some(150),
        },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut clock = PausedDrainingClock::new(dir.path());
    ensure_git_fixture(dir.path());
    let registry = registry();
    let mut policy = launch(daemon.connection.clone());
    policy.explicit_runon = true;
    let task_id = registry
        .spawn_remote(
            policy,
            SpawnPlan::Unsandboxed,
            "printf local-proof",
            resolve_posix_shell(),
            "session".into(),
            dir.path().into(),
            HashMap::new(),
            crate::bash_background::HardKill::After(Duration::from_secs(30)),
            dir.path().into(),
            crate::bash_background::TaskSlot::Background { max: 10 },
            true,
            false,
            Some(dir.path().into()),
        )
        .unwrap();
    let first = clock.now();
    clock.expect_sleep(first, first + 150).await;
    let waiting = registry.observed_status(&task_id, "session", 8192).unwrap();
    assert_eq!(waiting.info.status, BgTaskStatus::Starting, "{waiting:?}");
    assert_eq!(
        registry.runon_launch_pending(&task_id, "session"),
        Some(true)
    );
    assert!(
        waiting
            .output_preview
            .contains("draining for maintenance; retrying (attempt 2"),
        "{waiting:?}"
    );
    clock.advance(150);
    clock.expect_sleep(first + 150, first + 300).await;
    assert_eq!(exec_runs(&daemon).len(), 2);
    clock.advance(150);
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.exit_code, Some(0), "{done:?}");
    assert!(!done.output_preview.contains("local-proof"));
    let requests = exec_runs(&daemon);
    assert_eq!(requests.len(), 3);
    assert!(requests.windows(2).all(|pair| pair[0] == pair[1]));
    // The record keeps when the accepted resubmission was sent, so re-attach
    // limits count from it after a restart too.
    let saved = read_task(&registry.task(&task_id).unwrap().paths.json).unwrap();
    let resubmitted_at = saved
        .remote
        .as_ref()
        .and_then(|remote| remote.retry.as_ref())
        .and_then(|retry| retry.resubmitted_at_ms);
    assert!(
        resubmitted_at.is_some_and(|at| at >= saved.started_at),
        "the accepted resubmission must record its time: {resubmitted_at:?}"
    );
}

/// A draining retry the runner accepts late starts a new queue wait and run
/// there. AFT's re-attach limit must count from that resubmission, both in
/// the worker that sent it and after a daemon restart reads the record back.
#[tokio::test]
async fn reattach_limit_counts_from_a_late_draining_resubmission() {
    const MINUTE_MS: u64 = 60_000;
    let dir = tempfile::tempdir().unwrap();
    let now = unix_millis();
    let mut metadata = PersistedTask::starting(
        "bash-0000000000000002".into(),
        "session".into(),
        "cargo test".into(),
        dir.path().into(),
        None,
        // An untimed runon records the 30-minute background default.
        Some(30 * MINUTE_MS),
        true,
        false,
    );
    // Submitted first nine minutes ago; the runner drained until now.
    metadata.started_at = now - 9 * MINUTE_MS;
    metadata.remote = Some(RemoteTask {
        accepted_platform: None,
        connection_file: None,
        explicit_runon: true,
        harness: "runner".into(),
        session: "session".into(),
        requested_platform: None,
        requested_vcpus: None,
        requested_network: false,
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
        retry: Some(DrainingRetry {
            request_digest: "digest".into(),
            attempts: 1,
            first_refusal_at_ms: Some(metadata.started_at),
            next_retry_at_ms: None,
            waited_ms: None,
            resubmitted_at_ms: None,
        }),
        output_incomplete: None,
        workspace_holder: None,
        return_paths: Vec::new(),
        returned_files: None,
        return_first_seq: None,
        return_staging: Default::default(),
    });
    let minutes_left = |budget: &ReattachBudget| {
        budget
            .job_end
            .saturating_duration_since(tokio::time::Instant::now())
            .as_secs_f64()
            / 60.0
    };
    // 5 minutes of queue wait plus 30 of run, counted from the submission.
    let full_window = 35.0;

    let mut budget = ReattachBudget::new(dir.path(), &metadata);
    let before = minutes_left(&budget);
    assert!(
        (full_window - 9.5..=full_window - 9.0).contains(&before),
        "before the resubmission the limit counts from the first submission: {before:.2} minutes left"
    );

    metadata
        .remote
        .as_mut()
        .unwrap()
        .retry
        .as_mut()
        .unwrap()
        .resubmitted_at_ms = Some(now);
    budget.resubmitted(&metadata);
    let live = minutes_left(&budget);
    assert!(
        live >= full_window - 0.5,
        "the worker that resubmitted must get the full window from the resubmission: {live:.2} minutes left"
    );

    let record = dir.path().join("task.json");
    write_task(&record, &metadata).unwrap();
    let reloaded = read_task(&record).unwrap();
    let restarted = minutes_left(&ReattachBudget::new(dir.path(), &reloaded));
    assert!(
        restarted >= full_window - 0.5,
        "a restarted daemon must count from the recorded resubmission: {restarted:.2} minutes left"
    );

    // A record written before the resubmission time existed still loads and
    // counts from the task's start.
    let mut older = serde_json::to_value(&reloaded).unwrap();
    let removed = older["remote"]["retry"]
        .as_object_mut()
        .unwrap()
        .remove("resubmitted_at_ms");
    assert!(removed.is_some());
    let older: PersistedTask = serde_json::from_value(older).unwrap();
    let fallback = minutes_left(&ReattachBudget::new(dir.path(), &older));
    assert!(
        (full_window - 9.5..=full_window - 9.0).contains(&fallback),
        "an older record counts from the task's start: {fallback:.2} minutes left"
    );
}

fn draining_budgets() -> &'static Mutex<HashMap<PathBuf, u64>> {
    static BUDGETS: std::sync::OnceLock<Mutex<HashMap<PathBuf, u64>>> = std::sync::OnceLock::new();
    BUDGETS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) fn draining_budget_ms(root: &Path) -> Option<u64> {
    draining_budgets().lock().unwrap().get(root).copied()
}

fn shorten_draining_budget(root: &Path, ms: u64) {
    draining_budgets().lock().unwrap().insert(root.into(), ms);
}

#[derive(Debug, PartialEq, Eq)]
enum DrainingClockEvent {
    Sleeping { from: u64, until: u64 },
    Stopped,
}

pub(super) struct DrainingClock {
    ticks: tokio::sync::watch::Sender<u64>,
    events: tokio::sync::mpsc::UnboundedSender<DrainingClockEvent>,
}

impl DrainingClock {
    pub(super) fn now(&self) -> u64 {
        *self.ticks.borrow()
    }

    pub(super) async fn sleep_until(&self, from: u64, until: u64) {
        let mut ticks = self.ticks.subscribe();
        let _ = self
            .events
            .send(DrainingClockEvent::Sleeping { from, until });
        ticks.wait_for(|now| *now >= until).await.unwrap();
    }
}

fn draining_clocks() -> &'static Mutex<HashMap<PathBuf, Arc<DrainingClock>>> {
    static CLOCKS: std::sync::OnceLock<Mutex<HashMap<PathBuf, Arc<DrainingClock>>>> =
        std::sync::OnceLock::new();
    CLOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Test clocks and budgets are keyed by the canonical project root. A test names
/// its temporary directory as created (on macOS under the `/var` symlink), while
/// a worker started through the bash handler records the resolved checkout path
/// (under `/private/var`). Without one key form the worker misses the paused
/// clock, sleeps on real time, and the test waits for an event that never comes.
fn test_clock_key(root: &Path) -> PathBuf {
    std::fs::canonicalize(root).unwrap_or_else(|_| root.into())
}

pub(super) fn draining_clock(root: &Path) -> Option<Arc<DrainingClock>> {
    draining_clocks()
        .lock()
        .unwrap()
        .get(&test_clock_key(root))
        .cloned()
}

pub(super) fn task_draining_clock(task: &BgTask) -> Option<Arc<DrainingClock>> {
    let state = task.state.lock().unwrap();
    draining_clock(state.metadata.project_root.as_deref()?)
}

// Remote workers have their own runtimes, so pausing the test's Tokio runtime
// cannot control their durable timestamps or sleeps. Share one clock per root
// across the original and replayed worker, without affecting parallel tests.
struct PausedDrainingClock {
    root: PathBuf,
    clock: Arc<DrainingClock>,
    events: tokio::sync::mpsc::UnboundedReceiver<DrainingClockEvent>,
}

impl PausedDrainingClock {
    fn new(root: &Path) -> Self {
        let root = test_clock_key(root);
        let (ticks, _) = tokio::sync::watch::channel(1_000_000);
        let (events, receiver) = tokio::sync::mpsc::unbounded_channel();
        let clock = Arc::new(DrainingClock { ticks, events });
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

    fn now(&self) -> u64 {
        self.clock.now()
    }

    fn advance(&self, ms: u64) {
        self.clock.ticks.send_modify(|now| *now += ms);
    }

    async fn expect_event(&mut self, expected: DrainingClockEvent) {
        // This is only a deadlock watchdog; the event's deadline, not elapsed
        // wall time, proves which budget or retry delay the worker selected.
        let event = tokio::time::timeout(Duration::from_secs(30), self.events.recv())
            .await
            .expect("draining worker must acknowledge its wait or stop")
            .expect("draining clock event channel closed");
        assert_eq!(event, expected);
    }

    async fn expect_sleep(&mut self, from: u64, until: u64) {
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

fn stopped_draining_roots() -> &'static Mutex<HashSet<PathBuf>> {
    static ROOTS: std::sync::OnceLock<Mutex<HashSet<PathBuf>>> = std::sync::OnceLock::new();
    ROOTS.get_or_init(|| Mutex::new(HashSet::new()))
}

pub(super) fn stop_draining_worker(root: &Path) -> bool {
    let stopped = stopped_draining_roots().lock().unwrap().remove(root);
    if stopped {
        if let Some(clock) = draining_clock(root) {
            let _ = clock.events.send(DrainingClockEvent::Stopped);
        }
    }
    stopped
}

fn start_runon_retry(registry: &BgTaskRegistry, dir: &Path, connection: PathBuf) -> String {
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
            dir.into(),
            crate::bash_background::TaskSlot::Background { max: 10 },
            true,
            false,
            Some(dir.into()),
        )
        .unwrap()
}

#[tokio::test]
async fn runon_draining_budget_caps_retry_after() {
    let daemon = daemon(
        Script::Draining {
            refusals: u32::MAX,
            retry_after_ms: Some(60_000),
        },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    shorten_draining_budget(dir.path(), 250);
    let mut clock = PausedDrainingClock::new(dir.path());
    let registry = registry();
    let task = start_runon_retry(&registry, dir.path(), daemon.connection.clone());
    let first = clock.now();
    clock.expect_sleep(first, first + 250).await;
    clock.advance(250);
    let done = terminal(&registry, &task).await;
    assert_eq!(done.info.status, BgTaskStatus::Failed);
    let saved = read_task(&registry.task(&task).unwrap().paths.json).unwrap();
    assert_eq!(saved.remote.unwrap().retry.unwrap().waited_ms, Some(250));
    assert_eq!(exec_runs(&daemon).len(), 1);
    let refusal = done.remote_refusal.unwrap();
    assert!(
        refusal
            .message
            .contains("runner_draining (maintenance) after retrying for "),
        "{refusal:?}"
    );
    assert!(refusal.message.contains("ms; command was not run"));
    assert_eq!(retry_duration(DRAINING_RETRY_BUDGET_MS), "10m");
}

#[tokio::test]
async fn runon_draining_retry_after_is_honoured() {
    let daemon = daemon(
        Script::Draining {
            refusals: 1,
            retry_after_ms: Some(300),
        },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut clock = PausedDrainingClock::new(dir.path());
    let registry = registry();
    let task = start_runon_retry(&registry, dir.path(), daemon.connection.clone());
    let first = clock.now();
    clock.expect_sleep(first, first + 300).await;
    clock.advance(100);
    assert_eq!(exec_runs(&daemon).len(), 1, "must not resubmit early");
    clock.advance(200);
    let done = terminal(&registry, &task).await;
    assert_eq!(done.exit_code, Some(0));
    assert_eq!(exec_runs(&daemon).len(), 2);
}

#[tokio::test]
async fn runon_draining_retry_only_draining() {
    let daemon = daemon(
        Script::DiskFull {
            refusals: 2,
            retry_after_ms: Some(20),
        },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    shorten_draining_budget(dir.path(), 300);
    let registry = registry();
    let task = start_runon_retry(&registry, dir.path(), daemon.connection.clone());
    let done = terminal(&registry, &task).await;
    assert_eq!(done.info.status, BgTaskStatus::Failed, "{done:?}");
    assert_runon_refusal(&refusal_response(done), "runner_disk_full (maintenance)");
    assert_eq!(exec_runs(&daemon).len(), 1);
}

#[tokio::test]
async fn runon_draining_kill_stops_retries() {
    let daemon = daemon(
        Script::Draining {
            refusals: u32::MAX,
            retry_after_ms: Some(500),
        },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    shorten_draining_budget(dir.path(), 1000);
    let mut clock = PausedDrainingClock::new(dir.path());
    let registry = registry();
    let task_id = start_runon_retry(&registry, dir.path(), daemon.connection.clone());
    let first = clock.now();
    clock.expect_sleep(first, first + 500).await;
    let killing = registry.clone();
    let killed_id = task_id.clone();
    // With the retry clock frozen, a terminal kill proves cancellation woke
    // the worker; the wall-clock timeout only guards against a deadlock.
    let killed = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || killing.kill(&killed_id, "session")),
    )
    .await
    .expect("kill must interrupt retry sleep without advancing the clock")
    .unwrap()
    .unwrap();
    assert_eq!(killed.info.status, BgTaskStatus::Killed);
    assert_eq!(clock.now(), first);
    clock.advance(550);
    assert_eq!(
        exec_runs(&daemon).len(),
        1,
        "kill must veto any new exec.run"
    );
}

#[tokio::test]
async fn runon_draining_restart_preserves_remaining_budget_and_request() {
    let daemon = crate::exec_remote::wire_tests::daemon_with_clients(
        Script::Draining {
            refusals: u32::MAX,
            retry_after_ms: Some(10_000),
        },
        "exec-remote/v1",
        2,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    shorten_draining_budget(dir.path(), 600);
    let mut clock = PausedDrainingClock::new(dir.path());
    stopped_draining_roots()
        .lock()
        .unwrap()
        .insert(dir.path().into());
    let original = registry();
    let task_id = start_runon_retry(&original, dir.path(), daemon.connection.clone());
    clock.expect_event(DrainingClockEvent::Stopped).await;
    let task = original.task(&task_id).unwrap();
    let saved = read_task(&task.paths.json).unwrap();
    let retry = saved.remote.as_ref().unwrap().retry.as_ref().unwrap();
    assert_eq!(retry.attempts, 1);
    let first = retry.first_refusal_at_ms.unwrap();
    assert_eq!(first, clock.now());
    assert_eq!(retry.next_retry_at_ms, Some(first + 10_000));
    assert!(!serde_json::to_string(&saved)
        .unwrap()
        .contains("private-value-proof"));
    let request =
        serde_json::to_value(restore_retry_request(&task, &retry.request_digest).unwrap()).unwrap();
    assert_eq!(request, exec_runs(&daemon)[0]);
    assert_eq!(
        request["env"]["REMOTE_REQUEST_PRIVATE_VALUE"],
        "private-value-proof"
    );
    clock.advance(400);
    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    // The saved refusal used 400 ms of its 600 ms budget. Replaying must
    // schedule only the remaining 200 ms, not another full 600 ms.
    clock.expect_sleep(first + 400, first + 600).await;
    let replayed_task = restarted.task(&task_id).unwrap();
    assert_eq!(
        serde_json::to_value(restore_retry_request(&replayed_task, &retry.request_digest).unwrap())
            .unwrap(),
        request
    );
    clock.advance(200);
    let done = terminal(&restarted, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Failed);
    assert_eq!(exec_runs(&daemon).len(), 1);
    let restored = read_task(&task.paths.json).unwrap();
    let retry = restored.remote.unwrap().retry.unwrap();
    assert_eq!(retry.first_refusal_at_ms, Some(first));
    assert_eq!(retry.attempts, 1);
    assert_eq!(retry.waited_ms, Some(600));
    assert_eq!(
        retry.request_digest,
        saved.remote.unwrap().retry.unwrap().request_digest
    );
}

#[tokio::test]
async fn runon_draining_restart_resubmits_and_accepts() {
    let daemon = crate::exec_remote::wire_tests::daemon_with_clients(
        Script::Draining {
            refusals: 1,
            retry_after_ms: Some(200),
        },
        "exec-remote/v1",
        2,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut clock = PausedDrainingClock::new(dir.path());
    stopped_draining_roots()
        .lock()
        .unwrap()
        .insert(dir.path().into());
    let original = registry();
    let task = start_runon_retry(&original, dir.path(), daemon.connection.clone());
    clock.expect_event(DrainingClockEvent::Stopped).await;
    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    let first = clock.now();
    clock.expect_sleep(first, first + 200).await;
    clock.advance(200);
    let done = terminal(&restarted, &task).await;
    assert_eq!(done.exit_code, Some(0), "{done:?}");
    let requests = exec_runs(&daemon);
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0], requests[1]);
}

#[test]
fn draining_default_backoff_is_bounded() {
    for (attempt, expected) in [
        (1, 5000),
        (2, 10000),
        (3, 20000),
        (4, 40000),
        (5, 60000),
        (100, 60000),
    ] {
        assert_eq!(draining_delay_ms(attempt, None), expected);
    }
    assert_eq!(draining_delay_ms(100, Some(17)), 17);
    assert_eq!(draining_delay_ms(1, Some(0)), 0);
}

#[tokio::test]
async fn runon_draining_restart_obeys_persisted_cancel() {
    let daemon = daemon(
        Script::Draining {
            refusals: u32::MAX,
            retry_after_ms: Some(10_000),
        },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut clock = PausedDrainingClock::new(dir.path());
    stopped_draining_roots()
        .lock()
        .unwrap()
        .insert(dir.path().into());
    let original = registry();
    let task_id = start_runon_retry(&original, dir.path(), daemon.connection.clone());
    clock.expect_event(DrainingClockEvent::Stopped).await;
    let task = original.task(&task_id).unwrap();
    original.record_remote_cancel(&task).unwrap();
    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    let done = terminal(&restarted, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Killed, "{done:?}");
    assert_eq!(exec_runs(&daemon).len(), 1);
}

#[tokio::test]
async fn runon_draining_restart_does_not_resubmit_ambiguous_dispatch() {
    let daemon = crate::exec_remote::wire_tests::daemon_with_clients(
        Script::Draining {
            refusals: 1,
            retry_after_ms: Some(10_000),
        },
        "exec-remote/v1",
        2,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut clock = PausedDrainingClock::new(dir.path());
    stopped_draining_roots()
        .lock()
        .unwrap()
        .insert(dir.path().into());
    let original = registry();
    let task_id = start_runon_retry(&original, dir.path(), daemon.connection.clone());
    clock.expect_event(DrainingClockEvent::Stopped).await;
    let task = original.task(&task_id).unwrap();
    let mut saved = read_task(&task.paths.json).unwrap();
    let remote = saved.remote.as_mut().unwrap();
    remote.terminal = None;
    remote.job_id = None;
    remote.retry.as_mut().unwrap().attempts = 2;
    write_task(&task.paths.json, &saved).unwrap();
    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    let done = terminal(&restarted, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::FateUnknown, "{done:?}");
    assert_eq!(
        exec_runs(&daemon).len(),
        1,
        "missing acceptance must never authorize resubmission"
    );
}

#[tokio::test]
async fn runon_draining_network_retry_preserves_outbound_and_vcpu_request() {
    let daemon = daemon(
        Script::Draining {
            refusals: 1,
            retry_after_ms: Some(50),
        },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut clock = PausedDrainingClock::new(dir.path());
    let ctx = restarted_context(dir.path());
    let ((reply, _), ()) = tokio::join!(
        orchestrated_reply(
            &ctx,
            &daemon,
            serde_json::json!({
                "command": "printf remote-proof",
                "runon": "linux,4c,net",
                "wait": true,
                "timeout": 5_000,
                "foreground_orchestrate": true,
                "compressed": false,
            }),
        ),
        async {
            let first = clock.now();
            clock.expect_sleep(first, first + 50).await;
            clock.advance(50);
        },
    );
    assert!(reply.success, "{reply:?}");
    assert_eq!(reply.data["status"], "completed", "{reply:?}");
    assert_eq!(reply.data["exit_code"], 0, "{reply:?}");
    let requests = exec_runs(&daemon);
    assert_eq!(
        requests.len(),
        2,
        "a never-started draining refusal must retry"
    );
    assert_eq!(requests[0], requests[1]);
    for request in requests {
        let request: RunRequest = serde_json::from_value(request).unwrap();
        assert_eq!(request.network, Some(Network::Outbound));
        assert_eq!(request.weight_hint, Some(4));
    }
}

#[tokio::test]
async fn head_blocking_runon_uses_configured_cap_while_queued() {
    let daemon = daemon(Script::GatedStages { position: 9 }, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    ctx.update_config(|config| config.bash.worker_wait_max_ms = 250);
    let (reply, waited) = orchestrated_reply_with_probe(
        &ctx,
        &daemon,
        serde_json::json!({
            "command": "uname -s",
            "runon": "linux",
            "wait": true,
            "timeout": 5_000,
            "foreground_orchestrate": true,
            "compressed": false,
        }),
        |task_id| {
            ctx.bash_background()
                .remote_progress(task_id, "session")
                .is_some_and(|progress| progress.phase == RemotePhase::Queued { position: 9 })
        },
    )
    .await;
    assert!(reply.success, "{reply:?}");
    assert!(waited >= Duration::from_millis(250), "{waited:?}");
    assert!(waited < plugin_transport_deadline(250), "{waited:?}");
    assert_eq!(reply.data["status"], "running", "{reply:?}");
    assert_eq!(reply.data["remote_phase"], "queued", "{reply:?}");
    assert_eq!(reply.data["queue_position"], 9, "{reply:?}");
    assert_eq!(reply.data["remote_job_id"], id().to_string(), "{reply:?}");
    assert_eq!(runner_cancels(&daemon), 0);
    assert_eq!(exec_runs(&daemon).len(), 1);
    let registry = ctx.bash_background().clone();
    let task_id = reply.data["task_id"].as_str().unwrap().to_string();
    tokio::task::spawn_blocking(move || registry.kill(&task_id, "session"))
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn head_blocking_runon_uses_configured_cap_before_any_stream_record() {
    assert_blocking_runon_limit_before_acceptance(5_000, Some(250)).await;
}

#[tokio::test]
async fn blocking_runon_wait_limit_before_accepted_reports_input_capture() {
    assert_blocking_runon_limit_before_acceptance(250, None).await;
}

async fn assert_blocking_runon_limit_before_acceptance(timeout_ms: u64, cap_ms: Option<u64>) {
    let daemon = daemon(Script::GatedAcceptance, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    if let Some(cap_ms) = cap_ms {
        ctx.update_config(|config| config.bash.worker_wait_max_ms = cap_ms);
    }
    let limit_ms = cap_ms.unwrap_or(timeout_ms);
    let (reply, waited) = orchestrated_reply_with_probe(
        &ctx,
        &daemon,
        serde_json::json!({
            "command": "uname -s",
            "runon": "linux",
            "wait": true,
            "timeout": timeout_ms,
            "foreground_orchestrate": true,
            "compressed": false,
        }),
        |_| !exec_runs(&daemon).is_empty(),
    )
    .await;
    assert!(reply.success, "{reply:?}");
    assert!(waited >= Duration::from_millis(limit_ms), "{waited:?}");
    assert!(waited < plugin_transport_deadline(limit_ms), "{waited:?}");
    assert_eq!(reply.data["status"], "starting", "{reply:?}");
    assert_eq!(reply.data["remote_phase"], "submitting", "{reply:?}");
    assert_eq!(reply.data["queue_position"], serde_json::Value::Null);
    assert_eq!(reply.data["remote_job_id"], serde_json::Value::Null);
    assert!(
        reply.data["output"]
            .as_str()
            .unwrap()
            .contains("capturing inputs; not accepted yet"),
        "{reply:?}"
    );
    assert_eq!(runner_cancels(&daemon), 0);
    assert_eq!(exec_runs(&daemon).len(), 1);
    // Let AFT learn the job ID so cleanup can cancel the held-open stream.
    daemon.acceptance_gate.notify_one();
    let task_id = reply.data["task_id"].as_str().unwrap().to_string();
    wait_for(
        "released acceptance must be processed before cancellation",
        || {
            ctx.bash_background()
                .remote_progress(&task_id, "session")
                .filter(|progress| progress.phase == RemotePhase::Queued { position: 1 })
        },
    )
    .await;
    let registry = ctx.bash_background().clone();
    tokio::task::spawn_blocking(move || registry.kill(&task_id, "session"))
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn runon_non_git_workdir_is_refused_by_name_before_dispatch() {
    let daemon = daemon(Script::Plain, "exec-remote/v1").await;
    let project = tempfile::tempdir().unwrap();
    let ctx = restarted_context(project.path());
    // The runner may place TMPDIR inside a checkout. Use the system's other
    // Unix temporary root so this workdir cannot inherit that repository.
    let workdir = tempfile::Builder::new()
        .prefix("aft-runon-non-git-")
        .tempdir_in("/var/tmp")
        .unwrap();
    assert!(worktree_root_containing(workdir.path()).unwrap().is_none());
    let marker = workdir.path().join("must-not-run-locally");
    let response = handle_with_policy(
        &ctx,
        Some(launch(daemon.connection.clone())),
        serde_json::json!({
            "command": format!("printf local-proof > '{}'", marker.display()),
            "runon": "linux",
            "workdir": workdir.path(),
            "compressed": false,
        }),
    );
    assert!(response.success, "{response:?}");
    let done = terminal(
        ctx.bash_background(),
        response.data["task_id"].as_str().unwrap(),
    )
    .await;
    assert_eq!(done.info.status, BgTaskStatus::Failed, "{done:?}");
    for expected in [
        "runon refused before remote dispatch; command did not run",
        "runon refused: remote execution is unavailable; command did not run",
        "cwd resolves outside the workspace key",
    ] {
        assert!(done.output_preview.contains(expected), "{done:?}");
    }
    let named = format!(
        "workdir {} is not inside a git checkout; this session's checkout is ",
        workdir.path().canonicalize().unwrap().display()
    );
    assert!(done.output_preview.contains(&named), "{done:?}");
    assert!(!marker.exists(), "a refused runon must never spawn locally");
    assert!(
        exec_runs(&daemon).is_empty(),
        "a non-git cwd must never dispatch"
    );
}

#[tokio::test]
async fn exec_remote_run_scope_revocation_reports_reason() {
    let daemon = daemon(Script::Cancel, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let task = start(&registry, dir.path(), daemon.connection.clone());
    wait_for(
        "acceptance must be consumed before revoking the scope",
        || {
            registry
                .task(&task)
                .unwrap()
                .state
                .lock()
                .unwrap()
                .metadata
                .remote
                .as_ref()
                .unwrap()
                .job_id
                .map(|_| ())
        },
    )
    .await;
    let killing = registry.clone();
    let done = tokio::task::spawn_blocking(move || {
        killing.kill_for_scope_revocation(&task, "session").unwrap()
    })
    .await
    .unwrap();
    assert_eq!(done.info.status, BgTaskStatus::Killed);
    assert_eq!(
        done.info.status_reason.as_deref(),
        Some(crate::bash_background::registry::SCOPE_REVOKED_REASON)
    );
    assert!(daemon
        .log
        .lock()
        .unwrap()
        .iter()
        .any(|(_, body)| body["method"] == "exec.cancel"));
}

#[tokio::test]
async fn optional_worker_broker_remote_local_fallback_without_service_runs() {
    use std::os::unix::fs::PermissionsExt;
    let daemon = daemon(Script::Refused, "exec-remote/v1").await;
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    ensure_git_fixture(project.path());
    let temp = project.path().join("000000000001");
    std::fs::create_dir(&temp).unwrap();
    std::fs::write(
        project.path().join("sandbox-launch"),
        "while [ \"$1\" != -- ]; do shift; done; shift; exec \"$@\"\n",
    )
    .unwrap();
    std::fs::set_permissions(
        project.path().join("sandbox-launch"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let mut profile = crate::sandbox_profile::SandboxProfile::build(
        vec![project.path().to_owned()],
        vec![],
        vec![],
        vec![],
        vec![],
        vec![],
        vec![],
        temp.clone(),
    )
    .unwrap();
    profile.mode = crate::config::SandboxMode::Strict;
    let plan = crate::sandbox_spawn::SpawnPlan::launcher_for_test(profile, "/bin/sh".into());
    let ctx = crate::context::AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(project.path().to_owned()),
            storage_dir: Some(storage.path().to_owned()),
            experimental_bash_background: true,
            ..Default::default()
        },
    );
    let command="test -z \"${AFT_GH_READ_SOCKET+x}\" && test -z \"${AFT_GH_READ_CAPABILITY_FILE+x}\" && test -z \"${AFT_GH_SHIM_TICKET+x}\" && printf FALLBACK_WITHOUT_BROKER";
    let response = crate::gh_shim_worker_broker::without_service_for_test(|| {
        crate::sandbox_spawn::with_worker_jail(true, || {
            crate::sandbox_spawn::with_spawn_plan_for_test(plan, || {
                crate::bash_background::spawn(
                    "optional-fallback",
                    "session",
                    command,
                    crate::bash_background::BashShell::Bash,
                    resolve_posix_shell(),
                    Some(project.path().to_owned()),
                    None,
                    crate::bash_background::HardKill::After(Duration::from_secs(30)),
                    &ctx,
                    true,
                    true,
                    false,
                    false,
                    24,
                    80,
                    vec![],
                    false,
                    None,
                    Some(launch(daemon.connection.clone())),
                )
            })
        })
    });
    assert!(response.success, "{response:?}");
    let task = response.data["task_id"].as_str().unwrap();
    let done = terminal(ctx.bash_background(), task).await;
    assert_eq!(done.exit_code, Some(0), "{}", done.output_preview);
    assert!(
        done.output_preview.contains("FALLBACK_WITHOUT_BROKER"),
        "{}",
        done.output_preview
    );
    assert!(
        done.output_preview.contains("ran locally"),
        "{}",
        done.output_preview
    );
    assert!(!temp.join(".gh-read-capability").exists());
    assert!(!temp.with_file_name("000000000001.gh-read.sock").exists());
}

fn hinted_retry_budgets() -> &'static Mutex<HashMap<PathBuf, u64>> {
    static BUDGETS: std::sync::OnceLock<Mutex<HashMap<PathBuf, u64>>> = std::sync::OnceLock::new();
    BUDGETS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) fn hinted_retry_budget_ms(root: &Path) -> Option<u64> {
    hinted_retry_budgets()
        .lock()
        .unwrap()
        .get(&test_clock_key(root))
        .copied()
}

/// Shorten the time a job submitted under `root` has for the runner to admit
/// it, which bounds how long AFT keeps resubmitting after hinted refusals.
fn shorten_hinted_retry_budget(root: &Path, ms: u64) {
    hinted_retry_budgets()
        .lock()
        .unwrap()
        .insert(test_clock_key(root), ms);
}

/// The worker asked the paused clock for no sleep since the last one a test
/// consumed.
fn assert_no_retry_sleep(clock: &mut PausedDrainingClock) {
    if let Ok(event) = clock.events.try_recv() {
        panic!("no retry wait was expected, got {event:?}");
    }
}

const SIGNER_REFUSED: &str = "runner_authentication_failed (runner_sign_unavailable)";

#[test]
fn hinted_retry_needs_the_named_refusal_ran_none_and_a_hint() {
    use crate::exec_remote::wire_tests::signer_refusal;
    assert_eq!(
        hinted_retry_after_ms(&signer_refusal("runner_sign_unavailable", Some(2000))),
        Some(2000)
    );
    assert_eq!(
        hinted_retry_after_ms(&signer_refusal("runner_sign_invalid_response", Some(5))),
        Some(5)
    );
    // A refusal that needs an operator is never resubmitted, even with a hint.
    assert_eq!(
        hinted_retry_after_ms(&signer_refusal("runner_sign_missing", Some(2000))),
        None
    );
    assert_eq!(
        hinted_retry_after_ms(&signer_refusal("runner_sign_unavailable", None)),
        None
    );
    // Only `ran: "none"` proves the command did not start.
    for ran in [
        None,
        Some(Ran::Remote),
        Some(Ran::Unknown("elsewhere".into())),
    ] {
        let mut record = signer_refusal("runner_sign_unavailable", Some(2000));
        record.ran = ran.clone();
        assert_eq!(hinted_retry_after_ms(&record), None, "{ran:?}");
    }
    // Other hinted refusals keep their own handling.
    for reason in [RefusalReason::RunnerDraining, RefusalReason::RunnerDiskFull] {
        let mut record = signer_refusal("runner_sign_unavailable", Some(2000));
        record.outcome = Outcome::RefusedBeforeStart { reason };
        assert_eq!(hinted_retry_after_ms(&record), None);
    }
}

#[tokio::test]
async fn runon_signer_refusal_is_resubmitted_after_its_hint_and_only_success_is_seen() {
    use crate::exec_remote::wire_tests::{signer_refusal, SIGNER_REFUSAL_VECTOR};
    // The fake runner sends ck-motor's record exactly, losing no field.
    assert_eq!(
        serde_json::to_value(StreamRecord::Terminal(signer_refusal(
            "runner_sign_unavailable",
            Some(2000)
        )))
        .unwrap(),
        serde_json::from_str::<serde_json::Value>(SIGNER_REFUSAL_VECTOR).unwrap()
    );
    let daemon = daemon(
        Script::SignerRefused {
            refusals: 1,
            detail: "runner_sign_unavailable",
            retry_after_ms: Some(2000),
        },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut clock = PausedDrainingClock::new(dir.path());
    let registry = registry();
    let task = start_runon_retry(&registry, dir.path(), daemon.connection.clone());
    let first = clock.now();
    clock.expect_sleep(first, first + 2000).await;
    let waiting = registry.observed_status(&task, "session", 8192).unwrap();
    assert!(!waiting.info.status.is_terminal(), "{waiting:?}");
    assert!(waiting.remote_refusal.is_none(), "{waiting:?}");
    assert_eq!(
        registry.runon_launch_pending(&task, "session"),
        Some(true),
        "a foreground launch keeps waiting while the retry is pending"
    );
    assert_eq!(exec_runs(&daemon).len(), 1);
    clock.advance(2000);
    let done = terminal(&registry, &task).await;
    assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");
    assert_eq!(done.exit_code, Some(0), "{done:?}");
    assert!(done.remote_refusal.is_none(), "{done:?}");
    for hidden in ["runner_authentication_failed", "refused", "local-proof"] {
        assert!(!done.output_preview.contains(hidden), "{done:?}");
    }
    let requests = exec_runs(&daemon);
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0], requests[1], "the same request is sent again");
    assert_no_retry_sleep(&mut clock);
}

#[tokio::test]
async fn runon_signer_hint_longer_than_the_admission_time_left_surfaces_the_refusal() {
    let daemon = daemon(
        Script::SignerRefused {
            refusals: u32::MAX,
            detail: "runner_sign_unavailable",
            retry_after_ms: Some(2000),
        },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    // Room for one 2 s wait, counted from the first submission; the second
    // refusal's hint is longer than the 1 s then left.
    shorten_hinted_retry_budget(dir.path(), 3000);
    let mut clock = PausedDrainingClock::new(dir.path());
    let registry = registry();
    let task = start_runon_retry(&registry, dir.path(), daemon.connection.clone());
    let first = clock.now();
    clock.expect_sleep(first, first + 2000).await;
    clock.advance(2000);
    let done = terminal(&registry, &task).await;
    assert_eq!(done.info.status, BgTaskStatus::Failed, "{done:?}");
    assert_runon_refusal(&refusal_response(done), SIGNER_REFUSED);
    assert_eq!(exec_runs(&daemon).len(), 2);
    assert_eq!(clock.now(), first + 2000);
    assert_no_retry_sleep(&mut clock);
}

#[tokio::test]
async fn runon_operator_needed_signer_refusal_surfaces_at_once() {
    let daemon = daemon(
        Script::SignerRefused {
            refusals: u32::MAX,
            detail: "runner_sign_missing",
            retry_after_ms: None,
        },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut clock = PausedDrainingClock::new(dir.path());
    let registry = registry();
    let task = start_runon_retry(&registry, dir.path(), daemon.connection.clone());
    let done = terminal(&registry, &task).await;
    assert_eq!(done.info.status, BgTaskStatus::Failed, "{done:?}");
    assert_runon_refusal(
        &refusal_response(done),
        "runner_authentication_failed (runner_sign_missing)",
    );
    assert_eq!(exec_runs(&daemon).len(), 1);
    assert_no_retry_sleep(&mut clock);
}

#[tokio::test]
async fn runon_signer_retries_stop_after_five_resubmissions() {
    let daemon = daemon(
        Script::SignerRefused {
            refusals: u32::MAX,
            detail: "runner_sign_unavailable",
            retry_after_ms: Some(100),
        },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut clock = PausedDrainingClock::new(dir.path());
    let registry = registry();
    let task = start_runon_retry(&registry, dir.path(), daemon.connection.clone());
    let first = clock.now();
    for resubmission in 0..5 {
        let from = first + resubmission * 100;
        clock.expect_sleep(from, from + 100).await;
        clock.advance(100);
    }
    let done = terminal(&registry, &task).await;
    assert_eq!(done.info.status, BgTaskStatus::Failed, "{done:?}");
    assert_runon_refusal(&refusal_response(done), SIGNER_REFUSED);
    assert_eq!(
        exec_runs(&daemon).len(),
        6,
        "one submission plus five resubmissions"
    );
    assert_no_retry_sleep(&mut clock);
}

#[tokio::test]
async fn runon_kill_during_a_signer_retry_wait_ends_the_task_without_resubmitting() {
    let daemon = daemon(
        Script::SignerRefused {
            refusals: u32::MAX,
            detail: "runner_sign_unavailable",
            retry_after_ms: Some(2000),
        },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut clock = PausedDrainingClock::new(dir.path());
    let registry = registry();
    let task_id = start_runon_retry(&registry, dir.path(), daemon.connection.clone());
    let first = clock.now();
    clock.expect_sleep(first, first + 2000).await;
    let killing = registry.clone();
    let killed_id = task_id.clone();
    // With the retry clock frozen, a terminal kill proves cancellation woke
    // the worker; the wall-clock timeout only guards against a deadlock.
    let killed = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || killing.kill(&killed_id, "session")),
    )
    .await
    .expect("kill must interrupt the retry wait without advancing the clock")
    .unwrap()
    .unwrap();
    assert_eq!(killed.info.status, BgTaskStatus::Killed);
    assert_eq!(clock.now(), first);
    clock.advance(2000);
    // Give a worker that wrongly kept waiting the chance to resubmit.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        exec_runs(&daemon).len(),
        1,
        "kill must veto any new exec.run"
    );
}

/// A draining resubmission is a fresh submission: a signer refusal with a
/// hint on it is resubmitted too, within an admission time counted from that
/// resubmission. The shortened budget is spent long before the draining wait
/// ends, so counting from the first submission would surface the refusal.
#[tokio::test]
async fn runon_signer_retry_follows_a_live_draining_resubmission() {
    let daemon = daemon(
        Script::DrainingThenSigner {
            signer_refusals: 1,
            draining_after_ms: 500,
            signer_after_ms: 100,
        },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    shorten_hinted_retry_budget(dir.path(), 150);
    let mut clock = PausedDrainingClock::new(dir.path());
    let registry = registry();
    let task = start_runon_retry(&registry, dir.path(), daemon.connection.clone());
    let first = clock.now();
    clock.expect_sleep(first, first + 500).await;
    clock.advance(500);
    clock.expect_sleep(first + 500, first + 600).await;
    assert_eq!(exec_runs(&daemon).len(), 2);
    clock.advance(100);
    let done = terminal(&registry, &task).await;
    assert_eq!(done.exit_code, Some(0), "{done:?}");
    assert!(done.remote_refusal.is_none(), "{done:?}");
    let requests = exec_runs(&daemon);
    assert_eq!(requests.len(), 3);
    assert!(requests.windows(2).all(|pair| pair[0] == pair[1]));
    assert_no_retry_sleep(&mut clock);
}

/// The same after a restart: the saved draining refusal is resumed by a new
/// daemon, whose draining resubmission meets a hinted signer refusal.
#[tokio::test]
async fn runon_signer_retry_follows_a_draining_resubmission_resumed_after_restart() {
    let daemon = crate::exec_remote::wire_tests::daemon_with_clients(
        Script::DrainingThenSigner {
            signer_refusals: 1,
            draining_after_ms: 200,
            signer_after_ms: 100,
        },
        "exec-remote/v1",
        2,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    shorten_hinted_retry_budget(dir.path(), 150);
    let mut clock = PausedDrainingClock::new(dir.path());
    stopped_draining_roots()
        .lock()
        .unwrap()
        .insert(dir.path().into());
    let original = registry();
    let task = start_runon_retry(&original, dir.path(), daemon.connection.clone());
    clock.expect_event(DrainingClockEvent::Stopped).await;
    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    let first = clock.now();
    clock.expect_sleep(first, first + 200).await;
    clock.advance(200);
    clock.expect_sleep(first + 200, first + 300).await;
    assert_eq!(exec_runs(&daemon).len(), 2);
    clock.advance(100);
    let done = terminal(&restarted, &task).await;
    assert_eq!(done.exit_code, Some(0), "{done:?}");
    assert!(done.remote_refusal.is_none(), "{done:?}");
    let requests = exec_runs(&daemon);
    assert_eq!(requests.len(), 3);
    assert!(requests.windows(2).all(|pair| pair[0] == pair[1]));
    assert_no_retry_sleep(&mut clock);
}

/// A retry wait that ends after the job's admission time has run out sends
/// nothing: the clock moves past the deadline while the worker waits.
#[tokio::test]
async fn runon_signer_retry_that_wakes_after_the_deadline_surfaces_the_refusal() {
    let daemon = daemon(
        Script::SignerRefused {
            refusals: u32::MAX,
            detail: "runner_sign_unavailable",
            retry_after_ms: Some(2000),
        },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    shorten_hinted_retry_budget(dir.path(), 3000);
    let mut clock = PausedDrainingClock::new(dir.path());
    let registry = registry();
    let task = start_runon_retry(&registry, dir.path(), daemon.connection.clone());
    let first = clock.now();
    clock.expect_sleep(first, first + 2000).await;
    // The worker wakes 5 s after the first submission, 2 s past its deadline.
    clock.advance(5000);
    let done = terminal(&registry, &task).await;
    assert_eq!(done.info.status, BgTaskStatus::Failed, "{done:?}");
    assert_runon_refusal(&refusal_response(done), SIGNER_REFUSED);
    assert_eq!(exec_runs(&daemon).len(), 1, "no request after the deadline");
    assert_no_retry_sleep(&mut clock);
}

/// A zero hint on a job whose admission time is already used up sends
/// nothing either, although it needs no wait at all.
#[tokio::test]
async fn runon_signer_zero_hint_after_the_deadline_surfaces_the_refusal() {
    let daemon = daemon(
        Script::SignerRefused {
            refusals: u32::MAX,
            detail: "runner_sign_unavailable",
            retry_after_ms: Some(0),
        },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    // No admission time at all: the deadline is the first submission itself.
    shorten_hinted_retry_budget(dir.path(), 0);
    let mut clock = PausedDrainingClock::new(dir.path());
    let registry = registry();
    let task = start_runon_retry(&registry, dir.path(), daemon.connection.clone());
    let done = terminal(&registry, &task).await;
    assert_eq!(done.info.status, BgTaskStatus::Failed, "{done:?}");
    assert_runon_refusal(&refusal_response(done), SIGNER_REFUSED);
    assert_eq!(exec_runs(&daemon).len(), 1, "no request after the deadline");
    assert_no_retry_sleep(&mut clock);
}

#[test]
fn precommit_remote_handle_failure_removes_layout() {
    use crate::bash_background::persistence::{with_task_io_fault, TaskIoFault};
    let dir = tempfile::tempdir().unwrap();
    ensure_git_fixture(dir.path());
    let registry = registry();
    let result = with_task_io_fault(TaskIoFault::OutputHandlesEnospc, || {
        registry.spawn_remote(
            launch(dir.path().join("unused-connection")),
            SpawnPlan::Unsandboxed,
            "exit 0",
            resolve_posix_shell(),
            "session".into(),
            dir.path().into(),
            HashMap::new(),
            crate::bash_background::HardKill::Default,
            dir.path().into(),
            crate::bash_background::TaskSlot::Foreground,
            true,
            false,
            Some(dir.path().into()),
        )
    });
    assert!(result.unwrap_err().contains("No space left on device"));
    let session = session_tasks_dir(dir.path(), "session");
    assert_eq!(
        fs::read_dir(session).unwrap().count(),
        0,
        "uncommitted remote layout leaked"
    );
    assert!(registry.list(0).is_empty());
}

/// A runner that offers chunked file return beside ordinary runs.
const RETURN_CLAIMS: &[&str] = &["exec-remote/v1", "exec-remote-return/v2"];

async fn return_daemon(vector: &'static str) -> crate::exec_remote::wire_tests::Daemon {
    crate::exec_remote::wire_tests::daemon_with_capabilities(
        Script::ReturnVector(vector),
        RETURN_CLAIMS,
        1,
    )
    .await
}

fn return_call(paths: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "command": "make dist", "runon": "linux", "wait": true,
        "foreground_orchestrate": true, "compressed": false,
        "return_paths": paths,
    })
}

#[tokio::test]
async fn runon_return_paths_places_verified_files_and_lists_every_path() {
    // One path returned, one missing on the server, in one job.
    let daemon = return_daemon("failed-missing").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    let (reply, _) = orchestrated_reply(
        &ctx,
        &daemon,
        return_call(&["dist/hello.txt", "dist/absent.txt"]),
    )
    .await;
    assert!(reply.success, "{reply:?}");
    let output = reply.data["output"].as_str().unwrap();
    let report = "Files requested back from the server (return_paths):\n  \
                  dist/hello.txt: returned (5 bytes)\n  \
                  dist/absent.txt: not returned (no such file on the server)";
    assert!(output.contains(report), "{output}");
    // The job id stays in the result header, with the copy-back list below.
    let header = output.lines().next().unwrap();
    assert!(header.contains(&format!("job {}", id())), "{output}");
    assert!(
        output.find(&format!("job {}", id())).unwrap() < output.find(report).unwrap(),
        "{output}"
    );
    assert_eq!(
        fs::read(dir.path().join("dist/hello.txt")).unwrap(),
        b"hello"
    );
    assert!(!dir.path().join("dist/absent.txt").exists());
    let leftovers: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".aft-return-")
        })
        .collect();
    assert!(leftovers.is_empty(), "staging files left: {leftovers:?}");
    // The request names the paths and always the cap.
    let runs = exec_runs(&daemon);
    assert_eq!(runs.len(), 1);
    assert_eq!(
        runs[0]["return_paths"],
        serde_json::json!(["dist/hello.txt", "dist/absent.txt"])
    );
    assert_eq!(runs[0]["return_chunked"], true);
    assert_eq!(runs[0]["return_max_bytes"], 10_485_760);
    // The completion notice prints the same saved report.
    let task_id = reply.data["task_id"].as_str().unwrap();
    let saved = read_task(&ctx.bash_background().task(task_id).unwrap().paths.json).unwrap();
    assert!(
        remote::remote_report(&saved).unwrap().starts_with(report),
        "{saved:?}"
    );
}

#[tokio::test]
async fn runon_return_paths_expired_replay_reports_expired_without_output_loss() {
    let daemon = return_daemon("replay-expired").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    let (reply, _) = orchestrated_reply(
        &ctx,
        &daemon,
        return_call(&["dist/app.bin", "dist/absent.txt"]),
    )
    .await;
    assert!(reply.success, "{reply:?}");
    let output = reply.data["output"].as_str().unwrap();
    assert!(
        output.contains(
            "  dist/app.bin: not returned (the server no longer keeps this job's files)\n  \
             dist/absent.txt: not returned (no such file on the server)"
        ),
        "{output}"
    );
    assert!(!output.contains("output lost"), "{output}");
    assert!(!dir.path().join("dist").exists());
    let attaches = daemon
        .log
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, body)| body["method"] == "exec.attach")
        .count();
    assert_eq!(attaches, 0, "the chunk gap is accepted, not recovered");
}

#[tokio::test]
async fn runon_return_paths_digest_mismatch_keeps_the_existing_file() {
    let daemon = return_daemon("refuse-digest-mismatch").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    fs::create_dir(dir.path().join("dist")).unwrap();
    fs::write(dir.path().join("dist/app.bin"), b"older build").unwrap();
    let (reply, _) = orchestrated_reply(&ctx, &daemon, return_call(&["dist/app.bin"])).await;
    assert!(reply.success, "{reply:?}");
    let output = reply.data["output"].as_str().unwrap();
    assert!(
        output.contains(
            "  dist/app.bin: not returned (the copy failed verification (digest_mismatch); nothing was written)"
        ),
        "{output}"
    );
    assert_eq!(
        fs::read(dir.path().join("dist/app.bin")).unwrap(),
        b"older build"
    );
}

#[tokio::test]
async fn runon_return_paths_without_the_return_capability_refuses_before_submit() {
    let daemon = daemon(Script::Plain, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    let marker = dir.path().join("local-spawn-proof");
    let mut call = return_call(&["dist/app.bin"]);
    call["command"] = serde_json::json!(format!("touch '{}'", marker.display()));
    let (reply, _) = orchestrated_reply(&ctx, &daemon, call).await;
    assert!(!reply.success, "{reply:?}");
    let message = reply.data["message"].as_str().unwrap();
    assert!(
        message.contains(
            "return_paths is unavailable: the build server doesn't offer copying files back yet (missing exec-remote-return/v2)"
        ),
        "{message}"
    );
    assert!(exec_runs(&daemon).is_empty(), "nothing may be submitted");
    assert!(!marker.exists(), "a refused runon never runs locally");
}

#[test]
fn runon_return_paths_outside_the_project_are_refused_before_anything_runs() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
    let marker = dir.path().join("local-spawn-proof");
    let cases: [(serde_json::Value, Option<&str>, &str); 4] = [
        (
            serde_json::json!(["../outside.txt"]),
            Some("linux"),
            "must not use ..",
        ),
        (
            serde_json::json!(["link/out.txt"]),
            Some("linux"),
            "return_paths refused: link/out.txt",
        ),
        (
            serde_json::json!([outside.path().join("x").display().to_string()]),
            Some("linux"),
            "must be relative to workdir",
        ),
        (
            serde_json::json!(["dist/app.bin"]),
            None,
            "return_paths needs runon",
        ),
    ];
    for (paths, runon, expected) in cases {
        let mut params = serde_json::json!({
            "command": format!("touch '{}'", marker.display()),
            "workdir": dir.path(),
            "return_paths": paths,
        });
        if let Some(runon) = runon {
            params["runon"] = serde_json::json!(runon);
        }
        let request = orchestrated_request(params);
        let response = crate::bash_background::with_remote_policy(
            Some(launch(dir.path().join("no-daemon"))),
            || crate::commands::bash::handle(&request, &ctx),
        );
        assert!(!response.success, "{response:?}");
        assert_eq!(
            response.data["code"], "return_paths_refused",
            "{response:?}"
        );
        let message = response.data["message"].as_str().unwrap();
        assert!(message.contains(expected), "{expected}: {message}");
    }
    assert!(!marker.exists(), "a refused call never runs");
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn local_fallback_with_return_paths_says_the_files_stayed_in_place() {
    // Prefix routing (not runon) may still fall back to a local run when the
    // runner refuses before start. A task that asked for files back says
    // they were written here and nothing was copied.
    let daemon = crate::exec_remote::wire_tests::daemon_with_capabilities(
        Script::KnownRefused,
        RETURN_CLAIMS,
        1,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    ensure_git_fixture(dir.path());
    let registry = registry();
    let mut policy = launch(daemon.connection.clone());
    policy.return_paths = vec!["out.txt".into()];
    let task = registry
        .spawn_remote(
            policy,
            SpawnPlan::Unsandboxed,
            "printf local > out.txt",
            resolve_posix_shell(),
            "session".into(),
            dir.path().into(),
            HashMap::new(),
            crate::bash_background::HardKill::After(Duration::from_secs(30)),
            dir.path().into(),
            crate::bash_background::TaskSlot::Background { max: 10 },
            true,
            false,
            Some(dir.path().into()),
        )
        .unwrap();
    let done = terminal(&registry, &task).await;
    assert_eq!(exec_runs(&daemon).len(), 1);
    assert_eq!(fs::read(dir.path().join("out.txt")).unwrap(), b"local");
    assert!(
        done.output_preview
            .contains(crate::bash_background::registry::return_files::LOCAL_RUN_NOTE),
        "{}",
        done.output_preview
    );
}

#[tokio::test]
async fn runon_return_paths_copied_files_are_not_listed_as_left_on_the_server() {
    // The command runs in `sub/`, so the runner names its files relative to
    // the checkout (`sub/dist/hello.txt`) while return_paths names them
    // relative to the workdir (`dist/hello.txt`).
    let daemon = crate::exec_remote::wire_tests::daemon_with_capabilities(
        Script::ReturnVectorReporting(
            "failed-missing",
            &["sub/dist/hello.txt", "sub/dist/absent.txt", "sub/other.txt"],
        ),
        RETURN_CLAIMS,
        1,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    let workdir = dir.path().join("sub");
    fs::create_dir(&workdir).unwrap();
    let mut call = return_call(&["dist/hello.txt", "dist/absent.txt"]);
    call["workdir"] = serde_json::json!(workdir);
    let (reply, _) = orchestrated_reply(&ctx, &daemon, call).await;
    assert!(reply.success, "{reply:?}");
    assert_eq!(fs::read(workdir.join("dist/hello.txt")).unwrap(), b"hello");
    let output = reply.data["output"].as_str().unwrap();
    assert!(
        output.contains(
            "These files changed on the server and were NOT copied back:\n  \
             sub/dist/absent.txt\n  sub/other.txt\n"
        ),
        "{output}"
    );
    // ck-motor sends no separate untracked list; its changed list holds them.
    assert!(
        output.contains("untracked files: not reported by the runner"),
        "{output}"
    );
    assert!(!output.contains("sub/dist/hello.txt"), "{output}");
}

#[test]
fn copied_back_files_leave_the_changed_list_on_linux_and_windows_runners() {
    use crate::bash_background::registry::return_files::{ReturnOutcome, ReturnedPath};
    let mut terminal = TerminalRecord::new(id(), Outcome::Exit { code: 0 }, 1, 0, 0);
    terminal.workspace_changes = Some(vec!["dist/hello.txt".into(), "other.txt".into()]);
    let mut remote: RemoteTask = serde_json::from_value(serde_json::json!({
        "harness": "broca", "session": "session", "stdout_len": 0,
        "stderr_len": 0, "cancel_requested": false,
    }))
    .unwrap();
    remote.accepted = true;
    remote.terminal = Some(terminal);
    remote.return_paths = vec!["dist/hello.txt".into()];
    remote.returned_files = Some(vec![ReturnedPath {
        path: "dist/hello.txt".into(),
        checkout_path: Some("dist/hello.txt".into()),
        outcome: ReturnOutcome::Returned { bytes: 5 },
    }]);
    let mut metadata = PersistedTask::starting(
        "task".into(),
        "session".into(),
        "make dist".into(),
        "/work".into(),
        Some("/work".into()),
        Some(30000),
        true,
        false,
    );
    // ck-motor names changed files the same way on both runners.
    for platform in [None, Some(Platform::Linux), Some(Platform::Windows)] {
        let mut remote = remote.clone();
        remote.requested_platform = platform.clone();
        remote.accepted_platform = platform.clone();
        metadata.remote = Some(remote);
        let report = remote_report(&metadata).unwrap();
        assert!(
            report.starts_with(
                "Files requested back from the server (return_paths):\n  \
                 dist/hello.txt: returned (5 bytes)\n\
                 These files changed on the server and were NOT copied back:\n  \
                 other.txt\n"
            ),
            "{platform:?}: {report}"
        );
        assert_eq!(report.matches("dist/hello.txt").count(), 1, "{report}");
    }
}

#[tokio::test]
async fn runon_return_paths_restart_before_the_manifest_replays_chunks_and_places_once() {
    let daemon = return_daemon("three-chunks-short-last").await;
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    let original = registry();
    let db = Arc::new(Mutex::new(crate::db::open(&root.join("aft.db")).unwrap()));
    original.set_harness(crate::harness::Harness::Runner);
    original.set_db_pool(db.clone());
    let layout = allocate_task_layout(&root, "session").unwrap();
    let task_id = layout.paths.task_id.clone();
    let mut metadata = PersistedTask::starting(
        task_id.clone(),
        "session".into(),
        "make dist".into(),
        root.clone(),
        Some(root.clone()),
        Some(30000),
        true,
        false,
    );
    metadata.status = BgTaskStatus::Running;
    metadata.harness = Some("runner".into());
    metadata.execution_note = Some("ran remotely on ck-motor".into());
    metadata.remote = Some(RemoteTask {
        accepted_platform: None,
        explicit_runon: true,
        connection_file: Some(daemon.connection.clone()),
        harness: "broca".into(),
        session: "session".into(),
        requested_platform: None,
        requested_vcpus: None,
        requested_network: false,
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
        return_paths: vec!["dist/app.bin".into()],
        returned_files: None,
        return_first_seq: None,
        return_staging: Default::default(),
    });
    let handles = TaskIoHandles::create(&layout, BgMode::Pipes, true).unwrap();
    write_task_at(&layout, &metadata).unwrap();
    original.dual_write_task(&layout.paths, &metadata);
    original
        .insert_rehydrated_task(metadata, layout.paths.clone(), false)
        .unwrap();
    let task = original.task(&task_id).unwrap();
    task.state.lock().unwrap().io_handles = Some(handles);
    let mut sink = TaskSink::new(&original, task.clone()).unwrap();
    let mut consumer = exec::StreamConsumer::new();
    consumer
        .consume(StreamRecord::Accepted(Accepted::new(id(), 1)), &mut sink)
        .unwrap();
    consumer
        .consume(
            StreamRecord::Output(Output::new(
                0,
                OutputStream::Stdout,
                BytePayload(Vec::new()),
            )),
            &mut sink,
        )
        .unwrap();
    // Only the first of the file's three chunks arrives before the restart.
    let first_chunk =
        crate::exec_remote::return_vectors::return_vector_records("three-chunks-short-last")
            .into_iter()
            .find(|record| matches!(record, StreamRecord::ReturnChunk(_)))
            .unwrap();
    consumer.consume(first_chunk, &mut sink).unwrap();
    // Checkpoint the cursor past the chunk, so only the saved first-chunk
    // seq can bring the chunk back after the restart.
    sink.checkpoint_progress().unwrap();
    // Received bytes are staged in the task's private control folder.
    let staging = layout
        .dirs
        .control
        .path()
        .join(format!(".aft-return-{task_id}-0.part"));
    assert_eq!(fs::read(&staging).unwrap(), b"0123");
    let saved = read_task_at(&layout).unwrap();
    let remote = saved.remote.as_ref().unwrap();
    assert_eq!(remote.return_first_seq, Some(1));
    assert_eq!(
        remote.last_seq,
        Some(1),
        "the saved cursor is past the chunk"
    );
    assert_eq!(remote.return_staging.files, [staging.clone()]);
    assert!(remote.return_staging.dirs.is_empty());
    assert!(
        !root.join("dist").exists(),
        "nothing in the project before the manifest"
    );
    // A crash leaves the staging file behind: its receiver never cleans up.
    std::mem::forget(sink.returns.take());
    drop(sink);
    drop(consumer);
    original.inner.shutdown.store(true, Ordering::SeqCst);
    drop(task);
    drop(original);

    let restarted = registry();
    restarted.set_harness(crate::harness::Harness::Runner);
    restarted.set_db_pool(db);
    restarted.replay_session(&root, "session").unwrap();
    let done = terminal(&restarted, &task_id).await;
    let log = daemon.log.lock().unwrap();
    let attaches: Vec<_> = log
        .iter()
        .filter(|(_, body)| body["method"] == "exec.attach")
        .map(|(_, body)| body["params"]["from_seq"].clone())
        .collect();
    assert_eq!(
        attaches,
        [serde_json::json!(1)],
        "one attach, from the first chunk"
    );
    assert!(!log.iter().any(|(_, body)| body["method"] == "exec.run"));
    assert_eq!(fs::read(root.join("dist/app.bin")).unwrap(), b"0123456789");
    assert_eq!(
        done.output_preview.matches("dist/app.bin:").count(),
        1,
        "{}",
        done.output_preview
    );
    assert!(
        done.output_preview
            .contains("  dist/app.bin: returned (10 bytes)"),
        "{}",
        done.output_preview
    );
    assert!(!staging.exists(), "the stale staging file is gone");
    let dist: Vec<_> = fs::read_dir(root.join("dist"))
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name())
        .collect();
    assert_eq!(dist, ["app.bin"]);
    let saved = read_task_at(&layout).unwrap();
    assert!(saved.remote.unwrap().return_staging.is_empty());
}

#[test]
fn runon_windows_with_return_paths_is_refused_before_anything_runs() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    let marker = dir.path().join("local-spawn-proof");
    let request = orchestrated_request(serde_json::json!({
        "command": format!("touch '{}'", marker.display()),
        "workdir": dir.path(),
        "runon": "windows",
        "return_paths": ["dist/app.exe"],
    }));
    let response = crate::bash_background::with_remote_policy(
        Some(launch(dir.path().join("no-daemon"))),
        || crate::commands::bash::handle(&request, &ctx),
    );
    assert!(!response.success, "{response:?}");
    assert_eq!(
        response.data["code"], "return_paths_refused",
        "{response:?}"
    );
    assert_eq!(
        response.data["message"],
        "return_paths refused: return_paths is not supported on Windows runners; the command did not run"
    );
    assert!(!marker.exists(), "a refused call never runs");
}

#[tokio::test]
async fn runon_return_paths_unsupported_by_the_runner_is_final_and_never_runs_locally() {
    // ck-motor advertises chunked return, but the runner that would take the
    // job (a Windows runner or an older Linux one) cannot copy files back.
    let daemon = crate::exec_remote::wire_tests::daemon_with_capabilities(
        Script::ReturnPathsUnsupported,
        RETURN_CLAIMS,
        1,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    let marker = dir.path().join("local-spawn-proof");
    let mut call = return_call(&["dist/app.bin"]);
    call["command"] = serde_json::json!(format!("touch '{}'", marker.display()));
    let (reply, _) = orchestrated_reply(&ctx, &daemon, call).await;
    assert!(!reply.success, "{reply:?}");
    assert_eq!(reply.data["code"], "remote_unavailable", "{reply:?}");
    // The refusal names the job like every other runon outcome.
    assert_eq!(
        reply.data["message"],
        format!("{RETURN_PATHS_UNSUPPORTED_MESSAGE}; remote job {}", id())
    );
    assert_eq!(
        exec_runs(&daemon).len(),
        1,
        "the refusal is final, never retried"
    );
    assert!(!marker.exists(), "never a local run");
}

#[tokio::test]
async fn return_paths_unsupported_never_falls_back_even_for_prefix_routing() {
    let daemon = crate::exec_remote::wire_tests::daemon_with_capabilities(
        Script::ReturnPathsUnsupported,
        RETURN_CLAIMS,
        1,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    ensure_git_fixture(dir.path());
    let registry = registry();
    let mut policy = launch(daemon.connection.clone());
    policy.return_paths = vec!["out.txt".into()];
    let task = registry
        .spawn_remote(
            policy,
            SpawnPlan::Unsandboxed,
            "printf local > out.txt",
            resolve_posix_shell(),
            "session".into(),
            dir.path().into(),
            HashMap::new(),
            crate::bash_background::HardKill::After(Duration::from_secs(30)),
            dir.path().into(),
            crate::bash_background::TaskSlot::Background { max: 10 },
            true,
            false,
            Some(dir.path().into()),
        )
        .unwrap();
    let done = terminal(&registry, &task).await;
    assert_eq!(exec_runs(&daemon).len(), 1);
    assert!(!dir.path().join("out.txt").exists(), "never a local run");
    assert!(
        done.output_preview
            .contains(RETURN_PATHS_UNSUPPORTED_MESSAGE),
        "{}",
        done.output_preview
    );
}

#[tokio::test]
async fn runon_return_paths_restart_during_gap_recovery_rewinds_to_the_first_chunk() {
    // Chunks 1 and 2 of the file arrive, chunk 3 does not, then the manifest
    // and terminal do: the terminal proof is saved for gap recovery. After a
    // restart the staged bytes are gone, so recovery must attach from the
    // first chunk, not from the gap, while keeping the saved terminal proof.
    let daemon = return_daemon("three-chunks-short-last").await;
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    let original = registry();
    let db = Arc::new(Mutex::new(crate::db::open(&root.join("aft.db")).unwrap()));
    original.set_harness(crate::harness::Harness::Runner);
    original.set_db_pool(db.clone());
    let layout = allocate_task_layout(&root, "session").unwrap();
    let task_id = layout.paths.task_id.clone();
    let mut metadata = PersistedTask::starting(
        task_id.clone(),
        "session".into(),
        "make dist".into(),
        root.clone(),
        Some(root.clone()),
        Some(30000),
        true,
        false,
    );
    metadata.status = BgTaskStatus::Running;
    metadata.harness = Some("runner".into());
    metadata.execution_note = Some("ran remotely on ck-motor".into());
    let mut remote: RemoteTask = serde_json::from_value(serde_json::json!({
        "harness": "broca", "session": "session", "stdout_len": 0,
        "stderr_len": 0, "cancel_requested": false,
    }))
    .unwrap();
    remote.explicit_runon = true;
    remote.connection_file = Some(daemon.connection.clone());
    remote.return_paths = vec!["dist/app.bin".into()];
    metadata.remote = Some(remote);
    let handles = TaskIoHandles::create(&layout, BgMode::Pipes, true).unwrap();
    write_task_at(&layout, &metadata).unwrap();
    original.dual_write_task(&layout.paths, &metadata);
    original
        .insert_rehydrated_task(metadata, layout.paths.clone(), false)
        .unwrap();
    let task = original.task(&task_id).unwrap();
    task.state.lock().unwrap().io_handles = Some(handles);
    let mut sink = TaskSink::new(&original, task.clone()).unwrap();
    let mut consumer = exec::StreamConsumer::new();
    consumer
        .consume(StreamRecord::Accepted(Accepted::new(id(), 1)), &mut sink)
        .unwrap();
    consumer
        .consume(
            StreamRecord::Output(Output::new(
                0,
                OutputStream::Stdout,
                BytePayload(Vec::new()),
            )),
            &mut sink,
        )
        .unwrap();
    let mut gap = None;
    for record in
        crate::exec_remote::return_vectors::return_vector_records("three-chunks-short-last")
    {
        if matches!(&record, StreamRecord::ReturnChunk(chunk) if chunk.seq == 3) {
            continue;
        }
        if let Err(error) = consumer.consume(record, &mut sink) {
            gap = Some(error);
        }
    }
    assert!(
        matches!(gap, Some(exec::Error::RecoveryRequired { .. })),
        "{gap:?}"
    );
    let saved = read_task_at(&layout).unwrap();
    let remote = saved.remote.as_ref().unwrap();
    assert_eq!(remote.return_first_seq, Some(1));
    assert_eq!(remote.last_seq, Some(2), "the cursor stops at the gap");
    assert!(remote.gap_recovery.is_some(), "the terminal proof is saved");
    // A crash leaves the staged bytes behind; the restart removes them.
    std::mem::forget(sink.returns.take());
    drop(sink);
    drop(consumer);
    original.inner.shutdown.store(true, Ordering::SeqCst);
    drop(task);
    drop(original);

    let restarted = registry();
    restarted.set_harness(crate::harness::Harness::Runner);
    restarted.set_db_pool(db);
    restarted.replay_session(&root, "session").unwrap();
    let done = terminal(&restarted, &task_id).await;
    let attaches: Vec<_> = daemon
        .log
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, body)| body["method"] == "exec.attach")
        .map(|(_, body)| body["params"]["from_seq"].clone())
        .collect();
    assert_eq!(
        attaches.first(),
        Some(&serde_json::json!(1)),
        "recovery attaches from the first chunk, not the gap: {attaches:?}"
    );
    assert_eq!(done.exit_code, Some(0), "{}", done.output_preview);
    assert_eq!(fs::read(root.join("dist/app.bin")).unwrap(), b"0123456789");
    assert!(
        done.output_preview
            .contains("  dist/app.bin: returned (10 bytes)"),
        "{}",
        done.output_preview
    );
}
