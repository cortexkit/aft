use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
#[cfg(test)]
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use tempfile::TempDir;

use super::*;

use crate::path_identity::ProjectRootId;
use crate::protocol::Response;

pub fn ok(id: impl Into<String>) -> Response {
    Response::success(id, serde_json::json!({"ok": true}))
}

#[path = "stale_write_tests.rs"]
#[cfg_attr(not(test), allow(dead_code, unused_imports))]
pub mod stale_write;

pub fn test_root(label: &str) -> (TempDir, ProjectRootId) {
    let dir = tempfile::Builder::new()
        .prefix(&format!("aft-executor-{label}-"))
        .tempdir()
        .expect("create temp actor root");
    let root = ProjectRootId::from_path(dir.path()).expect("canonicalize actor root");
    (dir, root)
}

pub fn observe_max(max_seen: &AtomicUsize, value: usize) {
    let mut current = max_seen.load(Ordering::Acquire);
    while value > current {
        match max_seen.compare_exchange(current, value, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => break,
            Err(next) => current = next,
        }
    }
}

const ASYNC_COMPLETION_TIMEOUT: Duration = Duration::from_secs(60);

#[track_caller]
pub fn recv_async(rx: tokio::sync::oneshot::Receiver<Response>, awaited: &str) -> Response {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("build current-thread runtime")
        .block_on(async {
            match tokio::time::timeout(ASYNC_COMPLETION_TIMEOUT, rx).await {
                Ok(Ok(response)) => response,
                Ok(Err(_)) => {
                    panic!("async completion sender dropped while awaiting {awaited}")
                }
                Err(_) => panic!(
                    "timed out after {}s awaiting {awaited}",
                    ASYNC_COMPLETION_TIMEOUT.as_secs()
                ),
            }
        })
}

#[test]
fn single_flight() {
    let flight = Arc::new(SingleFlight::<String, usize>::new());
    let build_count = Arc::new(AtomicUsize::new(0));
    let racers = 16;
    let barrier = Arc::new(std::sync::Barrier::new(racers));
    let mut threads = Vec::new();

    for _ in 0..racers {
        let flight = Arc::clone(&flight);
        let build_count = Arc::clone(&build_count);
        let barrier = Arc::clone(&barrier);
        threads.push(thread::spawn(move || {
            barrier.wait();
            flight.get_or_build("resource".to_string(), 7, || -> Result<usize, ()> {
                build_count.fetch_add(1, Ordering::AcqRel);
                thread::sleep(Duration::from_millis(50));
                Ok(42)
            })
        }));
    }

    for thread in threads {
        let value = thread
            .join()
            .expect("single-flight racer joins")
            .expect("single-flight value builds");
        assert_eq!(*value, 42);
    }
    assert_eq!(build_count.load(Ordering::Acquire), 1);
}

#[test]
fn single_flight_clears_building_after_panic_or_error() {
    let flight = SingleFlight::<String, usize>::new();
    let success_count = AtomicUsize::new(0);

    let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _: Result<Arc<usize>, ()> =
            flight.get_or_build("panic-resource".to_string(), 1, || -> Result<usize, ()> {
                panic!("single-flight builder panic")
            });
    }));
    assert!(panic_result.is_err());

    let value = flight
        .get_or_build("panic-resource".to_string(), 1, || -> Result<usize, ()> {
            success_count.fetch_add(1, Ordering::AcqRel);
            Ok(7)
        })
        .expect("panic-cleared key rebuilds");
    assert_eq!(*value, 7);

    let error = flight.get_or_build(
        "error-resource".to_string(),
        1,
        || -> Result<usize, &'static str> { Err("transient build error") },
    );
    assert_eq!(
        error.expect_err("first build returns error"),
        "transient build error"
    );

    let value = flight
        .get_or_build(
            "error-resource".to_string(),
            1,
            || -> Result<usize, &'static str> {
                success_count.fetch_add(1, Ordering::AcqRel);
                Ok(8)
            },
        )
        .expect("error-cleared key rebuilds");
    assert_eq!(*value, 8);
    assert_eq!(success_count.load(Ordering::Acquire), 2);
}

#[test]
fn detached_thread_installer_rehomes_job_cancellation() {
    let token = JobCancellation::new();
    let worker_token = token.clone();
    let (started_tx, started_rx) = crossbeam_channel::bounded(1);
    let (finished_tx, finished_rx) = crossbeam_channel::bounded(1);
    let worker = std::thread::spawn(move || {
        let _installed = install_job_cancellation(worker_token);
        started_tx.send(()).expect("detached worker starts");
        while !current_job_cancellation()
            .is_some_and(|current| current.cancel_requested_before_commit())
        {
            std::thread::sleep(Duration::from_millis(2));
        }
        finished_tx
            .send(())
            .expect("detached worker observes cancel");
    });
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("detached worker installs cancellation");
    token.request_cancel();
    finished_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("detached worker observes cancellation promptly");
    worker.join().expect("detached worker exits");
}

#[test]
fn cancel_and_seal_race_has_exactly_one_winner() {
    // The canceller and the committing job race on the shared state machine;
    // whatever interleaving occurs, exactly one side wins: seal-won means the
    // cancel reports committed, cancel-won means the seal fails and the job
    // must abort. Both-win (mutate + discarded completion) must be impossible.
    for _ in 0..200 {
        let token = JobCancellation::new();
        token.mark_running();
        let seal_token = token.clone();
        let cancel_token = token.clone();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let seal_barrier = Arc::clone(&barrier);
        let sealer = thread::spawn(move || {
            seal_barrier.wait();
            seal_token.try_seal_committed()
        });
        let canceller = thread::spawn(move || {
            barrier.wait();
            cancel_token.signal_cancel()
        });
        let seal_won = sealer.join().expect("sealer thread");
        let cancel_observed = canceller.join().expect("canceller thread");
        if seal_won {
            // The job proceeds to mutate; the canceller must know the commit
            // happened (observed committed, i.e. RunningCommitted outcome).
            assert_eq!(
                cancel_observed, JOB_CANCEL_STATE_COMMITTED,
                "seal won but canceller was told the job would abort"
            );
            assert!(!token.cancel_requested_before_commit());
        } else {
            // The cancel won; the job must abort and later checkpoints agree.
            assert_ne!(
                cancel_observed, JOB_CANCEL_STATE_COMMITTED,
                "cancel won but observed a committed state"
            );
            assert!(token.cancel_requested_before_commit());
        }
    }
}

#[test]
fn wait_for_cancellation_does_not_self_deadlock_when_its_root_vanishes_mid_wait() {
    // The full cancellation check can decide the root is gone and signal the
    // job, and signalling takes the same lock the waiter holds. If that check
    // runs while the waiter still holds the lock, the waiter deadlocks on
    // itself, and so does anyone who later signals or observes it (on the live
    // daemon, the health report on the frame loop).
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().join("checkout");
    std::fs::create_dir(&root).expect("root");
    let token = JobCancellation::new().with_root(&root);
    token.mark_running();
    let waiter = token.clone();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let started = Arc::new(std::sync::Barrier::new(2));
    let started_waiter = Arc::clone(&started);
    thread::spawn(move || {
        started_waiter.wait();
        // The root check runs after the wait, so the removal below must land
        // before this wait ends. A 400 ms wait lost that race on a loaded
        // Windows runner; 3 s leaves the removal ample margin.
        let cancelled = waiter.wait_for_cancellation(Duration::from_secs(3));
        let _ = done_tx.send(cancelled);
    });
    started.wait();
    thread::sleep(Duration::from_millis(100));
    std::fs::remove_dir(&root).expect("remove root");
    let cancelled = done_rx
        .recv_timeout(Duration::from_secs(60))
        .expect("wait_for_cancellation deadlocked after its root was removed");
    assert!(cancelled, "a removed root cancels the waiting job");
    // A second signaller (the reaper, or an observer that signals) must not
    // block either.
    let (signal_tx, signal_rx) = std::sync::mpsc::channel();
    let signaller = token.clone();
    thread::spawn(move || {
        signaller.request_cancel();
        let _ = signal_tx.send(());
    });
    signal_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("signalling a job that waited on a removed root blocked");
}

#[test]
fn cancel_already_requested_only_reads_and_never_signals() {
    // Observers such as the health report must not change a job's state by
    // looking at it, even when its root is gone.
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().join("checkout");
    std::fs::create_dir(&root).expect("root");
    let token = JobCancellation::new().with_root(&root);
    token.mark_running();
    std::fs::remove_dir(&root).expect("remove root");
    assert!(!token.cancel_already_requested());
    assert_eq!(token.state(), JOB_CANCEL_STATE_RUNNING);
    assert!(token.cancel_requested_before_commit());
    assert!(token.cancel_already_requested());
}

/// Bound on how long a route bind may wait in the executor before its
/// configure starts. subc refuses a module's binds once relays exceed 12s; the
/// bind path has to stay far inside that no matter what else is running.
pub const BIND_STORM_ACK_BOUND: Duration = Duration::from_secs(3);

/// The response if it arrives within `within`, without consuming the
/// receiver otherwise.
pub fn try_recv_within(
    response: &mut tokio::sync::oneshot::Receiver<Response>,
    within: Duration,
) -> Option<Response> {
    let deadline = Instant::now() + within;
    loop {
        if let Ok(response) = response.try_recv() {
            return Some(response);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(2));
    }
}

pub fn is_stuck_reader_blocker(blocker: &str) -> bool {
    blocker.starts_with("waiting_on_readers_stuck(")
}
