//! Clocks and fault-injection gates shared with the remote worker test code.

use super::*;

pub(super) fn reattach_budgets() -> &'static Mutex<HashMap<PathBuf, Duration>> {
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

pub(super) fn reattach_clocks() -> &'static Mutex<HashMap<PathBuf, Arc<ReattachClock>>> {
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

pub(super) fn crash_tasks() -> &'static Mutex<HashSet<String>> {
    static TASKS: std::sync::OnceLock<Mutex<HashSet<String>>> = std::sync::OnceLock::new();
    TASKS.get_or_init(|| Mutex::new(HashSet::new()))
}

pub(super) fn simulate_crash_before_local_pid(task: &str) -> bool {
    crash_tasks().lock().unwrap().remove(task)
}

pub(super) fn post_spawn_failures() -> &'static Mutex<HashSet<String>> {
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

pub(super) struct MarkerGate {
    pub(super) entered: std::sync::mpsc::Sender<()>,
    pub(super) release: std::sync::mpsc::Receiver<()>,
}

pub(super) fn marker_gates() -> &'static Mutex<HashMap<String, MarkerGate>> {
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

pub(super) type RemoteLaunchLogCapture = Arc<Mutex<Vec<String>>>;

pub(super) fn remote_launch_log_captures(
) -> &'static Mutex<HashMap<PathBuf, RemoteLaunchLogCapture>> {
    static CAPTURES: std::sync::OnceLock<Mutex<HashMap<PathBuf, RemoteLaunchLogCapture>>> =
        std::sync::OnceLock::new();
    CAPTURES.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) fn capture_remote_launch_logs<R>(task: &BgTask, run: impl FnOnce() -> R) -> R {
    let root = task.state.lock().unwrap().metadata.project_root.clone();
    // Metadata retains the caller's spelling, but capture keys are canonical.
    // macOS temporary roots can name the same directory through /var or /private/var.
    let root = root.as_deref().map(canonicalized_path);
    let capture = root.and_then(|root| remote_launch_log_captures().lock().unwrap().remove(&root));
    if let Some(capture) = capture {
        crate::logging::capture_log_lines_into(capture, run)
    } else {
        run()
    }
}

pub(super) fn exited_remote_workers() -> &'static Mutex<HashSet<String>> {
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
pub(super) fn remote_worker_counts() -> &'static Mutex<HashMap<String, (usize, usize)>> {
    static COUNTS: std::sync::OnceLock<Mutex<HashMap<String, (usize, usize)>>> =
        std::sync::OnceLock::new();
    COUNTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Called as a remote worker thread for `task` is about to be spawned.
pub(super) fn note_remote_worker_start(task: &BgTask) {
    cancellation_at_worker_start().lock().unwrap().insert(
        task.task_id.clone(),
        task.state
            .lock()
            .unwrap()
            .metadata
            .remote
            .as_ref()
            .is_some_and(|remote| remote.cancel_requested),
    );
    remote_worker_counts()
        .lock()
        .unwrap()
        .entry(task.task_id.clone())
        .or_default()
        .0 += 1;
}

pub(super) fn cancellation_at_worker_start() -> &'static Mutex<HashMap<String, bool>> {
    static CANCELLATIONS: std::sync::OnceLock<Mutex<HashMap<String, bool>>> =
        std::sync::OnceLock::new();
    CANCELLATIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) async fn wait_for<T>(reason: &str, mut probe: impl FnMut() -> Option<T>) -> T {
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

pub(super) fn draining_budgets() -> &'static Mutex<HashMap<PathBuf, u64>> {
    static BUDGETS: std::sync::OnceLock<Mutex<HashMap<PathBuf, u64>>> = std::sync::OnceLock::new();
    BUDGETS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) fn draining_budget_ms(root: &Path) -> Option<u64> {
    draining_budgets().lock().unwrap().get(root).copied()
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum DrainingClockEvent {
    Sleeping { from: u64, until: u64 },
    Stopped,
}

pub(super) struct DrainingClock {
    pub(super) ticks: tokio::sync::watch::Sender<u64>,
    pub(super) events: tokio::sync::mpsc::UnboundedSender<DrainingClockEvent>,
    pub(super) retry_jitter_ms: std::sync::atomic::AtomicU64,
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

pub(super) fn draining_clocks() -> &'static Mutex<HashMap<PathBuf, Arc<DrainingClock>>> {
    static CLOCKS: std::sync::OnceLock<Mutex<HashMap<PathBuf, Arc<DrainingClock>>>> =
        std::sync::OnceLock::new();
    CLOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Test clocks and budgets are keyed by the canonical project root. A test names
/// its temporary directory as created (on macOS under the `/var` symlink), while
/// a worker started through the bash handler records the resolved checkout path
/// (under `/private/var`). Without one key form the worker misses the paused
/// clock, sleeps on real time, and the test waits for an event that never comes.
pub(super) fn test_clock_key(root: &Path) -> PathBuf {
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

pub(super) fn stopped_draining_roots() -> &'static Mutex<HashSet<PathBuf>> {
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

pub(super) fn refused_resubmission_roots() -> &'static Mutex<HashSet<PathBuf>> {
    static ROOTS: std::sync::OnceLock<Mutex<HashSet<PathBuf>>> = std::sync::OnceLock::new();
    ROOTS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Make the next retry resubmission under `root` fail as a call that never
/// reached the runner.
pub(super) fn refuse_next_resubmission(root: &Path) {
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

pub(super) type DrainingGateKey = (PathBuf, DrainingGate);

pub(super) fn draining_gates() -> &'static Mutex<
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

pub(super) fn failed_owed_cancel_attempts() -> &'static Mutex<HashMap<String, usize>> {
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

pub(super) type OwedCancelSaveGate = (
    tokio::sync::oneshot::Sender<()>,
    std::sync::mpsc::Receiver<()>,
);

pub(super) fn owed_cancel_save_gates() -> &'static Mutex<HashMap<String, OwedCancelSaveGate>> {
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

pub(super) fn hinted_retry_budgets() -> &'static Mutex<HashMap<PathBuf, u64>> {
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

pub(super) fn lost_submission_roots() -> &'static Mutex<HashSet<PathBuf>> {
    static ROOTS: std::sync::OnceLock<Mutex<HashSet<PathBuf>>> = std::sync::OnceLock::new();
    ROOTS.get_or_init(|| Mutex::new(HashSet::new()))
}

pub(super) fn lose_submission_replies(root: &Path) -> bool {
    lost_submission_roots()
        .lock()
        .unwrap()
        .contains(&test_clock_key(root))
}
