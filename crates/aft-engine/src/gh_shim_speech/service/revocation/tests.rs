use super::super::{new_service, Code, LaunchAuthority};
use super::*;
use std::path::Path;
use std::sync::{mpsc, Mutex};
use subc_protocol::scope::{ScopeAttributes, ScopeKind, ScopeStamp};

const INCARNATIONS: [&str; 3] = ["inc-1", "inc-2", "inc-3"];
const WAIT: Duration = Duration::from_secs(10);

struct Pause {
    delay: Duration,
    resume: mpsc::Sender<()>,
}

struct State {
    now: Instant,
    failures_per_incarnation: Option<usize>,
    attempts: HashMap<String, usize>,
}

#[derive(Clone)]
struct FakeStore {
    state: Arc<Mutex<State>>,
    sleeping: mpsc::Sender<Pause>,
}

impl FakeStore {
    fn new(failures_per_incarnation: Option<usize>) -> (Self, mpsc::Receiver<Pause>) {
        let (sleeping, pauses) = mpsc::channel();
        let state = Arc::new(Mutex::new(State {
            now: Instant::now(),
            failures_per_incarnation,
            attempts: HashMap::new(),
        }));
        (Self { state, sleeping }, pauses)
    }

    fn assert_attempts_per_incarnation(&self, expected: usize) {
        let state = self.state.lock().unwrap();
        for inc in INCARNATIONS {
            assert_eq!(state.attempts[inc], expected, "{inc}");
        }
    }
}

impl RevocationIo for FakeStore {
    fn write(&self, _store: &Store, incarnation: &str, reason: &str) -> Result<(), StorageError> {
        assert_eq!(reason, "terminal");
        let mut state = self.state.lock().unwrap();
        let failures = state.failures_per_incarnation;
        let attempts = state.attempts.entry(incarnation.to_owned()).or_default();
        *attempts += 1;
        if failures.is_none_or(|failures| *attempts <= failures) {
            Err(StorageError::SchemaTooOld(21))
        } else {
            Ok(())
        }
    }

    fn sleep(&self, delay: Duration) {
        let (resume, resumed) = mpsc::channel();
        self.sleeping.send(Pause { delay, resume }).unwrap();
        resumed.recv_timeout(WAIT).unwrap();
        self.state.lock().unwrap().now += delay;
    }

    fn now(&self) -> Instant {
        self.state.lock().unwrap().now
    }
}

fn serving(root: &Path) -> Arc<Service> {
    let service = new_service(root.to_owned());
    *service.lease.lock().unwrap() =
        Some(crate::cache_location::acquire_speech_serving_lease(root).unwrap());
    service
}

fn queue_three(service: &Arc<Service>, io: &FakeStore) -> JoinHandle<()> {
    let mut worker = None;
    for inc in INCARNATIONS {
        let (result, started) = service.revoke_using(inc, "terminal", io.clone());
        let refusal = result.unwrap_err();
        assert_eq!(refusal.code, Code::AuthorityUnavailable);
        assert_eq!(refusal.reason, "snapshot_invalid");
        if let Some(started) = started {
            assert!(
                worker.replace(started).is_none(),
                "started a second retry thread"
            );
        }
    }
    worker.expect("started one retry thread")
}

fn assert_pending(service: &Service, pending: bool) {
    let head = ScopeStamp::new(
        subc_protocol::Principal::Reserved {
            module_id: "prefrontal-core".into(),
        },
        "head",
        1,
        ScopeKind::Head,
    )
    .with_attributes(
        ScopeAttributes::new()
            .with_agent_id(Some("agent-a".into()))
            .with_delegates(true),
    )
    .with_owner_authorized(true);
    let launch = LaunchAuthority::classic(head, None).unwrap();
    for inc in INCARNATIONS {
        let result = service.check_pending_in_memory(Some(inc), &launch, "session", "task");
        if pending {
            assert_eq!(result.unwrap_err().reason, "revocation_pending", "{inc}");
        } else {
            result.unwrap();
        }
    }
}

fn next_pause(pauses: &mpsc::Receiver<Pause>, millis: u64) -> mpsc::Sender<()> {
    let pause = pauses.recv_timeout(WAIT).unwrap();
    assert_eq!(pause.delay, Duration::from_millis(millis));
    pause.resume
}

fn join_drained(worker: JoinHandle<()>, io: FakeStore, pauses: mpsc::Receiver<Pause>) {
    drop(io);
    assert!(matches!(
        pauses.recv_timeout(WAIT),
        Err(mpsc::RecvTimeoutError::Disconnected)
    ));
    worker.join().unwrap();
}

#[test]
fn persistent_revocations_share_one_worker_and_keep_retirement_blocked() {
    let dir = tempfile::tempdir().unwrap();
    let service = serving(dir.path());
    let runtime = service.runtime_guard();
    let (io, pauses) = FakeStore::new(None);
    let worker = queue_three(&service, &io);

    assert_pending(&service, true);
    let resume = next_pause(&pauses, 100);
    assert!(!runtime.retire());
    assert!(crate::cache_location::acquire_speech_serving_lease(dir.path()).is_err());
    resume.send(()).unwrap();
    let mut resume = next_pause(&pauses, 200);
    io.assert_attempts_per_incarnation(2);
    for delay in [400, 800, 1_600, 3_200, 5_000] {
        resume.send(()).unwrap();
        resume = next_pause(&pauses, delay);
    }
    // Advance past five minutes without using wall-clock sleeps.
    for _ in 0..61 {
        resume.send(()).unwrap();
        resume = next_pause(&pauses, 5_000);
    }
    assert_pending(&service, true);
    assert!(!runtime.retire());

    // Restore storage for teardown so the test does not leave a retry thread.
    io.state.lock().unwrap().failures_per_incarnation = Some(0);
    resume.send(()).unwrap();
    join_drained(worker, io, pauses);
    assert!(runtime.retire());
}

#[test]
fn shared_worker_clears_all_pending_revocations_when_storage_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let service = serving(dir.path());
    let runtime = service.runtime_guard();
    let (io, pauses) = FakeStore::new(Some(2));
    let worker = queue_three(&service, &io);
    next_pause(&pauses, 100).send(()).unwrap();
    let resume = next_pause(&pauses, 200);
    io.assert_attempts_per_incarnation(2);
    assert_pending(&service, true);
    assert!(!runtime.retire());

    let attempts = io.state.clone();
    resume.send(()).unwrap();
    join_drained(worker, io, pauses);
    for inc in INCARNATIONS {
        assert_eq!(attempts.lock().unwrap().attempts[inc], 3, "{inc}");
    }
    assert_pending(&service, false);
    assert!(runtime.retire());
    let successor = crate::cache_location::acquire_speech_serving_lease(dir.path()).unwrap();
    assert!(crate::cache_location::release_quiescent_speech_serving_lease(&successor));
}
