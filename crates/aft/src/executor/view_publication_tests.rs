use super::*;
use crossbeam_channel::{Receiver, Sender};
use std::{path::Path, time::Duration};

#[test]
fn semantic_index_wait_is_classified_without_changing_retry_backoff() {
    let message = crate::context::VIEW_PUBLICATION_WAITING_FOR_SEMANTIC_INDEX;
    assert_eq!(
        classify_publication_failure(message),
        PublicationFailureKind::SemanticIndexWaiting
    );
    assert_eq!(
        classify_publication_failure("view I/O failed writing manifest: permission denied"),
        PublicationFailureKind::Failure
    );

    let root = tempfile::tempdir().unwrap();
    let mut retry = PublicationRetry::default();
    retry.failed(root.path(), message);
    assert_eq!(retry.failures, 1);
    assert_eq!(retry.delay, Duration::from_secs(1));
    retry.failed(root.path(), message);
    assert_eq!(retry.failures, 2);
    assert_eq!(retry.delay, Duration::from_secs(2));
    assert!(retry.due().is_some());
}

#[test]
fn semantic_index_waits_do_not_silence_the_first_real_failure() {
    let root = tempfile::tempdir().unwrap();
    let failures = Failures::new(root.path().to_path_buf());
    let waiting = crate::context::VIEW_PUBLICATION_WAITING_FOR_SEMANTIC_INDEX;
    let real = "view I/O failed writing semantic blobs: permission denied";
    let mut retry = PublicationRetry::default();
    retry.failed(root.path(), waiting);
    retry.failed(root.path(), waiting);
    assert!(!failures.next().3, "a semantic-index wait must not warn");
    assert!(!failures.next().3, "a semantic-index wait must not warn");
    retry.failed(root.path(), real);
    let (_, delay, count, warned) = failures.next();
    assert!(warned, "the first real failure after waits must warn");
    assert_eq!(count, 3, "waits still count toward the shared retry streak");
    assert_eq!(delay, Duration::from_secs(4), "retry backoff is unchanged");
    retry.failed(root.path(), real);
    assert!(failures.next().3, "the second real failure warns too");
    retry.failed(root.path(), real);
    assert!(
        !failures.next().3,
        "a repeat inside the log interval is counted, not logged"
    );
}

/// A publication that keeps failing over a missing payload stays visible:
/// health names a typed reason, and the log keeps reporting the streak once
/// per interval with the number of retries it did not log.
#[test]
fn persistent_missing_payload_failures_keep_logging_and_report_a_typed_reason() {
    let root = tempfile::tempdir().unwrap();
    let failures = Failures::new(root.path().to_path_buf());
    let error = "manifest references missing Callgraph blob 00ff";
    assert_eq!(
        classify_publication_failure(error),
        PublicationFailureKind::MissingPayload
    );
    let mut retry = PublicationRetry::default();
    assert!(retry.failure_snapshot().is_none());
    for _ in 0..3 {
        retry.failed(root.path(), error);
    }
    assert!(failures.next().3);
    assert!(failures.next().3);
    assert!(!failures.next().3, "inside the log interval");
    assert_eq!(retry.suppressed, 1);

    // Every later backoff step that falls outside the interval is logged.
    for repeat in 4..=6 {
        retry.last_warned = Instant::now().checked_sub(retry.log_interval);
        retry.failed(root.path(), error);
        let (_, _, count, warned) = failures.next();
        assert_eq!(count, repeat);
        assert!(warned, "backoff step {repeat} is logged");
        assert_eq!(retry.suppressed, 0);
    }

    let failure = retry
        .failure_snapshot()
        .expect("a failing streak is reported");
    assert_eq!(failure.reason, "missing_payload");
    assert_eq!(failure.repeat_count, 6);
    assert_eq!(failure.error, error);
    assert!(failure.next_retry_ms.is_some());
    retry.reset(root.path());
    assert!(retry.failure_snapshot().is_none());
}

static CAS_TIMINGS: LazyLock<Mutex<HashMap<PathBuf, Vec<Duration>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
pub(super) fn record_cas(root: PathBuf, elapsed: Duration) {
    eprintln!(
        "view pointer CAS and handle swap: {} ms",
        elapsed.as_millis()
    );
    CAS_TIMINGS.lock().entry(root).or_default().push(elapsed);
}

static SETTLED: LazyLock<Mutex<HashMap<PathBuf, Sender<Completion>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CompletionOutcome {
    Published,
    Noop,
    Failed { reason: String },
    Cancelled { reason: String },
}

#[derive(Debug)]
pub(crate) struct Completion {
    pub(crate) root: PathBuf,
    pub(crate) id: u64,
    pub(crate) outcome: CompletionOutcome,
}

type Failure = (Instant, Duration, usize, bool);

#[test]
fn standalone_queued_writer_blocks_new_readers_but_not_nested_dispatch() {
    let outer = standalone_request_gate();
    let (writer_tx, writer_rx) = crossbeam_channel::bounded(1);
    let writer = std::thread::spawn(move || {
        let _write = STANDALONE_EPOCH.epoch.write();
        writer_tx.send(()).unwrap();
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while STANDALONE_EPOCH.epoch.try_read().is_some() {
        assert!(Instant::now() < deadline, "writer must reach the read gate");
        std::thread::yield_now();
    }
    let nested = standalone_request_gate();
    let admitted_new_reader = std::thread::spawn(|| {
        if standalone_read_is_nested() {
            STANDALONE_EPOCH.epoch.try_read_recursive().is_some()
        } else {
            STANDALONE_EPOCH.epoch.try_read().is_some()
        }
    })
    .join()
    .unwrap();
    drop(nested);
    drop(outer);
    writer_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("writer progress after readers finish");
    writer.join().unwrap();
    assert!(
        !admitted_new_reader,
        "an unrelated deferred reader must not bypass a queued publication writer"
    );
}
static FAILURES: LazyLock<Mutex<HashMap<PathBuf, Sender<Failure>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(super) fn record_failure(root: &Path, delay: Duration, count: usize, warned: bool) {
    if let Some(tx) = FAILURES.lock().get(root) {
        let _ = tx.send((Instant::now(), delay, count, warned));
    }
}

struct Failures {
    root: PathBuf,
    rx: Receiver<Failure>,
}
impl Failures {
    fn new(root: PathBuf) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        FAILURES.lock().insert(root.clone(), tx);
        Self { root, rx }
    }
    fn next(&self) -> Failure {
        self.rx
            .recv_timeout(Duration::from_secs(60))
            .expect("publication failure")
    }
}
impl Drop for Failures {
    fn drop(&mut self) {
        FAILURES.lock().remove(&self.root);
    }
}

pub(super) fn settled(id: u64, root: &Path, outcome: CompletionOutcome) {
    if let Some(tx) = SETTLED.lock().get(root) {
        let _ = tx.send(Completion {
            root: root.to_owned(),
            id,
            outcome,
        });
    }
}

pub(crate) struct Completions {
    root: PathBuf,
    rx: Receiver<Completion>,
}
impl Completions {
    pub(crate) fn new(root: PathBuf) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        SETTLED.lock().insert(root.clone(), tx);
        Self { root, rx }
    }
    pub(crate) fn next_event(&self) -> Completion {
        self.rx
            .recv_timeout(Duration::from_secs(60))
            .unwrap_or_else(|error| {
                panic!(
                    "missing publication-settled event for root {} within 60s: {error}",
                    self.root.display()
                )
            })
    }
}
impl Drop for Completions {
    fn drop(&mut self) {
        SETTLED.lock().remove(&self.root);
    }
}

#[test]
fn publication_completion_reports_a_worker_panic_as_failure() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().to_owned();
    let completions = Completions::new(root.clone());
    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let token = JobCancellation::new().with_root(&root);
    token.mark_running();
    JOBS.lock().insert(
        id,
        Running {
            root: root.clone(),
            context: 0,
            content_generation: 0,
            token,
            phase: "manifest".into(),
            started: Instant::now(),
            phase_started: Instant::now(),
            paths: BTreeSet::new(),
            rescan: false,
            pending: None,
            completion_outcome: Some(CompletionOutcome::Published),
        },
    );
    let lifecycle = Lifecycle(id);
    assert!(std::thread::spawn(move || {
        let _lifecycle = lifecycle;
        panic!("injected publication worker panic");
    })
    .join()
    .is_err());
    let event = completions.next_event();
    assert_eq!(event.root, root);
    assert_eq!(event.id, id);
    assert_eq!(
        event.outcome,
        CompletionOutcome::Failed {
            reason: "publication worker panicked during manifest".into()
        }
    );
}

struct GateEntry {
    phase: &'static str,
    /// Matching phase reports to let through before the gate holds one.
    skip: usize,
    started: Sender<u64>,
    release: Receiver<()>,
}
static GATES: LazyLock<Mutex<HashMap<PathBuf, GateEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static PHASES: LazyLock<Mutex<HashMap<PathBuf, Vec<(u64, String)>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(super) fn phase_gate(id: u64, root: &Path, phase: &str) {
    PHASES
        .lock()
        .entry(root.to_owned())
        .or_default()
        .push((id, phase.to_owned()));
    let gate = {
        let mut gates = GATES.lock();
        match gates.get_mut(root) {
            Some(gate) if gate.phase == phase && gate.skip > 0 => {
                gate.skip -= 1;
                None
            }
            Some(gate) if gate.phase == phase => gates.remove(root),
            _ => None,
        }
    };
    if let Some(gate) = gate {
        let _ = gate.started.send(id);
        // Only dropping Gate releases the build, even if a test's hang guard fires.
        gate.release.recv().expect("release publication gate");
    }
}

#[test]
fn publication_failure_streak_survives_changing_artifact_error_paths() {
    let root = tempfile::tempdir().unwrap();
    let mut retry = PublicationRetry::default();
    retry.failed(
        root.path(),
        "view I/O failed writing trigram-1.bin: Permission denied",
    );
    retry.failed(
        root.path(),
        "view I/O failed writing trigram-2.bin: Permission denied",
    );
    assert_eq!(retry.failures, 2);
    assert_eq!(retry.delay, Duration::from_secs(2));
}
