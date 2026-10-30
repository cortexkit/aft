use super::*;
use crate::bash_background::persistence::{read_task, write_task};
use crate::exec_remote::wire_tests::{daemon, id, Script};

#[test]
fn remote_admission_progress_lines_are_exact_and_detail_is_plain_text() {
    use exec::types::{Progress, ProgressPhase};
    for (phase, elapsed, without, with) in [
        (ProgressPhase::Upload, 14_000,
         "The remote job is uploading its files (14s so far).",
         "The remote job is uploading its files (14s so far): <room> & **capacity**."),
        (ProgressPhase::PlacementWait, 130_000,
         "The remote job is waiting for a free build host (2m 10s so far).",
         "The remote job is waiting for a free build host (2m 10s so far): <room> & **capacity**."),
        (ProgressPhase::Connect, 3_000,
         "The remote job is connecting to its build host (3s so far).",
         "The remote job is connecting to its build host (3s so far): <room> & **capacity**."),
    ] {
        for detail in [None, Some(String::new()), Some(" \u{2003}  ".into())] {
            assert_eq!(admission_progress_line(&Progress::new(phase.clone(), elapsed, detail).unwrap()).as_deref(), Some(without));
        }
        assert_eq!(admission_progress_line(&Progress::new(phase, elapsed, Some("<room> & **capacity**".into())).unwrap()).as_deref(), Some(with));
    }
    assert_eq!(
        admission_progress_line(
            &Progress::new(
                ProgressPhase::Unknown("future_phase".into()),
                40_000,
                Some("unrecognized".into())
            )
            .unwrap()
        ),
        None
    );
}

#[test]
fn remote_admission_progress_detail_is_normalized_redacted_and_character_bounded() {
    use exec::types::{Progress, ProgressPhase};
    let token = format!("ghp_{}", "a".repeat(36));
    assert_eq!(
        admission_progress_line(
            &Progress::new(
                ProgressPhase::Upload,
                14_000,
                Some(format!(" \u{2003}{token}  "))
            )
            .unwrap()
        )
        .as_deref(),
        Some("The remote job is uploading its files (14s so far): <REDACTED_SECRET>.")
    );
    // Exercise the display boundary directly: the wire decoder independently
    // rejects multiline or oversized detail before constructing Progress.
    let detail = format!(
        "begin\n\r\0\u{001b} {token}\u{2028}\u{2029}end\t{}",
        "界".repeat(121)
    );
    let rendered = admission_progress_detail(&detail);
    assert_eq!(
        rendered,
        format!("begin <REDACTED_SECRET> end {}…", "界".repeat(91))
    );
    assert_eq!(rendered.chars().count(), 120);
    assert!(!rendered.contains("ghp_"));
    assert!(!rendered
        .chars()
        .any(|character| character.is_control() || matches!(character, '\u{2028}' | '\u{2029}')));
    assert!(!rendered.contains("  "));

    let boundary_token = admission_progress_detail(&format!("{} {token}", "x".repeat(110)));
    assert!(
        !boundary_token.contains("ghp_"),
        "credentials must be redacted before the character cut"
    );
    assert_eq!(boundary_token.chars().count(), 120);
    assert!(boundary_token.ends_with('…'));
    assert_eq!(
        admission_progress_detail(&"界".repeat(120)),
        "界".repeat(120)
    );
    assert_eq!(
        admission_progress_detail(&"界".repeat(121)),
        format!("{}…", "界".repeat(119))
    );
    assert_eq!(admission_progress_detail(" \r\n\t\0\u{2028}\u{2029}"), "");
}

#[tokio::test]
async fn remote_admission_progress_is_latest_only_ephemeral_and_changes_no_timers_or_cursor() {
    use exec::types::{Progress, ProgressPhase};
    let dir = tempfile::tempdir().unwrap();
    let (registry, task_id, paths) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused-connection"), "true");
    let task = registry.task(&task_id).unwrap();
    {
        let mut state = task.state.lock().unwrap();
        let remote = state.metadata.remote.as_mut().unwrap();
        remote.accepted = false;
        remote.explicit_runon = true;
        remote.last_seq = Some(7);
        remote.retry = Some(DrainingRetry {
            request_digest: "unchanged".into(),
            attempts: 2,
            first_refusal_at_ms: Some(1),
            next_retry_at_ms: Some(2),
            waited_ms: Some(3),
            resubmitted_at_ms: Some(4),
        });
        registry.persist_task(&paths, &state.metadata).unwrap();
    }
    let mut sink = TaskSink::new(&registry, task.clone()).unwrap();
    let metadata = task.state.lock().unwrap().metadata.clone();
    let saved = fs::read(&paths.json).unwrap();
    let started = task.started;
    let launch_ready = task.remote_launch_ready.load(Ordering::Acquire);
    let last_checkpoint = sink.last_progress_persist;
    let mut recovery = ReattachBudget::new(dir.path(), &metadata);
    recovery.deadline();
    recovery.attempts = 3;
    let deadlines = (
        recovery.job_end,
        recovery.deadline,
        recovery.grace,
        recovery.attempts,
    );
    let mut consumer = exec::StreamConsumer::keyed_run(Some(7));
    let unknown = StreamRecord::Progress(
        Progress::new(ProgressPhase::Unknown("future_phase".into()), 40_000, None).unwrap(),
    );
    let previous_phase_note = task.snapshot(0).remote_phase_note;
    consumer.consume(unknown.clone(), &mut sink).unwrap();
    assert!(task.state.lock().unwrap().remote_admission_note.is_none());
    assert_eq!(task.snapshot(0).remote_phase_note, previous_phase_note);
    for elapsed in 0..10_000 {
        let bytes = serde_json::to_vec(&StreamRecord::Progress(
            Progress::new(ProgressPhase::Upload, elapsed, Some("latest".into())).unwrap(),
        ))
        .unwrap();
        consumer.consume_bytes(&bytes, &mut sink).unwrap();
        recovery.observe(&StreamProgress::AdmissionProgress);
        assert!(consumer.resume_point().is_none());
        let state = task.state.lock().unwrap();
        assert!(state.remote_admission_note.as_ref().unwrap().len() < 100);
    }
    consumer.consume(unknown, &mut sink).unwrap();
    let latest = "The remote job is uploading its files (10s so far): latest.";
    assert_eq!(task.snapshot(0).remote_phase_note.as_deref(), Some(latest));
    assert_eq!(
        registry
            .runon_launch_state(&task_id, "session")
            .unwrap()
            .admission_note
            .as_deref(),
        Some(latest)
    );
    assert_eq!(
        serde_json::to_value(&task.state.lock().unwrap().metadata).unwrap(),
        serde_json::to_value(&metadata).unwrap()
    );
    assert_eq!(fs::read(&paths.json).unwrap(), saved);
    assert_eq!(fs::metadata(&paths.stdout).unwrap().len(), 0);
    assert_eq!(task.started, started);
    assert_eq!(sink.last_progress_persist, last_checkpoint);
    assert_eq!(sink.output_progress.last_seq, Some(7));
    assert_eq!(
        (
            recovery.job_end,
            recovery.deadline,
            recovery.grace,
            recovery.attempts
        ),
        deadlines
    );
    assert_eq!(
        task.remote_launch_ready.load(Ordering::Acquire),
        launch_ready
    );

    consumer
        .consume(StreamRecord::Accepted(Accepted::new(id(), 1)), &mut sink)
        .unwrap();
    let cursor = consumer.resume_point();
    let accepted = serde_json::to_value(&task.state.lock().unwrap().metadata).unwrap();
    consumer
        .consume(
            StreamRecord::Progress(Progress::new(ProgressPhase::Connect, 3_000, None).unwrap()),
            &mut sink,
        )
        .unwrap();
    assert_eq!(consumer.resume_point(), cursor);
    assert_eq!(
        serde_json::to_value(&task.state.lock().unwrap().metadata).unwrap(),
        accepted
    );
    assert!(task.state.lock().unwrap().remote_admission_note.is_none());
    assert!(registry
        .runon_launch_state(&task_id, "session")
        .unwrap()
        .admission_note
        .is_none());
    assert!(!task
        .snapshot(0)
        .remote_phase_note
        .unwrap()
        .contains("so far"));
    // A sink reused by an attach also refuses late display updates.
    sink.admission_progress(&Progress::new(ProgressPhase::Connect, 3_000, None).unwrap());
    assert!(task.state.lock().unwrap().remote_admission_note.is_none());
}

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

type RemoteLaunchLogCapture = Arc<Mutex<Vec<String>>>;

fn remote_launch_log_captures() -> &'static Mutex<HashMap<PathBuf, RemoteLaunchLogCapture>> {
    static CAPTURES: std::sync::OnceLock<Mutex<HashMap<PathBuf, RemoteLaunchLogCapture>>> =
        std::sync::OnceLock::new();
    CAPTURES.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) fn capture_remote_launch_logs<R>(task: &BgTask, run: impl FnOnce() -> R) -> R {
    let root = task.state.lock().unwrap().metadata.project_root.clone();
    let capture = root.and_then(|root| remote_launch_log_captures().lock().unwrap().remove(&root));
    if let Some(capture) = capture {
        crate::logging::capture_log_lines_into(capture, run)
    } else {
        run()
    }
}

fn exited_remote_workers() -> &'static Mutex<HashSet<String>> {
    static TASKS: std::sync::OnceLock<Mutex<HashSet<String>>> = std::sync::OnceLock::new();
    TASKS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Called by a remote task's worker thread as its last step.
pub(super) fn note_remote_worker_exit(task: &str) {
    exited_remote_workers()
        .lock()
        .unwrap()
        .insert(task.to_owned());
    remote_worker_counts()
        .lock()
        .unwrap()
        .entry(task.to_owned())
        .or_default()
        .1 += 1;
}

/// Per task id: remote worker threads started, and those that reached their
/// last step.
fn remote_worker_counts() -> &'static Mutex<HashMap<String, (usize, usize)>> {
    static COUNTS: std::sync::OnceLock<Mutex<HashMap<String, (usize, usize)>>> =
        std::sync::OnceLock::new();
    COUNTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Called as a remote worker thread for `task` is about to be spawned.
pub(super) fn note_remote_worker_start(task: &str) {
    remote_worker_counts()
        .lock()
        .unwrap()
        .entry(task.to_owned())
        .or_default()
        .0 += 1;
}

/// How many remote workers were started for `task`, and how many stopped.
fn remote_workers(task: &str) -> (usize, usize) {
    remote_worker_counts()
        .lock()
        .unwrap()
        .get(task)
        .copied()
        .unwrap_or_default()
}

/// Wait until the task's remote worker reaches its last step, so a test can
/// assert what the worker did not do afterwards without sleeping.
async fn remote_worker_exited(task: &str) {
    wait_for("the remote worker must stop", || {
        exited_remote_workers()
            .lock()
            .unwrap()
            .contains(task)
            .then_some(())
    })
    .await
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
    start_with_launch(registry, dir, launch(connection), hard_kill)
}

fn start_with_launch(
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
                "spawn entrypoint deferred-wait: command executed and frame-loop lock check completed"
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
                    None,
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
                    None,
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
                    None,
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
                        None,
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
            None,
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
            None,
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

#[tokio::test]
async fn memory_limit_delayed_fallback_keeps_submitted_cap_after_reload() {
    assert_delayed_fallback_cap(None).await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn memory_limit_attach_eperm_delayed_fallback_after_reload_enforces_watcher() {
    assert_delayed_fallback_cap(Some(libc::EPERM)).await;
}

async fn assert_delayed_fallback_cap(attach_error: Option<i32>) {
    #[cfg(target_os = "linux")]
    if attach_error.is_some()
        && !crate::bash_background::memory_limit::sampling_available_for_test()
    {
        super::super::memory_limit_tests::report_memory_runtime_skip(
            "memory_limit_attach_eperm_delayed_fallback_after_reload_enforces_watcher",
            "RSS sampling is unavailable for the forced fallback",
        );
        return;
    }
    let daemon = daemon(Script::GatedAcceptanceThenRefused, "exec-remote/v1").await;
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    ensure_git_fixture(project.path());
    let ctx = super::super::memory_limit_tests::memory_request_context(
        project.path(),
        storage.path(),
        Some(64),
    );
    let submitted = {
        let _pin = ctx.pin_config();
        super::super::memory_limit_tests::spawn_memory_command(
            &ctx,
            "memory-fallback-a",
            if attach_error.is_some() {
                super::super::memory_limit_tests::MEMORY_PRESSURE_COMMAND
            } else {
                "exit 0"
            },
            false,
            Some(launch(daemon.connection.clone())),
        )
    };
    assert!(submitted.success, "{submitted:?}");
    let task_id = submitted.data["task_id"].as_str().unwrap().to_owned();
    #[cfg(target_os = "linux")]
    let attach_fault = attach_error.map(|errno| {
        crate::bash_background::memory_limit::AttachFaultGuard::for_task(
            task_id.clone(),
            storage.path().join("denied-cgroup"),
            errno,
        )
    });
    let (saved_layout, saved_digest) = {
        let task = ctx.bash_background().task(&task_id).unwrap();
        let digest = task
            .state
            .lock()
            .unwrap()
            .metadata
            .remote
            .as_ref()
            .unwrap()
            .fallback_digest
            .clone()
            .unwrap();
        (
            resolve_task_layout(&task.paths.session_dir, &task_id).unwrap(),
            digest,
        )
    };

    let mut reloaded = ctx.config().as_ref().clone();
    reloaded.bash.memory_limit_mib = None;
    ctx.set_config(reloaded);
    let newer = {
        let _pin = ctx.pin_config();
        super::super::memory_limit_tests::spawn_memory_request(&ctx, "memory-fallback-b", None)
    };
    assert!(newer.success, "{newer:?}");
    daemon.acceptance_gate.notify_one();
    let done = terminal(ctx.bash_background(), &task_id).await;
    if attach_error.is_some() {
        assert_eq!(done.info.status, BgTaskStatus::Failed, "{done:?}");
        assert!(
            done.info
                .status_reason
                .as_deref()
                .unwrap_or_default()
                .contains("64 MiB memory cap"),
            "{done:?}"
        );
        assert!(
            done.output_preview
                .contains("Linux process-group RSS watcher; kernel memory controller unavailable"),
            "{done:?}"
        );
    } else {
        assert_eq!(done.exit_code, Some(0), "{done:?}");
    }
    #[cfg(target_os = "linux")]
    if let Some(fault) = &attach_fault {
        assert_eq!(fault.attempts(), 1);
        assert!(!storage.path().join("denied-cgroup").exists());
    }
    let task = ctx.bash_background().task(&task_id).unwrap();
    let state = task.state.lock().unwrap();
    assert!(state.metadata.local_fallback_started);
    assert_eq!(
        state
            .metadata
            .memory_policy
            .as_ref()
            .map(|policy| policy.mib),
        Some(64),
        "fallback lost the cap from its submitted request"
    );
    drop(state);
    let (_, _, _, _, saved_cap, _) =
        crate::sandbox_spawn::restore_local_launch(&saved_layout, &saved_digest).unwrap();
    assert_eq!(
        saved_cap,
        Some(64),
        "private fallback snapshot lost its admitted cap"
    );
}

#[tokio::test]
async fn memory_limit_replayed_fallback_keeps_saved_cap() {
    assert_replayed_fallback_cap(None).await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn memory_limit_attach_eperm_replayed_fallback_enforces_watcher() {
    assert_replayed_fallback_cap(Some(libc::EPERM)).await;
}

async fn assert_replayed_fallback_cap(attach_error: Option<i32>) {
    #[cfg(target_os = "linux")]
    if attach_error.is_some()
        && !crate::bash_background::memory_limit::sampling_available_for_test()
    {
        super::super::memory_limit_tests::report_memory_runtime_skip(
            "memory_limit_attach_eperm_replayed_fallback_enforces_watcher",
            "RSS sampling is unavailable for the forced fallback",
        );
        return;
    }
    let daemon = daemon(Script::AttachRefused, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let (original, task_id, paths) = persist_accepted_with_snapshot_cap(
        dir.path(),
        daemon.connection.clone(),
        if attach_error.is_some() {
            super::super::memory_limit_tests::MEMORY_PRESSURE_COMMAND
        } else {
            "exit 0"
        },
        Some(64),
    );
    drop(original);
    #[cfg(target_os = "linux")]
    let attach_fault = attach_error.map(|errno| {
        crate::bash_background::memory_limit::AttachFaultGuard::for_task(
            task_id.clone(),
            dir.path().join("denied-cgroup"),
            errno,
        )
    });
    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    let done = terminal(&restarted, &task_id).await;
    if attach_error.is_some() {
        assert_eq!(done.info.status, BgTaskStatus::Failed, "{done:?}");
        assert!(
            done.info
                .status_reason
                .as_deref()
                .unwrap_or_default()
                .contains("64 MiB memory cap"),
            "{done:?}"
        );
        assert!(
            done.output_preview
                .contains("Linux process-group RSS watcher; kernel memory controller unavailable"),
            "{done:?}"
        );
    } else {
        assert_eq!(done.exit_code, Some(0), "{done:?}");
    }
    #[cfg(target_os = "linux")]
    if let Some(fault) = &attach_fault {
        assert_eq!(fault.attempts(), 1);
        assert!(!dir.path().join("denied-cgroup").exists());
    }
    let metadata = read_task(&paths.json).unwrap();
    assert!(metadata.local_fallback_started);
    assert_eq!(
        metadata.memory_policy.as_ref().map(|policy| policy.mib),
        Some(64)
    );
}

#[tokio::test]
async fn bash_changed_files_new_runon_completion_keeps_local_window() {
    assert_new_remote_changed_files(false).await;
}

#[tokio::test]
async fn bash_changed_files_runon_local_fallback_completion_keeps_local_window() {
    assert_new_remote_changed_files(true).await;
}

async fn assert_new_remote_changed_files(fallback: bool) {
    let daemon = daemon(
        if fallback {
            Script::KnownRefused
        } else {
            Script::GatedStages { position: 1 }
        },
        "exec-remote/v1",
    )
    .await;
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    ensure_git_fixture(project.path());
    let ctx = crate::context::AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(project.path().into()),
            storage_dir: Some(storage.path().into()),
            bash: crate::config::BashConfig {
                changed_files: crate::config::BashChangedFiles::List,
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let capture = crate::bash_changed_files::Capture::start(&ctx).unwrap();
    let mut launch = launch(daemon.connection.clone());
    launch.explicit_runon = !fallback;
    let task_id = {
        let _capture = crate::bash_changed_files::scope(Some(capture.clone()));
        ctx.bash_background().spawn_remote(launch, SpawnPlan::Unsandboxed, "printf local-change > local.txt; while [ ! -e \"$CHANGED_FILES_RELEASE\" ]; do sleep 0.01; done", resolve_posix_shell(), "session".into(), project.path().into(), HashMap::from([(
            "CHANGED_FILES_RELEASE".into(),
            storage.path().join("release").display().to_string(),
        )]), crate::bash_background::HardKill::from_timeout_ms(Some(20_000)), None, storage.path().into(), crate::bash_background::TaskSlot::Background { max: 10 }, true, false, Some(project.path().into()))
            .unwrap()
    };
    if fallback {
        wait_for("local fallback must write the project file", || {
            project.path().join("local.txt").exists().then_some(())
        })
        .await;
    } else {
        std::fs::write(project.path().join("local.txt"), "local observer change\n").unwrap();
    }
    // Inject a file-create event into the project's changed-files tracker while
    // the remote stream or fallback shell is held open, independently of OS delivery timing.
    ctx.watcher_counters().bash_changes.get().unwrap().event(
        &notify::Event::new(notify::EventKind::Create(notify::event::CreateKind::File))
            .add_path(project.path().canonicalize().unwrap().join("local.txt")),
    );
    std::fs::write(storage.path().join("release"), "").unwrap();
    daemon.output_gate.notify_one();
    daemon.completion_gate.notify_one();
    let done = terminal(ctx.bash_background(), &task_id).await;
    assert_eq!(done.exit_code, Some(0), "{done:?}");
    let completion = wait_for("completion must retain its local change report", || {
        ctx.bash_background()
            .drain_completions_for_session(Some("session"))
            .into_iter()
            .find(|completion| completion.task_id == task_id)
    })
    .await;
    let report = completion
        .changed_files
        .as_ref()
        .expect("capture lost at remote registration");
    assert!(
        report.files.iter().any(|file| file.path == "local.txt"),
        "{report:?}"
    );
    assert!(serde_json::to_value(&completion).unwrap()["_bash_changed_files"].is_object());

    let task = ctx.bash_background().task(&task_id).unwrap();
    let metadata = task.state.lock().unwrap().metadata.clone();
    let replay = registry();
    let _capture = crate::bash_changed_files::scope(Some(capture));
    replay
        .insert_rehydrated_task(metadata, task.paths.clone(), true)
        .unwrap();
    assert!(replay.task(&task_id).unwrap().changed_files.is_none());
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

#[tokio::test]
async fn runon_pre_acceptance_refusal_ends_standalone_foreground_wait_promptly() {
    for (reason, detail) in [
        ("runner_full", Some("hosted_host_full")),
        ("network_unsupported", None),
        ("queue_wait_exceeded", None),
    ] {
        let daemon = daemon(
            Script::GatedPreStartRefusal { reason, detail },
            "exec-remote/v1",
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = Arc::new(remote_test_context(dir.path()));
        let request = orchestrated_request(serde_json::json!({
            "command": "printf must-not-run",
            "workdir": dir.path(),
            "runon": "linux",
            "wait": true,
            "foreground_orchestrate": true,
            "compressed": false,
        }));
        let spawned = crate::bash_background::with_remote_policy(
            Some(launch(daemon.connection.clone())),
            || crate::commands::bash::handle(&request, &ctx),
        );
        let task_id = spawned.data["task_id"].as_str().unwrap().to_owned();
        let crate::response_finalize::DispatchOutcome::Deferred(mut pending) =
            crate::commands::bash_orchestrate::build_bash_outcome(&request, &ctx, spawned)
        else {
            panic!("foreground must wait while the executor holds its refusal");
        };
        let crate::response_finalize::DispatchOutcome::Deferred(mut watch) =
            crate::commands::bash_watch::handle_deferred(
                &crate::protocol::RawRequest {
                    id: "refusal-watch".into(),
                    command: "bash_watch".into(),
                    session_id: Some("session".into()),
                    lsp_hints: None,
                    params: serde_json::json!({"task_id": task_id, "worker_session": true}),
                },
                ctx.clone(),
            )
        else {
            panic!("watch must wait while the executor holds its refusal");
        };
        daemon.wait_for_requests("exec.run", 1).await;
        daemon.acceptance_gate.notify_one();
        let (response, watch_response) = tokio::time::timeout(Duration::from_secs(1), async {
            let mut foreground_reply = None;
            let mut watch_reply = None;
            loop {
                foreground_reply = foreground_reply.or_else(|| (pending.poll)(&ctx));
                watch_reply = watch_reply.or_else(|| (watch.poll)(&ctx));
                if foreground_reply.is_some() && watch_reply.is_some() {
                    return (foreground_reply.unwrap(), watch_reply.unwrap());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("a pre-start refusal must not consume the thirty-minute wait budget");
        assert_runon_refusal(&response, &rendered_refusal_reason(reason, detail));
        assert_eq!(
            ctx.bash_background()
                .runon_inputs_captured(&task_id, "session"),
            Some(false)
        );
        let status = crate::commands::bash_status::handle(
            &crate::protocol::RawRequest {
                id: "refusal-status".into(),
                command: "bash_status".into(),
                session_id: Some("session".into()),
                lsp_hints: None,
                params: serde_json::json!({"task_id": task_id}),
            },
            &ctx,
        );
        assert_eq!(response.data["code"], status.data["code"]);
        assert_eq!(response.data["message"], status.data["message"]);
        assert_eq!(watch_response.data["waited"]["reason"], "exited");
        assert_eq!(
            watch_response.data["remote_refusal"]["code"],
            status.data["code"]
        );
        assert_eq!(
            watch_response.data["remote_refusal"]["message"],
            status.data["message"]
        );
    }
}

#[tokio::test]
async fn runon_pre_submission_failures_end_standalone_foreground_wait_promptly() {
    for unavailable_connection in [false, true] {
        let daemon = daemon(Script::Plain, "unrelated/v1").await;
        if unavailable_connection {
            std::fs::remove_file(&daemon.connection).unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        let ctx = remote_test_context(dir.path());
        let (response, _) = tokio::time::timeout(
            Duration::from_secs(1),
            orchestrated_reply_with_probe(
                &ctx,
                &daemon,
                serde_json::json!({
                    "command": "printf must-not-run", "runon": "linux", "wait": true,
                    "foreground_orchestrate": true, "compressed": false,
                }),
                |_| true,
            ),
        )
        .await
        .expect("a capability or connection refusal must end the foreground wait");
        assert!(!response.success, "{response:?}");
        assert_eq!(response.data["code"], "remote_unavailable", "{response:?}");
        assert!(
            response.data["message"]
                .as_str()
                .unwrap()
                .contains("command did not run"),
            "{response:?}"
        );
        assert!(exec_runs(&daemon).is_empty());
    }
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
    let expected = format!("remote outcome unknown (job {}); the remote job could not affect this machine or the network, so rerunning is safe; job {} may still be running on ck-motor and holding this workspace for up to its run timeout of 30s, and AFT can no longer fetch its result: a rerun in this workspace waits behind it and can be refused with queue_wait_exceeded, so wait for it to end or cancel it with bash_kill, then rerun", id(), id());
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
        submission: None,
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
        return_paths: Vec::new(),
        returned_files: None,
        return_first_seq: None,
        return_staging: Default::default(),
        expired_gap_unverified: Vec::new(),
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

    let recovered = self::registry();
    // Startup can publish a detach target before replay inserts its task.
    // The reserved handle must receive the canonical recovered reply too.
    let reserved = recovered.drain_reply_cache(&task_id, "session");
    let metadata = task.state.lock().unwrap().metadata.clone();
    recovered
        .insert_rehydrated_task(metadata, task.paths.clone(), true)
        .unwrap();
    let expected = with_runon_capture_state(&recovered, &task_id, "session", live_reply());
    let recovered_task = recovered.task(&task_id).unwrap();
    let _state = recovered_task.state.lock().unwrap();
    let _no_task_locks = crate::bash_background::registry::forbid_task_state_locks_for_test();
    let cached = reserved
        .snapshot()
        .uncaptured_terminal
        .expect("replayed terminal drain reply");
    assert_eq!(cached.data, expected.data);
    assert_eq!(cached.success, expected.success);
}

pub(crate) struct UncapturedTerminalBeforeRenderForTest {
    pub(crate) dir: tempfile::TempDir,
    pub(crate) registry: BgTaskRegistry,
    pub(crate) task: Arc<BgTask>,
    release: crossbeam_channel::Sender<()>,
    publisher: Option<std::thread::JoinHandle<()>>,
}

pub(crate) fn captured_task_after_losing_replay_for_test(
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

pub(crate) fn uncaptured_terminal_before_render_for_test(
    refused: bool,
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
            let verdict = if refused {
                Verdict::RunLocally {
                    reason: RefusalReason::Unreachable,
                    refusal_detail: None,
                }
            } else {
                Verdict::Signalled { signal: 9 }
            };
            registry.remote_terminal(
                &task,
                verdict,
                refused.then(|| {
                    "runon refused: remote refused: unreachable; command was not run".into()
                }),
            );
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
        Some(format!("remote outcome unknown (job {}); the remote job could not affect this machine or the network, so rerunning is safe; job {} may still be running on ck-motor and holding this workspace for up to its run timeout of 30s, and AFT can no longer fetch its result: a rerun in this workspace waits behind it and can be refused with queue_wait_exceeded, so wait for it to end or cancel it with bash_kill, then rerun", id(), id()).as_str())
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
            "job {} could not be re-attached before its own time limit passed (a queue wait allowance of 5m and its run timeout of 30s have passed since AFT submitted it, plus 5m of grace); command not rerun",
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
        reason.contains("wait for it to end or cancel it with bash_kill, then rerun"),
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

#[tokio::test]
async fn large_queued_runon_recovers_after_the_small_queue_deadline() {
    let daemon = crate::exec_remote::wire_tests::queued_outage_daemon().await;
    let dir = tempfile::tempdir().unwrap();
    let clock = PausedReattachClock::new(dir.path());
    let registry = registry();
    let mut remote = launch(daemon.connection.clone());
    remote.explicit_runon = true;
    remote.requested_vcpus = Some(16);
    let task_id = start_with_launch(
        &registry,
        dir.path(),
        remote,
        crate::bash_background::HardKill::After(DEFAULT_BG_TIMEOUT),
    );
    daemon.after_refused_connects(3).await;
    assert_eq!(
        registry.remote_progress(&task_id, "session").unwrap().phase,
        RemotePhase::Queued { position: 1 }
    );
    let run = &daemon.methods("exec.run")[0];
    assert_eq!(run["params"]["weight_hint"], 16);
    assert_eq!(run["params"]["timeout"], 1800);
    assert!(
        run["params"].get("queue_wait_limit_s").is_none(),
        "the runner owns admission policy"
    );

    // Pass the old 5m queue + 30m run + 5m grace deadline without real waiting.
    clock.advance(Duration::from_secs(41 * 60));
    let refused = *daemon.refused_connects.borrow();
    let still_trying = wait_for("the queued job must still be recoverable", || {
        let snapshot = registry.observed_status(&task_id, "session", 8192).unwrap();
        if snapshot.info.status.is_terminal() {
            return Some(Err(snapshot));
        }
        (*daemon.refused_connects.borrow() >= refused + 3).then_some(Ok(()))
    })
    .await;
    assert!(
        still_trying.is_ok(),
        "queued job abandoned: {still_trying:?}"
    );

    let metadata = registry
        .task(&task_id)
        .unwrap()
        .state
        .lock()
        .unwrap()
        .metadata
        .clone();
    let submission = KeyedSubmission {
        key: "queued-submission".into(),
        created_at_ms: metadata.started_at,
        request_digest: "digest".into(),
        attempts: 1,
        fresh_key_used: false,
        resuming: false,
        reattached: false,
        refusal: None,
    };
    assert_eq!(
        submission.expires_at(&metadata),
        metadata.started_at + 60 * 60 * 1000,
        "a lost Accepted must remain recoverable for the same queue and runtime allowance"
    );
    daemon.restore();
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");
    assert_eq!(
        fs::read(&registry.task(&task_id).unwrap().paths.stdout).unwrap(),
        b"AB"
    );
    assert_eq!(
        daemon.methods("exec.run").len(),
        1,
        "recovery must not submit another job"
    );
    assert!(!daemon.methods("exec.attach").is_empty());
}

#[tokio::test]
async fn large_queued_runon_recovery_ends_after_queue_runtime_and_grace() {
    let daemon = crate::exec_remote::wire_tests::queued_outage_daemon().await;
    let dir = tempfile::tempdir().unwrap();
    let clock = PausedReattachClock::new(dir.path());
    let registry = registry();
    let mut remote = launch(daemon.connection.clone());
    remote.explicit_runon = true;
    remote.requested_vcpus = Some(16);
    let task_id = start_with_launch(
        &registry,
        dir.path(),
        remote,
        crate::bash_background::HardKill::After(DEFAULT_BG_TIMEOUT),
    );
    daemon.after_refused_connects(3).await;
    clock.advance(Duration::from_secs(66 * 60));
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::FateUnknown, "{done:?}");
    assert!(done.output_preview.contains(&format!(
        "job {} could not be re-attached before its own time limit passed (a queue wait allowance of 30m and its run timeout of 30m have passed since AFT submitted it, plus 5m of grace); command not rerun", id()
    )), "{}", done.output_preview);
    assert_eq!(daemon.methods("exec.run").len(), 1);
}

#[tokio::test]
async fn short_queued_runon_keeps_the_five_minute_queue_allowance() {
    let daemon = crate::exec_remote::wire_tests::queued_outage_daemon().await;
    let dir = tempfile::tempdir().unwrap();
    let clock = PausedReattachClock::new(dir.path());
    let registry = registry();
    let mut remote = launch(daemon.connection.clone());
    remote.explicit_runon = true;
    remote.requested_vcpus = Some(8);
    let task_id = start_with_launch(
        &registry,
        dir.path(),
        remote,
        crate::bash_background::HardKill::After(Duration::from_secs(30)),
    );
    daemon.after_refused_connects(3).await;
    let metadata = registry
        .task(&task_id)
        .unwrap()
        .state
        .lock()
        .unwrap()
        .metadata
        .clone();
    let (remaining, _, _) = ReattachBudget::until_job_end(&metadata);
    assert!(
        (Duration::from_secs(325)..=Duration::from_secs(330)).contains(&remaining),
        "{remaining:?}"
    );
    clock.advance(Duration::from_secs(11 * 60));
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::FateUnknown, "{done:?}");
    assert!(done.output_preview.contains(&format!(
        "job {} could not be re-attached before its own time limit passed (a queue wait allowance of 5m and its run timeout of 30s have passed since AFT submitted it, plus 5m of grace); command not rerun", id()
    )), "{}", done.output_preview);
    assert_eq!(daemon.methods("exec.run").len(), 1);
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
    // The runner never fills the Git, untracked-file, or ignored-write
    // reports, so their absence stays silent.
    assert!(
        done.output_preview.ends_with(
            "These files changed on the server and were NOT copied back:\n  generated.txt"
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
                None,
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
            None,
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

fn cancel_terminal(outcome: Outcome, killed: Option<Killed>, ran: Ran) -> TerminalRecord {
    let mut terminal = TerminalRecord::new(id(), outcome, 1, 0, 0);
    terminal.killed = killed;
    terminal.ran = Some(ran);
    terminal
}

fn cancel_unknown_terminal() -> TerminalRecord {
    let mut terminal = crate::exec_remote::wire_tests::cancel_exit_status_unknown_terminal();
    terminal.job_id = id();
    terminal
}

async fn assert_cancel_terminal(
    record: fn() -> TerminalRecord,
    stderr: &'static str,
    expected: BgTaskStatus,
) {
    let daemon = daemon(
        Script::CancelTerminal {
            terminal: record,
            stderr,
        },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let task = start(&registry, dir.path(), daemon.connection.clone());
    wait_for("runner acceptance before cancel", || {
        registry
            .status(&task, "session", None, None, 0)
            .unwrap()
            .remote_job_id
    })
    .await;
    let killing = registry.clone();
    let kill_task = task.clone();
    tokio::task::spawn_blocking(move || killing.kill(&kill_task, "session").unwrap())
        .await
        .unwrap();
    let done = terminal(&registry, &task).await;
    assert_eq!(done.info.status, expected, "{done:?}");
    assert_eq!(done.exit_code, None);
    if expected == BgTaskStatus::Killed {
        assert_eq!(done.info.status_reason.as_deref(), Some("remote cancel"));
        assert!(!done.output_preview.contains("may still be running"));
    } else {
        assert!(done
            .info
            .status_reason
            .as_deref()
            .unwrap()
            .contains("may still be running"));
        assert!(done.output_preview.contains("may still be running"));
    }
    if !stderr.is_empty() {
        assert!(done.output_preview.contains(stderr), "{done:?}");
    }
    let saved = read_task(&registry.task(&task).unwrap().paths.json).unwrap();
    assert_eq!(saved.remote.unwrap().terminal.as_ref(), Some(&record()));
}

#[tokio::test]
async fn exec_remote_cancel_before_start_is_killed_without_unknown_warning() {
    assert_cancel_terminal(
        || cancel_terminal(Outcome::Cancelled, None, Ran::None),
        "",
        BgTaskStatus::Killed,
    )
    .await;
}

#[tokio::test]
async fn exec_remote_cancelled_exit_is_killed_without_unknown_warning() {
    for code in [143, 137] {
        assert_cancel_terminal(
            if code == 143 {
                || {
                    cancel_terminal(
                        Outcome::Exit { code: 143 },
                        Some(Killed::Cancel),
                        Ran::Remote,
                    )
                }
            } else {
                || {
                    cancel_terminal(
                        Outcome::Exit { code: 137 },
                        Some(Killed::Cancel),
                        Ran::Remote,
                    )
                }
            },
            "",
            BgTaskStatus::Killed,
        )
        .await;
    }
}

#[tokio::test]
async fn exec_remote_cancelled_signal_is_killed_without_unknown_warning() {
    assert_cancel_terminal(
        || {
            cancel_terminal(
                Outcome::Signal { signal: 15 },
                Some(Killed::Cancel),
                Ran::Remote,
            )
        },
        "",
        BgTaskStatus::Killed,
    )
    .await;
    assert_cancel_terminal(
        || {
            cancel_terminal(
                Outcome::Signal { signal: 9 },
                Some(Killed::Cancel),
                Ran::Remote,
            )
        },
        "",
        BgTaskStatus::Killed,
    )
    .await;
}

#[tokio::test]
async fn exec_remote_cancel_unknown_exit_keeps_warning_despite_stderr() {
    for stderr in [
        "runner monitoring: outcome_unknown error=cancel recorded after unit exited",
        "runner monitoring: outcome_unknown error=cancelled unit already exited",
        "runner monitoring: outcome_unknown error=restart_execution_not_reattachable",
        "",
    ] {
        assert_cancel_terminal(cancel_unknown_terminal, stderr, BgTaskStatus::FateUnknown).await;
    }
}

#[tokio::test]
async fn exec_remote_cancel_execution_ended_true_is_killed_without_unknown_warning() {
    assert_cancel_terminal(
        || cancel_unknown_terminal().with_execution_ended(),
        "",
        BgTaskStatus::Killed,
    )
    .await;
}

#[tokio::test]
async fn exec_remote_cancel_execution_ended_false_keeps_unknown_warning() {
    assert_cancel_terminal(
        || {
            let mut value = serde_json::to_value(cancel_unknown_terminal()).unwrap();
            value["execution_ended"] = false.into();
            serde_json::from_value(value).unwrap()
        },
        "",
        BgTaskStatus::FateUnknown,
    )
    .await;
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
            None,
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
        submission: None,
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
        return_paths: Vec::new(),
        returned_files: None,
        return_first_seq: None,
        return_staging: Default::default(),
        expired_gap_unverified: Vec::new(),
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
    persist_accepted_with_snapshot_cap(dir, connection, command, None)
}

fn persist_accepted_with_snapshot_cap(
    dir: &Path,
    connection: PathBuf,
    command: &str,
    memory_limit_mib: Option<u64>,
) -> (BgTaskRegistry, String, TaskPaths) {
    persist_accepted_with_snapshot_scope(dir, connection, command, memory_limit_mib, false, 86400)
}

fn persist_accepted_with_snapshot_scope(
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
        output_received: false,
        retry: None,
        output_incomplete: None,
        workspace_holder: None,
        return_paths: Vec::new(),
        returned_files: None,
        return_first_seq: None,
        return_staging: Default::default(),
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

fn restarted_context(dir: &Path) -> crate::context::AppContext {
    restarted_context_with_runon(dir, true)
}

fn finished_remote_for_cancel(
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

fn cancel_requests(daemon: &crate::exec_remote::wire_tests::Daemon) -> Vec<serde_json::Value> {
    daemon
        .log
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, body)| body["method"] == "exec.cancel")
        .map(|(_, body)| body.clone())
        .collect()
}

fn delivered_unknown_restart_fixture(
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

async fn kill_restarted_unknown(
    ctx: crate::context::AppContext,
    task: String,
    session: &str,
    scope: Option<subc_protocol::scope::ScopeStamp>,
) -> crate::protocol::Response {
    let request = crate::protocol::RawRequest {
        id: "restart-unknown-kill".into(),
        command: "bash_kill".into(),
        lsp_hints: None,
        session_id: Some(session.into()),
        params: serde_json::json!({"task_id": task}),
    };
    tokio::task::spawn_blocking(move || {
        super::super::with_scope_spawn(scope, || crate::commands::bash_kill::handle(&request, &ctx))
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn restart_delivered_unknown_kill_restores_owner_on_newer_epoch() {
    let _storage_env = crate::test_env::without_storage_override();
    let daemon = daemon(Script::Cancel, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let (task, paths, ctx) =
        delivered_unknown_restart_fixture(dir.path(), daemon.connection.clone(), false);
    let before = read_task(&paths.json).unwrap();
    let mut scope = before.scope.clone().unwrap();
    scope.scope_epoch += 1;
    let reply = kill_restarted_unknown(ctx, task, "session", Some(scope)).await;
    assert!(
        reply.success,
        "owning session must recover its scoped delivered task: {reply:?}"
    );
    assert_eq!(reply.data["status"], "fate_unknown");
    assert_eq!(reply.data["remote_cancel"], "runner confirmed cancellation");
    let requests = cancel_requests(&daemon);
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["params"]["job_id"], id().to_string());
    let after = read_task(&paths.json).unwrap();
    assert_eq!(after.status, before.status);
    assert_eq!(after.scope, before.scope);
    assert_eq!(
        after.remote.unwrap().terminal,
        before.remote.unwrap().terminal
    );
    assert!(after.completion_delivered);
}

#[tokio::test]
async fn restart_delivered_unknown_kill_refuses_other_scope_ref() {
    let _storage_env = crate::test_env::without_storage_override();
    let daemon = daemon(Script::Cancel, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let (task, _, ctx) =
        delivered_unknown_restart_fixture(dir.path(), daemon.connection.clone(), false);
    let mut scope = owner_scope("other-scope");
    scope.scope_epoch = 10;
    let reply = kill_restarted_unknown(ctx, task, "session", Some(scope)).await;
    assert!(
        !reply.success,
        "another scope ref must not restore the task: {reply:?}"
    );
    assert_eq!(reply.data["code"], "task_not_found");
    assert!(cancel_requests(&daemon).is_empty());
}

#[tokio::test]
async fn restart_delivered_unknown_kill_refuses_lower_epoch() {
    let _storage_env = crate::test_env::without_storage_override();
    let daemon = daemon(Script::Cancel, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let (task, _, ctx) =
        delivered_unknown_restart_fixture(dir.path(), daemon.connection.clone(), false);
    let mut scope = owner_scope("unknown-outcome");
    scope.scope_epoch = 8;
    let reply = kill_restarted_unknown(ctx, task, "session", Some(scope)).await;
    assert!(
        !reply.success,
        "an older scope epoch must not restore the task: {reply:?}"
    );
    assert_eq!(reply.data["code"], "task_not_found");
    assert!(cancel_requests(&daemon).is_empty());
}

#[tokio::test]
async fn restart_delivered_unknown_kill_refuses_foreign_callers() {
    let _storage_env = crate::test_env::without_storage_override();
    let mut other_owner = owner_scope("unknown-outcome");
    other_owner.scope_epoch = 10;
    other_owner.owner = subc_protocol::Principal::Reserved {
        module_id: "other-owner".into(),
    };
    let mut owner = owner_scope("unknown-outcome");
    owner.scope_epoch = 10;
    for (session, scope) in [
        ("other-session", Some(owner)),
        ("session", Some(other_owner)),
        ("session", None),
    ] {
        let daemon = daemon(Script::Cancel, "exec-remote/v1").await;
        let dir = tempfile::tempdir().unwrap();
        let (task, _, ctx) =
            delivered_unknown_restart_fixture(dir.path(), daemon.connection.clone(), false);
        let reply = kill_restarted_unknown(ctx, task, session, scope).await;
        assert!(
            !reply.success,
            "foreign caller must not restore the task: {reply:?}"
        );
        assert_eq!(reply.data["code"], "task_not_found");
        assert!(cancel_requests(&daemon).is_empty());
    }
}

#[tokio::test]
async fn restart_delivered_unknown_kill_refuses_other_project_or_harness() {
    let _storage_env = crate::test_env::without_storage_override();
    for other_harness in [false, true] {
        let daemon = daemon(Script::Cancel, "exec-remote/v1").await;
        let dir = tempfile::tempdir().unwrap();
        let (task, paths, ctx) =
            delivered_unknown_restart_fixture(dir.path(), daemon.connection.clone(), false);
        if other_harness {
            ctx.update_config(|config| config.harness = Some(crate::harness::Harness::Pi));
            ctx.bash_background()
                .set_harness(crate::harness::Harness::Pi);
        } else {
            let other_project = dir.path().join("other-project");
            fs::create_dir_all(&other_project).unwrap();
            ctx.update_config(|config| config.project_root = Some(other_project));
        }
        let scope = read_task(&paths.json).unwrap().scope;
        let reply = kill_restarted_unknown(ctx, task, "session", scope).await;
        assert!(
            !reply.success,
            "foreign namespace must not restore the task: {reply:?}"
        );
        assert_eq!(reply.data["code"], "task_not_found");
        assert!(cancel_requests(&daemon).is_empty());
    }
}

#[tokio::test]
async fn restart_delivered_unknown_kill_keeps_unscoped_session_recovery() {
    let _storage_env = crate::test_env::without_storage_override();
    let daemon = daemon(Script::Cancel, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let (task, paths, ctx) =
        delivered_unknown_restart_fixture(dir.path(), daemon.connection.clone(), false);
    let mut metadata = read_task(&paths.json).unwrap();
    metadata.scope = None;
    write_task(&paths.json, &metadata).unwrap();
    let reply = kill_restarted_unknown(ctx, task, "session", None).await;
    assert!(
        reply.success,
        "an unscoped saved task keeps session ownership: {reply:?}"
    );
    assert_eq!(reply.data["remote_cancel"], "runner confirmed cancellation");
    assert_eq!(cancel_requests(&daemon).len(), 1);
}

#[tokio::test]
async fn restart_delivered_unknown_kill_past_timeout_keeps_finished_answer() {
    let _storage_env = crate::test_env::without_storage_override();
    let daemon = daemon(Script::Cancel, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let (task, paths, ctx) =
        delivered_unknown_restart_fixture(dir.path(), daemon.connection.clone(), true);
    let scope = read_task(&paths.json).unwrap().scope;
    let reply = kill_restarted_unknown(ctx, task, "session", scope).await;
    assert!(
        reply.success,
        "an expired owned task still has its finished answer: {reply:?}"
    );
    assert_eq!(reply.data["status"], "fate_unknown");
    assert!(reply.data.get("remote_cancel").is_none());
    assert!(cancel_requests(&daemon).is_empty());
}

#[tokio::test]
async fn fate_unknown_bash_kill_cancels_recorded_job_and_reports_runner_answers() {
    for (script, expected) in [
        (Script::Cancel, "runner confirmed cancellation"),
        (
            Script::CancelTerminal {
                terminal: || {
                    let mut terminal = cancel_unknown_terminal().with_execution_ended();
                    terminal.killed = None;
                    terminal
                },
                stderr: "",
            },
            "runner reports job already ended",
        ),
        (Script::CancelRefused, "outcome is still unknown"),
        (
            Script::CancelTerminal {
                terminal: cancel_unknown_terminal,
                stderr: "runner monitoring: cancelled; unit already exited",
            },
            "outcome is still unknown",
        ),
    ] {
        let daemon = daemon(script, "exec-remote/v1").await;
        let dir = tempfile::tempdir().unwrap();
        let (registry, task, paths) = finished_remote_for_cancel(
            dir.path(),
            daemon.connection.clone(),
            cancel_unknown_terminal(),
        );
        let before = fs::read(&paths.json).unwrap();
        let killing = registry.clone();
        let reply = tokio::task::spawn_blocking(move || killing.kill(&task, "session").unwrap())
            .await
            .unwrap();
        let requests = cancel_requests(&daemon);
        assert_eq!(
            requests.len(),
            1,
            "fate_unknown must send exactly one exec.cancel"
        );
        assert_eq!(requests[0]["params"]["job_id"], id().to_string());
        let data = serde_json::to_value(&reply).unwrap();
        assert!(
            data["remote_cancel"].as_str().unwrap().contains(expected),
            "{data}"
        );
        assert_eq!(reply.info.status, BgTaskStatus::FateUnknown);
        assert_eq!(
            fs::read(&paths.json).unwrap(),
            before,
            "cancel must not invent terminal proof"
        );
    }
}

#[tokio::test]
async fn known_outcome_bash_kill_sends_no_remote_cancel() {
    let daemon = daemon(Script::Plain, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let (registry, task, _) = finished_remote_for_cancel(
        dir.path(),
        daemon.connection.clone(),
        TerminalRecord::new(id(), Outcome::Exit { code: 0 }, 1, 0, 0),
    );
    let reply = registry.kill(&task, "session").unwrap();
    assert_eq!(reply.info.status, BgTaskStatus::Completed);
    assert!(cancel_requests(&daemon).is_empty());
    registry.kill_scope(&owner_scope("unknown-outcome"));
    assert!(cancel_requests(&daemon).is_empty());
}

#[tokio::test]
async fn fate_unknown_bash_kill_outside_runner_limits_sends_no_cancel() {
    let daemon = daemon(Script::Plain, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let (registry, task, _) = finished_remote_for_cancel(
        dir.path(),
        daemon.connection.clone(),
        cancel_unknown_terminal(),
    );
    registry
        .task(&task)
        .unwrap()
        .state
        .lock()
        .unwrap()
        .metadata
        .started_at = 0;
    let reply = registry.kill(&task, "session").unwrap();
    assert_eq!(reply.info.status, BgTaskStatus::FateUnknown);
    registry.kill_scope(&owner_scope("unknown-outcome"));
    assert!(cancel_requests(&daemon).is_empty());
}

#[tokio::test]
async fn fate_unknown_scope_end_cancels_remote_without_rewriting_status() {
    let daemon = daemon(Script::Cancel, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let (registry, task, paths) = finished_remote_for_cancel(
        dir.path(),
        daemon.connection.clone(),
        cancel_unknown_terminal(),
    );
    let before = fs::read(&paths.json).unwrap();
    let stopping = registry.clone();
    tokio::task::spawn_blocking(move || stopping.kill_scope(&owner_scope("unknown-outcome")))
        .await
        .unwrap();
    let requests = cancel_requests(&daemon);
    assert_eq!(requests.len(), 1, "scope end must cancel fate_unknown jobs");
    assert_eq!(requests[0]["params"]["job_id"], id().to_string());
    assert_eq!(
        registry
            .task(&task)
            .unwrap()
            .state
            .lock()
            .unwrap()
            .metadata
            .status,
        BgTaskStatus::FateUnknown
    );
    assert_eq!(fs::read(&paths.json).unwrap(), before);
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
        crate::bash_background::persistence::read_task(&paths.json)
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

#[cfg(target_os = "linux")]
#[tokio::test]
async fn task_scope_standalone_fallback_never_starts_systemd_run() {
    const ROOT: &str = "AFT_TEST_STANDALONE_FALLBACK_SCOPE_ROOT";
    if let Some(root) = std::env::var_os(ROOT) {
        let root = PathBuf::from(root);
        for restarted in [false, true] {
            let daemon = daemon(
                if restarted {
                    Script::AttachRefused
                } else {
                    Script::Refused
                },
                "exec-remote/v1",
            )
            .await;
            let dir = root.join(if restarted { "restored" } else { "fresh" });
            fs::create_dir(&dir).unwrap();
            let counter = dir.join("local-count");
            let command = format!("printf x >> '{}'", counter.display());
            let (original, task_id, paths) = persist_accepted_with_snapshot_scope(
                &dir,
                daemon.connection.clone(),
                &command,
                None,
                true,
                7200,
            );
            original.configure_task_scope_runtime(false, Duration::from_secs(86400));
            original
                .inner
                .task_scopes
                .set_launcher_for_test(root.join("systemd-run"));
            if restarted {
                drop(original);
                let ctx = restarted_context(&dir);
                ctx.bash_background()
                    .inner
                    .task_scopes
                    .set_launcher_for_test(root.join("systemd-run"));
                assert!(!ctx.is_daemon_mode());
                ctx.bash_background()
                    .replay_session(&dir, "session")
                    .unwrap();
                let done = terminal(ctx.bash_background(), &task_id).await;
                assert_eq!(done.exit_code, Some(0));
            } else {
                let task = original.task(&task_id).unwrap();
                let layout = resolve_task_layout(&paths.session_dir, &task_id).unwrap();
                let remote = task.state.lock().unwrap().metadata.remote.clone().unwrap();
                let (
                    plan,
                    shell_path,
                    env,
                    linux_scope,
                    memory_limit_mib,
                    linux_scope_max_runtime_secs,
                ) = crate::sandbox_spawn::restore_local_launch(
                    &layout,
                    remote.fallback_digest.as_deref().unwrap(),
                )
                .unwrap();
                assert!(linux_scope);
                assert_eq!(linux_scope_max_runtime_secs, 7200);
                let request = exec::build_request(
                    &dir,
                    &dir,
                    &dir,
                    &command,
                    BTreeMap::new(),
                    Some(30),
                    &exec::PresetParams::default(),
                );
                original
                    .start_remote_worker(
                        task,
                        Some((
                            request.map_err(RemoteRequestError::Build),
                            LocalFallback {
                                plan,
                                shell_path,
                                env,
                                linux_scope,
                                memory_limit_mib,
                                linux_scope_max_runtime_secs,
                                capture_pipeline: false,
                            },
                        )),
                        None,
                    )
                    .unwrap();
                original.start_watchdog();
                let done = terminal(&original, &task_id).await;
                assert_eq!(done.exit_code, Some(0));
            }
            assert_eq!(fs::read_to_string(counter).unwrap(), "x");
        }
        fs::write(root.join("inner-ran"), "ran").unwrap();
        assert!(!root.join("systemd-calls").exists());
        return;
    }
    let root = tempfile::tempdir().unwrap();
    crate::executable_cache::write_executable(
        &root.path().join("systemd-run"),
        b"#!/bin/sh\nprintf called >> \"${0%/*}/systemd-calls\"\nexit 97\n",
    )
    .unwrap();
    let path = std::env::join_paths(std::iter::once(root.path().to_owned()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact", "bash_background::registry::remote::tests::task_scope_standalone_fallback_never_starts_systemd_run", "--nocapture"]).env(ROOT, root.path()).env("PATH", path).output().unwrap();
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fs::read_to_string(root.path().join("inner-ran")).unwrap(),
        "ran"
    );
    assert!(!root.path().join("systemd-calls").exists());
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
            let (
                plan,
                shell_path,
                env,
                linux_scope,
                memory_limit_mib,
                linux_scope_max_runtime_secs,
            ) = crate::sandbox_spawn::restore_local_launch(
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
                        request.map_err(RemoteRequestError::Build),
                        LocalFallback {
                            plan,
                            shell_path,
                            env,
                            linux_scope,
                            linux_scope_max_runtime_secs,
                            memory_limit_mib,
                            capture_pipeline: false,
                        },
                    )),
                    None,
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
            last_output_seq: None,
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
    // A report without a changed-file list says so, after the output. The
    // other workspace reports stay silent when absent.
    assert!(
        rendered.ends_with("changed files: not reported by the runner"),
        "{rendered}"
    );
}

#[tokio::test]
async fn runon_launch_logs_send_phases_and_acceptance_capture_time() {
    let daemon = daemon(Script::GatedAcceptance, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = remote_test_context(dir.path());
    let capture = Arc::new(Mutex::new(Vec::new()));
    remote_launch_log_captures()
        .lock()
        .unwrap()
        .insert(dir.path().canonicalize().unwrap(), capture.clone());
    let command = "printf runon-diagnostic-command";
    let secret = "runon-diagnostic-secret-value";
    let response = handle_with_policy(
        &ctx,
        Some(launch(daemon.connection.clone())),
        serde_json::json!({
            "command": command,
            "runon": "linux",
            "env": {"DIAGNOSTIC_TOKEN": secret},
        }),
    );
    assert!(response.success, "{response:?}");
    let task_id = response.data["task_id"].as_str().unwrap();
    daemon.wait_for_requests("exec.run", 1).await;
    wait_for(
        "exec.run sent must be logged before the executor accepts",
        || {
            capture
                .lock()
                .unwrap()
                .iter()
                .any(|line| line.contains("runon: exec.run sent "))
                .then_some(())
        },
    )
    .await;
    assert!(!capture
        .lock()
        .unwrap()
        .iter()
        .any(|line| line.contains("runon: remote task accepted ")));
    daemon.acceptance_gate.notify_one();
    wait_for("executor acceptance must be logged", || {
        capture
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains("runon: remote task accepted "))
            .then_some(())
    })
    .await;
    let registry = ctx.bash_background().clone();
    let kill_id = task_id.to_string();
    tokio::task::spawn_blocking(move || registry.kill(&kill_id, "session"))
        .await
        .unwrap()
        .unwrap();
    terminal(ctx.bash_background(), task_id).await;
    remote_worker_exited(task_id).await;
    let lines = capture.lock().unwrap().clone();
    let sent: Vec<_> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.contains("runon: exec.run sent "))
        .collect();
    assert_eq!(sent.len(), 1, "{lines:?}");
    let (sent_index, sent) = sent[0];
    assert!(sent.starts_with("INFO "), "{sent}");
    assert!(
        sent.contains(&format!("task_id={task_id} session=session ")),
        "{sent}"
    );
    let (_, timings) = sent.split_once("pre_send_ms=").unwrap();
    let (total, phases) = timings.split_once(" phases=").unwrap();
    let total: u128 = total.parse().unwrap();
    let phases: Vec<_> = phases
        .split(',')
        .map(|phase| {
            let (name, millis) = phase.split_once(':').unwrap();
            (name, millis.parse::<u128>().unwrap())
        })
        .collect();
    for required in [
        "secret_preflight",
        "workdir_return_paths",
        "permission_preflight",
        "sandbox_preflight",
        "changed_files_baseline",
        "workdir_repository_git",
        "cd_snapshot_git",
        "worker_dispatch",
        "connect_route",
        "supports_submission_key",
        "submission_persistence",
        "send",
    ] {
        assert!(
            phases.iter().any(|(name, _)| *name == required),
            "missing {required}: {sent}"
        );
    }
    let phase_total: u128 = phases.iter().map(|(_, millis)| millis).sum();
    assert!(
        phase_total <= total && total - phase_total < phases.len() as u128,
        "{sent}"
    );
    let (accepted_index, accepted) = lines
        .iter()
        .enumerate()
        .find(|(_, line)| line.contains("runon: remote task accepted "))
        .unwrap();
    assert!(sent_index < accepted_index, "{lines:?}");
    assert!(
        accepted.contains(&format!(
            "task_id={task_id} session=session job_id={}",
            id()
        )),
        "{accepted}"
    );
    accepted
        .split_once("capture_ms=")
        .unwrap()
        .1
        .parse::<u128>()
        .unwrap();
    for line in [sent, accepted] {
        assert!(!line.contains(command) && !line.contains(secret), "{line}");
    }
}

#[tokio::test]
async fn runon_macos_reaches_the_wire_request_as_macos_platform() {
    let daemon = daemon(Script::Plain, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    let response = handle_with_policy(
        &ctx,
        Some(launch(daemon.connection.clone())),
        serde_json::json!({"command": "uname -a", "runon": "macos", "compressed": false}),
    );
    assert!(response.success, "{response:?}");
    let task_id = response.data["task_id"].as_str().unwrap().to_string();
    let _done = terminal(ctx.bash_background(), &task_id).await;
    let runs = exec_runs(&daemon);
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0]["platform"], "macos");
    assert!(runs[0].get("weight_hint").is_none(), "{runs:?}");
    assert!(runs[0].get("network").is_none(), "{runs:?}");
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
        "macos,4c",
        "macos,net",
        "macos,wat",
    ] {
        let response = handle_with_policy(
            &restarted_context(dir.path()),
            Some(launch(daemon.connection.clone())),
            serde_json::json!({"command": command.clone(), "runon": runon}),
        );
        assert!(!response.success, "{runon}: {response:?}");
        let message = response.data["message"].as_str().unwrap_or_default();
        assert!(message.contains(runon), "{runon}: {response:?}");
        let reason = if runon == "windows,4c" || runon == "macos,4c" {
            "fixed at 4 vCPUs"
        } else if runon == "windows,net" || runon == "macos,net" {
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
    assert!(
        worktree_root_containing(outside.path(), exec::DirectoryKind::WorkingDirectory)
            .unwrap()
            .is_none()
    );
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
    // Absent (an older runner): only the missing changed-file list is named.
    // The Git, untracked-file, and ignored-write reports stay silent when
    // absent because the runner never fills them.
    assert_eq!(
        render_terminal_report(&report_vector("older-runner"), &[]).as_deref(),
        Some("These files changed on the server and were NOT copied back:\n  result.txt")
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

#[test]
fn unreported_git_untracked_and_ignored_reports_stay_silent() {
    // The build-server runner never fills the Git, untracked-file, or
    // ignored-write fields, so their absence carries no information and
    // renders nothing. Only a missing changed-file list is still named.
    let mut changed = TerminalRecord::new(id(), Outcome::Exit { code: 0 }, 1, 0, 0);
    changed.workspace_changes = Some(vec!["result.txt".into()]);
    let changed = render_terminal_report(&changed, &[]).unwrap();
    assert_eq!(
        changed,
        "These files changed on the server and were NOT copied back:\n  result.txt"
    );
    assert!(!changed.contains("not reported by the runner"), "{changed}");
    let mut missing = TerminalRecord::new(id(), Outcome::Exit { code: 0 }, 1, 0, 0);
    missing.workspace_changes = None;
    let missing = render_terminal_report(&missing, &[]).unwrap();
    assert_eq!(missing, "changed files: not reported by the runner");
    assert!(!missing.contains("git state"), "{missing}");
    assert!(!missing.contains("untracked files"), "{missing}");
    assert!(!missing.contains("ignored writes"), "{missing}");
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
        submission: None,
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
        return_paths: Vec::new(),
        returned_files: None,
        return_first_seq: None,
        return_staging: Default::default(),
        expired_gap_unverified: Vec::new(),
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
        submission: None,
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
        return_paths: Vec::new(),
        returned_files: None,
        return_first_seq: None,
        return_staging: Default::default(),
        expired_gap_unverified: Vec::new(),
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

/// A configured name the user forwarded through `bash.runon_env` travels to
/// the job, and so does a per-call name. Both live in the request map, which is
/// layered over the ambient environment rather than filtered like it.
#[tokio::test]
async fn runon_keeps_a_forwarded_configured_name_and_a_per_call_name() {
    let daemon = daemon(Script::GatedAcceptance, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    ensure_git_fixture(dir.path());
    let ctx = Arc::new(restarted_context(dir.path()));
    let mut policy = launch(daemon.connection.clone());
    policy.explicit_runon = true;
    // `TZ` is configured and forwarded; `OTHER` is configured but not
    // forwarded, so it is not in the request map at all.
    policy.configured_env_names = vec!["TZ".to_string(), "OTHER".to_string()];
    ctx.bash_background()
        .spawn_remote(
            policy,
            SpawnPlan::Unsandboxed,
            "uname -s",
            resolve_posix_shell(),
            "session".into(),
            dir.path().into(),
            HashMap::from([
                ("TZ".to_string(), "UTC".to_string()),
                ("CALL_ONLY".to_string(), "call-value".to_string()),
            ]),
            crate::bash_background::HardKill::After(Duration::from_secs(30)),
            None,
            dir.path().into(),
            crate::bash_background::TaskSlot::Background { max: 10 },
            true,
            false,
            Some(dir.path().into()),
        )
        .unwrap();
    daemon.wait_for_requests("exec.run", 1).await;
    let runs = exec_runs(&daemon);
    assert_eq!(runs.len(), 1, "{runs:?}");
    let env = &runs[0]["env"];
    assert_eq!(
        env["TZ"], "UTC",
        "a forwarded configured name must survive into the request: {env}"
    );
    assert_eq!(
        env["CALL_ONLY"], "call-value",
        "a per-call name must survive into the request: {env}"
    );
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
            None,
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
            None,
            dir.path().into(),
            crate::bash_background::TaskSlot::Background { max: 10 },
            true,
            false,
            Some(dir.path().into()),
        )
        .unwrap();
    let first = clock.now();
    clock.expect_sleep(first, first + 1000).await;
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
    clock.advance(5000);
    clock.expect_sleep(first + 5000, first + 6000).await;
    assert_eq!(exec_runs(&daemon).len(), 2);
    clock.advance(5000);
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
        submission: None,
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
        cancel_pending: false,
        owner_pause_cancel: None,
        terminal: None,
        pre_start_refusal: None,
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
        expired_gap_unverified: Vec::new(),
    });
    let minutes_left = |budget: &ReattachBudget| {
        budget
            .job_end
            .saturating_duration_since(tokio::time::Instant::now())
            .as_secs_f64()
            / 60.0
    };
    // A conservative 30-minute queue allowance plus 30 of run, counted from submission.
    let full_window = 60.0;

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
    retry_jitter_ms: std::sync::atomic::AtomicU64,
}

impl DrainingClock {
    pub(super) fn now(&self) -> u64 {
        *self.ticks.borrow()
    }

    pub(super) fn retry_jitter_ms(&self, max: u64) -> u64 {
        let jitter = self.retry_jitter_ms.load(Ordering::SeqCst);
        assert!(jitter <= max, "injected jitter {jitter} exceeds {max}");
        jitter
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

    fn now(&self) -> u64 {
        self.clock.now()
    }

    fn advance(&self, ms: u64) {
        self.clock.ticks.send_modify(|now| *now += ms);
    }

    fn set_retry_jitter_ms(&self, ms: u64) {
        self.clock.retry_jitter_ms.store(ms, Ordering::SeqCst);
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
            None,
            dir.into(),
            crate::bash_background::TaskSlot::Background { max: 10 },
            true,
            false,
            Some(dir.into()),
        )
        .unwrap()
}

fn retry_backoff_script(signer: bool, hint: u64) -> Script {
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

async fn assert_zero_hint_waits_for_floor(signer: bool) {
    let daemon = daemon(retry_backoff_script(signer, 0), "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let mut clock = PausedDrainingClock::new(dir.path());
    let registry = registry();
    let task = start_runon_retry(&registry, dir.path(), daemon.connection.clone());
    let first = clock.now();
    tokio::select! {
        _ = clock.expect_sleep(first, first + if signer { 5000 } else { 1000 }) => {},
        done = terminal(&registry, &task) => panic!("zero hint resubmitted without waiting: {done:?}"),
    }
    clock.advance(4999);
    if !signer {
        clock.expect_sleep(first + 4999, first + 5000).await;
    }
    assert_eq!(
        exec_runs(&daemon).len(),
        1,
        "must wait at least five seconds"
    );
    clock.advance(1);
    let done = terminal(&registry, &task).await;
    assert_eq!(done.exit_code, Some(0), "{done:?}");
    assert_eq!(exec_runs(&daemon).len(), 2);
}

#[tokio::test]
async fn runon_retry_backoff_draining_zero_hint_waits_five_seconds() {
    assert_zero_hint_waits_for_floor(false).await;
}

#[tokio::test]
async fn runon_retry_backoff_signer_zero_hint_waits_five_seconds() {
    assert_zero_hint_waits_for_floor(true).await;
}

#[tokio::test]
async fn runon_retry_backoff_large_hints_are_not_shortened() {
    for signer in [false, true] {
        let daemon = daemon(retry_backoff_script(signer, 30_000), "exec-remote/v1").await;
        let dir = tempfile::tempdir().unwrap();
        let mut clock = PausedDrainingClock::new(dir.path());
        let registry = registry();
        let task = start_runon_retry(&registry, dir.path(), daemon.connection.clone());
        let first = clock.now();
        clock
            .expect_sleep(first, first + if signer { 30_000 } else { 1000 })
            .await;
        clock.advance(29_999);
        if !signer {
            clock.expect_sleep(first + 29_999, first + 30_000).await;
        }
        assert_eq!(exec_runs(&daemon).len(), 1, "must honour the runner's hint");
        clock.advance(1);
        assert_eq!(terminal(&registry, &task).await.exit_code, Some(0));
        assert_eq!(exec_runs(&daemon).len(), 2);
    }
}

#[tokio::test]
async fn runon_retry_backoff_simultaneous_refusals_resubmit_at_different_times() {
    for signer in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut clock = PausedDrainingClock::new(dir.path());
        let registry = registry();
        let first = clock.now();
        let mut tasks = Vec::new();
        for jitter in [0, 500, 1000] {
            clock.set_retry_jitter_ms(jitter);
            let daemon = daemon(retry_backoff_script(signer, 5000), "exec-remote/v1").await;
            let task = start_runon_retry(&registry, dir.path(), daemon.connection.clone());
            clock
                .expect_sleep(first, first + if signer { 5000 + jitter } else { 1000 })
                .await;
            if !signer {
                let saved = read_task(&registry.task(&task).unwrap().paths.json).unwrap();
                assert_eq!(
                    saved.remote.unwrap().retry.unwrap().next_retry_at_ms,
                    Some(first + 5000 + jitter)
                );
            }
            tasks.push((daemon, task));
        }
        // All three refusals used the same logical time; only their jitter differs.
        clock.advance(5000);
        for (index, (daemon, task)) in tasks.iter().enumerate() {
            if index > 0 {
                assert_eq!(
                    exec_runs(daemon).len(),
                    1,
                    "later retries must still be waiting"
                );
                clock.advance(500);
            }
            assert_eq!(terminal(&registry, task).await.exit_code, Some(0));
            assert_eq!(exec_runs(daemon).len(), 2);
        }
    }
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
    clock.expect_sleep(first, first + 1000).await;
    clock.advance(100);
    assert_eq!(exec_runs(&daemon).len(), 1, "must not resubmit early");
    clock.advance(4900);
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
    shorten_draining_budget(dir.path(), 60_000);
    let mut clock = PausedDrainingClock::new(dir.path());
    let registry = registry();
    let task_id = start_runon_retry(&registry, dir.path(), daemon.connection.clone());
    let first = clock.now();
    clock.expect_sleep(first, first + 1000).await;
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
    clock.advance(6000);
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
    clock.set_retry_jitter_ms(1000);
    stopped_draining_roots()
        .lock()
        .unwrap()
        .insert(dir.path().into());
    let original = registry();
    let task = start_runon_retry(&original, dir.path(), daemon.connection.clone());
    clock.expect_event(DrainingClockEvent::Stopped).await;
    let first = clock.now();
    let saved = read_task(&original.task(&task).unwrap().paths.json).unwrap();
    assert_eq!(
        saved.remote.unwrap().retry.unwrap().next_retry_at_ms,
        Some(first + 6000)
    );
    // Replaying must keep the sampled deadline, not choose jitter again.
    clock.set_retry_jitter_ms(0);
    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    clock.expect_sleep(first, first + 1000).await;
    clock.advance(5999);
    clock.expect_sleep(first + 5999, first + 6000).await;
    assert_eq!(
        exec_runs(&daemon).len(),
        1,
        "persisted jitter must survive restart"
    );
    clock.advance(1);
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
        assert_eq!(draining_delay_ms(attempt, None, |_| 0), expected);
        assert_eq!(
            draining_delay_ms(attempt, None, |max| max),
            expected + expected / 5
        );
    }
    assert_eq!(draining_delay_ms(100, Some(17), |_| 0), 5000);
    assert_eq!(draining_delay_ms(1, Some(0), |max| max), 6000);
    assert_eq!(draining_delay_ms(1, Some(30_000), |max| max), 36_000);
    assert_eq!(draining_delay_ms(1, Some(u64::MAX), |max| max), u64::MAX);
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
            clock.expect_sleep(first, first + 1000).await;
            clock.advance(5000);
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

/// Points in a draining-retry worker where a test can hold it, so a pause can
/// land exactly between two of the worker's own decisions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum DrainingGate {
    /// After the worker found no cancellation intent, before it checks the
    /// retry budget and decides whether to resubmit.
    BeforeBudgetCheck,
    /// After the worker decided to resubmit, before it records the dispatch.
    BeforeResubmit,
    /// After a signer-refusal retry finished waiting out the runner's hint,
    /// before it rechecks for cancellation and records the resubmission.
    BeforeHintedResubmit,
    /// After the runner refused a submission that AFT will not retry, before
    /// the worker reports the refusal.
    BeforeRefusalReport,
    /// After a retry's resubmission failed without reaching the runner
    /// (see `refuse_resubmission`), before the worker reports that.
    BeforeNoStartRefusal,
    /// After the startup pass claimed a saved task's owed runner cancel,
    /// before it sends it. Keyed by the pass's storage root.
    OwedCancelClaimed,
}

fn refused_resubmission_roots() -> &'static Mutex<HashSet<PathBuf>> {
    static ROOTS: std::sync::OnceLock<Mutex<HashSet<PathBuf>>> = std::sync::OnceLock::new();
    ROOTS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Make the next retry resubmission under `root` fail as a call that never
/// reached the runner.
fn refuse_next_resubmission(root: &Path) {
    refused_resubmission_roots()
        .lock()
        .unwrap()
        .insert(test_clock_key(root));
}

/// Called by a retry worker as it resubmits: true once after
/// `refuse_next_resubmission` for its root.
pub(super) fn refuse_resubmission(root: &Path) -> bool {
    refused_resubmission_roots()
        .lock()
        .unwrap()
        .remove(&test_clock_key(root))
}

type DrainingGateKey = (PathBuf, DrainingGate);

fn draining_gates() -> &'static Mutex<
    HashMap<
        DrainingGateKey,
        (
            tokio::sync::watch::Sender<bool>,
            tokio::sync::watch::Receiver<bool>,
        ),
    >,
> {
    static GATES: std::sync::OnceLock<
        Mutex<
            HashMap<
                DrainingGateKey,
                (
                    tokio::sync::watch::Sender<bool>,
                    tokio::sync::watch::Receiver<bool>,
                ),
            >,
        >,
    > = std::sync::OnceLock::new();
    GATES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Called by the draining-retry worker at each `DrainingGate` point. A gate
/// holds the worker once, then is gone; unarmed points return at once.
pub(super) async fn draining_gate(root: &Path, gate: DrainingGate) {
    let armed = draining_gates()
        .lock()
        .unwrap()
        .remove(&(test_clock_key(root), gate));
    if let Some((reached, mut release)) = armed {
        reached.send_replace(true);
        let _ = release.wait_for(|released| *released).await;
    }
}

struct HeldDrainingGate {
    key: DrainingGateKey,
    reached: tokio::sync::watch::Receiver<bool>,
    release: tokio::sync::watch::Sender<bool>,
}

impl HeldDrainingGate {
    fn arm(root: &Path, gate: DrainingGate) -> Self {
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

    async fn reached(&mut self) {
        tokio::time::timeout(
            Duration::from_secs(30),
            self.reached.wait_for(|reached| *reached),
        )
        .await
        .expect("draining worker must reach the gate")
        .expect("draining gate dropped");
    }

    fn release(&self) {
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

fn owner_scope(scope_ref: &str) -> subc_protocol::scope::ScopeStamp {
    subc_protocol::scope::ScopeStamp::new(
        subc_protocol::Principal::Reserved {
            module_id: "prefrontal-core".into(),
        },
        scope_ref,
        1,
        subc_protocol::scope::ScopeKind::Worker,
    )
}

/// Start the owner's `bash.kill_scope` off the test runtime. It blocks until
/// every paused remote task settles its cancellation, or its own 10-second
/// deadline passes.
fn start_kill_scope(
    registry: &BgTaskRegistry,
    scope: &subc_protocol::scope::ScopeStamp,
) -> tokio::task::JoinHandle<crate::protocol::Response> {
    let registry = registry.clone();
    let scope = scope.clone();
    tokio::task::spawn_blocking(move || {
        crate::commands::bash_kill_scope::handle_bash_kill_scope(
            serde_json::json!({"scope":scope}),
            Some(&scope.owner),
            &[registry],
        )
    })
}

/// The pause has ended the task: it recorded its cancellation intent and the
/// paused terminal state. A worker held at a gate can be released from here on
/// to observe it. Releasing it earlier would let the worker race the pause to
/// the task's terminal state.
async fn pause_recorded(registry: &BgTaskRegistry, task: &str) {
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
async fn assert_pause_settles_promptly(
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

fn assert_paused(registry: &BgTaskRegistry, task: &str) {
    let done = registry.status(task, "session", None, None, 0).unwrap();
    assert_eq!(done.info.status, BgTaskStatus::Killed, "{done:?}");
    assert_eq!(done.info.status_reason.as_deref(), Some("paused_by_owner"));
}

fn start_scoped_runon_retry(
    registry: &BgTaskRegistry,
    dir: &Path,
    connection: PathBuf,
    scope: &subc_protocol::scope::ScopeStamp,
) -> String {
    super::super::with_scope_spawn(Some(scope.clone()), || {
        start_runon_retry(registry, dir, connection)
    })
}

/// The owner pauses a worker whose `runon` task sleeps before retrying a
/// draining runner. The worker wakes, sees the cancellation, and must settle
/// the pause itself: nothing else ever will.
#[tokio::test]
async fn bash_kill_scope_settles_while_draining_retry_sleeps() {
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
    let registry = registry();
    let scope = owner_scope("draining-sleep");
    let task = start_scoped_runon_retry(&registry, dir.path(), daemon.connection.clone(), &scope);
    let first = clock.now();
    clock.expect_sleep(first, first + 1000).await;
    assert_pause_settles_promptly(&registry, start_kill_scope(&registry, &scope), &task).await;
    // The retry clock never moved: cancellation, not a retry deadline, woke
    // the worker.
    assert_eq!(clock.now(), first);
    remote_worker_exited(&task).await;
    assert_paused(&registry, &task);
    assert_eq!(exec_runs(&daemon).len(), 1, "never resubmitted");
}

/// The pause lands after the worker decided to resubmit but before it records
/// the dispatch. Its recheck there must settle the pause.
#[tokio::test]
async fn bash_kill_scope_settles_when_paused_just_before_draining_resubmit() {
    let daemon = daemon(
        Script::Draining {
            refusals: 1,
            retry_after_ms: Some(200),
        },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut clock = PausedDrainingClock::new(dir.path());
    let registry = registry();
    let scope = owner_scope("draining-resubmit");
    let task = start_scoped_runon_retry(&registry, dir.path(), daemon.connection.clone(), &scope);
    let first = clock.now();
    clock.expect_sleep(first, first + 1000).await;
    let mut gate = HeldDrainingGate::arm(dir.path(), DrainingGate::BeforeResubmit);
    clock.advance(5000);
    gate.reached().await;
    let operation = start_kill_scope(&registry, &scope);
    pause_recorded(&registry, &task).await;
    gate.release();
    assert_pause_settles_promptly(&registry, operation, &task).await;
    assert_eq!(clock.now(), first + 5000);
    remote_worker_exited(&task).await;
    assert_paused(&registry, &task);
    assert_eq!(exec_runs(&daemon).len(), 1, "never resubmitted");
}

/// The pause lands just before the worker finds its retry budget spent. The
/// worker then records the draining refusal, which cannot replace the pause's
/// terminal state, and must still settle the pause.
#[tokio::test]
async fn bash_kill_scope_settles_when_paused_as_draining_budget_runs_out() {
    let daemon = daemon(
        Script::Draining {
            refusals: u32::MAX,
            retry_after_ms: Some(10_000),
        },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    shorten_draining_budget(dir.path(), 600);
    let mut clock = PausedDrainingClock::new(dir.path());
    let registry = registry();
    let scope = owner_scope("draining-budget");
    let task = start_scoped_runon_retry(&registry, dir.path(), daemon.connection.clone(), &scope);
    let first = clock.now();
    clock.expect_sleep(first, first + 600).await;
    let mut gate = HeldDrainingGate::arm(dir.path(), DrainingGate::BeforeBudgetCheck);
    clock.advance(600);
    gate.reached().await;
    let operation = start_kill_scope(&registry, &scope);
    pause_recorded(&registry, &task).await;
    gate.release();
    assert_pause_settles_promptly(&registry, operation, &task).await;
    remote_worker_exited(&task).await;
    assert_paused(&registry, &task);
    assert_eq!(exec_runs(&daemon).len(), 1, "never resubmitted");
}

/// After a restart, a draining retry is due and the worker must reconnect to
/// resubmit. The pause lands before that reconnect fails; the worker records
/// that it could not dispatch and must still settle the pause.
#[tokio::test]
async fn bash_kill_scope_settles_when_restored_draining_retry_cannot_reconnect() {
    let daemon = daemon(
        Script::Draining {
            refusals: u32::MAX,
            retry_after_ms: Some(200),
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
    let scope = owner_scope("draining-reconnect");
    let task_id =
        start_scoped_runon_retry(&original, dir.path(), daemon.connection.clone(), &scope);
    clock.expect_event(DrainingClockEvent::Stopped).await;
    // The runner's connection file is gone by the time the module restarts.
    let json = original.task(&task_id).unwrap().paths.json.clone();
    let mut saved = read_task(&json).unwrap();
    saved.remote.as_mut().unwrap().connection_file = Some(dir.path().join("missing.json"));
    write_task(&json, &saved).unwrap();
    clock.advance(5000);
    let mut gate = HeldDrainingGate::arm(dir.path(), DrainingGate::BeforeBudgetCheck);
    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    gate.reached().await;
    let operation = start_kill_scope(&restarted, &scope);
    pause_recorded(&restarted, &task_id).await;
    gate.release();
    assert_pause_settles_promptly(&restarted, operation, &task_id).await;
    assert_paused(&restarted, &task_id);
    assert_eq!(exec_runs(&daemon).len(), 1, "never resubmitted");
}

/// Start a command of `scope` that ignores SIGTERM, saved under `storage` as
/// the module saves its tasks, then let its registry go without stopping it,
/// as a module restart does. `bash.kill_scope` stops local tasks before
/// remote ones, and stopping this one takes the whole termination grace
/// period. A remote worker that recovery started before the pause therefore
/// has that long to act on its own. Returns the task id and the command's
/// process id.
async fn start_saved_term_ignoring_task(
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

/// Run the owner's `bash.kill_scope` as the module's frame loop does after a
/// restart: no live registry holds the scope's tasks, which are found in
/// their saved records under `storage`.
async fn kill_scope_with_saved_tasks(
    storage: &Path,
    scope: &subc_protocol::scope::ScopeStamp,
) -> crate::protocol::Response {
    tokio::time::timeout(
        Duration::from_secs(30),
        start_kill_scope_with_saved_tasks(storage, scope),
    )
    .await
    .expect("bash.kill_scope must reply")
    .unwrap()
}

/// Start the owner's `bash.kill_scope` as `kill_scope_with_saved_tasks`
/// runs it, off the test runtime, without waiting for its reply.
fn start_kill_scope_with_saved_tasks(
    storage: &Path,
    scope: &subc_protocol::scope::ScopeStamp,
) -> tokio::task::JoinHandle<crate::protocol::Response> {
    let storage = storage.to_path_buf();
    let scope = scope.clone();
    tokio::task::spawn_blocking(move || {
        crate::commands::bash_kill_scope::handle_bash_kill_scope_with_saved_tasks(
            serde_json::json!({"scope": scope}),
            Some(&scope.owner),
            &[],
            Some(&storage),
        )
    })
}

/// The reply names both stopped tasks, the remote one settled as sent, and
/// the saved local command is gone.
async fn assert_saved_pause_reply(
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
        (!crate::bash_background::process::is_process_alive(local_pid)).then_some(())
    })
    .await;
}

fn assert_saved_paused(json: &Path) {
    let saved = read_task(json).unwrap();
    assert_eq!(saved.status, BgTaskStatus::Killed, "{saved:?}");
    assert_eq!(saved.status_reason.as_deref(), Some("paused_by_owner"));
    assert!(!saved.local_fallback_started, "{saved:?}");
    assert_eq!(
        saved.remote.unwrap().owner_pause_cancel.as_deref(),
        Some("sent")
    );
}

fn exec_cancels(daemon: &crate::exec_remote::wire_tests::Daemon) -> Vec<serde_json::Value> {
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
async fn saved_task_owing_a_cancel(
    dir: &Path,
    clients: usize,
) -> (
    crate::exec_remote::wire_tests::Daemon,
    String,
    Uuid,
    PathBuf,
) {
    let daemon = crate::exec_remote::wire_tests::daemon_with_clients(
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
fn owed_marker(dir: &Path, task_id: &str) -> PathBuf {
    dir.join("bash-owed-cancels").join(task_id)
}

/// Run the startup pass that sends owed cancels under `storage` to its end,
/// off the test runtime that serves the fake runner.
async fn run_owed_cancel_pass(storage: &Path) -> super::OwedCancelReport {
    let handle = BgTaskRegistry::start_owed_cancel_pass(storage).expect("first pass for this root");
    tokio::time::timeout(
        Duration::from_secs(60),
        tokio::task::spawn_blocking(move || handle.join().unwrap()),
    )
    .await
    .expect("the owed cancel pass must finish")
    .unwrap()
}

/// The saved record shows the owed cancel sent once and nothing owed.
fn assert_owed_cancel_sent_once(
    daemon: &crate::exec_remote::wire_tests::Daemon,
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

/// A paused worker's checkout may never bind again after a restart, so its
/// task's owed runner cancel must not wait for a replay. The startup pass
/// finds it in the saved records and sends it, with no project bound and no
/// task restored.
#[tokio::test]
async fn an_owed_runner_cancel_is_sent_at_startup_without_a_bind() {
    let dir = tempfile::tempdir().unwrap();
    let (daemon, task_id, job_id, json) = saved_task_owing_a_cancel(dir.path(), 2).await;
    // A new module starts; no project binds.
    let restarted = registry();
    let report = run_owed_cancel_pass(dir.path()).await;
    assert_eq!(report.sent, 1, "{report:?}");
    assert_owed_cancel_sent_once(&daemon, job_id, &json);
    assert!(
        restarted.task(&task_id).is_none(),
        "the pass restores no task"
    );
    assert_eq!(
        remote_workers(&task_id),
        (1, 1),
        "the pass starts no worker"
    );
}

fn failed_owed_cancel_attempts() -> &'static Mutex<HashMap<String, usize>> {
    static ATTEMPTS: std::sync::OnceLock<Mutex<HashMap<String, usize>>> =
        std::sync::OnceLock::new();
    ATTEMPTS.get_or_init(Default::default)
}

/// Called by the owed cancel pass after each attempt that failed to reach
/// the runner.
pub(super) fn note_owed_cancel_attempt_failed(task: &str) {
    *failed_owed_cancel_attempts()
        .lock()
        .unwrap()
        .entry(task.to_owned())
        .or_default() += 1;
}

/// The runner's connection is not available when the startup pass first
/// tries to send an owed cancel. The pass waits and tries again, and the
/// cancel is sent once the runner can be reached.
#[tokio::test]
async fn the_owed_cancel_pass_retries_until_the_runner_is_reachable() {
    let dir = tempfile::tempdir().unwrap();
    let (daemon, task_id, job_id, json) = saved_task_owing_a_cancel(dir.path(), 2).await;
    let hidden = daemon.connection.with_extension("hidden");
    fs::rename(&daemon.connection, &hidden).unwrap();
    let pass = {
        let handle = BgTaskRegistry::start_owed_cancel_pass(dir.path()).unwrap();
        tokio::task::spawn_blocking(move || handle.join().unwrap())
    };
    wait_for("the first attempt must fail", || {
        (failed_owed_cancel_attempts()
            .lock()
            .unwrap()
            .get(&task_id)
            .copied()
            .unwrap_or_default()
            >= 1)
            .then_some(())
    })
    .await;
    fs::rename(&hidden, &daemon.connection).unwrap();
    let report = tokio::time::timeout(Duration::from_secs(60), pass)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.sent, 1, "{report:?}");
    assert_owed_cancel_sent_once(&daemon, job_id, &json);
}

/// The production entry point: `configure` of a standalone process (here
/// for another project) starts the owed cancel pass for its storage root.
/// The runner gets the paused task's cancel although the task's own project
/// never binds.
#[tokio::test]
async fn configure_sends_an_owed_runner_cancel_without_binding_its_project() {
    let dir = tempfile::tempdir().unwrap();
    let (daemon, task_id, job_id, json) = saved_task_owing_a_cancel(dir.path(), 2).await;
    let other = tempfile::tempdir().unwrap();
    let ctx = Arc::new(restarted_context(other.path()));
    let configure = {
        let (ctx, other, storage) = (
            ctx.clone(),
            other.path().to_path_buf(),
            dir.path().to_path_buf(),
        );
        tokio::task::spawn_blocking(move || {
            // The configured storage root, not a worker-wide override.
            let _storage = crate::test_env::without_storage_override();
            crate::commands::configure::handle_configure(
                &crate::protocol::RawRequest {
                    id: "configure-other-project".into(),
                    command: "configure".into(),
                    lsp_hints: None,
                    session_id: None,
                    params: serde_json::json!({
                        "project_root": other,
                        "storage_dir": storage,
                        "harness": "opencode",
                    }),
                },
                &ctx,
            )
        })
    };
    daemon.wait_for_requests("exec.cancel", 1).await;
    let configured = configure.await.unwrap();
    assert!(configured.success, "{configured:?}");
    wait_for("the sent cancel must be saved", || {
        let remote = read_task(&json).ok()?.remote?;
        (!remote.cancel_pending).then_some(())
    })
    .await;
    assert_owed_cancel_sent_once(&daemon, job_id, &json);
    assert!(
        ctx.bash_background().task_for_test(&task_id).is_none(),
        "the task's project was not bound"
    );
    assert_eq!(remote_workers(&task_id), (1, 1), "no worker was started");
}

/// The pass reads only the owed-cancel index, not the task history: with 200
/// other finished records saved beside the one owing a cancel, it examines
/// one marker.
#[tokio::test]
async fn the_owed_cancel_pass_reads_only_owed_markers() {
    let dir = tempfile::tempdir().unwrap();
    let (daemon, _task_id, job_id, json) = saved_task_owing_a_cancel(dir.path(), 2).await;
    let finished = read_task(&json).unwrap();
    for n in 0..200u64 {
        let mut record = finished.clone();
        record.task_id = format!("bash-{:016x}", 0xabc000 + n);
        record.remote.as_mut().unwrap().cancel_pending = false;
        let layout = crate::bash_background::persistence::create_task_layout(
            dir.path(),
            &record.session_id,
            &record.task_id,
        )
        .unwrap();
        write_task_at(&layout, &record).unwrap();
    }
    let report = run_owed_cancel_pass(dir.path()).await;
    assert_eq!(report.examined, 1, "{report:?}");
    assert_eq!(report.sent, 1, "{report:?}");
    assert_owed_cancel_sent_once(&daemon, job_id, &json);
}

/// A marker left behind after its record stopped owing the cancel (a crash
/// between the record write and the marker's removal) is removed by the
/// pass, which sends nothing.
#[tokio::test]
async fn a_stale_owed_cancel_marker_is_removed_without_sending() {
    let dir = tempfile::tempdir().unwrap();
    let (daemon, task_id, _job_id, json) = saved_task_owing_a_cancel(dir.path(), 2).await;
    let mut saved = read_task(&json).unwrap();
    saved.remote.as_mut().unwrap().cancel_pending = false;
    write_task(&json, &saved).unwrap();
    let report = run_owed_cancel_pass(dir.path()).await;
    assert_eq!(
        (report.examined, report.stale_markers, report.sent),
        (1, 1, 0),
        "{report:?}"
    );
    assert!(exec_cancels(&daemon).is_empty());
    assert!(!owed_marker(dir.path(), &task_id).exists());
}

type OwedCancelSaveGate = (
    tokio::sync::oneshot::Sender<()>,
    std::sync::mpsc::Receiver<()>,
);

fn owed_cancel_save_gates() -> &'static Mutex<HashMap<String, OwedCancelSaveGate>> {
    static GATES: std::sync::OnceLock<Mutex<HashMap<String, OwedCancelSaveGate>>> =
        std::sync::OnceLock::new();
    GATES.get_or_init(Default::default)
}

/// Called by the owed cancel pass between reading a task's record and
/// writing its outcome; holds the pass there once if a test armed it.
pub(super) fn owed_cancel_save_gate(task: &str) {
    let armed = owed_cancel_save_gates().lock().unwrap().remove(task);
    if let Some((reached, release)) = armed {
        let _ = reached.send(());
        let _ = release.recv_timeout(Duration::from_secs(30));
    }
}

/// While the startup pass saves a paused task's owed-cancel outcome, the
/// task's project (which replayed it meanwhile) takes the session's
/// cancelled-call notices, clearing the record's notice flag. The pass must
/// keep that change: the session gets the owner-cancellation notice once,
/// and not again after another replay.
#[tokio::test]
async fn the_owed_cancel_pass_keeps_a_cancelled_call_notice_taken_during_its_save() {
    let dir = tempfile::tempdir().unwrap();
    let (daemon, task_id, job_id, json) = saved_task_owing_a_cancel(dir.path(), 3).await;
    assert!(read_task(&json).unwrap().cancelled_call_unannounced);
    let mut claimed = HeldDrainingGate::arm(dir.path(), DrainingGate::OwedCancelClaimed);
    let (save_reached, release_save) = {
        let (reached_tx, reached) = tokio::sync::oneshot::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        owed_cancel_save_gates()
            .lock()
            .unwrap()
            .insert(task_id.clone(), (reached_tx, release_rx));
        (reached, release)
    };
    let pass = {
        let handle = BgTaskRegistry::start_owed_cancel_pass(dir.path()).unwrap();
        tokio::task::spawn_blocking(move || handle.join().unwrap())
    };
    claimed.reached().await;
    // The project replays the task while the pass holds its claim.
    let project = registry();
    project.replay_session(dir.path(), "session").unwrap();
    claimed.release();
    tokio::time::timeout(Duration::from_secs(30), save_reached)
        .await
        .expect("the pass must reach its save")
        .unwrap();
    // The session takes its notices between the pass's read and its write.
    let taking = {
        let project = project.clone();
        tokio::task::spawn_blocking(move || project.take_cancelled_call_notices("session"))
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    release_save.send(()).unwrap();
    let notices = taking.await.unwrap();
    assert_eq!(
        notices
            .iter()
            .filter(|notice| notice.contains(&task_id))
            .count(),
        1,
        "{notices:?}"
    );
    tokio::time::timeout(Duration::from_secs(60), pass)
        .await
        .unwrap()
        .unwrap();
    assert_owed_cancel_sent_once(&daemon, job_id, &json);
    let saved = read_task(&json).unwrap();
    assert!(!saved.cancelled_call_unannounced, "{saved:?}");
    project.detach();
    // Another replay must not announce the cancelled call again.
    let again = registry();
    again.replay_session(dir.path(), "session").unwrap();
    let repeated = again.take_cancelled_call_notices("session");
    assert!(
        !repeated.iter().any(|notice| notice.contains(&task_id)),
        "the notice was delivered again: {repeated:?}"
    );
}

/// The startup pass has claimed a saved task's owed cancel when its project
/// binds and replays the task. The replay must not start a worker that sends
/// the cancel a second time; it takes the pass's outcome instead.
#[tokio::test]
async fn a_replay_during_the_owed_cancel_pass_sends_no_second_cancel() {
    let dir = tempfile::tempdir().unwrap();
    let (daemon, task_id, job_id, json) = saved_task_owing_a_cancel(dir.path(), 3).await;
    let mut gate = HeldDrainingGate::arm(dir.path(), DrainingGate::OwedCancelClaimed);
    let pass = {
        let handle = BgTaskRegistry::start_owed_cancel_pass(dir.path()).unwrap();
        tokio::task::spawn_blocking(move || handle.join().unwrap())
    };
    gate.reached().await;
    let project = registry();
    project.replay_session(dir.path(), "session").unwrap();
    assert_eq!(
        remote_workers(&task_id),
        (1, 1),
        "the replay started a worker"
    );
    gate.release();
    let report = tokio::time::timeout(Duration::from_secs(60), pass)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.sent, 1, "{report:?}");
    assert_owed_cancel_sent_once(&daemon, job_id, &json);
    let task = project.task(&task_id).unwrap();
    let remote = task.state.lock().unwrap().metadata.remote.clone().unwrap();
    assert!(!remote.cancel_pending, "{remote:?}");
    assert_eq!(remote.owner_pause_cancel.as_deref(), Some("sent"));
    assert_eq!(remote_workers(&task_id), (1, 1));
}

/// The startup pass and a project's replay of the same saved task run at the
/// same moment, ten times. Whichever claims the owed cancel first sends it;
/// the runner must receive it exactly once each time.
#[tokio::test]
async fn the_owed_cancel_pass_and_a_replay_racing_send_one_cancel() {
    for round in 0..10 {
        let dir = tempfile::tempdir().unwrap();
        let (daemon, _task_id, job_id, json) = saved_task_owing_a_cancel(dir.path(), 3).await;
        let start = Arc::new(std::sync::Barrier::new(2));
        let pass = {
            let (start, storage) = (start.clone(), dir.path().to_path_buf());
            tokio::task::spawn_blocking(move || {
                start.wait();
                BgTaskRegistry::start_owed_cancel_pass(&storage)
                    .unwrap()
                    .join()
                    .unwrap()
            })
        };
        let project = registry();
        let replay = {
            let (project, storage) = (project.clone(), dir.path().to_path_buf());
            tokio::task::spawn_blocking(move || {
                start.wait();
                project.replay_session(&storage, "session").unwrap();
            })
        };
        tokio::time::timeout(Duration::from_secs(60), pass)
            .await
            .unwrap()
            .unwrap();
        replay.await.unwrap();
        wait_for(
            &format!("round {round}: the owed cancel must be saved"),
            || {
                let remote = read_task(&json).ok()?.remote?;
                (!remote.cancel_pending && remote.owner_pause_cancel.as_deref() == Some("sent"))
                    .then_some(())
            },
        )
        .await;
        // Give a second sender the time it would need to reach the runner.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            exec_cancels(&daemon).len(),
            1,
            "round {round}: {:?}",
            exec_cancels(&daemon)
        );
        assert_owed_cancel_sent_once(&daemon, job_id, &json);
        project.detach();
    }
}

/// `bash.kill_scope` stops a scope's local tasks in one pass and its remote
/// tasks in a second. Here the runner refuses a remote task between the two
/// passes, and its worker would run the command locally instead: the first
/// pass saw it remote, the second would see it local, so neither would stop
/// it. The pause must cover it anyway: its local fallback never starts, and
/// the reply names it as stopped.
#[tokio::test]
async fn bash_kill_scope_stops_a_task_that_falls_back_to_local_between_its_passes() {
    let daemon = daemon(Script::GatedAdmission { refuse: true }, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("local-fallback-ran");
    let registry = registry();
    let scope = owner_scope("fallback-between-passes");
    ensure_git_fixture(dir.path());
    let task_id = super::super::with_scope_spawn(Some(scope.clone()), || {
        registry.spawn_remote(
            launch(daemon.connection.clone()),
            SpawnPlan::Unsandboxed,
            &format!("printf x > '{}'", marker.display()),
            resolve_posix_shell(),
            "session".into(),
            dir.path().into(),
            HashMap::new(),
            crate::bash_background::HardKill::After(Duration::from_secs(30)),
            None,
            dir.path().into(),
            crate::bash_background::TaskSlot::Background { max: 10 },
            true,
            false,
            Some(dir.path().into()),
        )
    })
    .unwrap();
    daemon.wait_for_requests("exec.run", 1).await;
    // The runner refuses the job; hold the worker after it received the
    // refusal, before it decides on the local fallback.
    let mut refusal = HeldDrainingGate::arm(dir.path(), DrainingGate::BeforeRefusalReport);
    daemon.acceptance_gate.notify_one();
    refusal.reached().await;
    let (between_passes, release_remote_pass) =
        crate::commands::bash_kill_scope::saved_recovery_gate::arm(
            dir.path(),
            crate::commands::bash_kill_scope::saved_recovery_gate::Point::BetweenPasses,
        );
    let operation = {
        let registry = registry.clone();
        let scope = scope.clone();
        let storage = dir.path().to_path_buf();
        tokio::task::spawn_blocking(move || {
            crate::commands::bash_kill_scope::handle_bash_kill_scope_with_saved_tasks(
                serde_json::json!({"scope": scope}),
                Some(&scope.owner),
                &[registry],
                Some(&storage),
            )
        })
    };
    tokio::time::timeout(Duration::from_secs(30), between_passes)
        .await
        .expect("bash.kill_scope must finish its local pass")
        .unwrap();
    // Between the passes, the worker acts on the refusal.
    refusal.release();
    remote_worker_exited(&task_id).await;
    release_remote_pass.send(()).unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(30), operation)
        .await
        .expect("bash.kill_scope must reply")
        .unwrap();
    // Give a local child that wrongly started the time to write its marker.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!marker.exists(), "the local fallback ran: {reply:?}");
    assert!(reply.success, "{reply:?}");
    let killed = reply.data["killed"].as_array().unwrap();
    assert!(
        killed
            .iter()
            .any(|entry| entry["task_id"] == task_id.as_str()),
        "the stopped task must be reported: {reply:?}"
    );
    assert_paused(&registry, &task_id);
}

/// The owner pauses a scope whose `runon` job the runner has accepted, and
/// the module stops before its worker sends the cancellation. The pause
/// already saved the task as stopped, so the saved record must also say the
/// runner is still owed that cancellation: replaying it after the restart
/// sends it, and the runner job does not keep running unnoticed.
#[tokio::test]
async fn a_paused_runon_jobs_unsent_cancel_is_sent_after_a_restart() {
    let daemon =
        crate::exec_remote::wire_tests::daemon_with_clients(Script::Cancel, "exec-remote/v1", 2)
            .await;
    let dir = tempfile::tempdir().unwrap();
    let scope = owner_scope("unsent-cancel");
    let original = registry();
    let task_id =
        start_scoped_runon_retry(&original, dir.path(), daemon.connection.clone(), &scope);
    let job_id = wait_for("the runner must accept the job", || {
        let task = original.task(&task_id)?;
        let state = task.state.lock().unwrap();
        state.metadata.remote.as_ref()?.job_id
    })
    .await;
    // The worker stops right before it would send the cancellation.
    stopped_draining_roots()
        .lock()
        .unwrap()
        .insert(dir.path().into());
    let members = original.begin_scope_pause(&scope).unwrap();
    original.pause_scope(&scope, true, &members).unwrap();
    remote_worker_exited(&task_id).await;
    assert!(exec_cancels(&daemon).is_empty(), "the cancel was sent");
    let json = original.task(&task_id).unwrap().paths.json.clone();
    let saved = read_task(&json).unwrap();
    assert_eq!(saved.status, BgTaskStatus::Killed, "{saved:?}");
    original.detach();

    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    daemon.wait_for_requests("exec.cancel", 1).await;
    let cancels = exec_cancels(&daemon);
    assert_eq!(cancels.len(), 1, "{cancels:?}");
    assert!(
        cancels[0].to_string().contains(&job_id.to_string()),
        "{cancels:?}"
    );
    let settled = wait_for("the sent cancellation must be saved", || {
        let saved = read_task(&json).ok()?;
        let remote = saved.remote.clone()?;
        (!remote.cancel_pending && remote.owner_pause_cancel.is_some()).then_some(saved)
    })
    .await;
    assert_eq!(settled.status, BgTaskStatus::Killed, "{settled:?}");
    assert_eq!(
        settled.status_reason.as_deref(),
        Some("paused_by_owner"),
        "{settled:?}"
    );
    assert_eq!(
        settled.remote.unwrap().owner_pause_cancel.as_deref(),
        Some("sent")
    );
    assert_eq!(exec_runs(&daemon).len(), 1, "never resubmitted");
}

/// After a module restart, the owner pauses a worker whose saved draining
/// retry is already due. Recovering that saved task for the pause must not
/// let it resubmit: the resubmitted command is the one the owner is stopping.
/// The scope's saved local command keeps the pause busy long enough for a
/// worker started during recovery to reach the runner.
#[tokio::test]
async fn bash_kill_scope_never_resubmits_a_due_saved_draining_retry() {
    let daemon = crate::exec_remote::wire_tests::daemon_with_clients(
        Script::Draining {
            refusals: u32::MAX,
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
    let scope = owner_scope("saved-draining-retry");
    let original = registry();
    let task_id =
        start_scoped_runon_retry(&original, dir.path(), daemon.connection.clone(), &scope);
    clock.expect_event(DrainingClockEvent::Stopped).await;
    let json = original.task(&task_id).unwrap().paths.json.clone();
    original.detach();
    let (local_task, local_pid) = start_saved_term_ignoring_task(dir.path(), &scope).await;
    // The retry is due by the time the owner pauses the restarted module.
    clock.advance(5000);
    let reply = kill_scope_with_saved_tasks(dir.path(), &scope).await;
    assert_eq!(
        exec_runs(&daemon).len(),
        1,
        "the paused draining retry was resubmitted: {reply:?}"
    );
    assert_saved_pause_reply(&reply, &task_id, &local_task, local_pid).await;
    assert_saved_paused(&json);
    assert_eq!(exec_runs(&daemon).len(), 1, "never resubmitted");
}

/// A `runon` task of `scope` whose draining retry is saved and due, as a
/// module restart leaves it: its worker stopped and its registry is gone.
/// Returns the runner, the paused retry clock, the task id and the path of
/// its saved record. The runner refuses the first `refusals` runs as
/// draining and accepts the rest, which exit 0. It accepts a second
/// connection, so a worker started after the restart can reach it.
async fn saved_due_draining_retry(
    dir: &Path,
    scope: &subc_protocol::scope::ScopeStamp,
    refusals: u32,
) -> (
    crate::exec_remote::wire_tests::Daemon,
    PausedDrainingClock,
    String,
    PathBuf,
) {
    let daemon = crate::exec_remote::wire_tests::daemon_with_clients(
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
fn assert_one_saved_task_paused(reply: &crate::protocol::Response, task: &str) {
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
fn assert_project_told_of_the_stop_once(project: &BgTaskRegistry, task: &str) {
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

/// After a module restart, the owner pauses a scope whose saved draining
/// retry is due, and the task's project binds while `bash.kill_scope` holds
/// the copy it recovered, not yet paused. The bind must register that same
/// task, held without a worker, not build a second copy whose worker would
/// resubmit the command the owner is stopping.
#[tokio::test]
async fn bash_kill_scope_recovery_then_a_bind_share_one_saved_task() {
    let dir = tempfile::tempdir().unwrap();
    let scope = owner_scope("saved-recovery-then-bind");
    let (daemon, _clock, task_id, json) =
        saved_due_draining_retry(dir.path(), &scope, u32::MAX).await;
    let (reached, release) = crate::commands::bash_kill_scope::saved_recovery_gate::arm(
        dir.path(),
        crate::commands::bash_kill_scope::saved_recovery_gate::Point::AfterRecovery,
    );
    let operation = start_kill_scope_with_saved_tasks(dir.path(), &scope);
    tokio::time::timeout(Duration::from_secs(30), reached)
        .await
        .expect("bash.kill_scope must recover the saved task")
        .unwrap();
    let project = registry();
    project.replay_session(dir.path(), "session").unwrap();
    assert_eq!(
        remote_workers(&task_id),
        (1, 1),
        "the bind started a worker before the pause"
    );
    release.send(()).unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(30), operation)
        .await
        .expect("bash.kill_scope must reply")
        .unwrap();
    assert_one_saved_task_paused(&reply, &task_id);
    // One worker since the restart: the one the pause started to settle it.
    assert_eq!(remote_workers(&task_id).0, 2, "{reply:?}");
    wait_for("the paused task's worker must stop", || {
        (remote_workers(&task_id) == (2, 2)).then_some(())
    })
    .await;
    // The bound project sees the owner's stop on the task it registered.
    assert_paused(&project, &task_id);
    assert_saved_paused(&json);
    assert_eq!(exec_runs(&daemon).len(), 1, "never resubmitted");
    assert_project_told_of_the_stop_once(&project, &task_id);
}

/// After a module restart, the task's project binds and its worker is about
/// to resubmit the saved, due draining retry when the owner's
/// `bash.kill_scope` recovers the scope's saved tasks, before the module
/// lists the project's registry as live. The pause must register the bound
/// project's task, so that its worker sees the stop; a private copy would be
/// paused while that worker resubmits.
#[tokio::test]
async fn a_bind_then_bash_kill_scope_recovery_share_one_saved_task() {
    let dir = tempfile::tempdir().unwrap();
    let scope = owner_scope("saved-bind-then-recovery");
    let (daemon, _clock, task_id, json) =
        saved_due_draining_retry(dir.path(), &scope, u32::MAX).await;
    let mut gate = HeldDrainingGate::arm(dir.path(), DrainingGate::BeforeBudgetCheck);
    let project = registry();
    project.replay_session(dir.path(), "session").unwrap();
    gate.reached().await;
    assert_eq!(remote_workers(&task_id), (2, 1));
    let operation = start_kill_scope_with_saved_tasks(dir.path(), &scope);
    wait_for("the pause must be saved", || {
        let saved = read_task(&json).ok()?;
        (saved.status == BgTaskStatus::Killed
            && saved.status_reason.as_deref()
                == Some(crate::bash_background::registry::OWNER_PAUSED_REASON))
        .then_some(())
    })
    .await;
    gate.release();
    let reply = tokio::time::timeout(Duration::from_secs(30), operation)
        .await
        .expect("bash.kill_scope must reply")
        .unwrap();
    assert_one_saved_task_paused(&reply, &task_id);
    // The bound project's worker is the only one since the restart.
    assert_eq!(remote_workers(&task_id).0, 2, "{reply:?}");
    wait_for("the paused task's worker must stop", || {
        (remote_workers(&task_id) == (2, 2)).then_some(())
    })
    .await;
    assert_paused(&project, &task_id);
    assert_saved_paused(&json);
    assert_eq!(exec_runs(&daemon).len(), 1, "never resubmitted");
    assert_project_told_of_the_stop_once(&project, &task_id);
}

/// After a module restart, `bash.kill_scope` recovers and stops a saved
/// task. The task's project binds just as the stop is saved: its replay read
/// the task's record from before the stop, so it registers the task the
/// recovery registry holds, which is already stopped. The stop's notices,
/// which the recovery registry could not deliver, must reach the project's
/// session once.
#[tokio::test]
async fn a_bind_racing_a_saved_tasks_stop_gets_its_notices() {
    let dir = tempfile::tempdir().unwrap();
    let scope = owner_scope("saved-stop-then-bind");
    let (daemon, _clock, task_id, json) =
        saved_due_draining_retry(dir.path(), &scope, u32::MAX).await;
    let before_stop = fs::read(&json).unwrap();
    let (reached, release) = crate::commands::bash_kill_scope::saved_recovery_gate::arm(
        dir.path(),
        crate::commands::bash_kill_scope::saved_recovery_gate::Point::BeforeDetach,
    );
    let operation = start_kill_scope_with_saved_tasks(dir.path(), &scope);
    tokio::time::timeout(Duration::from_secs(30), reached)
        .await
        .expect("bash.kill_scope must stop the saved task")
        .unwrap();
    assert_saved_paused(&json);
    // Stand in for a replay that read the record just before the stop
    // saved it.
    fs::write(&json, &before_stop).unwrap();
    let project = registry();
    project.replay_session(dir.path(), "session").unwrap();
    release.send(()).unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(30), operation)
        .await
        .expect("bash.kill_scope must reply")
        .unwrap();
    assert_one_saved_task_paused(&reply, &task_id);
    wait_for("the paused task's worker must stop", || {
        (remote_workers(&task_id) == (2, 2)).then_some(())
    })
    .await;
    assert_eq!(
        remote_workers(&task_id),
        (2, 2),
        "the bind started a worker"
    );
    assert_paused(&project, &task_id);
    assert_eq!(exec_runs(&daemon).len(), 1, "never resubmitted");
    assert_project_told_of_the_stop_once(&project, &task_id);
}

/// What the threads of a stress round that has not finished are doing.
struct RoundSnapshot {
    /// Which of the shared task's main locks are held, and what each
    /// relevant thread is blocked in, by syscall and (for a lock wait) by
    /// the lock's name.
    report: String,
    /// A round thread is in a syscall other than a lock wait (on the build
    /// servers, typically an `fsync` of a task record that is slow under
    /// load, made while holding the task's state lock), or a background-task
    /// thread is in a disk syscall: the round may still be making progress.
    /// Always true where threads cannot be inspected.
    progressing: bool,
}

/// Disk syscalls (read, write, open, openat, fsync, fdatasync, rename,
/// renameat2, unlink, unlinkat, newfstatat, getdents64, ftruncate) by number.
#[cfg(target_arch = "x86_64")]
const DISK_SYSCALLS: &[&str] = &[
    "0", "1", "2", "257", "74", "75", "82", "316", "87", "263", "262", "217", "77",
];
#[cfg(target_arch = "aarch64")]
const DISK_SYSCALLS: &[&str] = &[
    "63", "64", "56", "82", "83", "38", "276", "35", "79", "61", "46",
];
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
const DISK_SYSCALLS: &[&str] = &[];

/// Snapshot the round's threads. On Linux, a thread blocked on a std mutex
/// or condvar waits in `futex` (syscall 202 on x86_64, 98 on aarch64) with
/// the address of that mutex or condvar as its first argument; the snapshot
/// compares it with the named locks below and labels each waiter it matches.
fn round_snapshot(
    task: &BgTask,
    recovery: &BgTaskRegistry,
    project: &BgTaskRegistry,
) -> RoundSnapshot {
    fn held<T>(lock: &Mutex<T>) -> bool {
        matches!(lock.try_lock(), Err(std::sync::TryLockError::WouldBlock))
    }
    fn addr<T>(lock: &T) -> usize {
        lock as *const T as usize
    }
    let mut named: Vec<(String, usize)> = vec![
        ("claims".into(), addr(saved_remote_task_claims())),
        ("task.state".into(), addr(&task.state)),
        ("task.notice_registry".into(), addr(&task.notice_registry)),
        (
            "task.routed".into(),
            addr(&task.terminal_notification_routed),
        ),
        ("task.terminal_render".into(), addr(&task.terminal_render)),
        ("task.terminal_at".into(), addr(&task.terminal_at)),
        (
            "task.completion_secrets".into(),
            addr(&task.completion_secrets),
        ),
        ("task.kill_settled".into(), addr(&task.kill_settled)),
        (
            "task.json_writes".into(),
            addr(&task.db_write_order.json_writes),
        ),
        ("task.db_pending".into(), addr(&task.db_write_order.pending)),
        ("task.db_written".into(), addr(&task.db_write_order.written)),
        ("db_write_orders".into(), addr(db_write_orders())),
    ];
    for (name, registry) in [("recovery", recovery), ("project", project)] {
        let inner = &registry.inner;
        named.extend([
            (format!("{name}.tasks"), addr(&inner.tasks)),
            (
                format!("{name}.watchdog_tasks"),
                addr(&inner.watchdog_tasks),
            ),
            (format!("{name}.cleanup_queue"), addr(&inner.cleanup_queue)),
            (format!("{name}.completions"), addr(&inner.completions)),
            (
                format!("{name}.watch_registry"),
                addr(&inner.watch_registry),
            ),
            (
                format!("{name}.session_harnesses"),
                addr(&inner.session_harnesses),
            ),
            (
                format!("{name}.live_delivery_sessions"),
                addr(&inner.live_delivery_sessions),
            ),
            (
                format!("{name}.cancelled_call_tasks"),
                addr(&inner.cancelled_call_tasks),
            ),
            (
                format!("{name}.held_remote_workers"),
                addr(&inner.held_remote_workers),
            ),
        ]);
    }
    let held_now = [
        ("claims", held(saved_remote_task_claims())),
        ("task.state", held(&task.state)),
        ("task.notice_registry", held(&task.notice_registry)),
        ("task.routed", held(&task.terminal_notification_routed)),
        ("task.terminal_render", held(&task.terminal_render)),
    ]
    .into_iter()
    .filter(|(_, held)| *held)
    .map(|(name, _)| name)
    .collect::<Vec<_>>();
    let Ok(entries) = fs::read_dir("/proc/self/task") else {
        return RoundSnapshot {
            report: format!("held: {held_now:?}; threads cannot be inspected here"),
            progressing: true,
        };
    };
    let mut threads = Vec::new();
    let mut progressing = false;
    for entry in entries.flatten() {
        let dir = entry.path();
        let comm = fs::read_to_string(dir.join("comm")).unwrap_or_default();
        let comm = comm.trim();
        let syscall = fs::read_to_string(dir.join("syscall")).unwrap_or_default();
        let mut fields = syscall.split_whitespace();
        let number = fields.next().unwrap_or("").to_string();
        let lock_wait = number == "202" || number == "98";
        let target = fields
            .next()
            .and_then(|arg| usize::from_str_radix(arg.trim_start_matches("0x"), 16).ok());
        let lock = target
            .filter(|_| lock_wait)
            .and_then(|target| {
                named
                    .iter()
                    .find(|(_, start)| target >= *start && target < *start + 64)
            })
            .map(|(name, _)| name.as_str());
        // A round thread outside a futex wait may still be making progress;
        // so may a background-task thread (watchdog, scratch cleanup) in a
        // disk syscall, possibly while holding a lock a round thread waits for.
        if (comm.starts_with("stress-") && !lock_wait)
            || (comm.starts_with("aft-bash") && DISK_SYSCALLS.contains(&number.as_str()))
        {
            progressing = true;
        }
        if lock.is_some() || comm.starts_with("stress-") {
            let wchan = fs::read_to_string(dir.join("wchan")).unwrap_or_default();
            threads.push(format!(
                "{comm}: syscall {number} ({}){}",
                wchan.trim(),
                lock.map(|lock| format!(" waiting on {lock}"))
                    .unwrap_or_default()
            ));
        }
    }
    RoundSnapshot {
        report: format!("held: {held_now:?}; threads: {threads:?}"),
        progressing,
    }
}

/// The owner's stop of a saved task that `bash.kill_scope` recovered, and
/// the task's project registering the same task, run at the same moment on
/// two threads, 200 times. Each round must finish (a lock-order deadlock
/// between the two paths would hang one of them) and must queue exactly one
/// completion for the project's session, whichever path gets there first.
/// Both paths write the task's record while holding its state lock, and on
/// a loaded build server one such disk sync can take several seconds; a
/// round is therefore failed as deadlocked once it has run five seconds with
/// its threads only waiting on locks, and otherwise after thirty.
#[tokio::test]
async fn concurrent_stop_and_registration_of_a_shared_task_never_deadlock() {
    const ROUNDS: usize = 200;
    // A round unfinished this long whose threads only wait on locks is
    // deadlocked, and fails at once.
    const DEADLOCK_DEADLINE: Duration = Duration::from_secs(5);
    // A round whose threads are still doing disk I/O (slow syncs under load)
    // gets this long in all, so a stuck round can never hang the suite.
    const ROUND_CAP: Duration = Duration::from_secs(30);
    for round in 0..ROUNDS {
        let dir = tempfile::tempdir().unwrap();
        let scope = owner_scope(&format!("stop-registration-race-{round}"));
        let (_daemon, _clock, task_id, _json) =
            saved_due_draining_retry(dir.path(), &scope, u32::MAX).await;
        let (recovered, errors) =
            BgTaskRegistry::recover_saved_scope_tasks(dir.path(), &scope, &[]);
        assert!(errors.is_empty(), "round {round}: {errors:?}");
        let recovery = recovered.into_iter().next().unwrap();
        let shared = recovery.task(&task_id).unwrap();
        let project = registry();
        let start = Arc::new(std::sync::Barrier::new(2));
        let (finished, results) = std::sync::mpsc::channel::<(&str, Result<(), String>)>();
        {
            let (start, finished, recovery, scope) = (
                start.clone(),
                finished.clone(),
                recovery.clone(),
                scope.clone(),
            );
            std::thread::Builder::new()
                .name("stress-stop".into())
                .spawn(move || {
                    start.wait();
                    let result = recovery
                        .begin_scope_pause(&scope)
                        .and_then(|members| recovery.pause_scope(&scope, true, &members))
                        .map(|_| ());
                    let _ = finished.send(("stop", result));
                })
                .unwrap();
        }
        {
            let (project, storage) = (project.clone(), dir.path().to_path_buf());
            std::thread::Builder::new()
                .name("stress-register".into())
                .spawn(move || {
                    start.wait();
                    let result = project.replay_session(&storage, "session");
                    let _ = finished.send(("registration", result));
                })
                .unwrap();
        }
        let started = Instant::now();
        let mut finished_paths = Vec::new();
        while finished_paths.len() < 2 {
            let deadline = if started.elapsed() < DEADLOCK_DEADLINE {
                DEADLOCK_DEADLINE
            } else {
                // Past the deadlock deadline, keep waiting while the snapshot
                // reports possible progress; look again each second.
                started.elapsed() + Duration::from_secs(1)
            };
            match results.recv_timeout(deadline.saturating_sub(started.elapsed())) {
                Ok((path, result)) => {
                    result.unwrap_or_else(|error| panic!("round {round}: {path} failed: {error}"));
                    finished_paths.push(path);
                }
                Err(_) => {
                    let snapshot = round_snapshot(&shared, &recovery, &project);
                    if !snapshot.progressing {
                        panic!(
                            "round {round}: DEADLOCK: a concurrent stop and project registration of one shared task did not both finish within {:?}, and their threads only wait on locks (finished: {finished_paths:?}); {}",
                            started.elapsed(),
                            snapshot.report
                        );
                    }
                    if started.elapsed() >= ROUND_CAP {
                        panic!(
                            "round {round}: a concurrent stop and project registration of one shared task did not both finish within {ROUND_CAP:?} (finished: {finished_paths:?}); {}",
                            snapshot.report
                        );
                    }
                }
            }
        }
        let completions = project
            .drain_completions_for_session(Some("session"))
            .into_iter()
            .filter(|completion| completion.task_id == task_id)
            .count();
        assert_eq!(completions, 1, "round {round}: completions for {task_id}");
        assert_paused(&project, &task_id);
        recovery.detach();
        project.detach();
    }
}

/// Repeated owner stops can replace metadata during every optimistic replay
/// open. A busy writer is not a corrupt task: replay must still register it
/// and deliver its stop, even when the bounded open retries are exhausted.
#[tokio::test]
async fn a_stop_racing_every_replay_metadata_open_still_delivers_one_completion() {
    use std::os::unix::fs::MetadataExt;

    // Disk discovery resolves then reads metadata (skip 0/1); the batch's
    // revalidation repeats those opens (skip 2/3). Project-scoped discovery
    // reads metadata directly after resolving directories (skip 0, true).
    for (skip_metadata_opens, project_scoped) in
        [(1, false), (0, false), (2, false), (3, false), (0, true)]
    {
        let open_attempts = crate::bash_background::persistence::metadata_open_attempts_for_test();
        let dir = tempfile::tempdir().unwrap();
        let scope = owner_scope(&format!(
            "stop-during-replay-metadata-open-{skip_metadata_opens}-{project_scoped}"
        ));
        let (_daemon, _clock, task_id, json) =
            saved_due_draining_retry(dir.path(), &scope, u32::MAX).await;
        let (recovered, errors) =
            BgTaskRegistry::recover_saved_scope_tasks(dir.path(), &scope, &[]);
        assert!(errors.is_empty(), "{errors:?}");
        let recovery = recovered.into_iter().next().unwrap();
        let project = registry();
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let registration = {
            let (project, storage) = (project.clone(), dir.path().to_path_buf());
            std::thread::spawn(move || {
                let mut metadata_opens = 0;
                crate::bash_background::persistence::with_artifact_validation_hook_for_test(
                    move |file| {
                        let opened = file.metadata().unwrap();
                        let current = fs::metadata(&json).unwrap();
                        if (opened.dev(), opened.ino()) != (current.dev(), current.ino()) {
                            return;
                        }
                        metadata_opens += 1;
                        // Pause with the old metadata open, before its link count
                        // is checked. Each owner write unlinks that exact handle.
                        if (skip_metadata_opens + 1..=skip_metadata_opens + open_attempts)
                            .contains(&metadata_opens)
                        {
                            reached_tx
                                .send(metadata_opens - skip_metadata_opens)
                                .unwrap();
                            release_rx
                                .recv_timeout(Duration::from_secs(30))
                                .expect("the owner must release the metadata reader");
                        }
                    },
                    || {
                        if project_scoped {
                            project.replay_session_for_project(&storage, "session", &storage)
                        } else {
                            project.replay_session(&storage, "session")
                        }
                    },
                )
            })
        };
        for attempt in 1..=open_attempts {
            assert_eq!(
                reached_rx.recv_timeout(Duration::from_secs(30)).unwrap(),
                attempt
            );
            if attempt < open_attempts {
                recovery
                    .record_owner_pause_intent(&recovery.task(&task_id).unwrap())
                    .unwrap();
            } else {
                let members = recovery.begin_scope_pause(&scope).unwrap();
                recovery.pause_scope(&scope, true, &members).unwrap();
            }
            release_tx.send(()).unwrap();
        }
        registration.join().unwrap().unwrap();
        assert_project_told_of_the_stop_once(&project, &task_id);
        assert_paused(&project, &task_id);
        recovery.detach();
        project.detach();
    }
}

/// `bash.kill_scope` recovered a saved task and holds it without a worker.
/// The task's project then binds and registers that same task. The pause
/// records no cancellation (as when it fails part way), so the held task must
/// go to the project's registry, whose one worker resumes it as the project's
/// replay would have.
#[tokio::test]
async fn an_unpaused_held_task_is_handed_to_the_project_that_registered_it() {
    let dir = tempfile::tempdir().unwrap();
    let scope = owner_scope("held-handoff");
    let (daemon, _clock, task_id, _json) = saved_due_draining_retry(dir.path(), &scope, 1).await;
    let (recovered, errors) = BgTaskRegistry::recover_saved_scope_tasks(dir.path(), &scope, &[]);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(recovered.len(), 1);
    let project = registry();
    project.replay_session(dir.path(), "session").unwrap();
    assert_eq!(
        remote_workers(&task_id),
        (1, 1),
        "nothing may run while the task is held"
    );
    recovered[0].start_held_remote_workers();
    recovered[0].detach();
    let done = terminal(&project, &task_id).await;
    assert_eq!(done.exit_code, Some(0), "{done:?}");
    wait_for("the project's worker must stop", || {
        (remote_workers(&task_id) == (2, 2)).then_some(())
    })
    .await;
    assert_eq!(
        exec_runs(&daemon).len(),
        2,
        "the first, refused run and one resubmission after the restart"
    );
}

/// A registry restored a saved task, and its worker stopped without finishing
/// it. Another registry that restores the same task registers it and must run
/// it with one worker of its own; both registries see the outcome.
#[tokio::test]
async fn a_later_registry_runs_a_restored_task_whose_worker_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let scope = owner_scope("late-restore");
    let (daemon, mut clock, task_id, _json) = saved_due_draining_retry(dir.path(), &scope, 1).await;
    stopped_draining_roots()
        .lock()
        .unwrap()
        .insert(dir.path().into());
    let first = registry();
    first.replay_session(dir.path(), "session").unwrap();
    clock.expect_event(DrainingClockEvent::Stopped).await;
    wait_for("the first restored worker must stop", || {
        (remote_workers(&task_id) == (2, 2)).then_some(())
    })
    .await;
    assert!(!first.task(&task_id).unwrap().is_terminal());
    let later = registry();
    later.replay_session(dir.path(), "session").unwrap();
    let done = terminal(&later, &task_id).await;
    assert_eq!(done.exit_code, Some(0), "{done:?}");
    wait_for("the later registry's worker must stop", || {
        (remote_workers(&task_id) == (3, 3)).then_some(())
    })
    .await;
    assert_eq!(
        exec_runs(&daemon).len(),
        2,
        "the first, refused run and one resubmission after the restart"
    );
    assert!(first.task(&task_id).unwrap().is_terminal());
}

/// After a module restart, the owner pauses a worker whose saved remote job
/// the runner refused, which a restored worker runs locally instead. Covers
/// a refusal the worker learns when it reattaches and one saved before the
/// restart. Recovering that saved task for the pause must not start the
/// local run.
#[tokio::test]
async fn bash_kill_scope_never_starts_a_saved_tasks_local_fallback() {
    for saved_refusal in [false, true] {
        let daemon = daemon(Script::AttachRefused, "exec-remote/v1").await;
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("local-fallback-ran");
        let (original, task_id, paths) = persist_accepted_with_snapshot(
            dir.path(),
            daemon.connection.clone(),
            &format!("printf x >> '{}'", marker.display()),
        );
        let scope = owner_scope("saved-local-fallback");
        let mut saved = read_task(&paths.json).unwrap();
        saved.scope = Some(scope.clone());
        if saved_refusal {
            saved.remote.as_mut().unwrap().terminal = Some(TerminalRecord::new(
                id(),
                Outcome::RefusedBeforeStart {
                    reason: RefusalReason::Unknown("future_refusal".into()),
                },
                1,
                0,
                0,
            ));
        }
        write_task(&paths.json, &saved).unwrap();
        original.detach();
        let (local_task, local_pid) = start_saved_term_ignoring_task(dir.path(), &scope).await;
        let reply = kill_scope_with_saved_tasks(dir.path(), &scope).await;
        assert!(
            !marker.exists(),
            "saved_refusal={saved_refusal}: the paused task ran locally: {reply:?}"
        );
        assert_saved_pause_reply(&reply, &task_id, &local_task, local_pid).await;
        assert_saved_paused(&paths.json);
        assert!(!marker.exists(), "saved_refusal={saved_refusal}");
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
async fn runon_missing_workdir_is_refused_by_name_before_dispatch() {
    let daemon = daemon(Script::Plain, "exec-remote/v1").await;
    let project = tempfile::tempdir().unwrap();
    let ctx = remote_test_context(project.path());
    let missing_workdir = project.path().join("does-not-exist");
    let marker = project.path().join("must-not-run-locally");
    let response = handle_with_policy(
        &ctx,
        Some(launch(daemon.connection.clone())),
        serde_json::json!({
            "command": format!("printf local-proof > '{}'", marker.display()),
            "runon": "linux",
            "workdir": missing_workdir,
            "compressed": false,
        }),
    );
    assert!(response.success, "{response:?}");
    let task_id = response.data["task_id"].as_str().unwrap();
    let done = terminal(ctx.bash_background(), task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Failed, "{done:?}");
    let expected = format!(
        "runon refused: working directory does not exist: {}; command did not run (os error 2)",
        missing_workdir.display()
    );
    assert_eq!(
        done.info.status_reason.as_deref(),
        Some(expected.as_str()),
        "{done:?}"
    );
    assert_eq!(done.child_pid, None);
    assert_eq!(done.remote_job_id, None);
    let task = ctx.bash_background().task(task_id).unwrap();
    assert!(!task.state.lock().unwrap().metadata.local_fallback_started);
    assert!(!marker.exists(), "a refused runon must never spawn locally");
    assert!(
        exec_runs(&daemon).is_empty(),
        "a missing cwd must never dispatch"
    );
}

#[tokio::test]
async fn runon_missing_project_folder_is_refused_before_connecting() {
    let daemon = daemon(Script::Plain, "exec-remote/v1").await;
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let missing_root = project.path().join("does-not-exist");
    let marker = project.path().join("must-not-run-locally");
    let registry = registry();
    let mut policy = launch(daemon.connection.with_extension("missing"));
    policy.explicit_runon = true;
    let task_id = registry
        .spawn_remote(
            policy,
            SpawnPlan::Unsandboxed,
            &format!("printf local-proof > '{}'", marker.display()),
            PathBuf::from("/bin/sh"),
            "session".into(),
            project.path().to_path_buf(),
            HashMap::new(),
            crate::bash_background::HardKill::After(Duration::from_secs(30)),
            None,
            storage.path().to_path_buf(),
            crate::bash_background::TaskSlot::Foreground,
            false,
            false,
            Some(missing_root.clone()),
        )
        .unwrap();
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Failed, "{done:?}");
    let expected = format!(
        "runon refused: project folder does not exist: {}; command did not run (os error 2)",
        missing_root.display()
    );
    assert_eq!(
        done.info.status_reason.as_deref(),
        Some(expected.as_str()),
        "{done:?}"
    );
    assert_eq!(done.child_pid, None);
    assert_eq!(done.remote_job_id, None);
    let task = registry.task(&task_id).unwrap();
    assert!(!task.state.lock().unwrap().metadata.local_fallback_started);
    assert!(!marker.exists());
    assert!(exec_runs(&daemon).is_empty());
}

#[tokio::test]
async fn runon_transport_failure_still_reports_remote_unavailable() {
    let daemon = daemon(Script::Plain, "exec.remote/v1").await;
    let project = tempfile::tempdir().unwrap();
    let ctx = remote_test_context(project.path());
    let marker = project.path().join("must-not-run-locally");
    let response = handle_with_policy(
        &ctx,
        Some(launch(daemon.connection.clone())),
        serde_json::json!({
            "command": format!("printf local-proof > '{}'", marker.display()),
            "runon": "linux",
            "compressed": false,
        }),
    );
    assert!(response.success, "{response:?}");
    let task_id = response.data["task_id"].as_str().unwrap();
    let done = terminal(ctx.bash_background(), task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Failed, "{done:?}");
    assert!(
        done.info
            .status_reason
            .as_deref()
            .unwrap()
            .starts_with("runon refused: remote execution is unavailable; command did not run: remote execution route: "),
        "{done:?}"
    );
    let task = ctx.bash_background().task(task_id).unwrap();
    assert!(!task.state.lock().unwrap().metadata.local_fallback_started);
    assert!(!marker.exists(), "a refused runon must never spawn locally");
    assert!(exec_runs(&daemon).is_empty());
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
    assert!(
        worktree_root_containing(workdir.path(), exec::DirectoryKind::WorkingDirectory)
            .unwrap()
            .is_none()
    );
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
async fn runon_cd_refusal_has_distinct_code_and_no_execution() {
    let daemon = daemon(Script::Plain, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let checkout = dir.path().join("checkout");
    let other = dir.path().join("other");
    fs::create_dir_all(&checkout).unwrap();
    fs::create_dir_all(&other).unwrap();
    git_fixture(&other);
    let ctx = remote_test_context(&checkout);
    let marker = checkout.join("must-not-run-locally");
    let response = handle_with_policy(
        &ctx,
        Some(launch(daemon.connection.clone())),
        serde_json::json!({
            "command": format!("cd '{}'; printf local-proof > '{}'", other.display(), marker.display()),
            "runon": "linux", "compressed": false,
        }),
    );
    assert!(response.success, "{response:?}");
    let task_id = response.data["task_id"].as_str().unwrap();
    let done = terminal(ctx.bash_background(), task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Failed, "{done:?}");
    assert_eq!(
        done.exit_code, None,
        "refused before start, not a process exit"
    );
    assert_eq!(done.child_pid, None);
    let refusal = refusal_response(done);
    assert_eq!(
        refusal.data["code"], "runon_cd_outside_checkout",
        "{refusal:?}"
    );
    let expected = format!(
        "runon refused: the command changes into {}, which is outside the checkout the build server copies ({}); the command did not run. Set the bash tool's workdir to that folder instead of using cd.",
        other.canonicalize().unwrap().display(), checkout.canonicalize().unwrap().display()
    );
    assert_eq!(refusal.data["message"], expected);
    assert_eq!(
        crate::subc_format::format_response("bash", &refusal, false),
        expected
    );
    let task = ctx.bash_background().task(task_id).unwrap();
    let state = task.state.lock().unwrap();
    let persisted = serde_json::to_value(&state.metadata).unwrap();
    assert_eq!(
        persisted["remote"]["pre_start_refusal"]["code"],
        "runon_cd_outside_checkout"
    );
    assert_eq!(persisted["remote"]["pre_start_refusal"]["ran"], "none");
    assert!(persisted["remote"]["terminal"].is_null());
    assert!(persisted["remote"]["job_id"].is_null());
    let restored: PersistedTask = serde_json::from_value(persisted).unwrap();
    let restored_refusal = remote_refusal(&restored).unwrap();
    assert_eq!(restored_refusal.code, "runon_cd_outside_checkout");
    assert_eq!(restored_refusal.message, expected);
    assert!(!restored.local_fallback_started);
    let storage = crate::bash_background::task_storage_dir(&ctx);
    let persisted_path = task.paths.json.clone();
    drop(state);
    drop(task);
    let restarted = registry();
    assert!(restarted.task(task_id).is_none());
    assert!(persisted_path.exists());
    restarted.replay_session(&storage, "session").unwrap();
    let replayed = terminal(&restarted, task_id).await;
    let replayed_refusal = refusal_response(replayed);
    assert_eq!(replayed_refusal.data["code"], "runon_cd_outside_checkout");
    assert_eq!(replayed_refusal.data["message"], expected);
    assert!(exec_runs(&daemon).is_empty(), "a refusal must never submit");
    assert!(!marker.exists(), "a refusal must never spawn locally");
}

#[tokio::test]
async fn runon_cd_declared_siblings_are_allowed() {
    let dir = tempfile::tempdir().unwrap();
    let checkout = dir.path().join("checkout");
    let commons = dir.path().join("commons");
    fs::create_dir_all(&checkout).unwrap();
    fs::create_dir_all(commons.join("nested")).unwrap();
    git_fixture(&commons);
    let ctx = remote_test_context(&checkout);
    let commons = commons.canonicalize().unwrap();
    for line in [
        "cd ../commons && printf remote",
        "pushd ../commons/nested; printf remote",
    ] {
        let daemon = daemon(Script::Plain, "exec-remote/v1").await;
        let mut policy = launch(daemon.connection.clone());
        policy.params.siblings = vec![commons.to_str().unwrap().to_owned()];
        let response = handle_with_policy(
            &ctx,
            Some(policy),
            serde_json::json!({"command": line, "runon": "linux", "compressed": false}),
        );
        assert!(response.success, "{line}: {response:?}");
        let done = terminal(
            ctx.bash_background(),
            response.data["task_id"].as_str().unwrap(),
        )
        .await;
        assert_eq!(
            done.info.status,
            BgTaskStatus::Completed,
            "{line}: {done:?}"
        );
        let runs = exec_runs(&daemon);
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(runs[0]["command"], line);
        assert_eq!(runs[0]["siblings"], serde_json::json!([commons]));
    }
}

#[tokio::test]
async fn runon_cd_linked_worktree_sibling_alias_is_allowed() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main");
    let commons = dir.path().join("commons");
    git_fixture(&main);
    git_fixture(&commons);
    let worktree = dir.path().join("worktrees/worker");
    add_git_worktree(&main, &worktree, "cd-sibling-alias");
    let ctx = remote_test_context(&worktree);
    let worktree = worktree.canonicalize().unwrap();
    let commons = commons.canonicalize().unwrap();
    assert!(!worktree.parent().unwrap().join("commons").exists());
    for line in [
        "cd ../commons && printf remote",
        "cd ../commons/nested && printf remote",
    ] {
        let daemon = daemon(Script::Plain, "exec-remote/v1").await;
        let mut policy = launch(daemon.connection.clone());
        policy.params.siblings = vec![commons.to_str().unwrap().to_owned()];
        let response = handle_with_policy(
            &ctx,
            Some(policy),
            serde_json::json!({
                "command": line, "runon": "linux", "compressed": false,
            }),
        );
        assert!(response.success, "{response:?}");
        let done = terminal(
            ctx.bash_background(),
            response.data["task_id"].as_str().unwrap(),
        )
        .await;
        assert_eq!(
            done.info.status,
            BgTaskStatus::Completed,
            "{line}: {done:?}"
        );
        let runs = exec_runs(&daemon);
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(runs[0]["workspace_key"], worktree.to_str().unwrap());
        assert_eq!(runs[0]["siblings"], serde_json::json!([commons]));
        assert_eq!(runs[0]["command"], line);
    }
}

#[tokio::test]
async fn runon_cd_windows_absolute_targets_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let checkout = dir.path().join("checkout");
    let commons = dir.path().join("commons");
    git_fixture(&checkout);
    git_fixture(&commons);
    let ctx = remote_test_context(&checkout);
    let checkout = checkout.canonicalize().unwrap();
    let commons = commons.canonicalize().unwrap();
    let marker = checkout.join("must-not-run-locally");
    for target in [&checkout, &commons, &PathBuf::from("/tmp")] {
        let daemon = windows_daemon(Some(0), true).await;
        let mut policy = launch(daemon.connection.clone());
        policy.params.siblings = vec![commons.to_str().unwrap().to_owned()];
        let response = handle_with_policy(
            &ctx,
            Some(policy),
            serde_json::json!({
                "command": format!("cd '{}'; printf local-proof > '{}'", target.display(), marker.display()),
                "runon": "windows", "compressed": false,
            }),
        );
        assert!(response.success, "{response:?}");
        let done = terminal(
            ctx.bash_background(),
            response.data["task_id"].as_str().unwrap(),
        )
        .await;
        assert_eq!(
            done.info.status,
            BgTaskStatus::Failed,
            "{target:?}: {done:?}"
        );
        let refusal = refusal_response(done);
        assert_eq!(refusal.data["code"], "runon_cd_outside_checkout");
        let expected = format!(
            "runon refused: the command changes into {}, which is not valid on the Windows runner because absolute command paths are not translated (checkout {}); the command did not run. Set the bash tool's workdir to that folder instead of using cd.",
            target.canonicalize().unwrap().display(), checkout.display()
        );
        assert_eq!(refusal.data["message"], expected);
        assert!(exec_runs(&daemon).is_empty(), "a refusal must never submit");
        assert!(!marker.exists(), "a refusal must never spawn locally");
    }
}

#[tokio::test]
async fn runon_cd_windows_relative_targets_are_allowed() {
    let dir = tempfile::tempdir().unwrap();
    let checkout = dir.path().join("checkout");
    let commons = dir.path().join("commons");
    git_fixture(&checkout);
    git_fixture(&commons);
    fs::create_dir(checkout.join("nested")).unwrap();
    let ctx = remote_test_context(&checkout);
    let commons = commons.canonicalize().unwrap();
    for line in [
        "cd nested && printf remote",
        "cd ../commons && printf remote",
    ] {
        let daemon = windows_daemon(Some(0), true).await;
        let mut policy = launch(daemon.connection.clone());
        policy.params.siblings = vec![commons.to_str().unwrap().to_owned()];
        let response = handle_with_policy(
            &ctx,
            Some(policy),
            serde_json::json!({
                "command": line, "runon": "windows", "compressed": false,
            }),
        );
        assert!(response.success, "{response:?}");
        let done = terminal(
            ctx.bash_background(),
            response.data["task_id"].as_str().unwrap(),
        )
        .await;
        assert_eq!(
            done.info.status,
            BgTaskStatus::Completed,
            "{line}: {done:?}"
        );
        let runs = exec_runs(&daemon);
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(runs[0]["command"], line);
        assert_eq!(runs[0]["platform"], "windows");
    }
}

#[tokio::test]
async fn runon_cd_system_and_missing_non_home_paths_are_allowed() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = remote_test_context(dir.path());
    let outside = tempfile::Builder::new()
        .prefix("aft-runon-system-")
        .tempdir_in("/var/tmp")
        .unwrap();
    let outside = outside.path().canonicalize().unwrap();
    let home = PathBuf::from(std::env::var_os("HOME").unwrap())
        .canonicalize()
        .unwrap();
    assert!(!outside.starts_with(home));
    assert!(crate::root_identity::nearest_git_marker(&outside)
        .unwrap()
        .is_none());
    for target in [
        PathBuf::from("/tmp"),
        PathBuf::from("/usr"),
        outside.join("missing"),
    ] {
        let daemon = daemon(Script::Plain, "exec-remote/v1").await;
        let line = format!("cd '{}' && printf remote", target.display());
        let response = handle_with_policy(
            &ctx,
            Some(launch(daemon.connection.clone())),
            serde_json::json!({"command": line, "runon": "linux", "compressed": false}),
        );
        assert!(response.success, "{response:?}");
        let done = terminal(
            ctx.bash_background(),
            response.data["task_id"].as_str().unwrap(),
        )
        .await;
        assert_eq!(
            done.info.status,
            BgTaskStatus::Completed,
            "{target:?}: {done:?}"
        );
        let runs = exec_runs(&daemon);
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(runs[0]["command"], line);
    }
}

#[tokio::test]
async fn runon_cd_other_local_checkout_outside_home_is_refused() {
    let daemon = daemon(Script::Plain, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = remote_test_context(dir.path());
    let outside = tempfile::Builder::new()
        .prefix("aft-runon-other-repo-")
        .tempdir_in("/var/tmp")
        .unwrap();
    let outside = outside.path().canonicalize().unwrap();
    let home = PathBuf::from(std::env::var_os("HOME").unwrap())
        .canonicalize()
        .unwrap();
    assert!(!outside.starts_with(home));
    git_fixture(&outside);
    let marker = dir.path().join("must-not-run-locally");
    let response = handle_with_policy(
        &ctx,
        Some(launch(daemon.connection.clone())),
        serde_json::json!({
            "command": format!("cd '{}'; printf local-proof > '{}'", outside.join("missing").display(), marker.display()),
            "runon": "linux", "compressed": false,
        }),
    );
    assert!(response.success, "{response:?}");
    let done = terminal(
        ctx.bash_background(),
        response.data["task_id"].as_str().unwrap(),
    )
    .await;
    assert_eq!(done.info.status, BgTaskStatus::Failed, "{done:?}");
    let refusal = refusal_response(done);
    assert_eq!(refusal.data["code"], "runon_cd_outside_checkout");
    assert!(exec_runs(&daemon).is_empty(), "a refusal must never submit");
    assert!(!marker.exists(), "a refusal must never spawn locally");
}

#[tokio::test]
async fn runon_cd_outside_checkout_is_refused_before_submit() {
    let daemon = daemon(Script::Plain, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let checkout = dir.path().join("checkout");
    let sibling = dir.path().join("sibling-repo");
    fs::create_dir_all(&checkout).unwrap();
    fs::create_dir_all(&sibling).unwrap();
    git_fixture(&sibling);
    let ctx = remote_test_context(&checkout);
    let checkout = checkout.canonicalize().unwrap();
    let sibling = sibling.canonicalize().unwrap();
    let marker = checkout.join("must-not-run-locally");
    let home_target =
        PathBuf::from(std::env::var_os("HOME").unwrap()).join("aft-runon-outside-checkout");
    let cases = [
        (format!("cd '{}'", sibling.display()), sibling.clone()),
        ("cd ~/aft-runon-outside-checkout".into(), home_target),
        ("cd ../sibling-repo".into(), sibling.clone()),
        (
            "cd ../sibling-repo && printf remote; printf continuation".into(),
            sibling.clone(),
        ),
        (format!("pushd '{}'", sibling.display()), sibling),
    ];
    for (line, target) in cases {
        let command = format!("{line}; printf local-proof > '{}'", marker.display());
        let response = handle_with_policy(
            &ctx,
            Some(launch(daemon.connection.clone())),
            serde_json::json!({"command": command, "runon": "linux", "compressed": false}),
        );
        assert!(response.success, "{line}: {response:?}");
        let done = terminal(
            ctx.bash_background(),
            response.data["task_id"].as_str().unwrap(),
        )
        .await;
        assert_eq!(done.info.status, BgTaskStatus::Failed, "{line}: {done:?}");
        let refusal = refusal_response(done);
        assert_eq!(
            refusal.data["code"], "runon_cd_outside_checkout",
            "{refusal:?}"
        );
        let expected = format!(
            "runon refused: the command changes into {}, which is outside the checkout the build server copies ({}); the command did not run. Set the bash tool's workdir to that folder instead of using cd.",
            target.display(), checkout.display()
        );
        assert!(
            refusal.data["message"]
                .as_str()
                .unwrap()
                .contains(&expected),
            "{line}: {refusal:?}"
        );
        assert!(
            exec_runs(&daemon).is_empty(),
            "{line}: a refused command reached the runner"
        );
        assert!(!marker.exists(), "a refused runon must never spawn locally");
    }
}

#[tokio::test]
async fn runon_cd_unknown_set_keeps_known_folder_for_definite_exits() {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap())
        .canonicalize()
        .unwrap();
    let checkout = tempfile::Builder::new()
        .prefix("aft-runon-unknown-set-")
        .tempdir_in(&home)
        .unwrap();
    let ctx = remote_test_context(checkout.path());
    fs::create_dir(checkout.path().join("nested")).unwrap();
    let marker = checkout.path().join("must-not-run-locally");
    for line in [
        "set \"$OPTS\"; cd ..",
        "set -P; set \"$OPTS\"; cd ..",
        "set -o \"$OPT\"; cd ..",
        "set \"$OPTS\"; cd nested && cd .. && cd ..",
        "set \"$OPTS\" && cd ..",
        "set \"$OPTS\" || cd ..",
    ] {
        let daemon = daemon(Script::Plain, "exec-remote/v1").await;
        let response = handle_with_policy(
            &ctx,
            Some(launch(daemon.connection.clone())),
            serde_json::json!({
                "command": format!("{line}; printf local-proof > '{}'", marker.display()), "runon": "linux", "compressed": false,
            }),
        );
        assert!(response.success, "{line}: {response:?}");
        let done = terminal(
            ctx.bash_background(),
            response.data["task_id"].as_str().unwrap(),
        )
        .await;
        assert_eq!(done.info.status, BgTaskStatus::Failed, "{line}: {done:?}");
        let refusal = refusal_response(done);
        assert_eq!(refusal.data["code"], "runon_cd_outside_checkout");
        assert!(
            refusal.data["message"]
                .as_str()
                .unwrap()
                .contains(&format!("changes into {},", home.display())),
            "{line}: {refusal:?}"
        );
        assert!(exec_runs(&daemon).is_empty(), "a refusal must never submit");
        assert!(!marker.exists(), "runon must never spawn locally");
    }
}

#[tokio::test]
async fn runon_cd_unknown_mode_requires_both_resolutions_to_be_invalid() {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap())
        .canonicalize()
        .unwrap();
    let checkout = tempfile::Builder::new()
        .prefix("aft-runon-mode-pair-")
        .tempdir_in(&home)
        .unwrap();
    let outside = tempfile::Builder::new()
        .prefix("aft-runon-mode-target-")
        .tempdir_in(&home)
        .unwrap();
    let deep = checkout.path().join("deep");
    fs::create_dir(&deep).unwrap();
    fs::create_dir_all(outside.path().join("a/b/c")).unwrap();
    fs::create_dir_all(deep.join("a/b/c")).unwrap();
    std::os::unix::fs::symlink("..", deep.join("link")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("a/b/c"), deep.join("jump")).unwrap();
    std::os::unix::fs::symlink("a/b/c", deep.join("reenter")).unwrap();
    let ctx = remote_test_context(checkout.path());
    for (line, refused) in [
        ("set \"$OPTS\"; cd link/..", false),
        ("set \"$OPTS\"; cd link/.. && cd ../..", false),
        ("set \"$OPTS\"; cd reenter/../../..", false),
        ("set \"$OPTS\"; cd reenter/../../.. && cd ../../..", false),
        ("set \"$OPTS\"; cd jump/../../..", true),
        ("set \"$OPTS\"; cd -P link/..", true),
        ("set \"$OPTS\"; cd -L link/..", false),
    ] {
        let daemon = daemon(Script::Plain, "exec-remote/v1").await;
        let response = handle_with_policy(
            &ctx,
            Some(launch(daemon.connection.clone())),
            serde_json::json!({
                "command": line, "runon": "linux", "workdir": deep, "compressed": false,
            }),
        );
        assert!(response.success, "{line}: {response:?}");
        let done = terminal(
            ctx.bash_background(),
            response.data["task_id"].as_str().unwrap(),
        )
        .await;
        if refused {
            assert_eq!(done.info.status, BgTaskStatus::Failed, "{line}: {done:?}");
            assert_eq!(
                refusal_response(done).data["code"],
                "runon_cd_outside_checkout"
            );
            assert!(exec_runs(&daemon).is_empty(), "a refusal must never submit");
        } else {
            assert_eq!(
                done.info.status,
                BgTaskStatus::Completed,
                "{line}: {done:?}"
            );
            let runs = exec_runs(&daemon);
            assert_eq!(runs.len(), 1, "{line}: {runs:?}");
            assert_eq!(runs[0]["command"], line);
        }
    }
}

#[tokio::test]
async fn runon_cd_background_jobs_preserve_foreground_directory() {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap())
        .canonicalize()
        .unwrap();
    let checkout = tempfile::Builder::new()
        .prefix("aft-runon-background-")
        .tempdir_in(&home)
        .unwrap();
    let ctx = remote_test_context(checkout.path());
    fs::create_dir(checkout.path().join("nested")).unwrap();
    fs::write(checkout.path().join("nested/source.txt"), "source\n").unwrap();
    let marker = checkout.path().join("must-not-run-locally");
    for (line, refused) in [
        ("sleep 1 & cd ..", true),
        ("cd nested & cd ..", true),
        ("cd nested && sleep 1 & cd ..", true),
        ("false && cd nested & cd ..", true),
        ("source ./script & cd ..", true),
        ("set \"$OPTS\" & cd ..", true),
        ("set -- \"$OPTS\"; sleep 1 & cd ..", true),
        ("sleep 1 & cd nested && cd ..", false),
        ("cd nested && { sleep 1 & cd ..; }", false),
        ("false & cd ..", true),
        ("sleep 1 & false && cd ..", false),
        ("cd nested; sleep 1 & cd ..", false),
    ] {
        let daemon = daemon(Script::Plain, "exec-remote/v1").await;
        let command = format!("{line}; printf local-proof > '{}'", marker.display());
        let response = handle_with_policy(
            &ctx,
            Some(launch(daemon.connection.clone())),
            serde_json::json!({
                "command": command, "runon": "linux", "compressed": false,
            }),
        );
        assert!(response.success, "{line}: {response:?}");
        let done = terminal(
            ctx.bash_background(),
            response.data["task_id"].as_str().unwrap(),
        )
        .await;
        if refused {
            assert_eq!(done.info.status, BgTaskStatus::Failed, "{line}: {done:?}");
            let refusal = refusal_response(done);
            assert_eq!(refusal.data["code"], "runon_cd_outside_checkout");
            assert!(exec_runs(&daemon).is_empty(), "a refusal must never submit");
        } else {
            assert_eq!(
                done.info.status,
                BgTaskStatus::Completed,
                "{line}: {done:?}"
            );
            let runs = exec_runs(&daemon);
            assert_eq!(runs.len(), 1, "{line}: {runs:?}");
            assert_eq!(runs[0]["command"], command);
        }
        assert!(!marker.exists(), "runon must never spawn locally");
    }
}

#[tokio::test]
async fn runon_cd_shell_physical_option_tracks_later_changes() {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap())
        .canonicalize()
        .unwrap();
    let checkout = tempfile::Builder::new()
        .prefix("aft-runon-set-mode-")
        .tempdir_in(&home)
        .unwrap();
    let deep = checkout.path().join("deep");
    fs::create_dir(&deep).unwrap();
    std::os::unix::fs::symlink("..", deep.join("link")).unwrap();
    let ctx = remote_test_context(checkout.path());
    let marker = checkout.path().join("must-not-run-locally");
    for (line, refused) in [
        ("set -P; cd link && cd ..", true),
        ("set -o physical; cd link && cd ..", true),
        ("set -P; set +P; cd link && cd ..", false),
        ("set -o physical; set +o physical; cd link && cd ..", false),
        ("set -P; cd -L link && cd ..", true),
        ("set -P; cd -L link && cd -L ..", false),
        ("set +P; cd -P link && cd ..", true),
        ("set -- -P; cd link && cd ..", false),
        ("set value -P; cd link && cd ..", false),
        ("set -eP; cd link && cd ..", true),
        ("set -P +P; cd link && cd ..", false),
        ("set +P -P; cd link && cd ..", true),
        ("false && set -P; cd link && cd ..", false),
        ("true || set -P; cd link && cd ..", false),
        ("maybe && set -P; cd link && cd ..", true),
        ("false || set -P; cd link && cd ..", true),
        ("set -P & cd link && cd ..", false),
        ("set -P; set +P & cd link && cd ..", true),
        ("source ./script; cd link && cd ..", false),
        ("set \"$OPTS\"; cd link && cd ..", false),
        ("set -P; set \"$OPTS\"; cd link && cd ..", false),
    ] {
        let daemon = daemon(Script::Plain, "exec-remote/v1").await;
        let command = format!("{line}; printf local-proof > '{}'", marker.display());
        let response = handle_with_policy(
            &ctx,
            Some(launch(daemon.connection.clone())),
            serde_json::json!({
                "command": command, "runon": "linux", "workdir": deep, "compressed": false,
            }),
        );
        assert!(response.success, "{line}: {response:?}");
        let done = terminal(
            ctx.bash_background(),
            response.data["task_id"].as_str().unwrap(),
        )
        .await;
        if refused {
            assert_eq!(done.info.status, BgTaskStatus::Failed, "{line}: {done:?}");
            let refusal = refusal_response(done);
            assert_eq!(refusal.data["code"], "runon_cd_outside_checkout");
            assert!(exec_runs(&daemon).is_empty(), "a refusal must never submit");
        } else {
            assert_eq!(
                done.info.status,
                BgTaskStatus::Completed,
                "{line}: {done:?}"
            );
            let runs = exec_runs(&daemon);
            assert_eq!(runs.len(), 1, "{line}: {runs:?}");
            assert_eq!(runs[0]["command"], command);
        }
        assert!(!marker.exists(), "runon must never spawn locally");
    }
}

#[tokio::test]
async fn runon_cd_logical_symlinks_and_physical_mode_are_distinct() {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap())
        .canonicalize()
        .unwrap();
    let checkout = tempfile::Builder::new()
        .prefix("aft-runon-logical-")
        .tempdir_in(&home)
        .unwrap();
    let deep = checkout.path().join("deep");
    fs::create_dir(&deep).unwrap();
    std::os::unix::fs::symlink("..", deep.join("link")).unwrap();
    let ctx = remote_test_context(checkout.path());
    let marker = checkout.path().join("must-not-run-locally");
    for (line, refused) in [
        ("cd link && cd ..", false),
        ("pushd link && cd ..", false),
        ("cd link/..", false),
        ("cd -L link && cd ..", false),
        ("cd -P link && cd ..", true),
        ("cd -P link/..", true),
        ("cd -P -L link && cd ..", false),
        ("cd -L -P link && cd ..", true),
        ("cd -PL link && cd ..", false),
        ("cd -LP link && cd ..", true),
        ("cd '-P' link && cd ..", true),
        ("cd -- link && cd ..", false),
        ("maybe && cd link; cd ..", false),
        ("cd link || cd .; cd ..", false),
        ("cd -P link || cd .; cd ..", true),
    ] {
        let daemon = daemon(Script::Plain, "exec-remote/v1").await;
        let command = format!("{line}; printf local-proof > '{}'", marker.display());
        let response = handle_with_policy(
            &ctx,
            Some(launch(daemon.connection.clone())),
            serde_json::json!({
                "command": command, "runon": "linux", "workdir": deep, "compressed": false,
            }),
        );
        assert!(response.success, "{line}: {response:?}");
        let done = terminal(
            ctx.bash_background(),
            response.data["task_id"].as_str().unwrap(),
        )
        .await;
        if refused {
            assert_eq!(done.info.status, BgTaskStatus::Failed, "{line}: {done:?}");
            let refusal = refusal_response(done);
            assert_eq!(refusal.data["code"], "runon_cd_outside_checkout");
            assert!(
                refusal.data["message"]
                    .as_str()
                    .unwrap()
                    .contains(&format!("changes into {},", home.display())),
                "{line}: {refusal:?}"
            );
            assert!(exec_runs(&daemon).is_empty(), "a refusal must never submit");
        } else {
            assert_eq!(
                done.info.status,
                BgTaskStatus::Completed,
                "{line}: {done:?}"
            );
            let runs = exec_runs(&daemon);
            assert_eq!(runs.len(), 1, "{line}: {runs:?}");
            assert_eq!(runs[0]["command"], command);
        }
        assert!(!marker.exists(), "runon must never spawn locally");
    }
}

#[tokio::test]
async fn runon_cd_skipped_changes_do_not_hide_checkout_exits() {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap())
        .canonicalize()
        .unwrap();
    let checkout = tempfile::Builder::new()
        .prefix("aft-runon-branches-")
        .tempdir_in(&home)
        .unwrap();
    let ctx = remote_test_context(checkout.path());
    fs::create_dir(checkout.path().join("nested")).unwrap();
    fs::write(checkout.path().join("nested/source.txt"), "source\n").unwrap();
    let marker = checkout.path().join("must-not-run-locally");
    for (line, refused) in [
        ("false && cd nested; cd ..", true),
        ("maybe && cd nested; cd ..", true),
        ("cd nested; cd ..", false),
        ("true || cd nested; cd ..", true),
        ("cd \"$X\" || cd ..", true),
        ("cd nested && cd ..", false),
        ("false || cd nested && cd ..", false),
        ("false && cd nested || cd nested && cd ..", false),
        ("false && cd ../must-not-execute", false),
        ("true || cd ..", false),
        ("cd \"$X\" && cd ..", false),
    ] {
        let daemon = daemon(Script::Plain, "exec-remote/v1").await;
        let command = format!("{line}; printf local-proof > '{}'", marker.display());
        let response = handle_with_policy(
            &ctx,
            Some(launch(daemon.connection.clone())),
            serde_json::json!({
                "command": command, "runon": "linux", "compressed": false,
            }),
        );
        assert!(response.success, "{line}: {response:?}");
        let done = terminal(
            ctx.bash_background(),
            response.data["task_id"].as_str().unwrap(),
        )
        .await;
        if refused {
            assert_eq!(done.info.status, BgTaskStatus::Failed, "{line}: {done:?}");
            let refusal = refusal_response(done);
            assert_eq!(refusal.data["code"], "runon_cd_outside_checkout");
            assert!(
                refusal.data["message"]
                    .as_str()
                    .unwrap()
                    .contains(&format!("changes into {},", home.display())),
                "{line}: {refusal:?}"
            );
            assert!(exec_runs(&daemon).is_empty(), "a refusal must never submit");
        } else {
            assert_eq!(
                done.info.status,
                BgTaskStatus::Completed,
                "{line}: {done:?}"
            );
            let runs = exec_runs(&daemon);
            assert_eq!(runs.len(), 1, "{line}: {runs:?}");
            assert_eq!(runs[0]["command"], command);
        }
        assert!(!marker.exists(), "runon must never spawn locally");
    }
}

#[tokio::test]
async fn runon_cd_sequences_use_existing_copied_directories() {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap())
        .canonicalize()
        .unwrap();
    let dir = tempfile::Builder::new()
        .prefix("aft-runon-copied-cwd-")
        .tempdir_in(&home)
        .unwrap();
    let checkout = dir.path().join("checkout");
    let sibling = dir.path().join("commons");
    for folder in [
        checkout.join("packages/a"),
        checkout.join("packages/b"),
        sibling.join("a"),
        sibling.join("b"),
    ] {
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("source.txt"), "source\n").unwrap();
    }
    git_fixture(&sibling);
    let ctx = remote_test_context(&checkout);
    let marker = checkout.join("must-not-run-locally");
    for (line, refused) in [
        ("cd packages/a && true; cd ../b", false),
        ("pushd packages/a && true; cd ../b", true),
        ("cd ../commons/a && true; cd ../b", false),
        ("cd packages/missing && true; cd ../b", true),
        ("cd ../../outside", true),
        ("cd missing || cd ../../outside", true),
    ] {
        let daemon = daemon(Script::Plain, "exec-remote/v1").await;
        let mut policy = launch(daemon.connection.clone());
        policy.params.siblings = vec![sibling.to_str().unwrap().to_owned()];
        let command = format!("{line}; printf local-proof > '{}'", marker.display());
        let response = handle_with_policy(
            &ctx,
            Some(policy),
            serde_json::json!({"command": command, "runon": "linux", "compressed": false}),
        );
        assert!(response.success, "{line}: {response:?}");
        let done = terminal(
            ctx.bash_background(),
            response.data["task_id"].as_str().unwrap(),
        )
        .await;
        if refused {
            assert_eq!(done.info.status, BgTaskStatus::Failed, "{line}: {done:?}");
            let refusal = refusal_response(done);
            assert_eq!(refusal.data["code"], "runon_cd_outside_checkout");
            assert!(exec_runs(&daemon).is_empty(), "a refusal must never submit");
        } else {
            assert_eq!(
                done.info.status,
                BgTaskStatus::Completed,
                "{line}: {done:?}"
            );
            let runs = exec_runs(&daemon);
            assert_eq!(runs.len(), 1, "{line}: {runs:?}");
            assert_eq!(runs[0]["command"], command);
        }
        assert!(!marker.exists(), "runon must never spawn locally");
    }
}

fn runon_cd_snapshot_fixture() -> (tempfile::TempDir, RunRequest) {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap())
        .canonicalize()
        .unwrap();
    let dir = tempfile::Builder::new()
        .prefix("aft-runon-snapshot-cwd-")
        .tempdir_in(home)
        .unwrap();
    let checkout = dir.path().join("workspace/checkout");
    git_fixture(&checkout);
    let request = build_remote_request(
        &checkout,
        &checkout,
        "true",
        BTreeMap::new(),
        30,
        &exec::PresetParams::default(),
    )
    .unwrap();
    (dir, request)
}

#[test]
fn runon_cd_snapshot_ignored_existing_directories_keep_failure_branch() {
    let (_dir, mut request) = runon_cd_snapshot_fixture();
    let checkout = PathBuf::from(&request.workspace_key);
    fs::write(checkout.join(".gitignore"), "node_modules/\ndeleted/\n").unwrap();
    fs::write(checkout.join(".git/info/exclude"), "excluded/\n").unwrap();
    let global = checkout.join("global-ignore");
    fs::write(&global, "globalignored/\n").unwrap();
    hermetic_git(
        &checkout,
        &["config", "core.excludesFile", global.to_str().unwrap()],
    );
    fs::create_dir_all(checkout.join("nested")).unwrap();
    fs::write(checkout.join("nested/.gitignore"), "cache/\n").unwrap();
    for folder in [
        "node_modules/x",
        "excluded/x",
        "globalignored/x",
        "nested/cache/x",
        "deleted/x",
    ] {
        fs::create_dir_all(checkout.join(folder)).unwrap();
        if folder != "deleted/x" {
            fs::write(checkout.join(folder).join("source.txt"), "not copied\n").unwrap();
        }
    }
    fs::write(checkout.join("deleted/x/tracked.txt"), "deleted\n").unwrap();
    hermetic_git(&checkout, &["add", "-f", "deleted/x/tracked.txt"]);
    fs::remove_file(checkout.join("deleted/x/tracked.txt")).unwrap();
    let sibling = checkout.parent().unwrap().join("sibling");
    git_fixture(&sibling);
    fs::write(sibling.join(".gitignore"), "node_modules/\n").unwrap();
    fs::create_dir_all(sibling.join("node_modules/x")).unwrap();
    fs::write(sibling.join("node_modules/x/source.txt"), "not copied\n").unwrap();
    request.siblings = vec![sibling.to_str().unwrap().to_owned()];
    for line in [
        "cd node_modules/x; cd ../..",
        "cd excluded/x; cd ../..",
        "cd globalignored/x; cd ../..",
        "cd nested/cache/x; cd ../../..",
        "cd deleted/x; cd ../..",
        "cd ../sibling/node_modules/x; cd ../../../checkout",
    ] {
        assert!(
            runon_cd_refusal(line, &request).unwrap().is_some(),
            "{line}"
        );
    }
}

#[test]
fn runon_cd_snapshot_directories_without_copied_files_keep_failure_branch() {
    let (_dir, request) = runon_cd_snapshot_fixture();
    let checkout = Path::new(&request.workspace_key);
    fs::write(checkout.join(".gitignore"), "*.cache\n").unwrap();
    fs::create_dir_all(checkout.join("empty/x")).unwrap();
    fs::create_dir_all(checkout.join("onlyignored/x")).unwrap();
    fs::write(checkout.join("onlyignored/x/file.cache"), "not copied\n").unwrap();
    for line in ["cd empty/x; cd ../..", "cd onlyignored/x; cd ../.."] {
        assert!(
            runon_cd_refusal(line, &request).unwrap().is_some(),
            "{line}"
        );
    }
}

#[test]
fn runon_cd_snapshot_nonignored_and_tracked_directories_are_known() {
    let (_dir, mut request) = runon_cd_snapshot_fixture();
    let checkout = PathBuf::from(&request.workspace_key);
    fs::write(checkout.join(".gitignore"), "forced/\n").unwrap();
    for folder in ["tracked/x", "untracked/x", "forced/x", "src/target"] {
        fs::create_dir_all(checkout.join(folder)).unwrap();
        fs::write(checkout.join(folder).join("file.txt"), "source\n").unwrap();
    }
    hermetic_git(&checkout, &["add", "tracked/x/file.txt"]);
    hermetic_git(&checkout, &["add", "-f", "forced/x/file.txt"]);
    let sibling = checkout.parent().unwrap().join("sibling");
    git_fixture(&sibling);
    fs::write(sibling.join(".gitignore"), "forced/\n").unwrap();
    fs::create_dir_all(sibling.join("forced/x")).unwrap();
    fs::write(sibling.join("forced/x/file.txt"), "source\n").unwrap();
    hermetic_git(&sibling, &["add", "-f", "forced/x/file.txt"]);
    request.siblings = vec![sibling.to_str().unwrap().to_owned()];
    for line in [
        "cd tracked/x; cd ../..",
        "cd untracked/x; cd ../..",
        "cd forced/x; cd ../..",
        "cd src/target; cd ../..",
        "cd ../sibling/forced/x; cd ../../../checkout",
    ] {
        assert_eq!(runon_cd_refusal(line, &request).unwrap(), None, "{line}");
    }
}

#[test]
fn runon_cd_snapshot_mc_package_sequence_is_allowed() {
    let (_dir, request) = runon_cd_snapshot_fixture();
    let checkout = Path::new(&request.workspace_key);
    for package in ["pi-plugin", "plugin"] {
        let path = checkout.join("packages").join(package);
        fs::create_dir_all(&path).unwrap();
        for index in 0..200 {
            fs::write(path.join(format!("source-{index:03}.ts")), "source\n").unwrap();
        }
    }
    let line =
        "cd packages/pi-plugin && bun test src/__tests__; cd ../plugin && bun test src/__tests__";
    assert_eq!(runon_cd_refusal(line, &request).unwrap(), None);
}

#[test]
fn runon_cd_snapshot_missing_git_keeps_failure_branch() {
    let (_dir, request) = runon_cd_snapshot_fixture();
    let checkout = Path::new(&request.workspace_key);
    fs::create_dir_all(checkout.join("source/x")).unwrap();
    fs::write(checkout.join("source/x/source.txt"), "source\n").unwrap();
    assert_eq!(
        runon_cd_refusal("cd source/x; cd ../..", &request).unwrap(),
        None
    );
    assert!(runon_cd_refusal_with_git(
        "cd source/x; cd ../..",
        &request,
        &checkout.join("missing-git"),
    )
    .unwrap()
    .is_some());
}

#[test]
fn runon_cd_snapshot_rule_invalid_cd_keeps_failure_branch() {
    let (_dir, request) = runon_cd_snapshot_fixture();
    let checkout = Path::new(&request.workspace_key);
    fs::create_dir(checkout.join("a")).unwrap();
    fs::write(checkout.join("a/source.txt"), "source\n").unwrap();
    for line in [
        "cd a extra || cd ..",
        "cd -Z a || cd ..",
        "cd -PZ a || cd ..",
        "cd -- a extra || cd ..",
    ] {
        assert!(
            runon_cd_refusal(line, &request).unwrap().is_some(),
            "{line}"
        );
    }
}

#[test]
fn runon_cd_snapshot_rule_prior_command_keeps_failure_branch() {
    let (_dir, request) = runon_cd_snapshot_fixture();
    let checkout = Path::new(&request.workspace_key);
    fs::create_dir(checkout.join("a")).unwrap();
    fs::write(checkout.join("a/source.txt"), "source\n").unwrap();
    for line in [
        "mv a moved; cd a || cd ..",
        "rm -rf a; cd a || cd ..",
        "git clean -fd; cd a || cd ..",
        "./script; cd a || cd ..",
        "true; cd a || cd ..",
        "set +P; cd a || cd ..",
        "pushd .; cd a || cd ..",
        "rm -rf a & cd a || cd ..",
        "cd \"$(rm -rf a)\" || cd a || cd ..",
    ] {
        assert!(
            runon_cd_refusal(line, &request).unwrap().is_some(),
            "{line}"
        );
    }
}

#[test]
fn runon_cd_snapshot_rule_shared_deadline_bounds_git_lookups() {
    let (_dir, request) = runon_cd_snapshot_fixture();
    let checkout = Path::new(&request.workspace_key);
    let counter = checkout.join("git-spawns");
    let git = checkout.join("fake-git");
    crate::executable_cache::write_executable(
        &git,
        format!(
            "#!/bin/sh\nprintf 'spawn\\n' >> '{}'\nsleep 0.4\nfor target; do :; done\nprintf '%s/source.txt\\000' \"$target\"\n",
            counter.display()
        )
        .as_bytes(),
    )
    .unwrap();
    let mut line = String::from("cd d0");
    for index in 0..8 {
        let folder = checkout.join(format!("d{index}"));
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("source.txt"), "source\n").unwrap();
        if index > 0 {
            line.push_str(&format!("; cd ../d{index}"));
        }
    }
    let start = Instant::now();
    assert_eq!(
        runon_cd_refusal_with_git(&line, &request, &git).unwrap(),
        None
    );
    let elapsed = start.elapsed();
    let spawns = fs::read_to_string(counter).unwrap().lines().count();
    assert!((2..=6).contains(&spawns), "spawned Git {spawns} times");
    assert!(
        elapsed < Duration::from_millis(2500),
        "elapsed: {elapsed:?}"
    );
}

#[test]
fn runon_cd_snapshot_top_level_target_is_not_source() {
    let (_dir, mut request) = runon_cd_snapshot_fixture();
    let checkout = PathBuf::from(&request.workspace_key);
    let sibling = checkout.parent().unwrap().join("sibling");
    git_fixture(&sibling);
    for root in [&checkout, &sibling] {
        fs::create_dir_all(root.join("target/x")).unwrap();
        fs::write(root.join("target/x/tracked.txt"), "not copied\n").unwrap();
        hermetic_git(root, &["add", "-f", "target/x/tracked.txt"]);
    }
    request.siblings = vec![sibling.to_str().unwrap().to_owned()];
    for line in [
        "cd target/x; cd ../..",
        "cd ../sibling/target/x; cd ../../../checkout",
    ] {
        assert!(
            runon_cd_refusal(line, &request).unwrap().is_some(),
            "{line}"
        );
    }
}

#[tokio::test]
async fn runon_cd_sequential_targets_follow_the_shell_cwd() {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap())
        .canonicalize()
        .unwrap();
    let checkout = tempfile::Builder::new()
        .prefix("aft-runon-cwd-")
        .tempdir_in(&home)
        .unwrap();
    let ctx = remote_test_context(checkout.path());
    fs::create_dir(checkout.path().join("nested")).unwrap();
    let marker = checkout.path().join("must-not-run-locally");
    for (line, refused) in [
        ("cd nested && cd .. && printf remote", false),
        ("pushd nested && cd .. && printf remote", false),
        ("cd nested && cd ../.. && printf remote", true),
        ("cd \"$X\" && cd .. && printf remote", false),
        ("cd - && cd .. && printf remote", false),
        ("popd && cd .. && printf remote", false),
        ("(cd nested) && cd .. && printf remote", false),
    ] {
        let daemon = daemon(Script::Plain, "exec-remote/v1").await;
        let command = format!("{line}; printf local-proof > '{}'", marker.display());
        let response = handle_with_policy(
            &ctx,
            Some(launch(daemon.connection.clone())),
            serde_json::json!({"command": command, "runon": "linux", "compressed": false}),
        );
        assert!(response.success, "{line}: {response:?}");
        let done = terminal(
            ctx.bash_background(),
            response.data["task_id"].as_str().unwrap(),
        )
        .await;
        if refused {
            assert_eq!(done.info.status, BgTaskStatus::Failed, "{line}: {done:?}");
            let refusal = refusal_response(done);
            assert_eq!(refusal.data["code"], "runon_cd_outside_checkout");
            let expected = format!(
                "runon refused: the command changes into {}, which is outside the checkout the build server copies ({}); the command did not run. Set the bash tool's workdir to that folder instead of using cd.",
                home.display(), checkout.path().canonicalize().unwrap().display()
            );
            assert_eq!(refusal.data["message"], expected);
            assert!(exec_runs(&daemon).is_empty(), "a refusal must never submit");
        } else {
            assert_eq!(
                done.info.status,
                BgTaskStatus::Completed,
                "{line}: {done:?}"
            );
            let runs = exec_runs(&daemon);
            assert_eq!(runs.len(), 1, "{line}: {runs:?}");
            assert_eq!(runs[0]["command"], command);
        }
        assert!(!marker.exists(), "runon must never spawn locally");
    }
}

#[tokio::test]
async fn runon_cd_inside_checkout_and_unresolved_or_nested_targets_are_allowed() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = remote_test_context(dir.path());
    fs::create_dir(dir.path().join("nested")).unwrap();
    let lines = [
        "cd nested && printf remote",
        "cd ./nested/..; printf remote",
        "cd \"$OTHER_CHECKOUT\"; printf remote",
        "cd -; printf remote",
        "printf '%s' \"$(cd /outside-checkout; pwd)\"",
        "(cd /outside-checkout; printf remote)",
    ];
    for line in lines {
        let daemon = daemon(Script::Plain, "exec-remote/v1").await;
        let response = handle_with_policy(
            &ctx,
            Some(launch(daemon.connection.clone())),
            serde_json::json!({"command": line, "runon": "linux", "compressed": false}),
        );
        assert!(response.success, "{line}: {response:?}");
        let done = terminal(
            ctx.bash_background(),
            response.data["task_id"].as_str().unwrap(),
        )
        .await;
        assert_eq!(
            done.info.status,
            BgTaskStatus::Completed,
            "{line}: {done:?}"
        );
        let runs = exec_runs(&daemon);
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(runs[0]["command"], line);
    }
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
    remote_local_fallback_has_no_broker(crate::config::SandboxMode::Strict, false).await;
}

/// Relaxed macOS workers receive the broker on local launches; a remote launch
/// that falls back to local execution must still get none, even when a broker
/// service is available.
#[tokio::test]
async fn optional_worker_broker_relaxed_remote_local_fallback_with_service_has_no_broker() {
    let _state = crate::test_env::gh_shim_state_guard();
    remote_local_fallback_has_no_broker(crate::config::SandboxMode::Relaxed, true).await;
}

async fn remote_local_fallback_has_no_broker(
    mode: crate::config::SandboxMode,
    service_available: bool,
) {
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
    profile.mode = mode;
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
    ctx.update_config(|config| config.sandbox.profile.worker = mode);
    let command="test -z \"${AFT_GH_READ_SOCKET+x}\" && test -z \"${AFT_GH_READ_CAPABILITY_FILE+x}\" && test -z \"${AFT_GH_SHIM_TICKET+x}\" && printf FALLBACK_WITHOUT_BROKER";
    let launch_once = || {
        crate::logging::capture_log_lines(|| {
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
        })
    };
    let (response, logs) = if service_available {
        crate::gh_shim_worker_broker::with_available_service_for_test(project.path(), launch_once)
    } else {
        crate::gh_shim_worker_broker::without_service_for_test(launch_once)
    };
    assert!(response.success, "{response:?}");
    assert!(
        logs.iter().any(|line| line.starts_with("WARN ")
            && line.contains("gh_shim worker_read unavailable")
            && line.contains("reason=remote launch or local fallback has no host broker")),
        "{logs:?}"
    );
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
    clock.expect_sleep(first, first + 5000).await;
    let waiting = registry.observed_status(&task, "session", 8192).unwrap();
    assert!(!waiting.info.status.is_terminal(), "{waiting:?}");
    assert!(waiting.remote_refusal.is_none(), "{waiting:?}");
    assert_eq!(
        registry.runon_launch_pending(&task, "session"),
        Some(true),
        "a foreground launch keeps waiting while the retry is pending"
    );
    assert_eq!(exec_runs(&daemon).len(), 1);
    clock.advance(5000);
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
    // Room for one floored 5 s wait, counted from the first submission; the
    // second refusal's delay is longer than the 1 s then left.
    shorten_hinted_retry_budget(dir.path(), 6000);
    let mut clock = PausedDrainingClock::new(dir.path());
    let registry = registry();
    let task = start_runon_retry(&registry, dir.path(), daemon.connection.clone());
    let first = clock.now();
    clock.expect_sleep(first, first + 5000).await;
    clock.advance(5000);
    let done = terminal(&registry, &task).await;
    assert_eq!(done.info.status, BgTaskStatus::Failed, "{done:?}");
    assert_runon_refusal(&refusal_response(done), SIGNER_REFUSED);
    assert_eq!(exec_runs(&daemon).len(), 2);
    assert_eq!(clock.now(), first + 5000);
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
        let from = first + resubmission * 5000;
        clock.expect_sleep(from, from + 5000).await;
        clock.advance(5000);
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
    clock.expect_sleep(first, first + 5000).await;
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
    clock.advance(5000);
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
            draining_after_ms: 10_000,
            signer_after_ms: 100,
        },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    shorten_hinted_retry_budget(dir.path(), 6000);
    let mut clock = PausedDrainingClock::new(dir.path());
    let registry = registry();
    let task = start_runon_retry(&registry, dir.path(), daemon.connection.clone());
    let first = clock.now();
    clock.expect_sleep(first, first + 1000).await;
    clock.advance(10_000);
    clock.expect_sleep(first + 10_000, first + 15_000).await;
    assert_eq!(exec_runs(&daemon).len(), 2);
    clock.advance(5000);
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
            draining_after_ms: 10_000,
            signer_after_ms: 100,
        },
        "exec-remote/v1",
        2,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    shorten_hinted_retry_budget(dir.path(), 6000);
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
    clock.expect_sleep(first, first + 1000).await;
    clock.advance(10_000);
    clock.expect_sleep(first + 10_000, first + 15_000).await;
    assert_eq!(exec_runs(&daemon).len(), 2);
    clock.advance(5000);
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
    shorten_hinted_retry_budget(dir.path(), 6000);
    let mut clock = PausedDrainingClock::new(dir.path());
    let registry = registry();
    let task = start_runon_retry(&registry, dir.path(), daemon.connection.clone());
    let first = clock.now();
    clock.expect_sleep(first, first + 5000).await;
    // The worker wakes 10 s after the first submission, 4 s past its deadline.
    clock.advance(10_000);
    let done = terminal(&registry, &task).await;
    assert_eq!(done.info.status, BgTaskStatus::Failed, "{done:?}");
    assert_runon_refusal(&refusal_response(done), SIGNER_REFUSED);
    assert_eq!(exec_runs(&daemon).len(), 1, "no request after the deadline");
    assert_no_retry_sleep(&mut clock);
}

/// A zero hint on a job whose admission time is already used up sends
/// nothing either: the minimum retry delay exceeds the admission time left.
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
            None,
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

fn signer_retry_script(refusals: u32) -> Script {
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
fn assert_signer_retry_pause_saved(json: &Path) {
    assert_refused_pause_saved(json, "runner_sign_unavailable");
}

/// The saved record of a task the owner paused after the runner refused it
/// with `detail`: paused, the cancellation recorded as sent, no sign that the
/// command started, and the runner's refusal (`ran: none`) kept as the proof
/// that it never did.
fn assert_refused_pause_saved(json: &Path, detail: &str) {
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

/// The owner pauses a worker whose `runon` task waits out a signer refusal's
/// retry hint. The worker wakes, sees the cancellation, and must settle the
/// pause itself without resubmitting, as a draining retry does.
#[tokio::test]
async fn bash_kill_scope_settles_while_signer_retry_waits() {
    let daemon = daemon(signer_retry_script(u32::MAX), "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let mut clock = PausedDrainingClock::new(dir.path());
    let registry = registry();
    let scope = owner_scope("signer-wait");
    let task = start_scoped_runon_retry(&registry, dir.path(), daemon.connection.clone(), &scope);
    let first = clock.now();
    clock.expect_sleep(first, first + 5000).await;
    assert_pause_settles_promptly(&registry, start_kill_scope(&registry, &scope), &task).await;
    // The retry clock never moved: cancellation, not the hint, woke the worker.
    assert_eq!(clock.now(), first);
    remote_worker_exited(&task).await;
    assert_paused(&registry, &task);
    // Give a worker that wrongly kept waiting its chance to resubmit.
    clock.advance(5000);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(exec_runs(&daemon).len(), 1, "never resubmitted");
    assert_signer_retry_pause_saved(&registry.task(&task).unwrap().paths.json);
}

/// The pause lands after the signer-retry wait ended but before the worker
/// records the resubmission. Its recheck there must settle the pause.
#[tokio::test]
async fn bash_kill_scope_settles_when_paused_just_before_signer_resubmit() {
    let daemon = daemon(signer_retry_script(1), "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let mut clock = PausedDrainingClock::new(dir.path());
    let registry = registry();
    let scope = owner_scope("signer-resubmit");
    let task = start_scoped_runon_retry(&registry, dir.path(), daemon.connection.clone(), &scope);
    let first = clock.now();
    clock.expect_sleep(first, first + 5000).await;
    let mut gate = HeldDrainingGate::arm(dir.path(), DrainingGate::BeforeHintedResubmit);
    clock.advance(5000);
    gate.reached().await;
    let operation = start_kill_scope(&registry, &scope);
    pause_recorded(&registry, &task).await;
    gate.release();
    assert_pause_settles_promptly(&registry, operation, &task).await;
    assert_eq!(clock.now(), first + 5000);
    remote_worker_exited(&task).await;
    assert_paused(&registry, &task);
    assert_eq!(exec_runs(&daemon).len(), 1, "never resubmitted");
    assert_signer_retry_pause_saved(&registry.task(&task).unwrap().paths.json);
}

/// A signer-refusal retry is not resumable: its wait lives only in the worker,
/// so a daemon that stops during the wait saves just the runner's refusal.
/// After a module restart, the owner pauses that saved task. Recovering it
/// for the pause must neither resubmit it nor leave the pause unsettled. The
/// scope's saved local command keeps the pause busy long enough for a worker
/// started during recovery to reach the runner.
#[tokio::test]
async fn bash_kill_scope_never_resubmits_a_saved_signer_refusal() {
    let daemon = crate::exec_remote::wire_tests::daemon_with_clients(
        signer_retry_script(1),
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
    let scope = owner_scope("saved-signer-refusal");
    let original = registry();
    let task_id =
        start_scoped_runon_retry(&original, dir.path(), daemon.connection.clone(), &scope);
    clock.expect_event(DrainingClockEvent::Stopped).await;
    remote_worker_exited(&task_id).await;
    let json = original.task(&task_id).unwrap().paths.json.clone();
    // What survives the stop: the refusal, and no saved retry wait to resume.
    let saved = read_task(&json).unwrap();
    assert!(!saved.status.is_terminal(), "{saved:?}");
    let remote = saved.remote.as_ref().unwrap();
    assert_eq!(
        remote
            .terminal
            .as_ref()
            .and_then(|refusal| refusal.ran.clone()),
        Some(Ran::None),
        "{saved:?}"
    );
    let retry = remote.retry.as_ref().unwrap();
    assert_eq!(
        (retry.first_refusal_at_ms, retry.next_retry_at_ms),
        (None, None),
        "{saved:?}"
    );
    original.detach();
    let (local_task, local_pid) = start_saved_term_ignoring_task(dir.path(), &scope).await;
    // The hint has passed by the time the owner pauses the restarted module.
    clock.advance(5000);
    let reply = kill_scope_with_saved_tasks(dir.path(), &scope).await;
    assert_eq!(
        exec_runs(&daemon).len(),
        1,
        "the paused signer refusal was resubmitted: {reply:?}"
    );
    assert_saved_pause_reply(&reply, &task_id, &local_task, local_pid).await;
    assert_signer_retry_pause_saved(&json);
    assert_eq!(exec_runs(&daemon).len(), 1, "never resubmitted");
}

/// The runner refuses a job in a way AFT does not retry, and the owner's pause
/// lands after the refusal arrived but before the worker reports it. The
/// worker's report cannot replace the pause's terminal state, and it must
/// still settle the pause: the runner refused the job, so nothing runs.
#[tokio::test]
async fn bash_kill_scope_settles_when_paused_as_a_refusal_is_reported() {
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
    let scope = owner_scope("refusal-report");
    let mut gate = HeldDrainingGate::arm(dir.path(), DrainingGate::BeforeRefusalReport);
    let task = start_scoped_runon_retry(&registry, dir.path(), daemon.connection.clone(), &scope);
    gate.reached().await;
    let operation = start_kill_scope(&registry, &scope);
    pause_recorded(&registry, &task).await;
    gate.release();
    assert_pause_settles_promptly(&registry, operation, &task).await;
    remote_worker_exited(&task).await;
    assert_paused(&registry, &task);
    assert_eq!(exec_runs(&daemon).len(), 1, "never resubmitted");
    assert_refused_pause_saved(
        &registry.task(&task).unwrap().paths.json,
        "runner_sign_missing",
    );
    assert_no_retry_sleep(&mut clock);
}

/// The saved record of a task the owner paused while its retry's resubmission
/// failed without reaching the runner: paused, the cancellation recorded as
/// sent, and no runner job or sign that the command started.
fn assert_unsent_pause_saved(json: &Path) {
    assert_saved_paused(json);
    let remote = read_task(json).unwrap().remote.unwrap();
    assert!(
        remote.started.is_none() && !remote.output_received && remote.job_id.is_none(),
        "{remote:?}"
    );
}

/// A signer refusal's resubmission fails without reaching the runner, and the
/// owner's pause lands before the worker reports that. The worker must still
/// settle the pause: no submission of the command is left that could run.
#[tokio::test]
async fn bash_kill_scope_settles_when_paused_as_a_signer_resubmit_fails_unsent() {
    let daemon = daemon(signer_retry_script(1), "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let mut clock = PausedDrainingClock::new(dir.path());
    let registry = registry();
    let scope = owner_scope("signer-unsent");
    refuse_next_resubmission(dir.path());
    let mut gate = HeldDrainingGate::arm(dir.path(), DrainingGate::BeforeNoStartRefusal);
    let task = start_scoped_runon_retry(&registry, dir.path(), daemon.connection.clone(), &scope);
    let first = clock.now();
    clock.expect_sleep(first, first + 5000).await;
    clock.advance(5000);
    gate.reached().await;
    let operation = start_kill_scope(&registry, &scope);
    pause_recorded(&registry, &task).await;
    gate.release();
    assert_pause_settles_promptly(&registry, operation, &task).await;
    remote_worker_exited(&task).await;
    assert_paused(&registry, &task);
    assert_eq!(exec_runs(&daemon).len(), 1, "never resubmitted");
    assert_unsent_pause_saved(&registry.task(&task).unwrap().paths.json);
    assert_no_retry_sleep(&mut clock);
}

/// The same for a draining retry: its resubmission fails without reaching the
/// runner, and the pause lands before the worker reports that.
#[tokio::test]
async fn bash_kill_scope_settles_when_paused_as_a_draining_resubmit_fails_unsent() {
    let daemon = daemon(
        Script::Draining {
            refusals: 1,
            retry_after_ms: Some(200),
        },
        "exec-remote/v1",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut clock = PausedDrainingClock::new(dir.path());
    let registry = registry();
    let scope = owner_scope("draining-unsent");
    refuse_next_resubmission(dir.path());
    let mut gate = HeldDrainingGate::arm(dir.path(), DrainingGate::BeforeNoStartRefusal);
    let task = start_scoped_runon_retry(&registry, dir.path(), daemon.connection.clone(), &scope);
    let first = clock.now();
    clock.expect_sleep(first, first + 1000).await;
    clock.advance(5000);
    gate.reached().await;
    let operation = start_kill_scope(&registry, &scope);
    pause_recorded(&registry, &task).await;
    gate.release();
    assert_pause_settles_promptly(&registry, operation, &task).await;
    remote_worker_exited(&task).await;
    assert_paused(&registry, &task);
    assert_eq!(exec_runs(&daemon).len(), 1, "never resubmitted");
    assert_unsent_pause_saved(&registry.task(&task).unwrap().paths.json);
    assert_no_retry_sleep(&mut clock);
}

#[tokio::test]
async fn bash_kill_scope_runon_cancels_remote_job_and_releases_local_task() {
    assert_bash_kill_scope_runon_cancel(Script::Cancel, "sent").await;
}

#[tokio::test]
async fn bash_kill_scope_runon_cancel_failure_still_kills_local_task() {
    assert_bash_kill_scope_runon_cancel(Script::CancelRefused, "failed:").await;
}

#[tokio::test]
async fn bash_kill_scope_cancel_ack_is_local_stop_not_remote_proof() {
    assert_bash_kill_scope_runon_cancel(
        Script::CancelTerminal {
            terminal: cancel_unknown_terminal,
            stderr: "runner monitoring: outcome_unknown error=cancelled unit already exited",
        },
        "sent",
    )
    .await;
}

async fn assert_bash_kill_scope_runon_cancel(script: Script, expected: &str) {
    let daemon = daemon(script, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let scope = subc_protocol::scope::ScopeStamp::new(
        subc_protocol::Principal::Reserved {
            module_id: "prefrontal-core".into(),
        },
        "worker",
        1,
        subc_protocol::scope::ScopeKind::Worker,
    );
    let task = super::super::with_scope_spawn(Some(scope.clone()), || {
        let mut launch = launch(daemon.connection.clone());
        launch.explicit_runon = true;
        start_with_launch(
            &registry,
            dir.path(),
            launch,
            crate::bash_background::HardKill::After(Duration::from_secs(30)),
        )
    });
    let job = wait_for("runner acceptance before pause", || {
        registry
            .status(&task, "session", None, None, 0)
            .unwrap()
            .remote_job_id
    })
    .await;
    let paused_registry = registry.clone();
    let reply = tokio::task::spawn_blocking(move || {
        crate::commands::bash_kill_scope::handle_bash_kill_scope(
            serde_json::json!({"scope":scope}),
            Some(&scope.owner),
            &[paused_registry],
        )
    })
    .await
    .unwrap();
    assert!(reply.success, "{reply:?}");
    assert_eq!(reply.data["killed"][0]["remote"]["job_id"], job);
    assert!(
        reply.data["killed"][0]["remote"]["cancel"]
            .as_str()
            .unwrap()
            .starts_with(expected),
        "{reply:?}"
    );
    if expected == "failed:" {
        assert!(
            reply.data["killed"][0]["remote"]["cancel"]
                .as_str()
                .unwrap()
                .contains("runner unavailable"),
            "{reply:?}"
        );
    }
    assert_eq!(
        daemon
            .log
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, body)| body["method"] == "exec.cancel")
            .count(),
        1
    );
    let done = registry.status(&task, "session", None, None, 0).unwrap();
    assert_eq!(done.info.status, BgTaskStatus::Killed);
    assert_eq!(done.info.status_reason.as_deref(), Some("paused_by_owner"));
    // Owner pause stops the local task after the cancel acknowledgement; it
    // does not assert that the runner supplied a terminal or proved process exit.
    let saved = read_task(&registry.task(&task).unwrap().paths.json).unwrap();
    assert!(saved.remote.unwrap().terminal.is_none());
}

/// The scope's owner calls `bash.kill_scope` while the worker of an accepted
/// job in that scope is reconnecting after the runner's route closed. The worker sends the cancel
/// once it is connected again, and that settles the pause. From then on the
/// worker must stop: no further reconnect or `exec.cancel`, and the recorded
/// `sent` must still stand after the whole recovery deadline has passed.
#[tokio::test]
async fn bash_kill_scope_during_reconnect_settles_once_and_stops_the_worker() {
    let daemon = crate::exec_remote::wire_tests::outage_daemon().await;
    let dir = tempfile::tempdir().unwrap();
    let clock = PausedReattachClock::new(dir.path());
    let registry = registry();
    let scope = subc_protocol::scope::ScopeStamp::new(
        subc_protocol::Principal::Reserved {
            module_id: "prefrontal-core".into(),
        },
        "worker",
        1,
        subc_protocol::scope::ScopeKind::Worker,
    );
    let task = super::super::with_scope_spawn(Some(scope.clone()), || {
        let mut launch = launch(daemon.connection.clone());
        launch.explicit_runon = true;
        start_with_launch(
            &registry,
            dir.path(),
            launch,
            crate::bash_background::HardKill::After(Duration::from_secs(30)),
        )
    });
    // The route closed after acceptance and the runner is away, so the worker
    // is reconnecting. Hold one reconnect open and call `bash.kill_scope` as
    // the scope's owner during it.
    daemon.after_refused_connects(1).await;
    daemon.hold_next_connect();
    daemon.after_held_connect().await;
    let pausing = registry.clone();
    let pause_scope = scope.clone();
    let operation = tokio::task::spawn_blocking(move || {
        crate::commands::bash_kill_scope::handle_bash_kill_scope(
            serde_json::json!({"scope":pause_scope}),
            Some(&pause_scope.owner),
            &[pausing],
        )
    });
    wait_for("the pause must record its cancellation intent", || {
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
            .cancel_requested
            .then_some(())
    })
    .await;
    // The runner is back when the held reconnect completes, so the worker's
    // `exec.cancel` reaches it and the pause records `sent`.
    daemon.restore();
    daemon.release_held_connect();
    let reply = tokio::time::timeout(Duration::from_secs(30), operation)
        .await
        .expect("the pause must settle once the cancel is sent")
        .unwrap();
    assert!(reply.success, "{reply:?}");
    assert_eq!(
        reply.data["killed"][0]["remote"]["cancel"], "sent",
        "{reply:?}"
    );
    let connects_at_settlement = *daemon.connects.borrow();
    // Pass the job's own limits plus grace: a worker still trying to re-attach
    // gives up here and records its recovery failure.
    clock.advance(Duration::from_secs(12 * 60));
    remote_worker_exited(&task).await;
    assert_eq!(
        daemon.methods("exec.cancel").len(),
        1,
        "the settled pause must not send the cancel again"
    );
    assert_eq!(
        *daemon.connects.borrow(),
        connects_at_settlement,
        "the worker must not reconnect after the pause settled"
    );
    assert!(daemon.methods("exec.attach").is_empty());
    assert_eq!(daemon.methods("exec.run").len(), 1, "never resubmitted");
    let recorded = registry
        .task(&task)
        .unwrap()
        .state
        .lock()
        .unwrap()
        .metadata
        .remote
        .as_ref()
        .unwrap()
        .owner_pause_cancel
        .clone();
    assert_eq!(recorded.as_deref(), Some("sent"));
    let done = registry.status(&task, "session", None, None, 0).unwrap();
    assert_eq!(done.info.status, BgTaskStatus::Killed);
    assert_eq!(done.info.status_reason.as_deref(), Some("paused_by_owner"));
    drop(clock);
}

/// Without an owner pause, a reconnect that still finds the runner away is an
/// ordinary transport failure: the worker keeps reconnecting, never sends a
/// cancel, and delivers the job's result once the runner is back.
#[tokio::test]
async fn exec_remote_reconnect_failure_without_owner_pause_keeps_retrying() {
    let daemon = crate::exec_remote::wire_tests::outage_daemon().await;
    let dir = tempfile::tempdir().unwrap();
    let clock = PausedReattachClock::new(dir.path());
    let registry = registry();
    let task = start(&registry, dir.path(), daemon.connection.clone());
    daemon.after_refused_connects(1).await;
    daemon.hold_next_connect();
    daemon.after_held_connect().await;
    let refused = *daemon.refused_connects.borrow();
    // The held reconnect is answered while the runner is still away.
    daemon.release_held_connect();
    daemon.after_refused_connects(refused + 3).await;
    daemon.restore();
    let done = terminal(&registry, &task).await;
    assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");
    assert_eq!(done.exit_code, Some(0));
    let stdout = registry.task(&task).unwrap().paths.stdout.clone();
    assert_eq!(fs::read(stdout).unwrap(), b"AB");
    remote_worker_exited(&task).await;
    assert!(daemon.methods("exec.cancel").is_empty());
    assert_eq!(daemon.methods("exec.run").len(), 1, "never resubmitted");
    drop(clock);
}

#[tokio::test]
async fn bash_kill_scope_unaccepted_tasks_share_one_deadline_and_local_kills_are_first() {
    let dirs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
    // Prepare repositories before either remote worker connects: synchronous
    // Git setup must not starve the fake daemon's authentication handshake.
    for dir in &dirs {
        ensure_git_fixture(dir.path());
    }
    let first = daemon(Script::GatedRoute, "exec-remote/v1").await;
    let second = daemon(Script::GatedRoute, "exec-remote/v1").await;
    let registries = [registry(), registry()];
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/bash-kill-scope/wire.json"
    ))
    .unwrap();
    let scope: subc_protocol::scope::ScopeStamp =
        serde_json::from_value(fixture["unaccepted"]["request"]["params"]["scope"].clone())
            .unwrap();
    let mut tasks = Vec::new();
    for (index, connection) in [first.connection.clone(), second.connection.clone()]
        .into_iter()
        .enumerate()
    {
        let task = super::super::with_scope_spawn(Some(scope.clone()), || {
            let mut launch = launch(connection);
            launch.explicit_runon = true;
            start_with_launch(
                &registries[index],
                dirs[index].path(),
                launch,
                crate::bash_background::HardKill::After(Duration::from_secs(30)),
            )
        });
        tasks.push(task);
    }
    for daemon in [&first, &second] {
        wait_for("remote route setup must be in flight", || {
            daemon
                .log
                .lock()
                .unwrap()
                .iter()
                .any(|(_, body)| body["op"] == "route.open")
                .then_some(())
        })
        .await;
    }
    let local = super::super::with_scope_spawn(Some(scope.clone()), || {
        registries[1].spawn(
            SpawnPlan::Unsandboxed,
            "sleep 30",
            "local".into(),
            dirs[1].path().into(),
            HashMap::new(),
            crate::bash_background::HardKill::After(Duration::from_secs(30)),
            None,
            dirs[1].path().into(),
            10,
            true,
            false,
            Some(dirs[1].path().into()),
        )
    })
    .unwrap();
    let pausing = registries.clone();
    let started = Instant::now();
    let operation = tokio::task::spawn_blocking(move || {
        crate::commands::bash_kill_scope::handle_bash_kill_scope(
            serde_json::json!({"scope":scope}),
            Some(&scope.owner),
            &pausing,
        )
    });
    wait_for("local kill must not wait for remote cancellation", || {
        (registries[1]
            .status(&local, "local", None, None, 0)
            .unwrap()
            .info
            .status
            == BgTaskStatus::Killed)
            .then_some(())
    })
    .await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "local kill waited for a runner: {:?}",
        started.elapsed()
    );
    let reply = tokio::time::timeout(Duration::from_secs(12), operation)
        .await
        .expect("both unaccepted tasks must share one 10-second deadline")
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(12));
    assert!(reply.success, "{reply:?}");
    assert_eq!(reply.data["killed"].as_array().unwrap().len(), 3);
    for (index, task) in tasks.iter().enumerate() {
        let entry = reply.data["killed"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["task_id"] == *task)
            .unwrap();
        assert_eq!(
            entry["remote"],
            fixture["unaccepted"]["reply"]["data"]["killed"][0]["remote"]
        );
        assert_eq!(
            registries[index]
                .status(&task, "session", None, None, 0)
                .unwrap()
                .info
                .status,
            BgTaskStatus::Killed
        );
    }
    first.acceptance_gate.notify_one();
    second.acceptance_gate.notify_one();
    for (registry, task) in registries.iter().zip(&tasks) {
        wait_for(
            "paused submission must be withdrawn after route setup",
            || {
                registry
                    .task(task)
                    .unwrap()
                    .state
                    .lock()
                    .unwrap()
                    .metadata
                    .remote
                    .as_ref()
                    .unwrap()
                    .owner_pause_cancel
                    .clone()
            },
        )
        .await;
    }
    assert!(exec_runs(&first).is_empty());
    assert!(exec_runs(&second).is_empty());
}

#[tokio::test]
async fn bash_kill_scope_withdraws_submission_before_acceptance() {
    let daemon = daemon(Script::GatedAcceptance, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/bash-kill-scope/wire.json"
    ))
    .unwrap();
    let case = &fixture["withdrawn_before_acceptance"];
    let scope: subc_protocol::scope::ScopeStamp =
        serde_json::from_value(case["request"]["params"]["scope"].clone()).unwrap();
    let task = super::super::with_scope_spawn(Some(scope.clone()), || {
        let mut launch = launch(daemon.connection.clone());
        launch.explicit_runon = true;
        start_with_launch(
            &registry,
            dir.path(),
            launch,
            crate::bash_background::HardKill::After(Duration::from_secs(30)),
        )
    });
    daemon.wait_for_requests("exec.run", 1).await;
    let pausing = registry.clone();
    let reply = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::task::spawn_blocking(move || {
            crate::commands::bash_kill_scope::handle_bash_kill_scope(
                serde_json::json!({"scope":scope}),
                Some(&scope.owner),
                &[pausing],
            )
        }),
    )
    .await
    .expect("withdrawal must not wait for acceptance")
    .unwrap();
    assert!(reply.success, "{reply:?}");
    let mut expected = case["reply"]["data"].clone();
    expected["killed"][0]["task_id"] = serde_json::json!(task);
    assert_eq!(reply.data, expected);
    wait_for("call-key Cancel must reach the runner", || {
        daemon
            .log
            .lock()
            .unwrap()
            .iter()
            .any(|(header, _)| header.ty == subc_protocol::FrameType::Cancel)
            .then_some(())
    })
    .await;
    assert!(
        !daemon
            .log
            .lock()
            .unwrap()
            .iter()
            .any(|(_, body)| body["method"] == "exec.cancel"),
        "no job ID was known for exec.cancel"
    );
    daemon.acceptance_gate.notify_one();
}

async fn submission_daemon(
    never_accepted: bool,
    mismatch: bool,
    always_unknown: bool,
    clients: usize,
) -> crate::exec_remote::wire_tests::Daemon {
    crate::exec_remote::wire_tests::daemon_with_capabilities(
        Script::Submission {
            never_accepted,
            mismatch,
            always_unknown,
            disconnect: false,
        },
        &["exec-remote/v1", "exec-remote-submission-key/v1"],
        clients,
    )
    .await
}

fn submission_runs(daemon: &crate::exec_remote::wire_tests::Daemon) -> Vec<serde_json::Value> {
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
async fn submission_key_attach_resumes_output_exactly_once() {
    let daemon = submission_daemon(false, false, false, 1).await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");
    let task = registry.task(&task_id).unwrap();
    assert_eq!(
        fs::read_to_string(&task.paths.stdout).unwrap(),
        "line-0\nline-1\nline-2\nline-3\n"
    );
    let runs = submission_runs(&daemon);
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0]["submission_key"], format!("aft:{task_id}"));
    assert_eq!(runs[1]["resume_from_seq"], 1);
    let mut resumed = runs[1].clone();
    resumed.as_object_mut().unwrap().remove("resume_from_seq");
    assert_eq!(resumed, runs[0]);
    let durable = read_task(&task.paths.json).unwrap();
    assert_eq!(durable.remote.as_ref().unwrap().job_id, Some(id()));
    assert!(
        durable
            .remote
            .as_ref()
            .unwrap()
            .submission
            .as_ref()
            .unwrap()
            .reattached
    );
}

#[tokio::test]
async fn submission_key_never_accepted_uses_one_new_key() {
    let daemon = submission_daemon(true, false, false, 1).await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");
    let runs = submission_runs(&daemon);
    assert_eq!(runs.len(), 3);
    assert_eq!(runs[0]["submission_key"], runs[1]["submission_key"]);
    assert_ne!(runs[1]["submission_key"], runs[2]["submission_key"]);
    assert!(runs[2].get("resume_from_seq").is_none());
    let task = registry.task(&task_id).unwrap();
    let durable = serde_json::to_value(read_task(&task.paths.json).unwrap()).unwrap();
    assert_eq!(
        durable["remote"]["submission"]["key"],
        runs[2]["submission_key"]
    );
}

#[tokio::test]
async fn submission_key_without_capability_is_never_sent_or_retried() {
    let daemon = daemon(Script::Lost, "exec-remote/v1").await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    assert_eq!(
        terminal(&registry, &task_id).await.info.status,
        BgTaskStatus::FateUnknown
    );
    let runs = submission_runs(&daemon);
    assert_eq!(runs.len(), 1);
    assert!(runs[0].get("submission_key").is_none(), "{runs:?}");
    assert!(runs[0].get("resume_from_seq").is_none());
}

#[tokio::test]
async fn submission_key_mismatch_preserves_existing_job_for_cancel() {
    let daemon = submission_daemon(false, true, false, 2).await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    let done = terminal(&registry, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::FateUnknown);
    assert_eq!(submission_runs(&daemon).len(), 2);
    assert!(
        done.remote_refusal.is_none(),
        "the retry's refusal is not proof about the existing job"
    );
    assert!(
        done.output_preview.contains("submission_key_mismatch"),
        "{done:?}"
    );
    let task = registry.task(&task_id).unwrap();
    assert_eq!(
        read_task(&task.paths.json).unwrap().remote.unwrap().job_id,
        Some(id())
    );
    let recorded_id = read_task(&task.paths.json)
        .unwrap()
        .remote
        .unwrap()
        .job_id
        .unwrap();
    let client = ExecRemoteClient::connect(
        &daemon.connection,
        subc_protocol::BindIdentity::new(dir.path().display().to_string(), "broca", "session"),
    )
    .await
    .unwrap();
    client.cancel_job(recorded_id).await.unwrap();
    daemon.wait_for_requests("exec.cancel", 1).await;
    let log = daemon.log.lock().unwrap();
    assert_eq!(
        log.iter()
            .find(|(_, b)| b["method"] == "exec.cancel")
            .unwrap()
            .1["params"]["job_id"],
        id().to_string()
    );
}

async fn stopped_submission(
    dir: &Path,
    daemon: &crate::exec_remote::wire_tests::Daemon,
) -> (BgTaskRegistry, String) {
    stopped_draining_roots().lock().unwrap().insert(dir.into());
    let registry = registry();
    let task_id = start(&registry, dir, daemon.connection.clone());
    remote_worker_exited(&task_id).await;
    (registry, task_id)
}

#[tokio::test]
async fn submission_key_restart_restores_request_key_and_output_cursor() {
    let daemon = submission_daemon(false, false, false, 2).await;
    let dir = tempfile::tempdir().unwrap();
    let (old, task_id) = stopped_submission(dir.path(), &daemon).await;
    let task = old.task(&task_id).unwrap();
    let saved = read_task(&task.paths.json).unwrap();
    assert_eq!(saved.remote.as_ref().unwrap().last_seq, Some(1));
    let value = serde_json::to_value(&saved).unwrap();
    assert_eq!(
        value["remote"]["submission"]["key"],
        format!("aft:{task_id}")
    );
    drop(task);
    drop(old);
    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    assert_eq!(
        terminal(&restarted, &task_id).await.info.status,
        BgTaskStatus::Completed
    );
    let task = restarted.task(&task_id).unwrap();
    assert_eq!(
        fs::read_to_string(&task.paths.stdout).unwrap(),
        "line-0\nline-1\nline-2\nline-3\n"
    );
    let runs = submission_runs(&daemon);
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[1]["resume_from_seq"], 1);
    assert_eq!(runs[1]["submission_key"], runs[0]["submission_key"]);
}

#[tokio::test]
async fn submission_key_older_than_retention_is_not_resubmitted() {
    let daemon = submission_daemon(false, false, false, 2).await;
    let dir = tempfile::tempdir().unwrap();
    let (old, task_id) = stopped_submission(dir.path(), &daemon).await;
    let task = old.task(&task_id).unwrap();
    let mut value = serde_json::to_value(read_task(&task.paths.json).unwrap()).unwrap();
    assert!(value["remote"]["submission"].is_object());
    value["remote"]["submission"]["created_at_ms"] =
        serde_json::json!(unix_millis() - 24 * 60 * 60 * 1000);
    write_task(&task.paths.json, &serde_json::from_value(value).unwrap()).unwrap();
    drop(task);
    drop(old);
    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    assert_eq!(
        terminal(&restarted, &task_id).await.info.status,
        BgTaskStatus::FateUnknown
    );
    assert_eq!(submission_runs(&daemon).len(), 1);
}

#[test]
fn submission_key_reattachment_copy_is_not_enabled_without_approval() {
    assert_eq!(REATTACHED_RESULT_SENTENCE, None);
}

#[tokio::test]
async fn submission_key_recovery_attempts_are_bounded() {
    let daemon = submission_daemon(false, false, true, 1).await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    assert_eq!(
        terminal(&registry, &task_id).await.info.status,
        BgTaskStatus::FateUnknown
    );
    let runs = submission_runs(&daemon);
    assert_eq!(runs.len(), 4);
    let task = registry.task(&task_id).unwrap();
    assert_eq!(
        read_task(&task.paths.json).unwrap().remote.unwrap().job_id,
        Some(id())
    );
    for run in &runs[1..] {
        assert_eq!(run["submission_key"], runs[0]["submission_key"]);
        assert_eq!(run["resume_from_seq"], 1);
    }
}

#[tokio::test]
async fn submission_key_closed_runner_time_budget_is_not_resubmitted() {
    let daemon = submission_daemon(false, false, false, 2).await;
    let dir = tempfile::tempdir().unwrap();
    let (old, task_id) = stopped_submission(dir.path(), &daemon).await;
    let task = old.task(&task_id).unwrap();
    let mut saved = read_task(&task.paths.json).unwrap();
    saved.started_at =
        unix_millis() - saved.timeout_ms.unwrap() - duration_ms(RUNNER_QUEUE_WAIT_LIMIT) - 1;
    write_task(&task.paths.json, &saved).unwrap();
    drop(task);
    drop(old);
    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    assert_eq!(
        terminal(&restarted, &task_id).await.info.status,
        BgTaskStatus::FateUnknown
    );
    assert_eq!(submission_runs(&daemon).len(), 1);
}

#[tokio::test]
async fn submission_key_daemon_restart_reconnects_before_resubmission() {
    let daemon = crate::exec_remote::wire_tests::daemon_with_capabilities(
        Script::Submission {
            never_accepted: false,
            mismatch: false,
            always_unknown: false,
            disconnect: true,
        },
        &["exec-remote/v1", "exec-remote-submission-key/v1"],
        2,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = start(&registry, dir.path(), daemon.connection.clone());
    let done = terminal(&registry, &task_id).await;
    let operations: Vec<_> = daemon
        .log
        .lock()
        .unwrap()
        .iter()
        .map(|(_, body)| {
            body.get("method")
                .or_else(|| body.get("op"))
                .cloned()
                .unwrap_or_default()
        })
        .collect();
    assert_eq!(
        done.info.status,
        BgTaskStatus::Completed,
        "{done:?}; operations: {operations:?}"
    );
    let task = registry.task(&task_id).unwrap();
    assert_eq!(
        fs::read_to_string(&task.paths.stdout).unwrap(),
        "line-0\nline-1\nline-2\nline-3\n"
    );
    let runs = submission_runs(&daemon);
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0]["submission_key"], runs[1]["submission_key"]);
    assert_eq!(runs[1]["resume_from_seq"], 1);
}

#[tokio::test]
async fn submission_key_restart_before_acceptance_replays_from_beginning() {
    let daemon = submission_daemon(false, false, false, 2).await;
    let dir = tempfile::tempdir().unwrap();
    let (old, task_id) = stopped_submission(dir.path(), &daemon).await;
    let task = old.task(&task_id).unwrap();
    let mut saved = read_task(&task.paths.json).unwrap();
    let remote = saved.remote.as_mut().unwrap();
    remote.accepted = false;
    remote.job_id = None;
    remote.last_seq = None;
    remote.stdout_len = 0;
    remote.output_received = false;
    remote.terminal = None;
    write_task(&task.paths.json, &saved).unwrap();
    drop(task);
    drop(old);
    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    assert_eq!(
        terminal(&restarted, &task_id).await.info.status,
        BgTaskStatus::Completed
    );
    let task = restarted.task(&task_id).unwrap();
    assert_eq!(
        fs::read_to_string(&task.paths.stdout).unwrap(),
        "line-0\nline-1\nline-2\nline-3\n"
    );
    let runs = submission_runs(&daemon);
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[1]["submission_key"], runs[0]["submission_key"]);
    assert!(runs[1].get("resume_from_seq").is_none());
}

#[tokio::test]
async fn submission_key_queue_wait_does_not_consume_execution_timeout() {
    let daemon = submission_daemon(false, false, false, 2).await;
    let dir = tempfile::tempdir().unwrap();
    let (old, task_id) = stopped_submission(dir.path(), &daemon).await;
    let task = old.task(&task_id).unwrap();
    let mut saved = read_task(&task.paths.json).unwrap();
    saved.started_at = unix_millis() - saved.timeout_ms.unwrap() - 1;
    let remote = saved.remote.as_mut().unwrap();
    remote.submission.as_mut().unwrap().created_at_ms = saved.started_at;
    // Recover the checkpoint from admission, before any execution was observed.
    remote.last_seq = None;
    remote.stdout_len = 0;
    remote.output_received = false;
    remote.terminal = None;
    write_task(&task.paths.json, &saved).unwrap();
    drop(task);
    drop(old);
    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    assert_eq!(
        terminal(&restarted, &task_id).await.info.status,
        BgTaskStatus::Completed
    );
    assert_eq!(submission_runs(&daemon).len(), 2);
}

fn lost_submission_roots() -> &'static Mutex<HashSet<PathBuf>> {
    static ROOTS: std::sync::OnceLock<Mutex<HashSet<PathBuf>>> = std::sync::OnceLock::new();
    ROOTS.get_or_init(|| Mutex::new(HashSet::new()))
}

pub(super) fn lose_submission_replies(root: &Path) -> bool {
    lost_submission_roots()
        .lock()
        .unwrap()
        .contains(&test_clock_key(root))
}

struct LostSubmissionReplies(PathBuf);

impl LostSubmissionReplies {
    fn new(root: &Path) -> Self {
        let root = test_clock_key(root);
        assert!(lost_submission_roots().lock().unwrap().insert(root.clone()));
        Self(root)
    }
}

impl Drop for LostSubmissionReplies {
    fn drop(&mut self) {
        lost_submission_roots().lock().unwrap().remove(&self.0);
    }
}

async fn review_daemon(script: Script, clients: usize) -> crate::exec_remote::wire_tests::Daemon {
    crate::exec_remote::wire_tests::daemon_with_capabilities(
        script,
        &["exec-remote/v1", "exec-remote-submission-key/v1"],
        clients,
    )
    .await
}

async fn fresh_reply_loss_never_runs_locally(restored: bool) {
    let daemon = review_daemon(
        Script::SubmissionFreshReplyLost,
        if restored { 2 } else { 1 },
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    ensure_git_fixture(dir.path());
    let _loss = LostSubmissionReplies::new(dir.path());
    if restored {
        stopped_draining_roots()
            .lock()
            .unwrap()
            .insert(dir.path().into());
    }
    let old = registry();
    let counter = dir.path().join("local-spawn-counter");
    let task_id = old
        .spawn_remote(
            launch(daemon.connection.clone()),
            SpawnPlan::Unsandboxed,
            "printf x >> \"$LOCAL_SPAWN_COUNTER\"",
            resolve_posix_shell(),
            "session".into(),
            dir.path().into(),
            HashMap::from([("LOCAL_SPAWN_COUNTER".into(), counter.display().to_string())]),
            crate::bash_background::HardKill::After(Duration::from_secs(30)),
            None,
            dir.path().into(),
            crate::bash_background::TaskSlot::Background { max: 10 },
            true,
            false,
            Some(dir.path().into()),
        )
        .unwrap();
    let registry = if restored {
        remote_worker_exited(&task_id).await;
        drop(old);
        let restarted = registry();
        restarted.replay_session(dir.path(), "session").unwrap();
        restarted
    } else {
        old
    };
    let done = terminal(&registry, &task_id).await;
    let local_spawns = match fs::read(&counter) {
        Ok(bytes) => bytes.len(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
        Err(error) => panic!("read local spawn counter: {error}"),
    };
    assert_eq!(
        local_spawns, 0,
        "an uncertain fresh submission must not also run locally"
    );
    assert_eq!(done.info.status, BgTaskStatus::FateUnknown, "{done:?}");
    let runs = submission_runs(&daemon);
    assert!(
        runs.len() >= 2,
        "fresh dispatch must reach the fake executor"
    );
    assert_ne!(runs[0]["submission_key"], runs[1]["submission_key"]);
    for request in &runs[1..] {
        assert_eq!(request["submission_key"], runs[1]["submission_key"]);
    }
    let task = registry.task(&task_id).unwrap();
    let durable = read_task(&task.paths.json).unwrap();
    assert!(!durable.local_fallback_started);
    assert!(durable.child_pid.is_none());
    let remote = durable.remote.as_ref().unwrap();
    assert_eq!(
        serde_json::to_value(remote).unwrap()["submission"]["key"],
        runs[1]["submission_key"]
    );
    assert!(remote.terminal.is_none());
    assert!(remote.submission.as_ref().unwrap().refusal.is_none());
    assert!(remote.submission.as_ref().unwrap().fresh_key_used);
}

#[tokio::test]
async fn submission_review_fresh_reply_loss_never_runs_locally() {
    fresh_reply_loss_never_runs_locally(false).await;
}

#[tokio::test]
async fn submission_review_restored_fresh_reply_loss_never_runs_locally() {
    fresh_reply_loss_never_runs_locally(true).await;
}

async fn expired_gap_keeps_confirmed_exit(restored: bool) {
    for unknown in [false, true] {
        let daemon = review_daemon(
            Script::SubmissionGapExpired { unknown },
            if restored { 2 } else { 1 },
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let (registry, task_id) = if restored {
            let (old, task_id) = stopped_submission(dir.path(), &daemon).await;
            let task = old.task(&task_id).unwrap();
            let saved = read_task(&task.paths.json).unwrap();
            assert!(saved.remote.as_ref().unwrap().gap_recovery.is_some());
            drop(task);
            drop(old);
            let restarted = registry();
            restarted.replay_session(dir.path(), "session").unwrap();
            (restarted, task_id)
        } else {
            let registry = registry();
            let task_id = start(&registry, dir.path(), daemon.connection.clone());
            (registry, task_id)
        };
        let done = terminal(&registry, &task_id).await;
        assert_eq!(
            done.info.status,
            BgTaskStatus::Completed,
            "unknown={unknown}, {done:?}"
        );
        assert_eq!(done.exit_code, Some(0));
        let task = registry.task(&task_id).unwrap();
        assert_eq!(fs::read(&task.paths.stdout).unwrap(), b"AC");
        let durable = read_task(&task.paths.json).unwrap();
        assert_eq!(durable.incomplete_output, vec![(1, 1)]);
        assert!(
            done.output_preview
                .contains("output lost between seq 1 and 1"),
            "{done:?}"
        );
        assert!(!durable.local_fallback_started);
        let remote = durable.remote.unwrap();
        assert!(remote.gap_recovery.is_none());
        assert_eq!(remote.terminal.unwrap().outcome, Outcome::Exit { code: 0 });
    }
}

#[tokio::test]
async fn submission_review_expired_gap_keeps_confirmed_exit() {
    expired_gap_keeps_confirmed_exit(false).await;
}

#[tokio::test]
async fn submission_review_restored_expired_gap_keeps_confirmed_exit() {
    expired_gap_keeps_confirmed_exit(true).await;
}

#[tokio::test]
async fn submission_review_current_never_accepted_allows_local_fallback() {
    let daemon = review_daemon(Script::SubmissionFreshReplyLost, 2).await;
    let dir = tempfile::tempdir().unwrap();
    let (old, task_id) = stopped_submission(dir.path(), &daemon).await;
    let task = old.task(&task_id).unwrap();
    let mut saved = read_task(&task.paths.json).unwrap();
    saved
        .remote
        .as_mut()
        .unwrap()
        .submission
        .as_mut()
        .unwrap()
        .attempts = SUBMISSION_RECOVERY_ATTEMPTS;
    write_task(&task.paths.json, &saved).unwrap();
    drop(task);
    drop(old);
    let restarted = registry();
    restarted.replay_session(dir.path(), "session").unwrap();
    let done = terminal(&restarted, &task_id).await;
    assert_eq!(done.info.status, BgTaskStatus::Completed, "{done:?}");
    assert!(done.output_preview.contains("local-proof"));
    let task = restarted.task(&task_id).unwrap();
    assert!(read_task(&task.paths.json).unwrap().local_fallback_started);
    assert_eq!(submission_runs(&daemon).len(), 1);
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
    let daemon = crate::exec_remote::wire_tests::daemon_with_capabilities(
        Script::MotorExpiredReplay,
        RETURN_CLAIMS,
        1,
    )
    .await;
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
            None,
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
             sub/dist/absent.txt\n  sub/other.txt"
        ),
        "{output}"
    );
    // ck-motor sends no separate untracked list; its changed list holds them,
    // and an absent untracked report stays silent.
    assert!(!output.contains("untracked files"), "{output}");
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
                 other.txt"
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
        submission: None,
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
        return_paths: vec!["dist/app.bin".into()],
        returned_files: None,
        return_first_seq: None,
        return_staging: Default::default(),
        expired_gap_unverified: Vec::new(),
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
            None,
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

#[tokio::test]
async fn runon_uncounted_expired_gap_that_never_fills_is_reported_unverified_not_lost() {
    // The crate's replay-expired vector sends no chunk counts and its gap (3
    // seqs) is not the one chunk derived from the 10-byte file. Each
    // re-attach replays the same records, so the gap never fills.
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
            "records 3 to 5 could not be verified: they may be expired returned-file data or lost output, and the runner did not report chunk counts"
        ),
        "{output}"
    );
    assert!(!output.contains("output lost"), "{output}");
    assert!(
        output
            .contains("  dist/app.bin: not returned (the server no longer keeps this job's files)"),
        "{output}"
    );
}

#[test]
fn expired_gap_unverified_line_is_the_approved_text() {
    assert_eq!(
        expired_gap_unverified_line(4, 6),
        "records 4 to 6 could not be verified: they may be expired returned-file data or lost output, and the runner did not report chunk counts"
    );
    // A single record keeps the range form, like the lost-output line.
    assert_eq!(
        expired_gap_unverified_line(4, 4),
        "records 4 to 4 could not be verified: they may be expired returned-file data or lost output, and the runner did not report chunk counts"
    );
}
